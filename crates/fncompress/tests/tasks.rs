//! Task service and scheduler end to end, with real FFmpeg on lavfi samples.

mod common;

use std::time::Duration;

use fncompress::{
    app::{BatchRequest, ManualRequest},
    model::{AttemptKind, PendingReason, ServerSettings, TaskState},
};
use fncompress_core::pipeline::Outcome;

fn manual(path: &std::path::Path) -> ManualRequest {
    ManualRequest {
        path: path.to_owned(),
        profile_id: None,
        force: false,
    }
}

fn batch(dir: &std::path::Path) -> BatchRequest {
    BatchRequest {
        dir: dir.to_owned(),
        profile_id: None,
        recursive: true,
        force: false,
        include_attempted: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_transcode_publishes_and_marks_the_file() {
    let tools = require_tools!();
    let h = common::harness(&tools, common::plain(), |_| {});
    let src = h.dir.path().join("media/a.mkv");
    common::sample(&tools, &src, 2);

    let task = h.app.submit_manual(&manual(&src)).unwrap();
    assert_eq!(task.state, TaskState::Queued);
    let done = common::settled(&h.app, task.id).await;
    assert_eq!(done.state, TaskState::Published, "{done:?}");
    assert!(matches!(done.outcome, Some(Outcome::Published { .. })));
    assert_eq!(
        h.app.db.recycle_dirs().unwrap(),
        [src.parent().unwrap().join(".fncompress-recycle")]
    );

    // The output carries the marker: without force it is refused, and a
    // batch skips it.
    let again = h.app.submit_manual(&manual(&src)).unwrap_err();
    assert_eq!(again.code, "already_processed");
    let b = h.app.submit_batch(&batch(src.parent().unwrap())).unwrap();
    assert!(b.tasks.is_empty());
    assert_eq!(b.skipped[0].code, "already_processed");
}

/// A reserve no volume keeps holds transcodes in the queue, and both
/// previews say so beforehand; once it is lowered they run.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn transcodes_wait_in_the_queue_for_free_space() {
    let tools = require_tools!();
    let server = ServerSettings {
        min_free_gib: u32::MAX,
        ..common::plain()
    };
    let h = common::harness(&tools, server, |_| {});
    let media = h.dir.path().join("media");
    let src = media.join("a.mkv");
    common::sample(&tools, &src, 2);

    let preview = h.app.preview_batch(&batch(&media)).unwrap();
    assert_eq!(preview.space.len(), 1, "{preview:?}");
    assert_eq!(preview.space[0].files, 1);
    assert!(preview.space[0].short());
    let single = h.app.preview(&src, None, false).unwrap().space.unwrap();
    assert_eq!(single.files, 1);
    assert!(single.short());

    let task = h.app.submit_manual(&manual(&src)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        let s = h.app.scheduler.space_status();
        if s.waiting_tasks == [task.id] {
            break s;
        }
        assert!(std::time::Instant::now() < deadline, "{s:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(src.starts_with(&status.shortages[0].volume), "{status:?}");
    assert_eq!(status.shortages[0].waiting, 1);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(h.app.task(task.id).unwrap().state, TaskState::Queued);

    h.app.db.set_server_settings(&common::plain()).unwrap();
    h.app.scheduler.wake();
    let done = common::settled(&h.app, task.id).await;
    assert_eq!(done.state, TaskState::Published, "{done:?}");
    assert_eq!(h.app.scheduler.space_status(), Default::default());
}

#[tokio::test(flavor = "multi_thread")]
async fn attempt_records_stop_repeats_until_asked() {
    let tools = require_tools!();
    // Nothing saves 99%: every file ends as insufficient savings.
    let h = common::harness(&tools, common::plain(), |g| g.min_savings_pct = 99.0);
    let media = h.dir.path().join("media");
    common::sample(&tools, &media.join("a.mkv"), 2);
    common::sample(&tools, &media.join("sub/b.mkv"), 2);

    let first = h.app.submit_batch(&batch(&media)).unwrap();
    assert_eq!(first.tasks.len(), 2);
    assert!(first.skipped.is_empty());
    assert!(first
        .tasks
        .iter()
        .all(|t| t.batch_id == Some(first.batch_id)));
    for t in &first.tasks {
        let t = common::settled(&h.app, t.id).await;
        assert_eq!(t.state, TaskState::InsufficientSavings, "{t:?}");
    }
    let attempts = h.app.db.attempts(None, 10).unwrap();
    assert_eq!(attempts.len(), 2);
    assert!(attempts
        .iter()
        .all(|a| a.kind == AttemptKind::InsufficientSavings));

    let second = h.app.submit_batch(&batch(&media)).unwrap();
    assert!(second.tasks.is_empty());
    assert_eq!(second.skipped.len(), 2);
    assert!(second.skipped.iter().all(|s| s.code == "attempted"));

    // Another profile is another fingerprint: the files may be tried again.
    let mut other = h.app.db.profile("default").unwrap().unwrap();
    other.id = "other".into();
    other.profile.quality += 1;
    h.app.db.put_profile(&other).unwrap();
    let with_other = h
        .app
        .submit_batch(&BatchRequest {
            profile_id: Some("other".into()),
            ..batch(&media)
        })
        .unwrap();
    assert_eq!(with_other.tasks.len(), 2);
    for t in &with_other.tasks {
        assert_eq!(t.profile_id, "other");
        common::settled(&h.app, t.id).await;
    }

    // Including attempted files queues them again, and so does a manual retry.
    let third = h
        .app
        .submit_batch(&BatchRequest {
            include_attempted: true,
            recursive: false,
            ..batch(&media)
        })
        .unwrap();
    assert_eq!(third.tasks.len(), 1);
    common::settled(&h.app, third.tasks[0].id).await;
    let retry = h.app.retry(first.tasks[1].id).unwrap();
    assert_eq!(
        common::settled(&h.app, retry.id).await.state,
        TaskState::InsufficientSavings
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn observation_mode_holds_the_first_files_for_release() {
    let tools = require_tools!();
    let server = ServerSettings {
        observe_first_n: 1,
        ..common::plain()
    };
    let h = common::harness(&tools, server, |_| {});
    let a = h.dir.path().join("media/a.mkv");
    let b = h.dir.path().join("media/b.mkv");
    common::sample(&tools, &a, 2);
    common::sample(&tools, &b, 2);

    let ta = h.app.submit_manual(&manual(&a)).unwrap();
    let held = common::settled(&h.app, ta.id).await;
    assert_eq!(held.state, TaskState::Pending, "{held:?}");
    assert_eq!(held.pending_reason, Some(PendingReason::Observation));
    assert!(held.observe);
    assert!(held.pending_until.unwrap() > held.finished_at.unwrap() + 13 * 86_400);
    let candidate = held.candidate.clone().unwrap();
    assert!(candidate.path.exists());
    assert_eq!(h.app.db.observed_count().unwrap(), 1);

    // The second file is past the observation window.
    let tb = h.app.submit_manual(&manual(&b)).unwrap();
    assert_eq!(
        common::settled(&h.app, tb.id).await.state,
        TaskState::Published
    );

    let released = tokio::task::spawn_blocking({
        let app = h.app.clone();
        move || app.release(ta.id)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(released.state, TaskState::Released);
    assert!(matches!(released.outcome, Some(Outcome::Published { .. })));
    assert!(!candidate.path.exists());
    assert_eq!(h.app.release(ta.id).unwrap_err().code, "not_pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_output_waits_and_can_be_discarded() {
    let tools = require_tools!();
    // No SSIM reaches 1.01, so every spot check is a hard failure.
    let h = common::harness(&tools, common::plain(), |g| g.verify.ssim_hard = 1.01);
    let src = h.dir.path().join("media/a.mkv");
    common::sample(&tools, &src, 2);

    let t = h.app.submit_manual(&manual(&src)).unwrap();
    let pending = common::settled(&h.app, t.id).await;
    assert_eq!(pending.state, TaskState::Pending, "{pending:?}");
    assert_eq!(pending.pending_reason, Some(PendingReason::Rejected));
    let attempt = &h.app.db.attempts(None, 10).unwrap()[0];
    assert_eq!(attempt.kind, AttemptKind::Rejected);
    assert!(attempt.message.contains("SSIM"), "{}", attempt.message);
    // A pending output blocks another task for the same file.
    assert_eq!(
        h.app.submit_manual(&manual(&src)).unwrap_err().code,
        "task_exists"
    );

    let path = pending.candidate.unwrap().path;
    assert!(path.exists());
    let discarded = h.app.discard(t.id, TaskState::Discarded).unwrap();
    assert_eq!(discarded.state, TaskState::Discarded);
    assert!(!path.exists());
    assert!(src.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn quotas_cancel_and_shutdown() {
    let tools = require_tools!();
    let h = common::harness(&tools, common::plain(), |_| {});
    let media = h.dir.path().join("media");
    // Long enough to still be encoding when cancelled.
    for name in ["a.mkv", "b.mkv", "c.mkv"] {
        common::sample(&tools, &media.join(name), 120);
    }
    let ids: Vec<i64> = h
        .app
        .submit_batch(&batch(&media))
        .unwrap()
        .tasks
        .iter()
        .map(|t| t.id)
        .collect();
    let [a, b, c] = ids[..] else {
        panic!("{ids:?}")
    };

    // One CPU slot: a runs, b and c wait.
    common::wait_for(&h.app, a, |t| t.state == TaskState::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.app.task(b).unwrap().state, TaskState::Queued);
    assert_eq!(h.app.scheduler.running_ids(), [a]);

    // Cancelling a queued task is immediate; a running one stops FFmpeg.
    assert_eq!(h.app.cancel(c).unwrap().state, TaskState::Cancelled);
    common::wait_for(&h.app, a, |_| h.app.scheduler.progress(a).is_some()).await;
    h.app.cancel(a).unwrap();
    assert_eq!(common::settled(&h.app, a).await.state, TaskState::Cancelled);
    let leftovers: Vec<_> = std::fs::read_dir(&media)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(
        leftovers.is_empty(),
        "partial candidate removed: {leftovers:?}"
    );

    // Shutdown cancels b and puts it back in the queue.
    common::wait_for(&h.app, b, |t| t.state == TaskState::Running).await;
    h.app.scheduler.shutdown(Duration::from_secs(30)).await;
    assert_eq!(h.app.task(b).unwrap().state, TaskState::Queued);
    assert!(h.app.scheduler.running_ids().is_empty());
    tokio::time::timeout(Duration::from_secs(5), h.runner)
        .await
        .expect("dispatch loop ends")
        .unwrap();
}
