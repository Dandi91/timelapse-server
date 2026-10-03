//! yt-dlp keeps itself current on a schedule.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use timelapse_server::tools;
use tokio_util::sync::CancellationToken;

/// A yt-dlp whose installed version lives in a file next to it; `-U` moves it forward, slowly.
fn updatable_yt_dlp(dir: &std::path::Path) -> std::path::PathBuf {
    let state = dir.join("installed-version");
    std::fs::write(&state, "2026.08.19\n").unwrap();
    let state = state.display();
    write_script(
        dir,
        "updatable-yt-dlp",
        &format!(
            "case \"$1\" in\n\
             --version) cat {state} ;;\n\
             -U) sleep 0.5; echo 'Current version: 2026.08.19'; echo 2026.10.01 > {state}; \
                 echo 'Updated yt-dlp to stable@2026.10.01' ;;\n\
             esac"
        ),
    )
}

#[tokio::test]
async fn updates_on_a_schedule_and_records_the_outcome() {
    let (mut ctx, dir) = fixture_ctx("exit 1").await;
    ctx.tools.yt_dlp = updatable_yt_dlp(dir.path());
    ctx.tuning.yt_dlp_update = Some(Duration::from_millis(300));
    let ctx = Arc::new(ctx);
    tools::refresh_versions(&ctx).await;
    assert_eq!(ctx.versions.read().unwrap().yt_dlp.as_deref(), Some("2026.08.19"));

    let shutdown = CancellationToken::new();
    let schedule = tokio::spawn(tools::auto_update(ctx.clone(), shutdown.clone()));
    let updated = wait_for(Duration::from_secs(10), || async {
        ctx.last_update.read().unwrap().is_some()
    })
    .await;
    assert!(updated, "no scheduled update ran");

    let outcome = ctx.last_update.read().unwrap().clone().unwrap();
    assert!(outcome.ok && outcome.automatic);
    assert_eq!(outcome.before.as_deref(), Some("2026.08.19"));
    assert_eq!(outcome.after.as_deref(), Some("2026.10.01"));
    assert!(outcome.output.contains("Updated yt-dlp"), "{}", outcome.output);
    assert_eq!(ctx.versions.read().unwrap().yt_dlp.as_deref(), Some("2026.10.01"));

    // One update at a time: a request during a running update is turned away, not queued.
    let (first, second) = tokio::join!(tools::update_yt_dlp(&ctx, false), async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        tools::update_yt_dlp(&ctx, false).await
    });
    let results = [first.is_some(), second.is_some()];
    assert!(results.contains(&true) && results.contains(&false), "{results:?}");

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), schedule)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn no_schedule_means_no_updates() {
    let (ctx, _dir) = fixture_ctx("exit 1").await;
    assert!(ctx.tuning.yt_dlp_update.is_none());
    // Returns at once instead of waiting for a schedule that isn't there.
    tokio::time::timeout(
        Duration::from_secs(1),
        tools::auto_update(Arc::new(ctx), CancellationToken::new()),
    )
    .await
    .unwrap();
}
