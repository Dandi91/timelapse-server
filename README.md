# timelapse-server

Records livestreams (YouTube or anything else yt-dlp supports) around the clock as segmented
timelapses, prunes old footage per stream by size or recorded time, and keeps its index in SQLite.

Each stream runs `yt-dlp -o - | ffmpeg`. ffmpeg keeps every Nth frame, re-times the result and
writes MPEG-TS segments. It reports each finished segment on stdout, and the server records it in
the database. If a pipeline drops, it restarts with backoff in a new session.

The web UI with an HLS player and clip export is still to come. For now the server is managed
through the CLI.

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

### Data directory

```
data/timelapse.db                         streams, sessions, segments
data/streams/<stream>/capture.log         yt-dlp and ffmpeg messages (rotated at 5 MB)
data/streams/<stream>/<session>/000000.ts segments
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
