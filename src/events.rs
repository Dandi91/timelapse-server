//! What the server tells connected browsers as it happens. Events are hints: a client that misses
//! some (or gets `Resync`) refetches the current state from the API.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A recorder changed state: recording, retrying, offline, stopped.
    Status {
        stream_id: i64,
        status: String,
        detail: Option<String>,
        at: i64,
    },
    SegmentAdded {
        stream_id: i64,
        segment_id: i64,
        wall_start: i64,
        wall_end: i64,
        bytes: i64,
    },
    SegmentsRemoved {
        ids: Vec<i64>,
    },
    /// Streams were added, removed or reconfigured, from the UI or the CLI.
    StreamsChanged,
    /// A running export moved on.
    ExportUpdated {
        id: i64,
        state: String,
        progress: f64,
    },
    /// Exports were added, finished, failed or removed.
    ExportsChanged,
    /// This client fell behind and missed events.
    Resync,
}
