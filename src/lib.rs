pub mod db;
pub mod events;
pub mod exports;
pub mod parts;
pub mod pipeline;
pub mod postprocess;
pub mod procs;
pub mod reconcile;
pub mod retention;
pub mod server;
pub mod settings;
pub mod supervisor;
pub mod thumbs;
pub mod tools;
pub mod units;
pub mod web;

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::sync::{Notify, broadcast};

use crate::events::Event;
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
    /// How often yt-dlp updates itself; None turns that off.
    pub yt_dlp_update: Option<Duration>,
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
            yt_dlp_update: Some(Duration::from_secs(24 * 3600)),
        }
    }
}

/// First line of `--version` output, once known.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ToolVersions {
    pub yt_dlp: Option<String>,
    pub ffmpeg: Option<String>,
}

/// Everything the background tasks and the web server share.
pub struct Ctx {
    pub pool: SqlitePool,
    pub data_dir: PathBuf,
    pub tools: Tools,
    pub tuning: Tuning,
    /// SHA-256 of the web password; `None` leaves the web UI open.
    pub password_hash: Option<[u8; 32]>,
    /// Live events for connected browsers.
    pub events: broadcast::Sender<Event>,
    /// Wakes the recorder manager to reread the stream table now rather than at the next poll.
    pub wake: Notify,
    pub versions: RwLock<ToolVersions>,
    /// The last yt-dlp update, scheduled or requested.
    pub last_update: RwLock<Option<tools::UpdateOutcome>>,
    pub exports: exports::Control,
    /// Wakes the post-processing worker when a segment is finished.
    pub segment_finished: Notify,
}

impl Ctx {
    pub fn new(pool: SqlitePool, data_dir: PathBuf, tools: Tools, tuning: Tuning) -> Self {
        Self {
            pool,
            data_dir,
            tools,
            tuning,
            password_hash: None,
            events: broadcast::channel(256).0,
            wake: Notify::new(),
            versions: RwLock::default(),
            last_update: RwLock::default(),
            exports: exports::Control::default(),
            segment_finished: Notify::new(),
        }
    }

    /// Send an event to whoever is listening; nobody listening is fine.
    pub fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Record a stream's status and tell the browsers.
    pub async fn set_status(&self, stream_id: i64, status: &str, detail: Option<&str>) {
        if let Err(e) = db::set_status(&self.pool, stream_id, status, detail).await {
            tracing::warn!("updating status of stream {stream_id}: {e:#}");
        }
        self.emit(Event::Status {
            stream_id,
            status: status.into(),
            detail: detail.map(Into::into),
            at: db::now_ms(),
        });
    }

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
