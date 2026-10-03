pub mod db;
pub mod pipeline;
pub mod procs;
pub mod reconcile;
pub mod retention;
pub mod server;
pub mod settings;
pub mod supervisor;
pub mod units;
pub mod web;

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlx::SqlitePool;

use crate::pipeline::Tools;

/// Timings for the background loops. Tests shrink these.
#[derive(Debug, Clone)]
pub struct Tuning {
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// An attempt lasting this long resets the backoff.
    pub good_run: Duration,
    /// How long to wait before checking an offline stream again.
    pub offline_retry: Duration,
    /// How often the server rereads the stream list.
    pub poll_interval: Duration,
    pub retention_interval: Duration,
    /// Prune the oldest segments, across all streams, when free disk space drops below this.
    pub min_free_bytes: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            retry_min: Duration::from_secs(3),
            retry_max: Duration::from_secs(120),
            good_run: Duration::from_secs(120),
            offline_retry: Duration::from_secs(300),
            poll_interval: Duration::from_secs(5),
            retention_interval: Duration::from_secs(60),
            min_free_bytes: 5 << 30,
        }
    }
}

/// Everything the background tasks share.
pub struct Ctx {
    pub pool: SqlitePool,
    pub data_dir: PathBuf,
    pub tools: Tools,
    pub tuning: Tuning,
}

impl Ctx {
    pub fn streams_dir(&self) -> PathBuf {
        self.data_dir.join("streams")
    }

    pub fn stream_dir(&self, stream_id: i64) -> PathBuf {
        self.streams_dir().join(stream_id.to_string())
    }

    pub fn session_dir(&self, stream_id: i64, session_id: i64) -> PathBuf {
        self.stream_dir(stream_id).join(session_id.to_string())
    }

    /// Segment paths are stored relative to the data dir, so the dir can move.
    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.data_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    pub fn absolute(&self, relative: &str) -> PathBuf {
        self.data_dir.join(relative)
    }
}
