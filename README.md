# spotifast-jam-server

A small server that lets Spotifast listeners jam together: one shared
"now playing", one shared queue, everyone playing on their own Spotify
account. Spotify's own Jam has no public API, so Spotifast brings its own.

In Spotifast, choose **Start a jam** under the speaker button and enter this
server's address. Everyone else just opens the invite link.

## Run it

```sh
cargo run --release
# listening on 0.0.0.0:8787
```

| Flag | Environment | Default | Meaning |
|---|---|---|---|
| `--bind` | `JAM_BIND` | `0.0.0.0:8787` | Address and port to listen on |
| `--idle-hours` | `JAM_IDLE_HOURS` | `12` | Forget a jam nobody touched for this long |
| `--host-grace-minutes` | `JAM_HOST_GRACE_MINUTES` | `5` | End a jam whose host went quiet this long |
| `--max-sessions` | `JAM_MAX_SESSIONS` | `10000` | Most jams held at once |

Logging follows `RUST_LOG` (default `spotifast_jam_server=info`).

Everything lives in memory: a jam is a code, the playback, the queue and the
members, a few kilobytes. Restarting the server forgets every jam, and its
listeners are told it ended.

Put it behind a reverse proxy that terminates TLS (Caddy, nginx, …) for use
over the internet. It may live under a sub-path: invite pages work out the
server's address from their own URL. Long polls hold a request open for up
to 30 seconds, so keep the proxy's read timeout above that.

## How a jam works

* **The host decides what plays.** The host's Spotifast publishes its track,
  position, play state, context, shuffle, repeat and its own "Next up"
  (what its playlist or album plays next) whenever they change, and every
  15 seconds while playing.
* **Everyone else follows.** Listeners start the host's song at the host's
  position (computed with the server's clock). Syncing is loose on purpose:
  only a difference that lasts is corrected, and a listener seeks only when
  more than 5 seconds off.
* **Everyone has the host's controls.** A listener's play, pause, seek, next,
  previous, shuffle, repeat, "play this" or "play this queued song now"
  becomes a *command*. The host's Spotifast carries
  it out and publishes the result. Commands are numbered, and the host
  reports the newest one it carried out (`applied_seq`), so a listener's own
  pause is never undone by an answer from before it.
* **One shared queue.** Anyone adds songs (at the end, or first with Play
  next), reorders them and removes any of them. The queue lives here; the
  host's Spotifast hands only the next song to its real Spotify queue, the
  next one once that starts, so reordering works on any device.
* **Leaving.** A listener who leaves (or goes quiet for 90 seconds) drops out.
  The host leaving, or going quiet for the grace period, ends the jam.

## Testing with one Spotify account

`tools/fake_listener.py` (Python, no dependencies) joins a jam as a second
person. As a **host** it behaves like Spotifast's host without playing
sound: it signs in to Spotify once in your browser (with the Client ID in
Spotifast's settings and its registered address, `http://127.0.0.1:8989/login`)
to read real songs, albums and playlists, plays them on a clock with their
real lengths, moves through the jam's queue and then the playlist, with
shuffle and repeat as Spotify plays them, and carries out every request your
Spotifast sends. As a **guest** it sends the controls and songs a friend
would. Run it with the server's address and type `help`:

```sh
python3 tools/fake_listener.py http://127.0.0.1:8787
```

What it cannot imitate: Spotify's Autoplay after a playlist ends (it stops,
as with Autoplay off), and a real player's loading time.

## Invite links

`https://your-server/jam/ABC234` opens a small page that hands the jam to
Spotifast through `spotifast-jam://join?server=…&session=ABC234`. If nothing
answers that link, the page shows the address to paste into Spotifast.
The bare code `ABC234` also works in Spotifast, against the server in
Settings. Codes are six characters with no 0/O or 1/I and ignore case.

## API (v1)

All bodies are JSON. After creating or joining, send
`Authorization: Bearer <token>` on every request. Errors answer
`{"error": "…"}` with 400 (invalid), 401 (not a member), 403 (not allowed),
404 (no such jam), 409 (full) or 410 (ended).

| Method and path | Body | Answer |
|---|---|---|
| `GET /api/v1/info` | | `{name, version, protocol}` |
| `POST /api/v1/sessions` | `{name}` | 201 `{session, member_id, token}`; the caller hosts |
| `POST /api/v1/sessions/{code}/join` | `{name}` | `{session, member_id, token}` |
| `GET /api/v1/sessions/{code}?since=V&wait=S` | | The session, once its version passes `V` or after `S` seconds (at most 30) |
| `POST /api/v1/sessions/{code}/leave` | | 204; the host leaving ends the jam |
| `PUT /api/v1/sessions/{code}/playback` | `{track, context_uri, position_ms, playing, applied_seq}` | The session. Host only |
| `POST /api/v1/sessions/{code}/queue` | `{tracks: [Track], position?}` | The session; up to 100 songs at once, 500 queued. `position` 0 puts them first; absent, last |
| `POST /api/v1/sessions/{code}/queue/move` | `{id, to}` | The session, with the song moved to index `to` |
| `POST /api/v1/sessions/{code}/queue/remove` | `{ids: [..]}` | The session. Anyone removes any song |
| `POST /api/v1/sessions/{code}/commands` | `{command}` | `{seq, session}` |

A `Track` is `{uri, title, artists, duration_ms, image_url?}`; only `uri`
(a `spotify:` URI) matters for playback, the rest is for display.

A `command` is one of `{"type":"play"}`, `{"type":"pause"}`,
`{"type":"seek","position_ms":N}`, `{"type":"next"}`, `{"type":"previous"}`,
`{"type":"play_context","context_uri":U,"offset_uri":U?,"offset_index":N?}`,
`{"type":"play_uris","uris":[U],"index":N}`, `{"type":"play_queued","id":"q4"}`,
`{"type":"shuffle","on":true}`, `{"type":"repeat","mode":"off"|"context"|"track"}`.
The session keeps the newest 32 commands. The playback the host publishes
also carries `shuffle`, `repeat` and `next_up` (up to 50 Tracks).

A session looks like:

```json
{
  "id": "ABC234",
  "version": 12,
  "server_time_ms": 1763208000000,
  "host_id": "m1a2b3c4d5e6",
  "members": [{ "id": "m1a2b3c4d5e6", "name": "Sam", "host": true }],
  "playback": {
    "track": { "uri": "spotify:track:…", "title": "…", "artists": "…", "duration_ms": 215000 },
    "context_uri": "spotify:playlist:…",
    "position_ms": 42000,
    "playing": true,
    "updated_at_ms": 1763207999000,
    "applied_seq": 3,
    "shuffle": false,
    "repeat": "off"
  },
  "queue": [{ "id": "q4", "track": { "uri": "spotify:track:…" }, "added_by": "m…", "added_by_name": "Alex" }],
  "commands": [{ "seq": 3, "from": "m…", "from_name": "Alex", "command": { "type": "pause" } }],
  "ended": false
}
```

The wire types live in `src/protocol.rs`; Spotifast carries a copy in
`src/jam/protocol.rs`. Change both together, and bump `PROTOCOL` when an
older client could no longer understand the server.

## Limits

No accounts and no rate limiting. Anyone who knows a code can join that jam.
Everyone in a jam can also skip, reorder and clear its queue, as the host can.
A code is about a billion possibilities and jams are short-lived, but run the
server for people you trust, or put access control in front of it.
