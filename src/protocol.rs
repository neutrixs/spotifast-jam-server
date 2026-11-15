//! The wire format every Spotifast jam client speaks.
//!
//! A copy of these types lives in the client (`src/jam/protocol.rs` in
//! spotifast-jam-client). Change both together and bump [`PROTOCOL`] when an
//! older client could no longer understand the server.

use serde::{Deserialize, Serialize};

/// The protocol version `GET /api/v1/info` reports.
pub const PROTOCOL: u32 = 1;

/// Longest display name kept, in characters.
pub const MAX_NAME: usize = 40;
/// Longest Spotify URI accepted.
pub const MAX_URI: usize = 128;
/// Longest title, artist line, or image address kept, in characters.
pub const MAX_TEXT: usize = 300;
/// Most songs a jam queue holds.
pub const MAX_QUEUE: usize = 500;
/// Most songs one request may add.
pub const MAX_ADD: usize = 100;
/// Most listeners one jam holds.
pub const MAX_MEMBERS: usize = 50;
/// Most of the host's upcoming songs (its playlist or album) kept.
pub const MAX_NEXT_UP: usize = 50;
/// Commands kept for the host to read; older ones fall off.
pub const KEPT_COMMANDS: usize = 32;
/// Longest long poll a client may ask for, in seconds.
pub const MAX_WAIT_SECS: u64 = 30;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol: u32,
}

/// A song as listeners see it. Everything but `uri` is for display only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub uri: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artists: String,
    #[serde(default)]
    pub duration_ms: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
}

/// The host's repeat mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    #[default]
    Off,
    Context,
    Track,
}

/// What the host is playing. Only the host writes it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Playback {
    pub track: Option<Track>,
    /// The playlist, album or artist the song plays from, when there is one,
    /// so a listener's player carries on the same way the host's does.
    #[serde(default)]
    pub context_uri: Option<String>,
    /// Where the song was when the server received this, in milliseconds.
    pub position_ms: u32,
    pub playing: bool,
    /// Server clock, Unix milliseconds, when `position_ms` was true.
    pub updated_at_ms: u64,
    /// The newest command the host had carried out when it published this.
    #[serde(default)]
    pub applied_seq: u64,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: Repeat,
    /// What the host's playlist or album plays after the jam's queue: its
    /// own "Next up", so everyone sees what comes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next_up: Vec<Track>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub host: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub track: Track,
    /// The member id of whoever added it.
    pub added_by: String,
    pub added_by_name: String,
}

/// A request from any listener for the host's player to carry out. Every
/// listener has the same controls as the host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Play,
    Pause,
    Seek {
        position_ms: u32,
    },
    Next,
    Previous,
    /// Start a playlist, album, artist or show, optionally at a song.
    PlayContext {
        context_uri: String,
        #[serde(default)]
        offset_uri: Option<String>,
        #[serde(default)]
        offset_index: Option<u32>,
    },
    /// Start a list of songs at `index`.
    PlayUris {
        uris: Vec<String>,
        #[serde(default)]
        index: u32,
    },
    /// Play a song from the jam's queue now, skipping the songs before it.
    PlayQueued {
        id: String,
    },
    Shuffle {
        on: bool,
    },
    Repeat {
        mode: Repeat,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandEntry {
    pub seq: u64,
    pub from: String,
    pub from_name: String,
    pub command: Command,
}

/// Everything a listener needs to follow the jam, in one answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,
    /// Grows with every change; long polls wait for it to pass `since`.
    pub version: u64,
    /// The server's clock as it answered, Unix milliseconds.
    pub server_time_ms: u64,
    pub host_id: String,
    pub members: Vec<Member>,
    pub playback: Playback,
    pub queue: Vec<QueueItem>,
    pub commands: Vec<CommandEntry>,
    /// The host ended the jam or went away. Nothing changes after this.
    pub ended: bool,
}

// Requests.

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct JoinRequest {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct JoinResponse {
    pub session: SessionView,
    pub member_id: String,
    /// Sent back as `Authorization: Bearer <token>` on every later request.
    pub token: String,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct PublishPlayback {
    pub track: Option<Track>,
    #[serde(default)]
    pub context_uri: Option<String>,
    pub position_ms: u32,
    pub playing: bool,
    #[serde(default)]
    pub applied_seq: u64,
    #[serde(default)]
    pub shuffle: bool,
    #[serde(default)]
    pub repeat: Repeat,
    #[serde(default)]
    pub next_up: Vec<Track>,
    /// When `position_ms` was true, by the host's estimate of the server's
    /// clock (Unix milliseconds). Without it, or when it is not within the
    /// last [`MAX_MEASURED_AGE_MS`], the server's own time of receipt
    /// stands: that is later by the host's upload, so listeners would think
    /// the host behind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_at_ms: Option<u64>,
}

/// The oldest a host's own timestamp may be and still be taken.
pub const MAX_MEASURED_AGE_MS: u64 = 10_000;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct AddToQueue {
    pub tracks: Vec<Track>,
    /// Where in the queue they go; the end when absent or past it.
    #[serde(default)]
    pub position: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct MoveInQueue {
    pub id: String,
    /// The song's index once moved; past the end moves it last.
    pub to: usize,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct RemoveFromQueue {
    pub ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct SendCommand {
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct CommandAccepted {
    pub seq: u64,
    pub session: SessionView,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct ErrorBody {
    pub error: String,
}
