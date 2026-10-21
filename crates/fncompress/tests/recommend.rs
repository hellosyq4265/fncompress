//! 样片推荐 end to end: a recommendation runs in the scheduler, waits for a
//! choice, holds the monitor back meanwhile, and turns into a profile and
//! optionally a transcode. Needs a scoring FFmpeg with libvmaf.

mod common;

use std::{path::Path, path::PathBuf, time::Duration};

use fncompress::{
    app::{ChooseRequest, RecommendSubmit},
    config::MonitorInput,
    db::{now, TaskFilter},
    events::Event,
    janitor,
    model::{ServerSettings, Task, TaskKind, TaskSource, TaskState},
    server::{ServeOptions, Server},
};
use fncompress_core::recommend::{AutoMethod, RecommendRequest, SampleSpec};

/// The scoring FFmpeg: `FNCOMPRESS_SCORING_FFMPEG`, else the test FFmpeg
/// when it has libvmaf.
fn scorer(tools: &common::Tools) -> Option<PathBuf> {
    let path = std::env::var_os("FNCOMPRESS_SCORING_FFMPEG")
        .map_or_else(|| tools.ffmpeg.clone(), PathBuf::from);
    match fncompress_core::recommend::probe_scorer(&path) {
        Ok(_) => Some(path),
        Err(e) => {
            eprintln!("skipped: no usable scorer ({e}); set FNCOMPRESS_SCORING_FFMPEG");
            None
        }
    }
}

macro_rules! require_scorer {
    ($tools:expr) => {
        match scorer(&$tools) {
            Some(s) => s,
            None => return,
        }
    };
}

fn request() -> RecommendRequest {
    RecommendRequest {
        target_vmaf: 90.0,
        samples: SampleSpec::Auto {
            method: AutoMethod::Bitrate,
            count: 2,
            secs: 2.0,
        },
        presets: Some(vec!["ultrafast".into()]),
    }
}

fn submit(path: &Path) -> RecommendSubmit {
    RecommendSubmit {
        path: path.to_owned(),
        profile_id: None,
        request: request(),
    }
}

async fn server(tools: &common::Tools, scorer: &Path, dir: &Path) -> Server {
    let s = Server::start(&ServeOptions {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.join("data"),
        ffmpeg: Some(tools.ffmpeg.clone()),
        ffprobe: Some(tools.ffprobe.clone()),
        render_device: None,
        scoring_ffmpeg: Some(scorer.to_owned()),
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
    p.profile.encoder = fncompress_core::encoder::Encoder::Libx264;
    p.profile.quality = 23;
    p.profile.preset = Some("ultrafast".into());
    app.db.put_profile(&p).unwrap();
    app.db
        .set_server_settings(&ServerSettings {
            observe_first_n: 0,
            settle_secs: 1,
            min_free_gib: 0,
            ..Default::default()
        })
        .unwrap();
    s
}

async fn wait(s: &Server, id: i64, done: impl Fn(&Task) -> bool) -> Task {
    common::wait_for(&s.app, id, done).await
}

fn transcodes_of(s: &Server, path: &Path) -> Vec<Task> {
    s.app
        .db
        .tasks(&TaskFilter::default())
        .unwrap()
        .into_iter()
        .filter(|t| t.kind == TaskKind::Transcode && t.path == path)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn recommendation_waits_for_a_choice_and_holds_the_monitor_back() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    let dir = tempfile::tempdir().unwrap();
    let s = server(&tools, &scorer, dir.path()).await;
    let watch = dir.path().join("watch");
    let film = watch.join("film.mkv");
    common::sample(&tools, &film, 10);
    let film = fncompress::scan::canonical(&film).unwrap();
    let mut events = s.app.events.subscribe();

    let task = s.app.submit_recommend(&submit(&film)).unwrap();
    assert_eq!(task.kind, TaskKind::Recommend);
    assert_eq!(task.source, TaskSource::Manual);
    assert_eq!(task.recommend, Some(request()));
    // A second one for the same file is refused while this one is open.
    let again = s.app.submit_recommend(&submit(&film)).unwrap_err();
    assert_eq!(again.code, "task_exists");

    // The monitor finds the file but holds it back, without remembering that.
    s.app
        .create_monitor(&MonitorInput {
            path: watch.clone(),
            enabled: true,
            profile_id: None,
            recursive: true,
            excludes: vec![],
        })
        .unwrap();
    let t = wait(&s, task.id, |t| t.state == TaskState::AwaitingChoice).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(transcodes_of(&s, &film).is_empty());
    assert_eq!(s.app.db.seen_file(&film).unwrap(), None);

    let rec = t.recommendation.as_ref().unwrap();
    // The clip has one keyframe (x264's default GOP), so one window.
    assert_eq!(rec.samples.len(), 1);
    assert!(!rec.candidates.is_empty());
    assert!(t.pending_until.is_some());
    // Progress named the stages; measurements arrived before the end.
    let mut stages = Vec::new();
    let mut partial = false;
    while let Ok(ev) = events.try_recv() {
        match ev {
            Event::TaskProgress(p) if p.id == task.id => {
                stages.push(p.recommend.expect("recommendation progress").stage)
            }
            Event::TaskUpdated { task: u } if u.id == task.id => {
                partial |= u.state == TaskState::Running
                    && u.recommendation
                        .as_ref()
                        .is_some_and(|r| r.trials.iter().any(|t| !t.done));
            }
            _ => {}
        }
    }
    assert!(stages.len() >= 4, "{stages:?}");
    assert!(partial, "trials were sent while running");
    // Not deletable while awaiting a choice.
    assert_eq!(s.app.delete(task.id).unwrap_err().code, "task_active");

    // Choosing releases the file: the monitor queues it with its own profile.
    let q = rec.candidates[0].quality;
    assert_eq!(rec.candidates[0].preset.as_deref(), Some("ultrafast"));
    let chosen = s
        .app
        .choose(
            task.id,
            &ChooseRequest {
                preset: Some("ultrafast".into()),
                quality: q,
                profile_id: None,
                name: None,
                transcode: false,
            },
        )
        .unwrap();
    assert_eq!(chosen.task.state, TaskState::Chosen);
    assert_eq!(chosen.profile.id, format!("rec-{}-ultrafast-q{q}", task.id));
    assert_eq!(
        chosen.profile.name,
        format!("推荐：film 预设 ultrafast 质量 {q}")
    );
    assert_eq!(chosen.profile.profile.quality, q);
    assert_eq!(chosen.profile.profile.preset.as_deref(), Some("ultrafast"));
    assert!(chosen.transcode.is_none());
    let choice = chosen.task.choice.unwrap();
    assert_eq!((choice.quality, choice.transcode_task), (q, None));
    assert_eq!(choice.preset.as_deref(), Some("ultrafast"));

    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while transcodes_of(&s, &film).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "monitor did not queue"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let queued = &transcodes_of(&s, &film)[0];
    assert_eq!(queued.source, TaskSource::Monitor);
    assert_eq!(queued.profile_id, "default");
}

#[tokio::test(flavor = "multi_thread")]
async fn choose_with_transcode_discard_expiry_and_a_missing_scorer() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    let h = common::harness(&tools, common::plain(), |g| {
        g.scoring_ffmpeg = Some(scorer.clone())
    });
    let film = h.dir.path().join("media/film.mkv");
    common::sample(&tools, &film, 10);
    let film = fncompress::scan::canonical(&film).unwrap();

    // Choose and transcode at once; the transcode uses the new profile.
    let task = h.app.submit_recommend(&submit(&film)).unwrap();
    let t = common::wait_for(&h.app, task.id, |t| {
        t.state.is_final() || t.state == TaskState::AwaitingChoice
    })
    .await;
    assert_eq!(t.state, TaskState::AwaitingChoice, "{t:?}");
    let rec = t.recommendation.unwrap();
    let last = rec.candidates.last().unwrap();
    let q = last.quality;
    let chosen = h
        .app
        .choose(
            task.id,
            &ChooseRequest {
                preset: last.preset.clone(),
                quality: q,
                profile_id: Some("film-best".into()),
                name: Some("电影 最稳妥".into()),
                transcode: true,
            },
        )
        .unwrap();
    let transcode = chosen.transcode.expect("transcode queued");
    assert_eq!(chosen.transcode_error, None);
    assert_eq!(transcode.profile_id, "film-best");
    assert_eq!(transcode.snapshot.profile.quality, q);
    assert_eq!(
        chosen.task.choice.unwrap().transcode_task,
        Some(transcode.id)
    );
    let done = common::settled(&h.app, transcode.id).await;
    assert_eq!(done.state, TaskState::Published, "{done:?}");
    // A second choice is refused.
    let err = h
        .app
        .choose(
            task.id,
            &ChooseRequest {
                preset: last.preset.clone(),
                quality: q,
                profile_id: None,
                name: None,
                transcode: false,
            },
        )
        .unwrap_err();
    assert_eq!(err.code, "not_awaiting_choice");

    // Retry runs the same request again; discarding ends it unchosen.
    let other = h.dir.path().join("media/other.mkv");
    common::sample(&tools, &other, 10);
    let second = h.app.submit_recommend(&submit(&other)).unwrap();
    let t = common::wait_for(&h.app, second.id, |t| {
        t.state.is_final() || t.state == TaskState::AwaitingChoice
    })
    .await;
    assert_eq!(t.state, TaskState::AwaitingChoice, "{t:?}");
    let choose = |preset: &str, quality| {
        h.app.choose(
            second.id,
            &ChooseRequest {
                preset: Some(preset.into()),
                quality,
                profile_id: None,
                name: None,
                transcode: false,
            },
        )
    };
    assert_eq!(choose("ultrafast", 3).unwrap_err().code, "bad_request");
    // A candidate's quality with a preset that was not compared.
    let q = t.recommendation.as_ref().unwrap().candidates[0].quality;
    let bad = choose("medium", q).unwrap_err();
    assert_eq!(bad.code, "bad_request");
    assert!(bad.message.contains("预设 medium"), "{}", bad.message);
    // Presets the encoder does not have are refused up front.
    let mut odd = submit(&h.dir.path().join("media/other.mkv"));
    odd.request.presets = Some(vec!["placebo".into()]);
    assert_eq!(
        h.app.submit_recommend(&odd).unwrap_err().code,
        "bad_request"
    );
    let discarded = h.app.discard(second.id, TaskState::Discarded).unwrap();
    assert_eq!(discarded.state, TaskState::Discarded);
    let third = h.app.retry(second.id).unwrap();
    assert_eq!(third.kind, TaskKind::Recommend);
    assert_eq!(third.recommend, Some(request()));
    let t = common::wait_for(&h.app, third.id, |t| {
        t.state.is_final() || t.state == TaskState::AwaitingChoice
    })
    .await;
    assert_eq!(t.state, TaskState::AwaitingChoice, "{t:?}");

    // Nobody chooses: the janitor ends it after the retention period.
    let later = now() + 15 * 86_400;
    let swept = janitor::sweep(&h.app, later);
    assert_eq!(swept.expired_choices, [third.id]);
    assert_eq!(h.app.task(third.id).unwrap().state, TaskState::Expired);

    // Without a working scorer, nothing is queued.
    let mut g = h.app.db.global_settings().unwrap();
    g.scoring_ffmpeg = Some("no-such-ffmpeg".into());
    h.app.db.set_global_settings(&g).unwrap();
    let err = h.app.submit_recommend(&submit(&other)).unwrap_err();
    assert_eq!(err.code, "scoring_unavailable");
    assert!(err.message.contains("no-such-ffmpeg"), "{}", err.message);
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_target_fails_with_the_best_score() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    let h = common::harness(&tools, common::plain(), |g| {
        g.scoring_ffmpeg = Some(scorer.clone())
    });
    let film = h.dir.path().join("media/film.mkv");
    common::sample(&tools, &film, 6);
    let mut r = submit(&film);
    r.request.target_vmaf = 100.0;
    let task = h.app.submit_recommend(&r).unwrap();
    let t = common::settled(&h.app, task.id).await;
    assert_eq!(t.state, TaskState::Failed, "{t:?}");
    let msg = t.error.unwrap();
    assert!(
        msg.contains("VMAF 100") && msg.contains("最低样片分"),
        "{msg}"
    );
    let rec = t.recommendation.expect("measurements are kept");
    assert!(rec.candidates.is_empty());
    assert!(!rec.trials.is_empty());
    assert!(!h
        .dir
        .path()
        .join("recommend")
        .join(task.id.to_string())
        .exists());
}
