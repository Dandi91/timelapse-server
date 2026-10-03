# timelapse-server

Records livestreams (YouTube or anything else yt-dlp supports) around the clock as segmented
timelapses, prunes old footage per stream by size or recorded time, and keeps its index in SQLite.

Each stream runs `yt-dlp -o - | ffmpeg`. ffmpeg keeps every Nth frame, re-times the result and
writes MPEG-TS segments. It reports each finished segment on stdout, and the server records it in
the database. If a pipeline drops, it restarts with backoff in a new session.

`serve` also serves a web UI on `--bind` (default `127.0.0.1:8080`):

- **Player.** Choose a stream and a time range, and it plays the timelapse with a wall-clock
  readout. The timeline under it shows footage and gaps in clock time. You can scroll to zoom,
  drag to pan, click to jump, and shift-drag to select a clip. Hovering shows the time and a
  thumbnail. Live mode follows new segments as they finish; it shows how far behind real time the
  picture is, and the stretch still being recorded. Fullscreen covers the whole player, so the
  clock and timeline come along. Keys: space plays and pauses, ←/→ jump 5 s of video (30 s with
  shift), `,`/`.` step one frame, and `f` toggles fullscreen.
- **Wall.** Up to four cameras side by side, all following one clock. Each camera keeps to the
  clock, even across different recording speeds. A camera without footage at that moment shows
  when its footage resumes. The timeline has one lane per camera. One selection can queue an
  export for every camera.
- **Exports.** Pick a range under the player (or mark it from the playback position) and export it
  as an mp4. Fast mode copies the video, so it takes seconds, and starts at the keyframe before
  the requested time. Exact mode re-encodes and cuts to the frame. A range that spans different
  capture settings is always re-encoded. Jobs run one at a time in the background, with live
  progress. While a job runs, retention can't delete its segments. Interrupted jobs resume after a
  restart.
- **Streams.** Add, edit, enable, restart and delete streams; status updates live. You can read
  each stream's capture log, see disk usage and tool versions, and update yt-dlp.

Set `TIMELAPSE_PASSWORD` to require a login. Without it, anyone who can reach the port can manage
streams, and the server warns when it listens beyond localhost. Logins last 30 days and survive
restarts.

## Usage

```sh
timelapse-server add 'https://www.youtube.com/watch?v=FHJH2yMe6Hw' --label bridges \
    --sample-fps 5 --max-size 50G --max-duration 7d
timelapse-server serve          # records every enabled stream until SIGTERM/Ctrl+C
timelapse-server list           # status, speed, storage per stream
timelapse-server segments bridges
timelapse-server set bridges --crf 23 --disable
timelapse-server rm bridges     # also deletes its recordings
```

`serve` picks up `add`/`set`/`rm` within a few seconds. Changes to the URL or encode settings
restart that stream's recorder. Changes to retention limits don't.

By default a stream is recorded only while it is live (`--allow-vod` lifts that). A finished
stream shows as `offline` and is checked again every 5 minutes. Without this check, an ended
stream would turn into a full VOD download.

| Setting | Default | Meaning |
|---|---|---|
| `--sample-fps` | 5 | frames kept per second of stream; must divide `--source-fps` |
| `--source-fps` | 30 | the stream's frame rate |
| `--out-fps` | 30 | playback rate; speedup = out-fps / sample-fps |
| `--height` | 1080 | output height; the canvas is 16:9, with letterboxing if needed |
| `--crf`, `--preset` | 21, veryfast | x264 quality and speed |
| `--segment-minutes` | 10 | minutes of stream per segment file |
| `--max-size`, `--max-duration` | none | prune oldest segments beyond these (`none` clears) |

Global options are also available as environment variables:

| Option | Environment variable | Default |
|---|---|---|
| `--data-dir` | `TIMELAPSE_DATA_DIR` | `./data` |
| `--yt-dlp` | `TIMELAPSE_YT_DLP` | `yt-dlp` |
| `--ffmpeg` | `TIMELAPSE_FFMPEG` | `ffmpeg` |
| `--ffprobe` | `TIMELAPSE_FFPROBE` | `ffprobe` |
| `--min-free` (serve only) | `TIMELAPSE_MIN_FREE` | 5G; below this, the oldest segments across all streams are pruned |
| `--bind` (serve only) | `TIMELAPSE_BIND` | `127.0.0.1:8080` (`0.0.0.0:8080` in the Docker image) |
| `--password` (serve only) | `TIMELAPSE_PASSWORD` | none: no login |

### HTTP API

Once a password is set, everything except the login page needs the session cookie from
`POST /api/login`.

| Endpoint | Returns |
|---|---|
| `GET /api/streams` | streams with status, settings, and footage totals |
| `POST /api/streams` | create: `{label, url, settings?, max_bytes?, max_duration_secs?, live_only?, enabled?}` |
| `PATCH /api/streams/{id}` | change any of those fields; `settings` may be partial, and a limit set to `null` is removed |
| `DELETE /api/streams/{id}` | remove the stream and its recordings |
| `POST /api/streams/{id}/restart` | restart its pipeline |
| `GET /api/streams/{id}/log?lines=` | the end of its capture log |
| `GET /api/system` | disk usage, the free-space minimum, tool versions |
| `POST /api/system/update-yt-dlp` | run `yt-dlp -U` |
| `GET /api/exports` | export jobs, newest first |
| `POST /api/exports` | queue one: `{stream_id, from, to, mode: "fast" \| "exact"}` (wall-clock unix ms) |
| `DELETE /api/exports/{id}` | cancel or delete a job, and its file |
| `GET /api/exports/{id}/file` | the finished mp4, as a download |
| `GET /api/events` | server-sent events: status changes, segments added and removed, stream and export changes |
| `GET /api/streams/{id}/segments?from=&to=` | segments overlapping a wall-clock range (unix ms, either end optional) |
| `GET /streams/{id}/playlist.m3u8?from=&to=` | HLS VOD playlist for the range; sessions are separated by discontinuities |
| `GET /streams/{id}/playlist.m3u8?live=1&from=` | growing HLS EVENT playlist |
| `GET /streams/{id}/{session}/{n}.ts` | segment files, relative to the playlist |

### Data directory

```
data/timelapse.db                         streams, sessions, segments
data/streams/<stream>/capture.log         yt-dlp and ffmpeg messages (rotated at 5 MB)
data/streams/<stream>/<session>/000000.ts segments
data/streams/<stream>/<session>/000000.jpg keyframe thumbnails of that segment, one tile per keyframe
data/exports/<id>.mp4                     finished exports
```

At startup the server reconciles this directory with the database:
- It winds down any pipeline a crashed server left running, letting it finish its segment.
- It adopts segment files that aren't indexed.
- It drops rows whose files are gone and deletes orphaned files.

## Docker

The image bundles ffmpeg, the latest yt-dlp at build time, and deno, which yt-dlp needs for
YouTube. CI publishes it to `ghcr.io/dandi91/timelapse-server`. A weekly rebuild keeps yt-dlp
current.

```sh
docker pull ghcr.io/dandi91/timelapse-server:latest
docker run -d --name timelapse --restart unless-stopped --stop-timeout 120 \
    -p 8080:8080 -e TIMELAPSE_PASSWORD='choose-one' \
    -v /srv/timelapse:/data ghcr.io/dandi91/timelapse-server:latest
docker exec timelapse timelapse-server add 'https://www.youtube.com/watch?v=...' --label cam1
docker exec timelapse timelapse-server list
```

The container runs as uid 1000, so the host directory must be writable by it. `--stop-timeout`
gives each recorder time to finish its segment. Shutdown usually takes a few seconds, but its
worst case is about 100 s.

GHCR packages start out private. Either make the package public in its GitHub settings, or
`docker login ghcr.io` on the pulling host with a token that has `read:packages`.

## Development

```sh
cargo test     # needs ffmpeg/ffprobe on PATH; yt-dlp is replaced by a fake that streams a test pattern
```
