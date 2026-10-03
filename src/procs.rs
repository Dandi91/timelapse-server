//! Recognising our own processes across a restart, and winding down a pipeline that a crashed
//! server left running.
//!
//! A parent-death signal can't do this job: the kernel resends it each time the child is handed
//! from one dying thread of a multi-threaded parent to the next, and ffmpeg takes a second signal
//! as "abort now", dropping the buffered end of the segment in flight.

use std::fmt;
use std::time::Duration;

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;
use tokio::time::{Instant, sleep};
use tracing::{info, warn};

/// A pid plus its start time, so a pid the kernel has since reused is never mistaken for ours.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Identity {
    pub pid: u32,
    pub start: u64,
}

impl Identity {
    pub fn of(pid: u32) -> Option<Self> {
        stat(pid).map(|(_, start)| Self { pid, start })
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (pid, start) = text.split_once(':')?;
        Some(Self {
            pid: pid.parse().ok()?,
            start: start.parse().ok()?,
        })
    }

    pub fn is_running(&self) -> bool {
        matches!(stat(self.pid), Some((state, start)) if start == self.start && !matches!(state, 'Z' | 'X'))
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(self.pid as i32)
    }
}

impl fmt::Display for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.pid, self.start)
    }
}

/// State and start time (in clock ticks since boot) from `/proc/<pid>/stat`.
fn stat(pid: u32) -> Option<(char, u64)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesised and may itself contain spaces or parentheses.
    let rest = &text[text.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `rest` starts at field 3 (state); starttime is field 22.
    Some((fields.first()?.chars().next()?, fields.get(19)?.parse().ok()?))
}

/// Stop an orphaned pipeline the way a live server would: end the fetcher's whole group (yt-dlp
/// and the ffmpeg it runs), let the encoder finish its segment on EOF, and only then signal it,
/// exactly once.
pub async fn wind_down(fetcher: Option<Identity>, encoder: Option<Identity>) {
    let encoder = encoder.filter(Identity::is_running);
    let fetcher_running = fetcher.is_some_and(|f| f.is_running());
    if encoder.is_none() && !fetcher_running {
        return;
    }
    info!(
        ?fetcher,
        ?encoder,
        "winding down a pipeline left over from before the restart"
    );
    // The fetcher leads its own process group. Its leader may be gone while yt-dlp's ffmpeg still
    // feeds the encoder, so signal the group as long as anything of the pipeline is ours.
    if let Some(fetcher) = fetcher {
        let _ = killpg(fetcher.pid(), Signal::SIGTERM);
    }
    if let Some(encoder) = encoder
        && !wait_gone(&encoder, Duration::from_secs(20)).await
    {
        let _ = kill(encoder.pid(), Signal::SIGTERM);
        if !wait_gone(&encoder, Duration::from_secs(30)).await {
            warn!(pid = encoder.pid, "leftover ffmpeg ignored SIGTERM, killing it");
            let _ = kill(encoder.pid(), Signal::SIGKILL);
        }
    }
    if let Some(fetcher) = fetcher {
        let _ = killpg(fetcher.pid(), Signal::SIGKILL);
    }
}

async fn wait_gone(process: &Identity, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while process.is_running() {
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(100)).await;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_own_process() {
        let me = Identity::of(std::process::id()).unwrap();
        assert!(me.is_running());
        assert_eq!(Identity::parse(&me.to_string()), Some(me));
        assert!(
            !Identity {
                start: me.start + 1,
                ..me
            }
            .is_running(),
            "same pid, other start time"
        );
        assert_eq!(Identity::parse("12"), None);
    }
}
