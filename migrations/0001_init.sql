-- Times are unix milliseconds. Paths are relative to the data directory.

CREATE TABLE streams (
    id                INTEGER PRIMARY KEY,
    label             TEXT    NOT NULL UNIQUE,
    url               TEXT    NOT NULL,
    enabled           INTEGER NOT NULL DEFAULT 1,
    -- Refuse anything yt-dlp doesn't report as live or upcoming, so an ended
    -- stream doesn't turn into a full VOD download on the next retry.
    live_only         INTEGER NOT NULL DEFAULT 1,
    settings          TEXT    NOT NULL,           -- JSON EncodeSettings
    max_bytes         INTEGER,
    max_duration_secs INTEGER,                    -- of recorded wall-clock time
    -- Bumped whenever a change needs the recorder restarted.
    revision          INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    status            TEXT    NOT NULL DEFAULT 'idle',
    status_detail     TEXT,
    status_at         INTEGER
);

-- One run of the yt-dlp | ffmpeg pipeline. Segments within a session share
-- encode parameters and a continuous timestamp line; sessions don't.
CREATE TABLE sessions (
    id         INTEGER PRIMARY KEY,
    stream_id  INTEGER NOT NULL REFERENCES streams (id) ON DELETE CASCADE,
    started_at INTEGER NOT NULL,
    ended_at   INTEGER,
    settings   TEXT    NOT NULL,
    speedup    REAL    NOT NULL,
    -- The pipeline's processes while it runs, as "pid:starttime" (from /proc, so a reused pid
    -- is never mistaken for ours). Lets a restart after a crash wind down an orphaned pipeline.
    fetcher    TEXT,
    encoder    TEXT
);

CREATE INDEX sessions_stream ON sessions (stream_id);

CREATE TABLE segments (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    stream_id   INTEGER NOT NULL REFERENCES streams (id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    path        TEXT    NOT NULL UNIQUE,
    wall_start  INTEGER NOT NULL,
    wall_end    INTEGER NOT NULL,
    media_start REAL,
    media_end   REAL,
    media_dur   REAL    NOT NULL,
    bytes       INTEGER NOT NULL,
    -- 'ready', or 'deleting' between marking and unlinking during retention.
    state       TEXT    NOT NULL DEFAULT 'ready'
);

CREATE INDEX segments_stream_time ON segments (stream_id, wall_start);
CREATE INDEX segments_session ON segments (session_id);

-- Segments an export (or anything else) still needs; retention skips them.
CREATE TABLE segment_leases (
    segment_id INTEGER NOT NULL REFERENCES segments (id) ON DELETE CASCADE,
    holder     TEXT    NOT NULL,
    PRIMARY KEY (segment_id, holder)
);
