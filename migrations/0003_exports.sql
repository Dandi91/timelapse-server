-- Clips cut from a stream's recordings. Rows outlive their stream (stream_id goes NULL), so the
-- files stay downloadable after the stream is removed.
CREATE TABLE exports (
    id             INTEGER PRIMARY KEY,
    stream_id      INTEGER REFERENCES streams (id) ON DELETE SET NULL,
    stream_label   TEXT    NOT NULL,
    from_ms        INTEGER NOT NULL,
    to_ms          INTEGER NOT NULL,
    -- 'fast' (stream copy from the keyframe before from_ms) or 'exact' (re-encoded)
    mode           TEXT    NOT NULL,
    -- What actually ran: fast becomes exact when the range spans different capture settings.
    used_mode      TEXT,
    -- queued, running, done, failed
    state          TEXT    NOT NULL DEFAULT 'queued',
    progress       REAL    NOT NULL DEFAULT 0,
    error          TEXT,
    -- Relative to the data dir, once done.
    path           TEXT,
    bytes          INTEGER,
    duration       REAL,
    -- The wall-clock span the clip really covers (fast mode starts at a keyframe).
    actual_from_ms INTEGER,
    actual_to_ms   INTEGER,
    created_at     INTEGER NOT NULL,
    started_at     INTEGER,
    finished_at    INTEGER
);

CREATE INDEX exports_state ON exports (state, id);
