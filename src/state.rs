//! Jam sessions, held in memory.
//!
//! A session is a code, the host's playback, the shared queue, the members
//! and a short list of commands for the host. It costs a few kilobytes, so
//! nothing is written to disk: a restarted server simply forgets its jams,
//! and their listeners are told the jam is gone.
//!
//! Every method takes the time as Unix milliseconds so expiry can be tested
//! without waiting for it.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use rand::Rng;
use tokio::sync::watch;

use crate::protocol::*;

/// Characters a session code is made of: no 0/O or 1/I to misread.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 6;

#[derive(Clone, Debug)]
pub struct Config {
    /// A host not heard from for this long ends the jam.
    pub host_grace_ms: u64,
    /// A listener not heard from for this long leaves it.
    pub member_timeout_ms: u64,
    /// How long an ended jam still answers, so listeners learn it ended.
    pub ended_retention_ms: u64,
    /// A jam nobody has touched for this long is forgotten.
    pub idle_ttl_ms: u64,
    pub max_sessions: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host_grace_ms: 5 * 60 * 1000,
            member_timeout_ms: 90 * 1000,
            ended_retention_ms: 10 * 60 * 1000,
            idle_ttl_ms: 12 * 60 * 60 * 1000,
            max_sessions: 10_000,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotFound,
    Ended,
    Unauthorized,
    Forbidden(&'static str),
    Invalid(String),
    Full(&'static str),
}

struct MemberState {
    id: String,
    name: String,
    last_seen: u64,
}

struct Session {
    id: String,
    version: u64,
    host_id: String,
    members: Vec<MemberState>,
    playback: Playback,
    queue: Vec<QueueItem>,
    commands: VecDeque<CommandEntry>,
    next_seq: u64,
    next_item: u64,
    ended_at: Option<u64>,
    last_activity: u64,
    changed: watch::Sender<u64>,
}

impl Session {
    fn bump(&mut self) {
        self.version += 1;
        self.changed.send_replace(self.version);
    }

    fn view(&self, now: u64) -> SessionView {
        SessionView {
            id: self.id.clone(),
            version: self.version,
            server_time_ms: now,
            host_id: self.host_id.clone(),
            members: self
                .members
                .iter()
                .map(|member| Member {
                    id: member.id.clone(),
                    name: member.name.clone(),
                    host: member.id == self.host_id,
                })
                .collect(),
            playback: self.playback.clone(),
            queue: self.queue.clone(),
            commands: self.commands.iter().cloned().collect(),
            ended: self.ended_at.is_some(),
        }
    }

    fn member(&self, id: &str) -> Option<&MemberState> {
        self.members.iter().find(|member| member.id == id)
    }

    fn end(&mut self, now: u64) {
        if self.ended_at.is_none() {
            self.ended_at = Some(now);
            self.playback.playing = false;
            self.bump();
        }
    }
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    /// Token to (session code, member id).
    tokens: HashMap<String, (String, String)>,
}

pub struct Store {
    inner: Mutex<Inner>,
    config: Config,
}

/// Who a request comes from, once its token checks out.
struct Caller {
    member_id: String,
}

impl Store {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            config,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[cfg(test)]
    pub fn session_count(&self) -> usize {
        self.lock().sessions.len()
    }

    /// Start a jam with `name` as its host.
    pub fn create(&self, name: &str, now: u64) -> Result<JoinResponse, Error> {
        let name = clean_name(name)?;
        let mut inner = self.lock();
        if inner.sessions.len() >= self.config.max_sessions {
            return Err(Error::Full("the server holds as many jams as it can"));
        }
        let mut rng = rand::rng();
        let code = loop {
            let code: String = (0..CODE_LEN)
                .map(|_| CODE_ALPHABET[rng.random_range(0..CODE_ALPHABET.len())] as char)
                .collect();
            if !inner.sessions.contains_key(&code) {
                break code;
            }
        };
        let member_id = new_member_id(&mut rng);
        let token = new_token(&mut rng);
        let (changed, _) = watch::channel(1);
        let session = Session {
            id: code.clone(),
            version: 1,
            host_id: member_id.clone(),
            members: vec![MemberState {
                id: member_id.clone(),
                name,
                last_seen: now,
            }],
            playback: Playback {
                updated_at_ms: now,
                ..Playback::default()
            },
            queue: Vec::new(),
            commands: VecDeque::new(),
            next_seq: 1,
            next_item: 1,
            ended_at: None,
            last_activity: now,
            changed,
        };
        let view = session.view(now);
        inner.sessions.insert(code.clone(), session);
        inner
            .tokens
            .insert(token.clone(), (code, member_id.clone()));
        Ok(JoinResponse {
            session: view,
            member_id,
            token,
        })
    }

    pub fn join(&self, code: &str, name: &str, now: u64) -> Result<JoinResponse, Error> {
        let name = clean_name(name)?;
        let code = normalize_code(code).ok_or(Error::NotFound)?;
        let mut inner = self.lock();
        let session = inner.sessions.get_mut(&code).ok_or(Error::NotFound)?;
        if session.ended_at.is_some() {
            return Err(Error::Ended);
        }
        if session.members.len() >= MAX_MEMBERS {
            return Err(Error::Full("this jam is full"));
        }
        let mut rng = rand::rng();
        let member_id = new_member_id(&mut rng);
        let token = new_token(&mut rng);
        session.members.push(MemberState {
            id: member_id.clone(),
            name,
            last_seen: now,
        });
        session.last_activity = now;
        session.bump();
        let view = session.view(now);
        inner
            .tokens
            .insert(token.clone(), (code, member_id.clone()));
        Ok(JoinResponse {
            session: view,
            member_id,
            token,
        })
    }

    /// Checks the token belongs to `code` and notes the member as present.
    fn authorize<'a>(
        inner: &'a mut Inner,
        code: &str,
        token: &str,
        now: u64,
    ) -> Result<(&'a mut Session, Caller), Error> {
        let code = normalize_code(code).ok_or(Error::NotFound)?;
        let (owner, member_id) = inner.tokens.get(token).ok_or(Error::Unauthorized)?.clone();
        if owner != code {
            return Err(Error::Unauthorized);
        }
        let session = inner.sessions.get_mut(&code).ok_or(Error::NotFound)?;
        let member = session
            .members
            .iter_mut()
            .find(|member| member.id == member_id)
            // Timed out while away: the token no longer names a listener.
            .ok_or(Error::Unauthorized)?;
        member.last_seen = now;
        session.last_activity = now;
        Ok((session, Caller { member_id }))
    }

    /// The jam as `token`'s member sees it, and a channel that changes with
    /// it for a long poll to wait on.
    pub fn view(
        &self,
        code: &str,
        token: &str,
        now: u64,
    ) -> Result<(SessionView, watch::Receiver<u64>), Error> {
        let mut inner = self.lock();
        let (session, _) = Self::authorize(&mut inner, code, token, now)?;
        Ok((session.view(now), session.changed.subscribe()))
    }

    /// The host leaving ends the jam; anyone else just leaves it.
    pub fn leave(&self, code: &str, token: &str, now: u64) -> Result<(), Error> {
        let mut inner = self.lock();
        let (session, caller) = Self::authorize(&mut inner, code, token, now)?;
        if caller.member_id == session.host_id {
            session.end(now);
        } else {
            session
                .members
                .retain(|member| member.id != caller.member_id);
            session.bump();
        }
        inner.tokens.remove(token);
        Ok(())
    }

    pub fn publish(
        &self,
        code: &str,
        token: &str,
        playback: PublishPlayback,
        now: u64,
    ) -> Result<SessionView, Error> {
        let mut inner = self.lock();
        let (session, caller) = Self::authorize(&mut inner, code, token, now)?;
        live(session)?;
        if caller.member_id != session.host_id {
            return Err(Error::Forbidden("only the host publishes playback"));
        }
        let track = playback.track.map(clean_track).transpose()?;
        let context_uri = playback
            .context_uri
            .filter(|uri| !uri.is_empty())
            .map(|uri| clean_uri(&uri))
            .transpose()?;
        let next = Playback {
            track,
            context_uri,
            position_ms: playback.position_ms,
            playing: playback.playing,
            // The host's own stamp, if it is believable: never in the
            // future, never older than a slow upload could make it.
            updated_at_ms: playback.measured_at_ms.map_or(now, |at| {
                at.clamp(now.saturating_sub(MAX_MEASURED_AGE_MS), now)
            }),
            applied_seq: playback.applied_seq.min(session.next_seq.saturating_sub(1)),
            shuffle: playback.shuffle,
            repeat: playback.repeat,
            // Display only: a song that is not one is left out, not refused.
            next_up: playback
                .next_up
                .into_iter()
                .take(MAX_NEXT_UP)
                .filter_map(|track| clean_track(track).ok())
                .collect(),
        };
        session.playback = next;
        session.bump();
        Ok(session.view(now))
    }

    pub fn add(
        &self,
        code: &str,
        token: &str,
        add: AddToQueue,
        now: u64,
    ) -> Result<SessionView, Error> {
        let mut inner = self.lock();
        let (session, caller) = Self::authorize(&mut inner, code, token, now)?;
        live(session)?;
        if add.tracks.is_empty() {
            return Ok(session.view(now));
        }
        if add.tracks.len() > MAX_ADD {
            return Err(Error::Invalid(format!(
                "add at most {MAX_ADD} songs at once"
            )));
        }
        if session.queue.len() + add.tracks.len() > MAX_QUEUE {
            return Err(Error::Full("the jam's queue is full"));
        }
        let tracks = add
            .tracks
            .into_iter()
            .map(clean_track)
            .collect::<Result<Vec<_>, _>>()?;
        let added_by_name = session
            .member(&caller.member_id)
            .map(|member| member.name.clone())
            .unwrap_or_default();
        let at = add.position.unwrap_or(usize::MAX).min(session.queue.len());
        let items: Vec<QueueItem> = tracks
            .into_iter()
            .map(|track| {
                let id = format!("q{}", session.next_item);
                session.next_item += 1;
                QueueItem {
                    id,
                    track,
                    added_by: caller.member_id.clone(),
                    added_by_name: added_by_name.clone(),
                }
            })
            .collect();
        session.queue.splice(at..at, items);
        session.bump();
        Ok(session.view(now))
    }

    /// Anyone in the jam takes out any song, as the host can. Ids no
    /// longer queued are ignored: the song already played.
    pub fn remove(
        &self,
        code: &str,
        token: &str,
        remove: RemoveFromQueue,
        now: u64,
    ) -> Result<SessionView, Error> {
        let mut inner = self.lock();
        let (session, _) = Self::authorize(&mut inner, code, token, now)?;
        live(session)?;
        let before = session.queue.len();
        session.queue.retain(|item| !remove.ids.contains(&item.id));
        if session.queue.len() != before {
            session.bump();
        }
        Ok(session.view(now))
    }

    /// Anyone in the jam reorders its queue. A song no longer queued is
    /// ignored: it already played.
    pub fn reorder(
        &self,
        code: &str,
        token: &str,
        movement: MoveInQueue,
        now: u64,
    ) -> Result<SessionView, Error> {
        let mut inner = self.lock();
        let (session, _) = Self::authorize(&mut inner, code, token, now)?;
        live(session)?;
        if let Some(from) = session.queue.iter().position(|item| item.id == movement.id) {
            let item = session.queue.remove(from);
            let to = movement.to.min(session.queue.len());
            session.queue.insert(to, item);
            if from != to {
                session.bump();
            }
        }
        Ok(session.view(now))
    }

    pub fn command(
        &self,
        code: &str,
        token: &str,
        command: Command,
        now: u64,
    ) -> Result<CommandAccepted, Error> {
        let mut inner = self.lock();
        let (session, caller) = Self::authorize(&mut inner, code, token, now)?;
        live(session)?;
        check_command(&command)?;
        let from_name = session
            .member(&caller.member_id)
            .map(|member| member.name.clone())
            .unwrap_or_default();
        let seq = session.next_seq;
        session.next_seq += 1;
        session.commands.push_back(CommandEntry {
            seq,
            from: caller.member_id,
            from_name,
            command,
        });
        while session.commands.len() > KEPT_COMMANDS {
            session.commands.pop_front();
        }
        session.bump();
        Ok(CommandAccepted {
            seq,
            session: session.view(now),
        })
    }

    /// Lets quiet listeners go, ends jams whose host is gone and forgets old
    /// ones. Run every few seconds.
    pub fn sweep(&self, now: u64) {
        let config = &self.config;
        let mut inner = self.lock();
        let mut forget = Vec::new();
        for (code, session) in inner.sessions.iter_mut() {
            if let Some(ended_at) = session.ended_at {
                if now.saturating_sub(ended_at) > config.ended_retention_ms {
                    forget.push(code.clone());
                }
                continue;
            }
            if now.saturating_sub(session.last_activity) > config.idle_ttl_ms {
                forget.push(code.clone());
                continue;
            }
            let host_seen = session
                .member(&session.host_id)
                .map_or(0, |host| host.last_seen);
            if now.saturating_sub(host_seen) > config.host_grace_ms {
                session.end(now);
                continue;
            }
            let host_id = session.host_id.clone();
            let before = session.members.len();
            session.members.retain(|member| {
                member.id == host_id
                    || now.saturating_sub(member.last_seen) <= config.member_timeout_ms
            });
            if session.members.len() != before {
                session.bump();
            }
        }
        for code in &forget {
            // Dropping the session drops its watch sender, which wakes its
            // long polls to find it gone.
            inner.sessions.remove(code);
        }
        let Inner { sessions, tokens } = &mut *inner;
        // An ended jam keeps its tokens so its listeners can still learn it
        // ended; a live one only those of members still in it.
        tokens.retain(|_, (code, member_id)| {
            sessions.get(code).is_some_and(|session| {
                session.ended_at.is_some() || session.member(member_id).is_some()
            })
        });
    }
}

fn live(session: &Session) -> Result<(), Error> {
    if session.ended_at.is_some() {
        Err(Error::Ended)
    } else {
        Ok(())
    }
}

/// Codes are typed by people: accept any case and stray spaces.
pub fn normalize_code(code: &str) -> Option<String> {
    let code: String = code
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-'))
        .map(|c| c.to_ascii_uppercase())
        .collect();
    (code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

fn new_member_id(rng: &mut impl Rng) -> String {
    format!("m{:012x}", rng.random::<u64>() & 0xffff_ffff_ffff)
}

fn new_token(rng: &mut impl Rng) -> String {
    let bytes: [u8; 32] = rng.random();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn clean_text(text: &str, max: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn clean_name(name: &str) -> Result<String, Error> {
    let name = clean_text(name, MAX_NAME);
    if name.is_empty() {
        Err(Error::Invalid("a name is needed".into()))
    } else {
        Ok(name)
    }
}

fn clean_uri(uri: &str) -> Result<String, Error> {
    let shaped = uri.starts_with("spotify:")
        && uri.len() <= MAX_URI
        && uri.bytes().all(|byte| byte.is_ascii_graphic());
    if shaped {
        Ok(uri.to_owned())
    } else {
        Err(Error::Invalid(format!("not a Spotify URI: {uri:.60}")))
    }
}

fn clean_track(track: Track) -> Result<Track, Error> {
    Ok(Track {
        uri: clean_uri(&track.uri)?,
        title: clean_text(&track.title, MAX_TEXT),
        artists: clean_text(&track.artists, MAX_TEXT),
        duration_ms: track.duration_ms,
        image_url: track
            .image_url
            .filter(|url| url.starts_with("https://") && url.len() <= MAX_TEXT)
            .filter(|url| url.bytes().all(|byte| byte.is_ascii_graphic())),
    })
}

fn check_command(command: &Command) -> Result<(), Error> {
    match command {
        Command::PlayContext {
            context_uri,
            offset_uri,
            ..
        } => {
            clean_uri(context_uri)?;
            if let Some(offset) = offset_uri {
                clean_uri(offset)?;
            }
        }
        Command::PlayUris { uris, index } => {
            if uris.is_empty() || uris.len() > MAX_ADD {
                return Err(Error::Invalid(format!("play 1 to {MAX_ADD} songs")));
            }
            if *index as usize >= uris.len() {
                return Err(Error::Invalid("the start is past the last song".into()));
            }
            for uri in uris {
                clean_uri(uri)?;
            }
        }
        Command::PlayQueued { id } => {
            if id.is_empty() || id.len() > 32 {
                return Err(Error::Invalid("not a queued song".into()));
            }
        }
        Command::Play
        | Command::Pause
        | Command::Seek { .. }
        | Command::Next
        | Command::Previous
        | Command::Shuffle { .. }
        | Command::Repeat { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::new(Config::default())
    }

    fn track(uri: &str) -> Track {
        Track {
            uri: uri.into(),
            title: "Song".into(),
            artists: "Artist".into(),
            duration_ms: 180_000,
            image_url: None,
        }
    }

    #[test]
    fn a_created_jam_is_joined_by_its_code_in_any_case() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        assert_eq!(host.session.members.len(), 1);
        assert!(host.session.members[0].host);
        let code = host.session.id.to_lowercase();
        let guest = store.join(&format!(" {code} "), "Guest", 10).unwrap();
        assert_eq!(guest.session.members.len(), 2);
        assert!(!guest.session.members[1].host);
        assert!(guest.session.version > host.session.version);
    }

    #[test]
    fn codes_avoid_misreadable_characters() {
        let store = store();
        for _ in 0..200 {
            let code = store.create("H", 0).unwrap().session.id;
            assert_eq!(code.len(), CODE_LEN);
            assert!(!code.contains(['0', 'O', '1', 'I']));
        }
        assert_eq!(normalize_code("abc-def").as_deref(), Some("ABCDEF"));
        assert_eq!(normalize_code("ABCDE0"), None);
        assert_eq!(normalize_code("ABCDEFG"), None);
    }

    #[test]
    fn only_the_host_publishes_playback() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let guest = store.join(&code, "Guest", 0).unwrap();
        let playback = PublishPlayback {
            track: Some(track("spotify:track:a")),
            context_uri: Some("spotify:album:b".into()),
            position_ms: 1000,
            playing: true,
            applied_seq: 0,
            shuffle: false,
            repeat: Repeat::Off,
            next_up: Vec::new(),
            measured_at_ms: None,
        };
        assert!(matches!(
            store.publish(&code, &guest.token, playback.clone(), 5),
            Err(Error::Forbidden(_))
        ));
        let view = store.publish(&code, &host.token, playback, 5).unwrap();
        assert_eq!(view.playback.position_ms, 1000);
        assert_eq!(view.playback.updated_at_ms, 5);
        assert_eq!(
            view.playback.context_uri.as_deref(),
            Some("spotify:album:b")
        );
    }

    #[test]
    fn tokens_only_open_their_own_jam() {
        let store = store();
        let one = store.create("One", 0).unwrap();
        let two = store.create("Two", 0).unwrap();
        assert_eq!(
            store.view(&one.session.id, &two.token, 0).unwrap_err(),
            Error::Unauthorized
        );
        assert_eq!(
            store.view(&one.session.id, "nonsense", 0).unwrap_err(),
            Error::Unauthorized
        );
    }

    fn add(
        store: &Store,
        code: &str,
        token: &str,
        uri: &str,
        position: Option<usize>,
    ) -> SessionView {
        store
            .add(
                code,
                token,
                AddToQueue {
                    tracks: vec![track(uri)],
                    position,
                },
                1,
            )
            .unwrap()
    }

    fn uris(view: &SessionView) -> Vec<&str> {
        view.queue
            .iter()
            .map(|item| item.track.uri.as_str())
            .collect()
    }

    #[test]
    fn everyone_removes_any_song() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let guest = store.join(&code, "Guest", 0).unwrap();
        let other = store.join(&code, "Other", 0).unwrap();
        add(&store, &code, &host.token, "spotify:track:h", None);
        let view = add(&store, &code, &other.token, "spotify:track:o", None);
        assert_eq!(view.queue[1].added_by_name, "Other");
        let ids = view.queue.iter().map(|item| item.id.clone()).collect();
        let view = store
            .remove(&code, &guest.token, RemoveFromQueue { ids }, 2)
            .unwrap();
        assert!(view.queue.is_empty());
    }

    #[test]
    fn songs_go_where_they_are_put_and_anyone_reorders_them() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let guest = store.join(&code, "Guest", 0).unwrap();
        add(&store, &code, &host.token, "spotify:track:a", None);
        add(&store, &code, &host.token, "spotify:track:b", None);
        let view = add(&store, &code, &guest.token, "spotify:track:next", Some(0));
        assert_eq!(
            uris(&view),
            ["spotify:track:next", "spotify:track:a", "spotify:track:b"]
        );
        let view = add(&store, &code, &guest.token, "spotify:track:end", Some(99));
        assert_eq!(uris(&view).last(), Some(&"spotify:track:end"));

        let b = view.queue[2].id.clone();
        let moved = store
            .reorder(
                &code,
                &guest.token,
                MoveInQueue {
                    id: b.clone(),
                    to: 0,
                },
                3,
            )
            .unwrap();
        assert_eq!(uris(&moved)[0], "spotify:track:b");
        assert!(moved.version > view.version);
        let moved = store
            .reorder(&code, &guest.token, MoveInQueue { id: b, to: 99 }, 3)
            .unwrap();
        assert_eq!(uris(&moved).last(), Some(&"spotify:track:b"));
        // A song that already played is no error.
        let same = store
            .reorder(
                &code,
                &guest.token,
                MoveInQueue {
                    id: "gone".into(),
                    to: 0,
                },
                4,
            )
            .unwrap();
        assert_eq!(same.version, moved.version);
    }

    #[test]
    fn shuffle_and_repeat_travel_with_the_playback() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let view = store
            .publish(
                &host.session.id,
                &host.token,
                PublishPlayback {
                    track: None,
                    context_uri: None,
                    position_ms: 0,
                    playing: false,
                    applied_seq: 0,
                    shuffle: true,
                    repeat: Repeat::Track,
                    measured_at_ms: None,
                    next_up: (0..MAX_NEXT_UP + 5)
                        .map(|n| track(&format!("spotify:track:n{n}")))
                        .chain([Track {
                            uri: "not a uri".into(),
                            ..Track::default()
                        }])
                        .collect(),
                },
                1,
            )
            .unwrap();
        assert!(view.playback.shuffle);
        assert_eq!(view.playback.repeat, Repeat::Track);
        assert_eq!(view.playback.next_up.len(), MAX_NEXT_UP);
    }

    #[test]
    fn bad_songs_are_refused_and_text_is_trimmed() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let bad = Track {
            uri: "https://example.com".into(),
            ..track("x")
        };
        assert!(matches!(
            store.add(
                &code,
                &host.token,
                AddToQueue {
                    tracks: vec![bad],
                    position: None
                },
                0
            ),
            Err(Error::Invalid(_))
        ));
        let long = Track {
            title: "x".repeat(1000),
            image_url: Some("http://insecure".into()),
            ..track("spotify:track:ok")
        };
        let view = store
            .add(
                &code,
                &host.token,
                AddToQueue {
                    tracks: vec![long],
                    position: None,
                },
                0,
            )
            .unwrap();
        assert_eq!(view.queue[0].track.title.chars().count(), MAX_TEXT);
        assert_eq!(view.queue[0].track.image_url, None);
    }

    #[test]
    fn commands_are_numbered_and_only_the_newest_are_kept() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let guest = store.join(&code, "Guest", 0).unwrap();
        let first = store
            .command(&code, &guest.token, Command::Pause, 1)
            .unwrap();
        assert_eq!(first.seq, 1);
        for _ in 0..KEPT_COMMANDS + 5 {
            store
                .command(&code, &guest.token, Command::Next, 1)
                .unwrap();
        }
        let (view, _) = store.view(&code, &guest.token, 2).unwrap();
        assert_eq!(view.commands.len(), KEPT_COMMANDS);
        assert_eq!(view.commands.last().unwrap().seq, KEPT_COMMANDS as u64 + 6);
        assert_eq!(view.commands[0].from_name, "Guest");
        assert!(matches!(
            store.command(
                &code,
                &guest.token,
                Command::PlayUris {
                    uris: vec!["spotify:track:a".into()],
                    index: 1
                },
                3
            ),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn the_host_leaving_ends_the_jam_for_everyone() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let guest = store.join(&code, "Guest", 0).unwrap();
        store.leave(&code, &host.token, 1).unwrap();
        let (view, _) = store.view(&code, &guest.token, 2).unwrap();
        assert!(view.ended);
        assert_eq!(store.join(&code, "Late", 3).unwrap_err(), Error::Ended);
        assert_eq!(
            store
                .command(&code, &guest.token, Command::Play, 3)
                .unwrap_err(),
            Error::Ended
        );
    }

    #[test]
    fn a_guest_leaving_keeps_the_jam_going() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let guest = store.join(&code, "Guest", 0).unwrap();
        store.leave(&code, &guest.token, 1).unwrap();
        let (view, _) = store.view(&code, &host.token, 2).unwrap();
        assert!(!view.ended);
        assert_eq!(view.members.len(), 1);
        assert_eq!(
            store.view(&code, &guest.token, 2).unwrap_err(),
            Error::Unauthorized
        );
    }

    #[test]
    fn sweeping_lets_quiet_listeners_go_ends_hostless_jams_and_forgets_old_ones() {
        let config = Config::default();
        let store = Store::new(config.clone());
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let guest = store.join(&code, "Guest", 0).unwrap();

        // The host keeps polling; the guest goes quiet.
        let later = config.member_timeout_ms + 1;
        store.view(&code, &host.token, later).unwrap();
        store.sweep(later);
        let (view, _) = store.view(&code, &host.token, later).unwrap();
        assert_eq!(view.members.len(), 1);
        assert_eq!(
            store.view(&code, &guest.token, later).unwrap_err(),
            Error::Unauthorized
        );

        // Then the host goes quiet too.
        let gone = later + config.host_grace_ms + 1;
        store.sweep(gone);
        let (view, _) = store.view(&code, &host.token, gone).unwrap();
        assert!(view.ended);

        let forgotten = gone + config.ended_retention_ms + 1;
        store.sweep(forgotten);
        assert_eq!(store.session_count(), 0);
        assert_eq!(
            store.view(&code, &host.token, forgotten).unwrap_err(),
            Error::Unauthorized
        );
    }

    #[test]
    fn the_host_stamps_its_own_place_within_reason() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let publish = |measured_at_ms| PublishPlayback {
            track: Some(track("spotify:track:a")),
            context_uri: None,
            position_ms: 1000,
            playing: true,
            applied_seq: 0,
            shuffle: false,
            repeat: Repeat::Off,
            next_up: Vec::new(),
            measured_at_ms,
        };
        let now = 100_000;
        let at = |stamp| {
            store
                .publish(&code, &host.token, publish(stamp), now)
                .unwrap()
                .playback
                .updated_at_ms
        };
        assert_eq!(at(Some(now - 300)), now - 300, "a slow upload is undone");
        assert_eq!(at(None), now);
        assert_eq!(at(Some(now + 5_000)), now, "never in the future");
        assert_eq!(at(Some(1)), now - MAX_MEASURED_AGE_MS, "never too old");
    }

    #[test]
    fn a_full_server_refuses_new_jams() {
        let store = Store::new(Config {
            max_sessions: 1,
            ..Config::default()
        });
        store.create("One", 0).unwrap();
        assert!(matches!(store.create("Two", 0), Err(Error::Full(_))));
    }

    #[tokio::test]
    async fn a_change_wakes_a_waiting_poll() {
        let store = store();
        let host = store.create("Host", 0).unwrap();
        let code = host.session.id.clone();
        let (view, mut changed) = store.view(&code, &host.token, 0).unwrap();
        let since = view.version;
        store
            .add(
                &code,
                &host.token,
                AddToQueue {
                    tracks: vec![track("spotify:track:a")],
                    position: None,
                },
                1,
            )
            .unwrap();
        let woke = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            changed.wait_for(|version| *version > since),
        )
        .await;
        assert!(woke.is_ok());
    }
}
