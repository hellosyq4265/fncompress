//! Monitoring and retention end to end: the server's monitor queues files
//! in watched directories; the janitor expires pending outputs and purges
//! the recycle.

mod common;

use std::{path::Path, time::Duration};

use fncompress::{
    config::MonitorInput,
    db::now,
    db::TaskFilter,
    janitor,
    model::{
        AttemptKind, PendingReason, SavedProfile, ServerSettings, Task, TaskSource, TaskState,
    },
    server::{ServeOptions, Server},
};

async fn server(
    tools: &common::Tools,
    dir: &Path,
    tweak: impl FnOnce(&mut ServerSettings),
) -> Server {
    let s = Server::start(&ServeOptions {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.join("data"),
        ffmpeg: Some(tools.ffmpeg.clone()),
        ffprobe: Some(tools.ffprobe.clone()),
        render_device: None,
        scoring_ffmpeg: None,
        dovi_tool: None,
        bundled_scoring_ffmpeg: None,
        socket: None,
        roots_file: None,
        probe_encoders: false,
    })
    .await
    .unwrap();
    let app = &s.app;
    let mut p = app.db.profile("default").unwrap().unwrap();
    p.profile.encoder = fncompress_core::encoder::Encoder::Libx265;
    p.profile.quality = 32;
    p.profile.preset = Some("ultrafast".into());
    app.db.put_profile(&p).unwrap();
    let mut server = ServerSettings {
        observe_first_n: 0,
        settle_secs: 1,
        min_free_gib: 0,
        ..Default::default()
    };
    tweak(&mut server);
    app.db.set_server_settings(&server).unwrap();
    s
}

fn monitor(path: &Path) -> MonitorInput {
    MonitorInput {
        path: path.to_owned(),
        enabled: true,
        profile_id: None,
        recursive: true,
        excludes: vec![],
    }
}

fn tasks(s: &Server) -> Vec<Task> {
    let mut t = s.app.db.tasks(&TaskFilter::default()).unwrap();
    t.reverse();
    t
}

/// Waits until `n` tasks exist and none is queued or running.
async fn settled(s: &Server, n: usize) -> Vec<Task> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let t = tasks(s);
        if t.len() >= n
            && t.iter()
                .all(|t| !matches!(t.state, TaskState::Queued | TaskState::Running))
        {
            return t;
        }
        assert!(std::time::Instant::now() < deadline, "tasks: {t:#?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Creates the sample elsewhere and moves it in, as copy tools do.
fn drop_in(tools: &common::Tools, scratch: &Path, target: &Path) {
    let tmp = scratch.join(target.file_name().unwrap());
    common::sample(tools, &tmp, 2);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::rename(&tmp, target).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn monitor_queues_existing_and_new_files_by_deepest_directory() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let s = server(&tools, dir.path(), |_| {}).await;
    let watch = dir.path().join("watch");
    let scratch = dir.path().join("scratch");
    common::sample(&tools, &watch.join("old.mkv"), 2);
    common::sample(&tools, &watch.join("Extras/skip.mkv"), 2);

    let mut other = s.app.db.profile("default").unwrap().unwrap();
    other.id = "kids".into();
    other.profile.quality = 34;
    s.app
        .create_profile(&SavedProfile {
            name: "少儿".into(),
            ..other
        })
        .unwrap();
    let kids = watch.join("kids");
    std::fs::create_dir_all(&kids).unwrap();

    s.app
        .create_monitor(&MonitorInput {
            excludes: vec!["Extras/**".into()],
            ..monitor(&watch)
        })
        .unwrap();
    s.app
        .create_monitor(&MonitorInput {
            profile_id: Some("kids".into()),
            ..monitor(&kids)
        })
        .unwrap();

    // The startup scan finds the existing file; the excluded one stays.
    let t = settled(&s, 1).await;
    assert_eq!(t.len(), 1, "{t:#?}");
    assert_eq!(t[0].source, TaskSource::Monitor);
    assert_eq!(t[0].state, TaskState::Published);
    assert!(t[0].path.ends_with("old.mkv"));

    // New files arrive through inotify; the deepest directory's profile applies.
    drop_in(&tools, &scratch, &watch.join("new/a.mkv"));
    drop_in(&tools, &scratch, &kids.join("b.mkv"));
    let t = settled(&s, 3).await;
    let by_name = |n: &str| t.iter().find(|t| t.path.ends_with(n)).unwrap();
    assert_eq!(by_name("a.mkv").profile_id, "default");
    assert_eq!(by_name("b.mkv").profile_id, "kids");
    assert!(t.iter().all(|t| t.state == TaskState::Published), "{t:#?}");

    // Published outputs carry the marker, so the monitor leaves them alone.
    s.app.monitor_handle().scan();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(tasks(&s).len(), 3, "{:#?}", tasks(&s));
    let status = s.app.monitor_handle().status();
    assert!(status.enabled);
    assert_eq!(status.queued, 3);
    assert!(status.last_scan_at.is_some());
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attempt_records_keep_the_monitor_from_retrying() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let s = server(&tools, dir.path(), |_| {}).await;
    let mut g = s.app.db.global_settings().unwrap();
    g.min_savings_pct = 99.0;
    s.app.db.set_global_settings(&g).unwrap();
    let watch = dir.path().join("watch");
    let file = watch.join("a.mkv");
    common::sample(&tools, &file, 2);
    s.app.create_monitor(&monitor(&watch)).unwrap();

    let t = settled(&s, 1).await;
    assert_eq!(t[0].state, TaskState::InsufficientSavings);
    let attempts = s.app.db.attempts(None, 10).unwrap();
    assert_eq!(attempts.len(), 1);

    // Even when the scan looks at the file again, the attempt stops it.
    s.app.db.forget_seen_file(&t[0].path).unwrap();
    s.app.monitor_handle().scan();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(tasks(&s).len(), 1);
    assert_eq!(
        s.app.db.seen_file(&t[0].path).unwrap().unwrap().decision,
        "attempted"
    );

    // Forgetting the attempt lets it through.
    s.app.db.delete_attempt(attempts[0].id).unwrap();
    s.app.db.forget_seen_file(&t[0].path).unwrap();
    s.app.monitor_handle().scan();
    let t = settled(&s, 2).await;
    assert_eq!(t.len(), 2);

    // The master switch stops everything.
    let mut server = s.app.db.server_settings().unwrap();
    server.monitor_enabled = false;
    s.app
        .update_settings(&fncompress::config::SettingsUpdate {
            global: None,
            server: Some(server),
        })
        .unwrap();
    s.app.db.forget_seen_file(&t[0].path).unwrap();
    s.app
        .db
        .delete_attempt(s.app.db.attempts(None, 10).unwrap()[0].id)
        .unwrap();
    s.app.monitor_handle().scan();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(tasks(&s).len(), 2);
    assert!(!s.app.monitor_handle().status().enabled);
    s.stop().await;
}

/// A file that would not fit is held and the monitor says why; once space
/// is back it is queued without a scan or a settings change.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn monitor_pauses_while_space_is_short() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let s = server(&tools, dir.path(), |s| s.min_free_gib = u32::MAX).await;
    let watch = dir.path().join("watch");
    let file = watch.join("a.mkv");
    common::sample(&tools, &file, 2);
    s.app.create_monitor(&monitor(&watch)).unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let paused = loop {
        let st = s.app.monitor_handle().status();
        if !st.space_paused.is_empty() {
            break st.space_paused;
        }
        assert!(std::time::Instant::now() < deadline, "{st:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(paused.len(), 1);
    assert_eq!(paused[0].waiting, 1);
    assert!(file.starts_with(&paused[0].volume), "{paused:?}");
    assert!(tasks(&s).is_empty());
    assert!(s.app.db.seen_file(&file).unwrap().is_none());

    // Written behind the monitor's back: only its retry finds out.
    let mut server = s.app.db.server_settings().unwrap();
    server.min_free_gib = 0;
    s.app.db.set_server_settings(&server).unwrap();
    let t = settled(&s, 1).await;
    assert_eq!(t[0].state, TaskState::Published, "{t:#?}");
    // The status follows on the monitor's next tick, which a short sample
    // can beat.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !s.app.monitor_handle().status().space_paused.is_empty() {
        assert!(std::time::Instant::now() < deadline, "still paused");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    s.stop().await;
}

/// Older versions held a rejected output for review even when it saved too
/// little; the janitor ends such tasks as 收益不足.
#[tokio::test(flavor = "multi_thread")]
async fn janitor_closes_pending_outputs_that_save_too_little() {
    let tools = require_tools!();
    let h = common::harness(&tools, common::plain(), |g| g.verify.ssim_hard = 1.01);
    let a = h.dir.path().join("media/a.mkv");
    common::sample(&tools, &a, 2);
    let t = h
        .app
        .submit_manual(&fncompress::app::ManualRequest {
            path: a.clone(),
            profile_id: None,
            force: false,
        })
        .unwrap();
    let pending = common::settled(&h.app, t.id).await;
    assert_eq!(pending.state, TaskState::Pending, "{pending:?}");
    // A pending output that saves enough stays.
    assert_eq!(janitor::sweep(&h.app, now()), janitor::Sweep::default());

    // What an older version left: the same output saving 1%.
    let mut candidate = pending.candidate.clone().unwrap();
    candidate.savings.saved_pct = 1.0;
    h.app
        .db
        .mark_pending(
            t.id,
            pending.outcome.as_ref().unwrap(),
            &candidate,
            PendingReason::Rejected,
            pending.pending_until.unwrap(),
        )
        .unwrap();
    let swept = janitor::sweep(&h.app, now());
    assert_eq!(swept.insufficient_pending, [t.id]);
    assert!(!candidate.path.exists());
    assert!(a.exists());
    let closed = h.app.task(t.id).unwrap();
    assert_eq!(closed.state, TaskState::InsufficientSavings);
    assert!(matches!(
        closed.outcome,
        Some(fncompress_core::pipeline::Outcome::InsufficientSavings { .. })
    ));
    let attempt = &h.app.db.attempts(None, 10).unwrap()[0];
    assert_eq!(attempt.kind, AttemptKind::InsufficientSavings);
    assert!(
        attempt.message.contains("校验未通过"),
        "{}",
        attempt.message
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn janitor_expires_pending_outputs_and_purges_the_recycle() {
    let tools = require_tools!();
    let h = common::harness(&tools, common::plain(), |g| g.verify.ssim_hard = 1.01);
    let a = h.dir.path().join("media/a.mkv");
    common::sample(&tools, &a, 2);
    let t = h
        .app
        .submit_manual(&fncompress::app::ManualRequest {
            path: a.clone(),
            profile_id: None,
            force: false,
        })
        .unwrap();
    let pending = common::settled(&h.app, t.id).await;
    assert_eq!(pending.state, TaskState::Pending);
    let candidate = pending.candidate.unwrap().path;

    // Nothing is due yet.
    assert_eq!(janitor::sweep(&h.app, now()), janitor::Sweep::default());
    assert!(candidate.exists());

    let later = now() + 15 * 86_400;
    let swept = janitor::sweep(&h.app, later);
    assert_eq!(swept.expired_pending, [t.id]);
    assert!(!candidate.exists());
    assert!(a.exists());
    assert_eq!(h.app.task(t.id).unwrap().state, TaskState::Expired);

    // A published file's old source sits in the recycle until its retention ends.
    let mut g = h.app.db.global_settings().unwrap();
    g.verify.ssim_hard = 0.8;
    h.app.db.set_global_settings(&g).unwrap();
    let b = h.dir.path().join("media/b.mkv");
    common::sample(&tools, &b, 2);
    let t = h
        .app
        .submit_manual(&fncompress::app::ManualRequest {
            path: b,
            profile_id: None,
            force: false,
        })
        .unwrap();
    assert_eq!(
        common::settled(&h.app, t.id).await.state,
        TaskState::Published
    );
    assert_eq!(janitor::sweep(&h.app, now()).purged_recycle, 0);
    assert_eq!(janitor::sweep(&h.app, later).purged_recycle, 1);
}
