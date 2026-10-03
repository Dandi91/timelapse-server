//! SQLite access. All SQL lives here; everything else works with the plain structs below.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::types::Json;
use sqlx::{FromRow, SqlitePool};

use crate::settings::EncodeSettings;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub async fn connect(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));
    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .with_context(|| format!("opening database {}", path.display()))?;
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("running migrations")?;
    Ok(pool)
}

#[derive(Debug, Clone, FromRow)]
pub struct Stream {
    pub id: i64,
    pub label: String,
    pub url: String,
    pub enabled: bool,
    pub live_only: bool,
    pub settings: Json<EncodeSettings>,
    pub max_bytes: Option<i64>,
    pub max_duration_secs: Option<i64>,
    pub revision: i64,
    pub created_at: i64,
    pub status: String,
    pub status_detail: Option<String>,
    pub status_at: Option<i64>,
}

/// The user-editable part of a stream.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub label: String,
    pub url: String,
    pub enabled: bool,
    pub live_only: bool,
    pub settings: EncodeSettings,
    pub max_bytes: Option<i64>,
    pub max_duration_secs: Option<i64>,
}

impl Stream {
    pub fn config(&self) -> StreamConfig {
        StreamConfig {
            label: self.label.clone(),
            url: self.url.clone(),
            enabled: self.enabled,
            live_only: self.live_only,
            settings: self.settings.0.clone(),
            max_bytes: self.max_bytes,
            max_duration_secs: self.max_duration_secs,
        }
    }
}

impl StreamConfig {
    pub fn validate(&self) -> Result<()> {
        if self.label.trim().is_empty() {
            anyhow::bail!("label must not be empty");
        }
        let url = url::Url::parse(&self.url).with_context(|| format!("invalid url {:?}", self.url))?;
        if !matches!(url.scheme(), "http" | "https") {
            anyhow::bail!("only http(s) urls are accepted");
        }
        if self.max_bytes.is_some_and(|b| b <= 0) || self.max_duration_secs.is_some_and(|d| d <= 0) {
            anyhow::bail!("retention limits must be positive");
        }
        self.settings.validate()
    }

    /// Whether going from `self` to `other` needs the recorder restarted. Retention limits don't.
    fn needs_restart(&self, other: &StreamConfig) -> bool {
        self.url != other.url
            || self.enabled != other.enabled
            || self.live_only != other.live_only
            || self.settings != other.settings
    }
}

pub async fn list_streams(pool: &SqlitePool) -> Result<Vec<Stream>> {
    Ok(sqlx::query_as(
        "SELECT id, label, url, enabled, live_only, settings, max_bytes, max_duration_secs, revision, \
         created_at, status, status_detail, status_at FROM streams ORDER BY id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn find_stream(pool: &SqlitePool, label: &str) -> Result<Option<Stream>> {
    Ok(sqlx::query_as(
        "SELECT id, label, url, enabled, live_only, settings, max_bytes, max_duration_secs, revision, \
         created_at, status, status_detail, status_at FROM streams WHERE label = ?",
    )
    .bind(label)
    .fetch_optional(pool)
    .await?)
}

pub async fn insert_stream(pool: &SqlitePool, config: &StreamConfig) -> Result<i64> {
    config.validate()?;
    let id = sqlx::query(
        "INSERT INTO streams (label, url, enabled, live_only, settings, max_bytes, max_duration_secs, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&config.label)
    .bind(&config.url)
    .bind(config.enabled)
    .bind(config.live_only)
    .bind(Json(&config.settings))
    .bind(config.max_bytes)
    .bind(config.max_duration_secs)
    .bind(now_ms())
    .execute(pool)
    .await
    .with_context(|| format!("adding stream {:?}", config.label))?
    .last_insert_rowid();
    Ok(id)
}

pub async fn update_stream(pool: &SqlitePool, current: &Stream, config: &StreamConfig) -> Result<()> {
    config.validate()?;
    let bump = i64::from(current.config().needs_restart(config));
    sqlx::query(
        "UPDATE streams SET label = ?, url = ?, enabled = ?, live_only = ?, settings = ?, max_bytes = ?, \
         max_duration_secs = ?, revision = revision + ? WHERE id = ?",
    )
    .bind(&config.label)
    .bind(&config.url)
    .bind(config.enabled)
    .bind(config.live_only)
    .bind(Json(&config.settings))
    .bind(config.max_bytes)
    .bind(config.max_duration_secs)
    .bind(bump)
    .bind(current.id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Restart a stream's recorder without changing anything else.
pub async fn bump_revision(pool: &SqlitePool, id: i64) -> Result<bool> {
    let done = sqlx::query("UPDATE streams SET revision = revision + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Deletes the stream's rows. Its files are removed by the server once the recorder has stopped,
/// or by the next startup reconcile.
pub async fn delete_stream(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM streams WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_status(pool: &SqlitePool, id: i64, status: &str, detail: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE streams SET status = ?, status_detail = ?, status_at = ? WHERE id = ?")
        .bind(status)
        .bind(detail)
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub id: i64,
    pub stream_id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub speedup: f64,
}

pub async fn create_session(pool: &SqlitePool, stream_id: i64, settings: &EncodeSettings) -> Result<i64> {
    Ok(
        sqlx::query("INSERT INTO sessions (stream_id, started_at, settings, speedup) VALUES (?, ?, ?, ?)")
            .bind(stream_id)
            .bind(now_ms())
            .bind(Json(settings))
            .bind(settings.speedup())
            .execute(pool)
            .await?
            .last_insert_rowid(),
    )
}

pub async fn end_session(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE sessions SET ended_at = ?, fetcher = NULL, encoder = NULL WHERE id = ?")
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete_session(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn list_sessions(pool: &SqlitePool) -> Result<Vec<Session>> {
    Ok(
        sqlx::query_as("SELECT id, stream_id, started_at, ended_at, speedup FROM sessions")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn set_session_pids(pool: &SqlitePool, id: i64, fetcher: u32, encoder: u32) -> Result<()> {
    let fetcher = crate::procs::Identity::of(fetcher).map(|p| p.to_string());
    let encoder = crate::procs::Identity::of(encoder).map(|p| p.to_string());
    sqlx::query("UPDATE sessions SET fetcher = ?, encoder = ? WHERE id = ?")
        .bind(fetcher)
        .bind(encoder)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Pipelines of sessions that never ended: the processes of a crashed server, if they still run.
#[derive(Debug, Clone, FromRow)]
pub struct OpenPipeline {
    pub id: i64,
    pub fetcher: Option<String>,
    pub encoder: Option<String>,
}

pub async fn open_pipelines(pool: &SqlitePool) -> Result<Vec<OpenPipeline>> {
    Ok(
        sqlx::query_as("SELECT id, fetcher, encoder FROM sessions WHERE ended_at IS NULL")
            .fetch_all(pool)
            .await?,
    )
}

/// Close sessions left open by a crash. Only valid while no recorder is running.
/// Nothing is recording before the server starts its recorders: `stopped` for disabled streams,
/// `idle` for the rest until their recorder reports in.
pub async fn reset_statuses(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE streams SET status = CASE WHEN enabled THEN 'idle' ELSE 'stopped' END, \
         status_detail = NULL, status_at = ?",
    )
    .bind(now_ms())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn end_open_sessions(pool: &SqlitePool) -> Result<u64> {
    Ok(
        sqlx::query("UPDATE sessions SET ended_at = ?, fetcher = NULL, encoder = NULL WHERE ended_at IS NULL")
            .bind(now_ms())
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

/// Drop ended sessions that no longer hold any segment.
pub async fn delete_empty_sessions(pool: &SqlitePool) -> Result<Vec<Session>> {
    Ok(sqlx::query_as(
        "DELETE FROM sessions WHERE ended_at IS NOT NULL \
         AND NOT EXISTS (SELECT 1 FROM segments WHERE segments.session_id = sessions.id) \
         RETURNING id, stream_id, started_at, ended_at, speedup",
    )
    .fetch_all(pool)
    .await?)
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Segment {
    pub id: i64,
    pub session_id: i64,
    pub stream_id: i64,
    pub seq: i64,
    pub path: String,
    pub wall_start: i64,
    pub wall_end: i64,
    pub media_start: Option<f64>,
    pub media_end: Option<f64>,
    pub media_dur: f64,
    pub bytes: i64,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct NewSegment {
    pub session_id: i64,
    pub stream_id: i64,
    pub seq: i64,
    pub path: String,
    pub wall_start: i64,
    pub wall_end: i64,
    pub media_start: Option<f64>,
    pub media_end: Option<f64>,
    pub media_dur: f64,
    pub bytes: i64,
}

pub async fn insert_segment(pool: &SqlitePool, s: &NewSegment) -> Result<i64> {
    Ok(sqlx::query(
        "INSERT INTO segments (session_id, stream_id, seq, path, wall_start, wall_end, media_start, media_end, \
         media_dur, bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(s.session_id)
    .bind(s.stream_id)
    .bind(s.seq)
    .bind(&s.path)
    .bind(s.wall_start)
    .bind(s.wall_end)
    .bind(s.media_start)
    .bind(s.media_end)
    .bind(s.media_dur)
    .bind(s.bytes)
    .execute(pool)
    .await?
    .last_insert_rowid())
}

pub async fn list_segments(pool: &SqlitePool, stream_id: i64) -> Result<Vec<Segment>> {
    Ok(sqlx::query_as(
        "SELECT id, session_id, stream_id, seq, path, wall_start, wall_end, media_start, media_end, media_dur, \
         bytes, state FROM segments WHERE stream_id = ? ORDER BY wall_start, id",
    )
    .bind(stream_id)
    .fetch_all(pool)
    .await?)
}

pub async fn session_last_wall_end(pool: &SqlitePool, session_id: i64) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT MAX(wall_end) FROM segments WHERE session_id = ?")
            .bind(session_id)
            .fetch_one(pool)
            .await?,
    )
}

/// Ready segments overlapping `[from, to)` in wall-clock time, in playback order: session by
/// session, each in recording order.
pub async fn segments_in_range(
    pool: &SqlitePool,
    stream_id: i64,
    from: Option<i64>,
    to: Option<i64>,
) -> Result<Vec<Segment>> {
    Ok(sqlx::query_as(
        "SELECT id, session_id, stream_id, seq, path, wall_start, wall_end, media_start, media_end, media_dur, \
         bytes, state FROM segments WHERE stream_id = ? AND state = 'ready' AND wall_end > ? AND wall_start < ? \
         ORDER BY session_id, seq",
    )
    .bind(stream_id)
    .bind(from.unwrap_or(i64::MIN))
    .bind(to.unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?)
}

/// A stream with what it has on disk, for the UI.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct StreamSummary {
    pub id: i64,
    pub label: String,
    pub url: String,
    pub enabled: bool,
    pub live_only: bool,
    pub settings: Json<EncodeSettings>,
    pub max_bytes: Option<i64>,
    pub max_duration_secs: Option<i64>,
    pub status: String,
    pub status_detail: Option<String>,
    pub status_at: Option<i64>,
    pub segments: i64,
    pub bytes: i64,
    pub first_wall: Option<i64>,
    pub last_wall: Option<i64>,
}

pub async fn stream_summaries(pool: &SqlitePool) -> Result<Vec<StreamSummary>> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.label, s.url, s.enabled, s.live_only, s.settings, s.max_bytes, s.max_duration_secs, \
         s.status, s.status_detail, s.status_at, COUNT(g.id) AS segments, COALESCE(SUM(g.bytes), 0) AS bytes, \
         MIN(g.wall_start) AS first_wall, MAX(g.wall_end) AS last_wall \
         FROM streams s LEFT JOIN segments g ON g.stream_id = s.id AND g.state = 'ready' \
         GROUP BY s.id ORDER BY s.id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn stream_summary(pool: &SqlitePool, id: i64) -> Result<Option<StreamSummary>> {
    Ok(stream_summaries(pool).await?.into_iter().find(|s| s.id == id))
}

pub async fn total_segment_bytes(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COALESCE(SUM(bytes), 0) FROM segments")
        .fetch_one(pool)
        .await?)
}

pub async fn create_web_session(pool: &SqlitePool, token_hash: &str, expires_at: i64) -> Result<()> {
    let now = now_ms();
    sqlx::query("DELETE FROM web_sessions WHERE expires_at < ?")
        .bind(now)
        .execute(pool)
        .await?;
    sqlx::query("INSERT INTO web_sessions (token_hash, created_at, expires_at) VALUES (?, ?, ?)")
        .bind(token_hash)
        .bind(now)
        .bind(expires_at)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn web_session_valid(pool: &SqlitePool, token_hash: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM web_sessions WHERE token_hash = ? AND expires_at > ?)")
            .bind(token_hash)
            .bind(now_ms())
            .fetch_one(pool)
            .await?,
    )
}

pub async fn delete_web_session(pool: &SqlitePool, token_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM web_sessions WHERE token_hash = ?")
        .bind(token_hash)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_stream(pool: &SqlitePool, id: i64) -> Result<Option<Stream>> {
    Ok(sqlx::query_as(
        "SELECT id, label, url, enabled, live_only, settings, max_bytes, max_duration_secs, revision, \
         created_at, status, status_detail, status_at FROM streams WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

pub async fn all_segments(pool: &SqlitePool) -> Result<Vec<Segment>> {
    Ok(sqlx::query_as(
        "SELECT id, session_id, stream_id, seq, path, wall_start, wall_end, media_start, media_end, media_dur, \
         bytes, state FROM segments ORDER BY wall_start, id",
    )
    .fetch_all(pool)
    .await?)
}

/// Segment as retention sees it: newest first, with whether anything still holds it.
#[derive(Debug, Clone, FromRow)]
pub struct PruneCandidate {
    pub id: i64,
    pub stream_id: i64,
    pub bytes: i64,
    pub wall_start: i64,
    pub wall_end: i64,
    pub leased: bool,
}

pub async fn prune_candidates(pool: &SqlitePool, stream_id: i64) -> Result<Vec<PruneCandidate>> {
    Ok(sqlx::query_as(
        "SELECT id, stream_id, bytes, wall_start, wall_end, \
         EXISTS (SELECT 1 FROM segment_leases l WHERE l.segment_id = s.id) AS leased \
         FROM segments s WHERE stream_id = ? AND state = 'ready' ORDER BY wall_start DESC, id DESC",
    )
    .bind(stream_id)
    .fetch_all(pool)
    .await?)
}

/// Oldest unleased segments across all streams, for when the disk runs low.
pub async fn oldest_unleased(pool: &SqlitePool, limit: i64) -> Result<Vec<PruneCandidate>> {
    Ok(sqlx::query_as(
        "SELECT id, stream_id, bytes, wall_start, wall_end, FALSE AS leased FROM segments s \
         WHERE state = 'ready' AND NOT EXISTS (SELECT 1 FROM segment_leases l WHERE l.segment_id = s.id) \
         ORDER BY wall_start, id LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

/// Segment paths marked for deletion, with whether their session has ended (so its directory
/// may be removed once empty).
#[derive(Debug, Clone, FromRow)]
pub struct DoomedSegment {
    pub id: i64,
    pub path: String,
    pub session_ended: bool,
}

/// Step one of a delete: flag the rows, so a crash before the unlink is finished by reconcile.
pub async fn mark_deleting(pool: &SqlitePool, ids: &[i64]) -> Result<Vec<DoomedSegment>> {
    let ids = serde_json::to_string(ids)?;
    let mut tx = pool.begin().await?;
    // Re-checks leases: an export may have taken one since the candidates were picked.
    sqlx::query(
        "UPDATE segments SET state = 'deleting' WHERE id IN (SELECT value FROM json_each(?)) \
         AND NOT EXISTS (SELECT 1 FROM segment_leases l WHERE l.segment_id = segments.id)",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    let doomed = sqlx::query_as(
        "SELECT s.id, s.path, ss.ended_at IS NOT NULL AS session_ended FROM segments s \
         JOIN sessions ss ON ss.id = s.session_id \
         WHERE s.id IN (SELECT value FROM json_each(?)) AND s.state = 'deleting'",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(doomed)
}

pub async fn deleting_segments(pool: &SqlitePool) -> Result<Vec<DoomedSegment>> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.path, ss.ended_at IS NOT NULL AS session_ended FROM segments s \
         JOIN sessions ss ON ss.id = s.session_id WHERE s.state = 'deleting'",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn delete_segment_rows(pool: &SqlitePool, ids: &[i64]) -> Result<()> {
    sqlx::query("DELETE FROM segments WHERE id IN (SELECT value FROM json_each(?))")
        .bind(serde_json::to_string(ids)?)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn lease_segment(pool: &SqlitePool, segment_id: i64, holder: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO segment_leases (segment_id, holder) VALUES (?, ?)")
        .bind(segment_id)
        .bind(holder)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn release_leases(pool: &SqlitePool, holder: &str) -> Result<()> {
    sqlx::query("DELETE FROM segment_leases WHERE holder = ?")
        .bind(holder)
        .execute(pool)
        .await?;
    Ok(())
}

/// Leases belong to in-process jobs, so none survive a restart.
pub async fn clear_leases(pool: &SqlitePool) -> Result<()> {
    sqlx::query("DELETE FROM segment_leases").execute(pool).await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Export {
    pub id: i64,
    pub stream_id: Option<i64>,
    pub stream_label: String,
    pub from_ms: i64,
    pub to_ms: i64,
    pub mode: String,
    pub used_mode: Option<String>,
    pub state: String,
    pub progress: f64,
    pub error: Option<String>,
    pub path: Option<String>,
    pub bytes: Option<i64>,
    pub duration: Option<f64>,
    pub actual_from_ms: Option<i64>,
    pub actual_to_ms: Option<i64>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

const EXPORT_COLUMNS: &str = "id, stream_id, stream_label, from_ms, to_ms, mode, used_mode, state, progress, error, \
    path, bytes, duration, actual_from_ms, actual_to_ms, created_at, started_at, finished_at";

pub async fn create_export(pool: &SqlitePool, stream: &Stream, from_ms: i64, to_ms: i64, mode: &str) -> Result<i64> {
    Ok(sqlx::query(
        "INSERT INTO exports (stream_id, stream_label, from_ms, to_ms, mode, created_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(stream.id)
    .bind(&stream.label)
    .bind(from_ms)
    .bind(to_ms)
    .bind(mode)
    .bind(now_ms())
    .execute(pool)
    .await?
    .last_insert_rowid())
}

pub async fn list_exports(pool: &SqlitePool) -> Result<Vec<Export>> {
    let sql = format!("SELECT {EXPORT_COLUMNS} FROM exports ORDER BY id DESC");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql)).fetch_all(pool).await?)
}

pub async fn get_export(pool: &SqlitePool, id: i64) -> Result<Option<Export>> {
    let sql = format!("SELECT {EXPORT_COLUMNS} FROM exports WHERE id = ?");
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// The oldest queued export, marked running.
pub async fn start_next_export(pool: &SqlitePool) -> Result<Option<Export>> {
    let sql = format!(
        "UPDATE exports SET state = 'running', progress = 0, error = NULL, started_at = ? \
         WHERE id = (SELECT id FROM exports WHERE state = 'queued' ORDER BY id LIMIT 1) RETURNING {EXPORT_COLUMNS}"
    );
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(now_ms())
        .fetch_optional(pool)
        .await?)
}

pub async fn set_export_progress(pool: &SqlitePool, id: i64, progress: f64) -> Result<()> {
    sqlx::query("UPDATE exports SET progress = ? WHERE id = ?")
        .bind(progress)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// What the job settled on before running, so the UI can show it while it runs.
pub async fn set_export_plan(
    pool: &SqlitePool,
    id: i64,
    used_mode: &str,
    actual_from: i64,
    actual_to: i64,
) -> Result<()> {
    sqlx::query("UPDATE exports SET used_mode = ?, actual_from_ms = ?, actual_to_ms = ? WHERE id = ?")
        .bind(used_mode)
        .bind(actual_from)
        .bind(actual_to)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn finish_export(pool: &SqlitePool, id: i64, path: &str, bytes: i64, duration: f64) -> Result<()> {
    sqlx::query(
        "UPDATE exports SET state = 'done', progress = 1, path = ?, bytes = ?, duration = ?, finished_at = ? \
         WHERE id = ?",
    )
    .bind(path)
    .bind(bytes)
    .bind(duration)
    .bind(now_ms())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn fail_export(pool: &SqlitePool, id: i64, error: &str) -> Result<()> {
    sqlx::query("UPDATE exports SET state = 'failed', error = ?, finished_at = ? WHERE id = ?")
        .bind(error)
        .bind(now_ms())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Put a job back in the queue, e.g. one interrupted by a shutdown.
pub async fn requeue_export(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE exports SET state = 'queued', progress = 0, started_at = NULL WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn requeue_running_exports(pool: &SqlitePool) -> Result<u64> {
    Ok(
        sqlx::query("UPDATE exports SET state = 'queued', progress = 0, started_at = NULL WHERE state = 'running'")
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

pub async fn delete_export(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM exports WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn total_export_bytes(pool: &SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COALESCE(SUM(bytes), 0) FROM exports WHERE state = 'done'")
            .fetch_one(pool)
            .await?,
    )
}

/// A segment with the capture settings of its session, as an export needs it.
#[derive(Debug, Clone, FromRow)]
pub struct ExportSegment {
    pub id: i64,
    pub session_id: i64,
    pub path: String,
    pub wall_start: i64,
    pub wall_end: i64,
    pub media_dur: f64,
    pub settings: Json<EncodeSettings>,
}

/// Lease every ready segment of the stream overlapping `[from, to)` for `holder`, in one step,
/// and return them in playback order. Retention can't delete a leased segment.
pub async fn lease_range(
    pool: &SqlitePool,
    stream_id: i64,
    from: i64,
    to: i64,
    holder: &str,
) -> Result<Vec<ExportSegment>> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT OR IGNORE INTO segment_leases (segment_id, holder) SELECT id, ? FROM segments \
         WHERE stream_id = ? AND state = 'ready' AND wall_end > ? AND wall_start < ?",
    )
    .bind(holder)
    .bind(stream_id)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?;
    let segments = sqlx::query_as(
        "SELECT s.id, s.session_id, s.path, s.wall_start, s.wall_end, s.media_dur, ss.settings \
         FROM segments s JOIN sessions ss ON ss.id = s.session_id \
         JOIN segment_leases l ON l.segment_id = s.id AND l.holder = ? \
         WHERE s.state = 'ready' ORDER BY s.session_id, s.seq",
    )
    .bind(holder)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(segments)
}

pub async fn count_segments_in_range(pool: &SqlitePool, stream_id: i64, from: i64, to: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM segments WHERE stream_id = ? AND state = 'ready' AND wall_end > ? AND wall_start < ?",
    )
    .bind(stream_id)
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?)
}
