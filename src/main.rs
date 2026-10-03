use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use timelapse_server::db::{self, StreamConfig};
use timelapse_server::pipeline::Tools;
use timelapse_server::settings::EncodeSettings;
use timelapse_server::units::{format_duration, format_size, format_utc, parse_duration_secs, parse_size};
use timelapse_server::{Ctx, Tuning, server};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// Record livestreams as segmented timelapses, around the clock.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Where the database, segments and logs live.
    #[arg(long, env = "TIMELAPSE_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,
    #[arg(long, env = "TIMELAPSE_YT_DLP", default_value = "yt-dlp", global = true)]
    yt_dlp: PathBuf,
    #[arg(long, env = "TIMELAPSE_FFMPEG", default_value = "ffmpeg", global = true)]
    ffmpeg: PathBuf,
    #[arg(long, env = "TIMELAPSE_FFPROBE", default_value = "ffprobe", global = true)]
    ffprobe: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record every enabled stream until stopped; picks up stream changes as they happen.
    Serve {
        /// Address for the web UI and API.
        #[arg(long, env = "TIMELAPSE_BIND", default_value = "127.0.0.1:8080")]
        bind: String,
        /// Password for the web UI. Without one, anyone who can reach `--bind` can manage streams.
        #[arg(long, env = "TIMELAPSE_PASSWORD", hide_env_values = true)]
        password: Option<String>,
        /// How often yt-dlp updates itself, in hours; 0 turns that off.
        #[arg(long, env = "TIMELAPSE_YT_DLP_UPDATE_HOURS", default_value_t = 24.0)]
        yt_dlp_update_hours: f64,
        /// Prune the oldest segments across all streams when free space drops below this.
        #[arg(long, env = "TIMELAPSE_MIN_FREE", default_value = "5G", value_parser = parse_size)]
        min_free: i64,
    },
    /// Add a stream.
    Add {
        url: String,
        #[arg(long)]
        label: String,
        #[command(flatten)]
        options: StreamOptions,
    },
    /// Change a stream's settings; only the options given are changed.
    Set {
        label: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        rename: Option<String>,
        #[command(flatten)]
        options: StreamOptions,
    },
    /// Remove a stream and all its recordings.
    Rm { label: String },
    /// List streams with their status and storage.
    List,
    /// List a stream's segments.
    Segments {
        label: String,
        /// Show only the newest N.
        #[arg(long, default_value_t = 20)]
        last: usize,
    },
}

#[derive(Args)]
struct StreamOptions {
    #[arg(long)]
    sample_fps: Option<f64>,
    #[arg(long)]
    source_fps: Option<u32>,
    #[arg(long)]
    out_fps: Option<u32>,
    #[arg(long)]
    height: Option<u32>,
    #[arg(long)]
    crf: Option<u8>,
    #[arg(long)]
    preset: Option<String>,
    #[arg(long)]
    segment_minutes: Option<f64>,
    /// Keep at most this much on disk, e.g. 50G. "none" removes the limit.
    #[arg(long)]
    max_size: Option<String>,
    /// Keep at most this much recorded time, e.g. 7d. "none" removes the limit.
    #[arg(long)]
    max_duration: Option<String>,
    /// Record even if the URL is not live, e.g. a finished stream's VOD.
    #[arg(long, conflicts_with = "live_only")]
    allow_vod: bool,
    /// Only record while the URL is live (the default).
    #[arg(long)]
    live_only: bool,
    #[arg(long, conflicts_with = "enable")]
    disable: bool,
    #[arg(long)]
    enable: bool,
}

impl StreamOptions {
    fn apply(&self, config: &mut StreamConfig) -> Result<()> {
        let s = &mut config.settings;
        if let Some(v) = self.sample_fps {
            s.sample_fps = v;
        }
        if let Some(v) = self.source_fps {
            s.source_fps = v;
        }
        if let Some(v) = self.out_fps {
            s.out_fps = v;
        }
        if let Some(v) = self.height {
            s.height = v;
        }
        if let Some(v) = self.crf {
            s.crf = v;
        }
        if let Some(v) = &self.preset {
            s.preset = v.clone();
        }
        if let Some(v) = self.segment_minutes {
            s.segment_minutes = v;
        }
        if let Some(v) = &self.max_size {
            config.max_bytes = optional(v, parse_size)?;
        }
        if let Some(v) = &self.max_duration {
            config.max_duration_secs = optional(v, parse_duration_secs)?;
        }
        if self.allow_vod {
            config.live_only = false;
        } else if self.live_only {
            config.live_only = true;
        }
        if self.disable {
            config.enabled = false;
        } else if self.enable {
            config.enabled = true;
        }
        Ok(())
    }
}

fn optional(text: &str, parse: fn(&str) -> Result<i64>) -> Result<Option<i64>> {
    if text.eq_ignore_ascii_case("none") {
        Ok(None)
    } else {
        parse(text).map(Some)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let cli = Cli::parse();

    std::fs::create_dir_all(&cli.data_dir).with_context(|| format!("creating {}", cli.data_dir.display()))?;
    let data_dir = cli.data_dir.canonicalize()?;
    let pool = db::connect(&data_dir.join("timelapse.db")).await?;

    match cli.command {
        Command::Serve {
            bind,
            password,
            yt_dlp_update_hours,
            min_free,
        } => {
            let listener = tokio::net::TcpListener::bind(&bind)
                .await
                .with_context(|| format!("listening on {bind}"))?;
            let tools = Tools {
                yt_dlp: cli.yt_dlp,
                ffmpeg: cli.ffmpeg,
                ffprobe: cli.ffprobe,
            };
            let tuning = Tuning {
                min_free_bytes: min_free as u64,
                yt_dlp_update: (yt_dlp_update_hours > 0.0)
                    .then(|| Duration::from_secs_f64(yt_dlp_update_hours * 3600.0)),
                ..Tuning::default()
            };
            let mut ctx = Ctx::new(pool.clone(), data_dir, tools, tuning);
            let password = password.filter(|p| !p.is_empty());
            ctx.password_hash = password.as_deref().map(timelapse_server::web::auth::hash);
            if password.is_none() && !listener.local_addr()?.ip().is_loopback() {
                tracing::warn!(
                    "the web UI on {bind} has no password; anyone who can reach it can manage streams \
                     (set TIMELAPSE_PASSWORD)"
                );
            }
            let ctx = Arc::new(ctx);
            let shutdown = CancellationToken::new();
            tokio::spawn(stop_on_signal(shutdown.clone()));
            server::serve(ctx, shutdown, Some(listener)).await?;
        }
        Command::Add { url, label, options } => {
            let mut config = StreamConfig {
                label,
                url,
                enabled: true,
                live_only: true,
                settings: EncodeSettings::default(),
                max_bytes: None,
                max_duration_secs: None,
            };
            options.apply(&mut config)?;
            let id = db::insert_stream(&pool, &config).await?;
            println!("added stream {} (id {id})", config.label);
        }
        Command::Set {
            label,
            url,
            rename,
            options,
        } => {
            let stream = find(&pool, &label).await?;
            let mut config = stream.config();
            if let Some(url) = url {
                config.url = url;
            }
            if let Some(name) = rename {
                config.label = name;
            }
            options.apply(&mut config)?;
            db::update_stream(&pool, &stream, &config).await?;
            println!("updated {}", config.label);
        }
        Command::Rm { label } => {
            let stream = find(&pool, &label).await?;
            db::delete_stream(&pool, stream.id).await?;
            println!("removed {label}; a running server deletes its files within a few seconds");
        }
        Command::List => list(&pool).await?,
        Command::Segments { label, last } => {
            let stream = find(&pool, &label).await?;
            let segments = db::list_segments(&pool, stream.id).await?;
            let skip = segments.len().saturating_sub(last);
            for seg in &segments[skip..] {
                println!(
                    "{:>6} {}  {}  {:>7.1}s  {:>8}  {}",
                    seg.session_id,
                    format_utc(seg.wall_start),
                    format_utc(seg.wall_end),
                    seg.media_dur,
                    format_size(seg.bytes),
                    seg.path
                );
            }
            println!("{} segment(s) in total", segments.len());
        }
    }
    pool.close().await;
    Ok(())
}

async fn find(pool: &sqlx::SqlitePool, label: &str) -> Result<db::Stream> {
    match db::find_stream(pool, label).await? {
        Some(stream) => Ok(stream),
        None => bail!("no stream labelled {label:?}"),
    }
}

async fn list(pool: &sqlx::SqlitePool) -> Result<()> {
    for stream in db::list_streams(pool).await? {
        let segments = db::list_segments(pool, stream.id).await?;
        let bytes: i64 = segments.iter().map(|s| s.bytes).sum();
        let secs: i64 = segments.iter().map(|s| (s.wall_end - s.wall_start) / 1000).sum();
        let s = &stream.settings.0;
        let limit = |v: Option<i64>, f: fn(i64) -> String| v.map(f).unwrap_or_else(|| "-".into());
        println!(
            "{} [{}{}] {}",
            stream.label,
            stream.status,
            if stream.enabled { "" } else { ", disabled" },
            stream.url
        );
        println!(
            "    {} fps sampled, {}x speed, {}p crf {} {} | {} / {} on disk, {} / {} recorded",
            s.sample_fps,
            s.speedup(),
            s.height,
            s.crf,
            s.preset,
            format_size(bytes),
            limit(stream.max_bytes, format_size),
            format_duration(secs),
            limit(stream.max_duration_secs, format_duration),
        );
        if let Some(detail) = &stream.status_detail {
            println!("    {detail}");
        }
    }
    Ok(())
}

async fn stop_on_signal(shutdown: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("signal received, finishing segments in flight");
    shutdown.cancel();
    // A second Ctrl+C means "now"; so does a shutdown that outlasts the recorders' own
    // worst-case wind-down by a wide margin.
    let _ = tokio::time::timeout(Duration::from_secs(180), tokio::signal::ctrl_c()).await;
    tracing::warn!("exiting without waiting for recorders");
    std::process::exit(130);
}
