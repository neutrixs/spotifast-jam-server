#!/usr/bin/env python3
"""A pretend jam listener, for testing Spotifast with one Spotify account.

It speaks the jam protocol but plays no sound.

As a host it behaves like Spotifast's host: it signs in to Spotify (once,
in your browser) to read real songs, albums and playlists, then plays them
on a clock with their real lengths. Songs end on time and the next one
follows: the jam's queue first, then the playlist or album, in order or
shuffled, with repeat off, all or one, as Spotify plays them. It carries out
every request a listener can send and publishes what it plays, its Next up
and its own timestamps, as the app does.

As a guest it sends the controls and songs a friend would.

    python3 tools/fake_listener.py http://127.0.0.1:8787

Then type `help`. Only the Python standard library is used.

Signing in uses the Client ID in Spotifast's settings (or --client-id, or
SPOTIFY_CLIENT_ID) and the redirect address your Spotify app already lists
for Spotifast, http://127.0.0.1:8989/login. Close Spotifast's sign-in, if it
is open, the first time: both answer on that port. The sign-in is kept in
~/.cache/spotifast-jam/fake-listener.json, readable only by you.
"""

import argparse
import base64
import hashlib
import http.server
import json
import os
import random
import re
import secrets
import shlex
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from collections import deque
from pathlib import Path

API = "https://api.spotify.com/v1"
AUTHORIZE = "https://accounts.spotify.com/authorize"
TOKEN = "https://accounts.spotify.com/api/token"
REDIRECT = "http://127.0.0.1:8989/login"
SCOPES = "playlist-read-private playlist-read-collaborative user-library-read"
CACHE = Path.home() / ".cache" / "spotifast-jam" / "fake-listener.json"

# As in Spotifast (src/app/jam.rs and the queue rules).
HEARTBEAT_S = 15
RESTART_BEFORE_PREVIOUS_MS = 3_000
MAX_NEXT_UP = 50
# The longest context loaded, so a huge playlist does not take minutes.
MAX_CONTEXT = 1_000


def say(text):
    sys.stdout.write(f"\r  {text}\n> ")
    sys.stdout.flush()


def now_ms():
    return int(time.time() * 1000)


class JamError(Exception):
    def __init__(self, status, message):
        super().__init__(f"{status}: {message}")
        self.status = status


def spotify_uri(text):
    """spotify:kind:id from an open.spotify.com link, or the text as it is."""
    match = re.search(
        r"open\.spotify\.com/(?:intl-[\w-]+/)?(track|episode|album|playlist|artist|show)/(\w+)",
        text,
    )
    if match:
        return f"spotify:{match.group(1)}:{match.group(2)}"
    if "open.spotify.com/collection/tracks" in text:
        return "spotify:collection"
    return text.strip()


def jam_code(text):
    """The code from a bare code, an invite page or a spotifast-jam: link."""
    match = re.search(r"(?:/jam/|session=)(\w{6})", text)
    return (match.group(1) if match else text).upper()


def kind_of(uri):
    if uri.endswith(":collection") or uri == "spotify:collection":
        return "collection"
    parts = uri.split(":")
    return parts[1] if len(parts) >= 3 else None


def id_of(uri):
    return uri.rsplit(":", 1)[-1]


# Spotify: a browser sign-in, then reads.


def b64url(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def default_client_id():
    if os.environ.get("SPOTIFY_CLIENT_ID"):
        return os.environ["SPOTIFY_CLIENT_ID"]
    for settings in [
        Path.home() / ".config" / "spotifast" / "settings.json",
        Path.home() / "Library" / "Application Support" / "spotifast" / "settings.json",
        Path(os.environ.get("APPDATA", "")) / "spotifast" / "settings.json",
    ]:
        try:
            client_id = json.loads(settings.read_text()).get("web_client_id")
        except (OSError, ValueError):
            continue
        if client_id:
            return client_id.strip()
    return None


class Spotify:
    def __init__(self, client_id):
        self.client_id = client_id
        self.grant = None
        self.lock = threading.Lock()

    def _save(self):
        CACHE.parent.mkdir(parents=True, exist_ok=True)
        descriptor = os.open(CACHE, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(descriptor, "w") as file:
            json.dump(self.grant, file)

    def _load(self):
        try:
            grant = json.loads(CACHE.read_text())
        except (OSError, ValueError):
            return None
        return grant if grant.get("client_id") == self.client_id else None

    def _token_request(self, fields):
        data = urllib.parse.urlencode(fields).encode()
        request = urllib.request.Request(TOKEN, data=data, method="POST")
        request.add_header("content-type", "application/x-www-form-urlencoded")
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                answer = json.loads(response.read())
        except urllib.error.HTTPError as error:
            raise JamError(error.code, error.read().decode(errors="replace")) from None
        self.grant = {
            "client_id": self.client_id,
            "access_token": answer["access_token"],
            "refresh_token": answer.get("refresh_token")
            or (self.grant or {}).get("refresh_token"),
            "expires_at": time.time() + answer.get("expires_in", 3600) - 60,
        }
        self._save()

    def _sign_in(self):
        verifier = b64url(secrets.token_bytes(64))
        challenge = b64url(hashlib.sha256(verifier.encode()).digest())
        state = secrets.token_urlsafe(16)
        url = AUTHORIZE + "?" + urllib.parse.urlencode({
            "client_id": self.client_id,
            "response_type": "code",
            "redirect_uri": REDIRECT,
            "code_challenge_method": "S256",
            "code_challenge": challenge,
            "state": state,
            "scope": SCOPES,
        })
        answer = {}

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                parts = urllib.parse.urlsplit(self.path)
                if parts.path == "/login":
                    query = urllib.parse.parse_qs(parts.query)
                    answer.update({key: values[0] for key, values in query.items()})
                self.send_response(200)
                self.send_header("content-type", "text/html; charset=utf-8")
                self.end_headers()
                self.wfile.write(
                    b"<p>The fake listener is signed in. You can close this tab.</p>"
                )

            def log_message(self, *_):
                pass

        try:
            server = http.server.HTTPServer(("127.0.0.1", 8989), Handler)
        except OSError:
            raise JamError(
                0, "port 8989 is in use: close Spotifast's sign-in and try again"
            ) from None
        server.timeout = 1
        print(f"\n  Sign in to Spotify in your browser. If it does not open:\n  {url}\n")
        webbrowser.open(url)
        deadline = time.time() + 300
        with server:
            while "code" not in answer and "error" not in answer:
                if time.time() > deadline:
                    raise JamError(0, "no sign-in within five minutes")
                server.handle_request()
        if answer.get("state") != state or "code" not in answer:
            raise JamError(0, f"sign-in refused: {answer.get('error', 'wrong state')}")
        self._token_request({
            "grant_type": "authorization_code",
            "code": answer["code"],
            "redirect_uri": REDIRECT,
            "client_id": self.client_id,
            "code_verifier": verifier,
        })

    def token(self):
        with self.lock:
            if self.grant is None:
                self.grant = self._load()
            if self.grant and time.time() < self.grant["expires_at"]:
                return self.grant["access_token"]
            if self.grant and self.grant.get("refresh_token"):
                try:
                    self._token_request({
                        "grant_type": "refresh_token",
                        "refresh_token": self.grant["refresh_token"],
                        "client_id": self.client_id,
                    })
                    return self.grant["access_token"]
                except JamError:
                    self.grant = None
            self._sign_in()
            return self.grant["access_token"]

    def get(self, path, params=None):
        url = API + path
        if params:
            url += "?" + urllib.parse.urlencode(params)
        for attempt in range(4):
            request = urllib.request.Request(url)
            request.add_header("authorization", "Bearer " + self.token())
            try:
                with urllib.request.urlopen(request, timeout=20) as response:
                    return json.loads(response.read())
            except urllib.error.HTTPError as error:
                if error.code == 401 and attempt == 0:
                    with self.lock:
                        if self.grant:
                            self.grant["expires_at"] = 0
                    continue
                if error.code == 429:
                    wait = int(error.headers.get("retry-after", "2"))
                    say(f"Spotify asks to wait {wait} s")
                    time.sleep(min(wait, 30))
                    continue
                raise JamError(error.code, error.read().decode(errors="replace")[:200]) from None
        raise JamError(429, "Spotify kept refusing")


def pick_image(images):
    """The smallest cover at least 64 pixels wide, as Spotifast picks."""
    usable = [image for image in images or [] if image.get("url", "").startswith("https://")]
    if not usable:
        return None
    big_enough = [image for image in usable if (image.get("width") or 0) >= 64]
    if not big_enough:
        return usable[0]["url"]
    return min(big_enough, key=lambda image: image.get("width") or 0)["url"]


def jam_track(item, images=None):
    """A song or episode from Spotify as the jam carries it."""
    if not item or not item.get("uri") or item.get("is_local"):
        return None
    artists = ", ".join(artist.get("name", "") for artist in item.get("artists") or [])
    if not artists and item.get("show"):
        artists = item["show"].get("name", "")
    return {
        "uri": item["uri"],
        "title": item.get("name", ""),
        "artists": artists,
        "duration_ms": int(item.get("duration_ms") or 0),
        "image_url": pick_image(
            images
            or (item.get("album") or {}).get("images")
            or item.get("images")
            or (item.get("show") or {}).get("images")
        ),
    }


class Library:
    """Songs and contexts read from Spotify, kept for the session."""

    def __init__(self, spotify):
        self.spotify = spotify
        self.tracks = {}

    def track(self, uri):
        if uri in self.tracks:
            return self.tracks[uri]
        kind = kind_of(uri)
        if kind == "track":
            found = jam_track(self.spotify.get(f"/tracks/{id_of(uri)}"))
        elif kind == "episode":
            found = jam_track(self.spotify.get(f"/episodes/{id_of(uri)}"))
        else:
            found = None
        if found:
            self.tracks[uri] = found
        return found

    def _pages(self, path, params, offset=0):
        while offset < MAX_CONTEXT:
            page = self.spotify.get(path, {**params, "limit": 50, "offset": offset})
            items = page.get("items", [])
            yield from items
            offset += len(items)
            if not page.get("next") or not items:
                return

    def context(self, uri):
        """The songs a context plays, in order, and its name."""
        kind = kind_of(uri)
        if kind == "album":
            album = self.spotify.get(f"/albums/{id_of(uri)}")
            images = album.get("images")
            first = album.get("tracks", {})
            items = list(first.get("items", []))
            if first.get("next"):
                items += list(self._pages(f"/albums/{id_of(uri)}/tracks", {}, len(items)))
            songs = [jam_track(item, images) for item in items]
            name = album.get("name", uri)
        elif kind == "playlist":
            name = self.spotify.get(f"/playlists/{id_of(uri)}", {"fields": "name"}).get("name", uri)
            entries = self._pages(
                f"/playlists/{id_of(uri)}/items", {"additional_types": "track,episode"}
            )
            songs = [
                jam_track(entry.get("item") or entry.get("track"))
                for entry in entries
                if not entry.get("is_local")
            ]
        elif kind == "artist":
            artist = self.spotify.get(f"/artists/{id_of(uri)}")
            top = self.spotify.get(f"/artists/{id_of(uri)}/top-tracks")
            songs = [jam_track(item) for item in top.get("tracks", [])]
            name = artist.get("name", uri)
        elif kind == "show":
            show = self.spotify.get(f"/shows/{id_of(uri)}")
            songs = [
                jam_track(item, show.get("images"))
                for item in self._pages(f"/shows/{id_of(uri)}/episodes", {})
            ]
            name = show.get("name", uri)
        elif kind == "collection":
            songs = [jam_track(entry.get("track")) for entry in self._pages("/me/tracks", {})]
            name = "Liked Songs"
        else:
            raise JamError(0, f"not something to play from: {uri}")
        songs = [song for song in songs if song]
        for song in songs:
            self.tracks.setdefault(song["uri"], song)
        return name, songs


# The pretend player: Spotify's play order, on a clock.


class Player:
    def __init__(self):
        self.context_uri = None
        self.context_name = None
        self.songs = []      # the context's songs, in its own order
        self.order = []      # indices into songs, in play order
        self.at = -1         # where in `order` the context stands
        self.track = None    # what plays, from the context or the jam queue
        self.from_queue = False
        self.position_ms = 0
        self.since = time.time()
        self.playing = False
        self.shuffle = False
        self.repeat = "off"

    def position(self):
        if self.playing:
            return int(self.position_ms + (time.time() - self.since) * 1000)
        return self.position_ms

    def set_position(self, position_ms):
        self.position_ms = max(0, int(position_ms))
        self.since = time.time()

    def pause(self):
        if self.playing:
            self.set_position(self.position())
            self.playing = False

    def resume(self):
        if not self.playing and self.track:
            self.set_position(self.position_ms)
            self.playing = True

    def start(self, track, from_queue=False):
        self.track = track
        self.from_queue = from_queue
        self.set_position(0)
        self.playing = True

    def load(self, context_uri, name, songs, start):
        """Plays `songs` as a context from index `start`."""
        self.context_uri = context_uri
        self.context_name = name
        self.songs = songs
        self._arrange(start)
        self.start(self.songs[self.order[self.at]])

    def _arrange(self, current):
        """Play order from `current`: shuffled after it, or in order."""
        indices = list(range(len(self.songs)))
        if self.shuffle:
            rest = [index for index in indices if index != current]
            random.shuffle(rest)
            self.order = [current] + rest
            self.at = 0
        else:
            self.order = indices
            self.at = current

    def set_shuffle(self, on):
        if on == self.shuffle:
            return
        self.shuffle = on
        if self.songs and 0 <= self.at < len(self.order):
            self._arrange(self.order[self.at])

    def context_next(self):
        """The context's next song, moving on to it; None at its end."""
        if not self.order:
            return None
        if self.at + 1 < len(self.order):
            self.at += 1
        elif self.repeat == "context":
            if self.shuffle:
                random.shuffle(self.order)
            self.at = 0
        else:
            return None
        return self.songs[self.order[self.at]]

    def context_previous(self):
        if not self.order:
            return None
        self.at = max(0, self.at - 1)
        return self.songs[self.order[self.at]]

    def upcoming(self, limit=MAX_NEXT_UP):
        """The context's songs after this one: the host's Next up."""
        if not self.order:
            return []
        ahead = [self.songs[index] for index in self.order[self.at + 1:]]
        if self.repeat == "context":
            while len(ahead) < limit:
                ahead += [self.songs[index] for index in self.order]
        return ahead[:limit]


# The jam.


class Listener:
    def __init__(self, server, client_id):
        self.server = server.rstrip("/")
        self.client_id = client_id
        self.token = None
        self.code = None
        self.me = None
        self.host = False
        self.view = None
        self.lock = threading.RLock()
        self.generation = 0
        self.applied = 0
        self.player = Player()
        self.library = None
        self.removing = set()
        self.published_at = 0.0
        # The server's clock minus this one's, read from recent answers.
        self.clock = deque(maxlen=32)

    # HTTP.

    def call(self, method, path, body=None, timeout=10):
        data = None if body is None else json.dumps(body).encode()
        request = urllib.request.Request(self.server + "/api/v1" + path, data=data, method=method)
        if data is not None:
            request.add_header("content-type", "application/json")
        if self.token:
            request.add_header("authorization", "Bearer " + self.token)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read()
                answer = json.loads(raw) if raw else None
        except urllib.error.HTTPError as error:
            try:
                message = json.loads(error.read()).get("error", "")
            except Exception:
                message = error.reason
            raise JamError(error.code, message) from None
        # Every answer reads the server's clock, short by how long it
        # travelled: the highest reading is the quickest answer's, as in
        # Spotifast.
        session = answer.get("session", answer) if isinstance(answer, dict) else None
        if isinstance(session, dict) and "server_time_ms" in session:
            self.clock.append(session["server_time_ms"] - now_ms())
        return answer

    def server_clock(self):
        return now_ms() + max(self.clock) if self.clock else None

    def session(self):
        return f"/sessions/{self.code}"

    def spotify(self):
        if self.library is None:
            client_id = self.client_id or default_client_id()
            if not client_id:
                raise JamError(
                    0, "no Client ID: pass --client-id, set SPOTIFY_CLIENT_ID, or sign in to Spotifast"
                )
            library = Library(Spotify(client_id))
            library.spotify.token()
            self.library = library
        return self.library

    # Entering and leaving.

    def enter(self, joined):
        with self.lock:
            self.token = joined["token"]
            self.me = joined["member_id"]
            self.view = joined["session"]
            self.code = self.view["id"]
            self.host = self.view["host_id"] == self.me
            self.applied = max((c["seq"] for c in self.view["commands"]), default=0)
            self.generation += 1
            generation = self.generation
        threading.Thread(target=self.poll, args=(generation,), daemon=True).start()
        if self.host:
            threading.Thread(target=self.play_clock, args=(generation,), daemon=True).start()
        role = "hosting" if self.host else "joined"
        say(f"{role} jam {self.code}  invite: {self.server}/jam/{self.code}")
        self.show()

    def create(self, name):
        self.spotify()  # a host plays real songs: sign in first
        self.enter(self.call("POST", "/sessions", {"name": name}))

    def join(self, code, name):
        self.enter(self.call("POST", f"/sessions/{jam_code(code)}/join", {"name": name}))

    def leave(self):
        if not self.token:
            return
        try:
            self.call("POST", self.session() + "/leave")
        except JamError as error:
            say(f"leave: {error}")
        with self.lock:
            self.token = None
            self.generation += 1
        say("left")

    def poll(self, generation):
        since = self.view["version"]
        wait = 0
        while self.generation == generation:
            try:
                view = self.call("GET", f"{self.session()}?since={since}&wait={wait}", timeout=40)
            except JamError as error:
                if self.generation == generation:
                    say(f"the jam is gone ({error})")
                    self.token = None
                return
            except Exception as error:
                say(f"poll trouble: {error}; retrying")
                time.sleep(2)
                wait = 0
                continue
            wait = 25
            if self.generation != generation:
                return
            since = view["version"]
            self.on_view(view)
            if view["ended"]:
                say("the jam ended")
                self.token = None
                return

    def on_view(self, view):
        with self.lock:
            old = self.view or {}
            self.view = view
            self.removing &= {item["id"] for item in view["queue"]}
            if old.get("playback") != view["playback"] and not self.host:
                say("host now: " + describe(view["playback"], view["server_time_ms"]))
            if [i["id"] for i in old.get("queue", [])] != [i["id"] for i in view["queue"]]:
                say("jam queue: " + (", ".join(
                    f"[{i['id']}] {i['track']['title'] or i['track']['uri']} (by {i['added_by_name']})"
                    for i in view["queue"]) or "empty"))
            if [m["id"] for m in old.get("members", [])] != [m["id"] for m in view["members"]]:
                say("listening: " + ", ".join(
                    m["name"] + (" (host)" if m["host"] else "") for m in view["members"]))
            fresh = [c for c in view["commands"] if c["seq"] > self.applied and c["from"] != self.me]
            self.applied = max((c["seq"] for c in view["commands"]), default=self.applied)
        for entry in fresh:
            say(f"request #{entry['seq']} from {entry['from_name']}: {entry['command']}")
            if self.host:
                try:
                    self.carry_out(entry["command"])
                except JamError as error:
                    say(f"couldn't carry it out: {error}")
        if self.host and fresh:
            self.publish()

    # Hosting: what Spotifast's host does.

    def queue_head(self):
        for item in (self.view or {}).get("queue", []):
            if item["id"] not in self.removing:
                return item
        return None

    def take_from_queue(self, ids):
        ids = [item_id for item_id in ids if item_id not in self.removing]
        if ids:
            self.removing.update(ids)
            try:
                self.call("POST", self.session() + "/queue/remove", {"ids": ids})
            except JamError as error:
                say(f"queue: {error}")

    def complete(self, track):
        """A song with its length and name, read from Spotify if missing."""
        if track and (not track.get("duration_ms") or not track.get("title")):
            try:
                known = self.spotify().track(track["uri"])
            except JamError as error:
                say(f"couldn't read {track['uri']}: {error}")
                known = None
            if known:
                return known
        return track

    def advance(self, by_hand):
        """Next, or a song's end: the jam's queue first, then the context.
        Repeat one replays the song when it ends, not when skipped."""
        player = self.player
        if not by_hand and player.repeat == "track" and player.track:
            player.set_position(0)
            player.playing = True
            return
        head = self.queue_head()
        if head:
            self.take_from_queue([head["id"]])
            player.start(self.complete(dict(head["track"])), from_queue=True)
            return
        following = player.context_next()
        if following:
            player.start(self.complete(following))
            return
        # The end of the context with repeat off: Spotify stops on the last
        # song. (With Autoplay on, Spotify would go on with its own picks,
        # which nothing here can read.)
        player.pause()
        player.set_position(0)

    def previous(self):
        player = self.player
        if player.position() > RESTART_BEFORE_PREVIOUS_MS or (player.from_queue and not player.order):
            player.set_position(0)
            return
        before = player.context_previous()
        if before:
            player.start(self.complete(before))
        else:
            player.set_position(0)

    def play_context(self, context_uri, offset_uri=None, offset_index=None):
        say(f"reading {context_uri} from Spotify…")
        name, songs = self.spotify().context(context_uri)
        if not songs:
            raise JamError(0, f"{name} has nothing playable")
        start = 0
        if offset_index is not None and 0 <= offset_index < len(songs):
            start = offset_index
        elif offset_uri:
            start = next((i for i, song in enumerate(songs) if song["uri"] == offset_uri), 0)
        elif self.player.shuffle:
            start = random.randrange(len(songs))
        self.player.load(context_uri, name, songs, start)
        self.player.track = self.complete(self.player.track)
        say(f"playing from {name} ({len(songs)} songs)")

    def play_uris(self, uris, index=0):
        known = self.library.tracks if self.library else {}
        songs = [
            known.get(uri) or {"uri": uri, "title": "", "artists": "", "duration_ms": 0}
            for uri in uris
        ]
        self.player.load(None, None, songs, min(max(index, 0), len(songs) - 1))
        self.player.track = self.complete(self.player.track)

    def play_queued(self, item_id):
        """Plays a song from the jam's queue now; those before it are skipped
        (queue rule 5), and the context carries on after it."""
        queue = (self.view or {}).get("queue", [])
        ids = [item["id"] for item in queue]
        if item_id not in ids:
            raise JamError(0, f"no queued song {item_id}")
        at = ids.index(item_id)
        self.take_from_queue(ids[: at + 1])
        self.player.start(self.complete(dict(queue[at]["track"])), from_queue=True)

    def carry_out(self, command):
        kind = command["type"]
        player = self.player
        with self.lock:
            if kind == "play":
                player.resume()
            elif kind == "pause":
                player.pause()
            elif kind == "seek":
                player.set_position(command["position_ms"])
            elif kind == "next":
                self.advance(by_hand=True)
            elif kind == "previous":
                self.previous()
            elif kind == "play_context":
                self.play_context(
                    command["context_uri"], command.get("offset_uri"), command.get("offset_index")
                )
            elif kind == "play_uris":
                self.play_uris(command["uris"], command.get("index", 0))
            elif kind == "play_queued":
                self.play_queued(command["id"])
            elif kind == "shuffle":
                player.set_shuffle(command["on"])
            elif kind == "repeat":
                player.repeat = command["mode"]

    def publish(self):
        player = self.player
        with self.lock:
            track = player.track
            body = {
                "track": track,
                "context_uri": player.context_uri,
                "position_ms": player.position(),
                "playing": player.playing and track is not None,
                "applied_seq": self.applied,
                "shuffle": player.shuffle,
                "repeat": player.repeat,
                "next_up": player.upcoming(),
                "measured_at_ms": self.server_clock(),
            }
            self.published_at = time.time()
        try:
            self.call("PUT", self.session() + "/playback", body)
        except JamError as error:
            say(f"publish: {error}")
        say("playing: " + describe(body, None))

    def play_clock(self, generation):
        """Ends songs on time and publishes a heartbeat, as the host does."""
        while self.generation == generation:
            time.sleep(0.25)
            with self.lock:
                track = self.player.track
                ended = bool(
                    track
                    and self.player.playing
                    and track.get("duration_ms")
                    and self.player.position() >= track["duration_ms"]
                )
                stale = self.player.playing and time.time() - self.published_at >= HEARTBEAT_S
            if ended:
                with self.lock:
                    self.advance(by_hand=False)
                self.publish()
            elif stale:
                self.publish()

    # Guest controls.

    def request(self, command):
        accepted = self.call("POST", self.session() + "/commands", {"command": command})
        say(f"sent request #{accepted['seq']}: {command}")

    def show(self):
        view = self.view
        if not view:
            say("not in a jam")
            return
        say(f"jam {view['id']} v{view['version']}  "
            + describe(view["playback"], view["server_time_ms"]))
        if self.host and self.player.context_name:
            say(f"playing from: {self.player.context_name}")
        say("listening: " + ", ".join(
            m["name"] + (" (host)" if m["host"] else "") for m in view["members"]))
        say("jam queue: " + (", ".join(
            f"[{i['id']}] {i['track']['title'] or i['track']['uri']}" for i in view["queue"]) or "empty"))

    def show_next_up(self):
        upcoming = (
            self.player.upcoming()
            if self.host
            else (self.view or {}).get("playback", {}).get("next_up", [])
        )
        if not upcoming:
            say("next up: nothing")
            return
        for number, song in enumerate(upcoming[:15], 1):
            say(f"{number:2}. {song.get('title') or song['uri']} · {song.get('artists', '')}")


def describe(playback, server_now):
    track = playback.get("track")
    if not track:
        return "nothing"
    position = playback["position_ms"]
    if server_now and playback.get("playing"):
        position += server_now - playback["updated_at_ms"]
    state = "playing" if playback["playing"] else "paused"
    length = track.get("duration_ms") or 0
    modes = f"shuffle {'on' if playback.get('shuffle') else 'off'}, repeat {playback.get('repeat', 'off')}"
    name = track.get("title") or track["uri"]
    if track.get("artists"):
        name += f" · {track['artists']}"
    return f"{state} {name} at {position / 1000:.1f}/{length / 1000:.0f}s ({modes})"


HELP = """
  host [name]                 start a jam as a host (signs in to Spotify once)
  join CODE|LINK [name]       join a jam as a guest (one Spotifast hosts)

  play LINK [N|SONG LINK]     play a playlist, album, artist, show or Liked
                              Songs (open.spotify.com/collection/tracks),
                              from its Nth song or a given song; or one song
  pause | resume              pause or play
  seek SECONDS                move to a place in the song
  next | prev                 skip; prev restarts after three seconds
  shuffle on|off              set shuffle
  repeat off|context|track    set repeat (track: repeat one)

  add URI|LINK [title]        add a song to the end of the jam's queue
  addnext URI|LINK [title]    add a song to the front (Play next)
  remove ID                   take a song out (ids are shown in brackets)
  move ID INDEX               move a song; 0 is the top
  clear                       empty the jam's queue
  playq ID                    play a queued song now, skipping those before it

  upnext                      show what plays next
  state                       show the jam
  leave                       leave (ends the jam when hosting)
  quit                        stop without leaving, as a crashed app would
"""


def host_play(listener, args):
    """`play` typed by the host: a context, optionally at a song."""
    uri = spotify_uri(args[0])
    if kind_of(uri) in ("track", "episode"):
        listener.play_uris([uri])
        return
    offset_uri = offset_index = None
    if len(args) > 1:
        if args[1].isdigit():
            offset_index = int(args[1]) - 1
        else:
            offset_uri = spotify_uri(args[1])
    listener.play_context(uri, offset_uri, offset_index)


HOST_COMMANDS = {
    "pause": lambda args: {"type": "pause"},
    "resume": lambda args: {"type": "play"},
    "seek": lambda args: {"type": "seek", "position_ms": int(float(args[0]) * 1000)},
    "next": lambda args: {"type": "next"},
    "prev": lambda args: {"type": "previous"},
    "shuffle": lambda args: {"type": "shuffle", "on": args[0] == "on"},
    "repeat": lambda args: {"type": "repeat", "mode": args[0]},
    "playq": lambda args: {"type": "play_queued", "id": args[0]},
}


def guest_command(verb, args):
    if verb == "play":
        uri = spotify_uri(args[0])
        if kind_of(uri) in ("track", "episode"):
            return {"type": "play_uris", "uris": [uri], "index": 0}
        return {"type": "play_context", "context_uri": uri}
    make = HOST_COMMANDS.get(verb)
    return make(args) if make else None


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("server", help="the jam server, such as http://127.0.0.1:8787")
    parser.add_argument("--client-id", help="your Spotify app's Client ID (default: Spotifast's)")
    arguments = parser.parse_args()
    listener = Listener(arguments.server, arguments.client_id)
    sys.stdout.write("type `help` for commands\n> ")
    sys.stdout.flush()
    for line in sys.stdin:
        try:
            words = shlex.split(line)
        except ValueError as error:
            say(str(error))
            continue
        if not words:
            sys.stdout.write("> ")
            sys.stdout.flush()
            continue
        verb, args = words[0], words[1:]
        try:
            if verb == "help":
                print(HELP)
                sys.stdout.write("> ")
            elif verb == "host":
                listener.create(args[0] if args else "Fake host")
            elif verb == "join":
                listener.join(args[0], args[1] if len(args) > 1 else "Fake listener")
            elif verb == "quit":
                break
            elif not listener.token:
                say("start or join a jam first")
            elif verb in ("add", "addnext"):
                uri = spotify_uri(args[0])
                # Signed in, it sends the song's details, as Spotifast does;
                # as a guest it knows only the song, and the others look it up.
                song = listener.library.track(uri) if listener.library else None
                song = dict(song or {"uri": uri, "title": ""})
                if args[1:]:
                    song["title"] = " ".join(args[1:])
                listener.call("POST", listener.session() + "/queue",
                              {"tracks": [song], "position": 0 if verb == "addnext" else None})
            elif verb == "remove":
                listener.call("POST", listener.session() + "/queue/remove", {"ids": args})
            elif verb == "move":
                listener.call("POST", listener.session() + "/queue/move",
                              {"id": args[0], "to": int(args[1])})
            elif verb == "clear":
                listener.call("POST", listener.session() + "/queue/remove",
                              {"ids": [item["id"] for item in listener.view["queue"]]})
            elif verb == "state":
                listener.show()
            elif verb == "upnext":
                listener.show_next_up()
            elif verb == "leave":
                listener.leave()
            elif listener.host:
                if verb == "play":
                    with listener.lock:
                        host_play(listener, args)
                elif verb in HOST_COMMANDS:
                    listener.carry_out(HOST_COMMANDS[verb](args))
                else:
                    say(f"unknown command {verb!r}; try `help`")
                    continue
                listener.publish()
            else:
                command = guest_command(verb, args)
                if command is None:
                    say(f"unknown command {verb!r}; try `help`")
                else:
                    listener.request(command)
        except (JamError, OSError) as error:
            say(f"error: {error}")
        except (IndexError, ValueError, KeyError):
            say(f"`{verb}` needs more; try `help`")


if __name__ == "__main__":
    main()
