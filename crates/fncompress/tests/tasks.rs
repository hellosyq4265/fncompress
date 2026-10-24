//! Task service and scheduler end to end, with real FFmpeg on lavfi samples.

mod common;

use std::time::Duration;

use fncompress::{
    app::{BatchRequest, ManualRequest},
    events::{Event, ScanPhase},
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
        files: None,
        scan_id: None,
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
async fn predicted_no_gain_is_recorded_and_a_retry_transcodes_in_full() {
    let tools = require_tools!();
    // Nothing saves 99%: the estimate falls short before the film is encoded.
    let h = common::harness(&tools, common::plain(), |g| g.min_savings_pct = 99.0);
    let mut profile = h.app.db.profile("default").unwrap().unwrap();
    profile.profile.predict_savings = true;
    h.app.db.put_profile(&profile).unwrap();
    // Long enough for an estimate.
    let src = h.dir.path().join("media/long.mkv");
    common::sample(&tools, &src, 420);

    let task = h.app.submit_manual(&manual(&src)).unwrap();
    let done = common::settled(&h.app, task.id).await;
    assert_eq!(done.state, TaskState::InsufficientSavings, "{done:?}");
    let Some(Outcome::PredictedNoGain { estimate, .. }) = &done.outcome else {
        panic!("{done:?}");
    };
    assert!(estimate.saved_pct < 94.0, "{estimate:?}");
    let attempt = &h.app.db.attempts(None, 10).unwrap()[0];
    assert_eq!(attempt.kind, AttemptKind::PredictedNoGain);
    assert!(
        attempt.message.starts_with("预判收益不足"),
        "{}",
        attempt.message
    );

    // A retry skips the prediction: the whole film is encoded and measured.
    let retry = h.app.retry(task.id).unwrap();
    assert!(!retry.snapshot.profile.predict_savings);
    let again = common::settled(&h.app, retry.id).await;
    assert!(
        matches!(again.outcome, Some(Outcome::InsufficientSavings { .. })),
        "{again:?}"
    );
    let attempts = h.app.db.attempts(None, 10).unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "the same file and fingerprint: one record"
    );
    assert_eq!(attempts[0].kind, AttemptKind::InsufficientSavings);
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
async fn a_rejected_output_that_saves_too_little_is_not_held() {
    let tools = require_tools!();
    // Every spot check fails, and nothing saves 99%.
    let h = common::harness(&tools, common::plain(), |g| {
        g.verify.ssim_hard = 1.01;
        g.min_savings_pct = 99.0;
    });
    let src = h.dir.path().join("media/a.mkv");
    common::sample(&tools, &src, 2);

    let t = h.app.submit_manual(&manual(&src)).unwrap();
    let done = common::settled(&h.app, t.id).await;
    assert_eq!(done.state, TaskState::InsufficientSavings, "{done:?}");
    assert!(done.candidate.is_none());
    let Some(Outcome::InsufficientSavings { report, .. }) = &done.outcome else {
        panic!("{done:?}");
    };
    // The report keeps the verdict.
    assert_eq!(report.verdict, fncompress_core::verify::Verdict::Rejected);
    let leftovers: Vec<_> = std::fs::read_dir(src.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().contains(".fncompress-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let attempt = &h.app.db.attempts(None, 10).unwrap()[0];
    assert_eq!(attempt.kind, AttemptKind::InsufficientSavings);
    assert!(
        attempt.message.starts_with("收益不足") && attempt.message.contains("校验未通过"),
        "{}",
        attempt.message
    );
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

/// Submitting a preview's file list queues just those files, without
/// walking the directory again, with one `batch_created` event; progress
/// events carry the request's `scan_id`. `force` skips probing, so this
/// runs without FFmpeg.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_queues_the_previewed_files_in_one_event() {
    let fake = common::Tools {
        ffmpeg: "missing-ffmpeg".into(),
        ffprobe: "missing-ffprobe".into(),
    };
    let h = common::harness(&fake, common::plain(), |_| {});
    // Nothing runs: only the queue is looked at.
    h.runner.abort();
    let media = h.dir.path().join("media");
    for name in ["a.mkv", "b.mp4", "sub/c.mkv", "notes.txt"] {
        let p = media.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, name).unwrap();
    }
    let req = BatchRequest {
        force: true,
        scan_id: Some("scan-1".into()),
        ..batch(&media)
    };
    let preview = h.app.preview_batch(&req).unwrap();
    assert_eq!(preview.files.len(), 3, "{preview:?}");

    // Added after the preview: not in the list, so not queued.
    std::fs::write(media.join("d.mkv"), "d").unwrap();
    let mut rx = h.app.events.subscribe();
    let listed = vec![preview.files[0].clone(), preview.files[2].clone()];
    let b = h
        .app
        .submit_batch(&BatchRequest {
            files: Some(listed.clone()),
            ..req.clone()
        })
        .unwrap();
    assert_eq!(
        b.tasks.iter().map(|t| t.path.clone()).collect::<Vec<_>>(),
        listed
    );

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }
    let progress: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::ScanProgress(p) => Some((p.scan_id.as_str(), p.phase, p.done, p.total)),
            _ => None,
        })
        .collect();
    assert_eq!(
        progress.first(),
        Some(&("scan-1", ScanPhase::Screening, 0, 2))
    );
    assert!(
        progress.contains(&("scan-1", ScanPhase::Screening, 2, 2)),
        "{progress:?}"
    );
    assert_eq!(
        progress.last(),
        Some(&("scan-1", ScanPhase::Queueing, 2, 2))
    );
    let created: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::TaskCreated { .. } | Event::BatchCreated { .. }))
        .collect();
    let [Event::BatchCreated { batch_id, tasks }] = &created[..] else {
        panic!("{created:?}")
    };
    assert_eq!(*batch_id, b.batch_id);
    assert_eq!(tasks.len(), 2);

    // A listed file must be a video under the directory, and not in a
    // subdirectory when the batch leaves them out.
    let outside = h.dir.path().join("x.mkv");
    std::fs::write(&outside, "x").unwrap();
    for (files, recursive) in [
        (vec![outside], true),
        (vec![preview.dir.join("../x.mkv")], true),
        (vec![preview.files[0].with_file_name("notes.txt")], true),
        (vec![preview.files[2].clone()], false),
    ] {
        let e = h
            .app
            .submit_batch(&BatchRequest {
                files: Some(files.clone()),
                recursive,
                ..req.clone()
            })
            .unwrap_err();
        assert_eq!(e.code, "bad_path", "{files:?}");
    }
}
