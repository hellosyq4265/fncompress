mod common;

use std::path::{Path, PathBuf};

use fncompress_core::{
    encoder::Encoder,
    probe::Prober,
    recommend::{
        self, AutoMethod, RecommendJob, RecommendOutcome, RecommendProgress, RecommendRequest,
        RecommendStage, Recommendation, SampleKind, SampleSpec,
    },
    route::SkipCode,
    settings::{Profile, SettingsSnapshot},
    tool::CancelToken,
};

/// The scoring FFmpeg: `FNCOMPRESS_SCORING_FFMPEG`, else the test FFmpeg
/// when it has libvmaf. `None` skips the test.
fn scorer(tools: &common::Tools) -> Option<PathBuf> {
    let path = std::env::var_os("FNCOMPRESS_SCORING_FFMPEG")
        .map_or_else(|| tools.ffmpeg.clone(), PathBuf::from);
    match recommend::probe_scorer(&path) {
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

fn snapshot(tools: &common::Tools, scorer: &Path) -> SettingsSnapshot {
    let mut s = SettingsSnapshot {
        profile: Profile {
            encoder: Encoder::Libx264,
            quality: 23,
            preset: Some("ultrafast".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    s.global.ffmpeg = tools.ffmpeg.clone();
    s.global.ffprobe = tools.ffprobe.clone();
    s.global.scoring_ffmpeg = Some(scorer.to_owned());
    s
}

/// 30 s of 320x240 at 24 fps, a keyframe every second; the second half is
/// noisier (harder to compress).
fn source(tools: &common::Tools, dir: &Path) -> PathBuf {
    let src = dir.join("film.mkv");
    common::ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=24:d=15",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=24:d=15,noise=alls=30:allf=t",
            "-filter_complex",
            "[0][1]concat=n=2:v=1[v]",
            "-map",
            "[v]",
            "-c:v",
            "libx264",
            "-g",
            "24",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            src.to_str().unwrap(),
        ],
    );
    src
}

struct Run {
    outcome: Result<RecommendOutcome, recommend::RecommendError>,
    progress: Vec<RecommendProgress>,
    updates: Vec<Recommendation>,
}

fn run(
    tools: &common::Tools,
    snapshot: &SettingsSnapshot,
    src: &Path,
    request: &RecommendRequest,
    workdir: &Path,
    cancel: Option<&CancelToken>,
) -> Run {
    let prober = Prober::new(&tools.ffprobe);
    let mut progress = Vec::new();
    let mut updates = Vec::new();
    let job = RecommendJob {
        source: src,
        snapshot,
        request,
        workdir,
        available: None,
        cancel,
    };
    let outcome = recommend::run(
        &prober,
        &job,
        |p| progress.push(p),
        |r| updates.push(r.clone()),
    );
    Run {
        outcome,
        progress,
        updates,
    }
}

fn finished(run: &Run) -> &Recommendation {
    match &run.outcome {
        Ok(RecommendOutcome::Finished { recommendation }) => recommendation,
        other => panic!("{other:?}"),
    }
}

#[test]
fn scorer_check_reports_the_version() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    let info = recommend::probe_scorer(&scorer).unwrap();
    assert!(!info.version.is_empty() && info.version != "未知");
    assert!(recommend::probe_scorer(Path::new("no-such-ffmpeg")).is_err());
}

#[test]
fn auto_samples_by_bitrate_give_candidates_that_reach_the_target() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    let dir = tempfile::tempdir().unwrap();
    let src = source(&tools, dir.path());
    let snapshot = snapshot(&tools, &scorer);
    let request = RecommendRequest {
        target_vmaf: 90.0,
        samples: SampleSpec::Auto {
            method: AutoMethod::Bitrate,
            count: 2,
            secs: 3.0,
        },
        // Fastest first, whatever the order asked.
        presets: Some(vec!["superfast".into(), "ultrafast".into()]),
    };
    let workdir = dir.path().join("work");
    let run = run(&tools, &snapshot, &src, &request, &workdir, None);
    let rec = finished(&run);
    assert!(!workdir.exists(), "scratch directory is removed");

    assert_eq!(rec.samples.len(), 2);
    for s in &rec.samples {
        assert_eq!(s.kind, SampleKind::Auto);
        assert_eq!(s.path, src);
        // 3 s at 24 fps from a keyframe; B-frame reordering adds a few.
        assert!((72..=76).contains(&s.frames), "{s:?}");
        assert!(s.video_bytes > 0 && s.bitrate_bps > 0.0);
        assert!(s.reason.starts_with("码率"), "{}", s.reason);
    }
    // The noisy half has the higher bitrate.
    let peak = rec
        .samples
        .iter()
        .find(|s| s.reason.contains("最高"))
        .unwrap();
    assert!(peak.start_s.unwrap() >= 14.0, "{peak:?}");

    assert_eq!(rec.model, "vmaf_v0.6.1");
    assert_eq!(rec.range, [14, 36]);
    assert_eq!(rec.trials[0].quality, 23, "starts at the base quality");
    let names: Vec<_> = rec.presets.iter().map(|p| p.preset.as_deref()).collect();
    assert_eq!(names, [Some("ultrafast"), Some("superfast")]);
    let of = |p: &str| -> Vec<_> {
        rec.trials
            .iter()
            .filter(|t| t.preset.as_deref() == Some(p))
            .collect()
    };
    let (first, second) = (of("ultrafast"), of("superfast"));
    assert_eq!(first.len() + second.len(), rec.trials.len());
    assert!(first.len() >= 3 && first.len() <= recommend::MAX_TRIALS);
    assert!(!second.is_empty() && second.len() <= recommend::MAX_TRIALS);
    // Trials run preset by preset; the second starts at the first's boundary.
    assert!(rec.trials[..first.len()]
        .iter()
        .all(|t| t.preset.as_deref() == Some("ultrafast")));
    let boundary = first.iter().filter(|t| t.passed).map(|t| t.quality).max();
    assert_eq!(Some(second[0].quality), boundary);
    for p in &rec.presets {
        assert!(p.done, "{p:?}");
        assert!(
            p.speed.unwrap() > 0.0 && p.full_secs.unwrap() > 0.0,
            "{p:?}"
        );
        assert!(!p.over_timeout);
        assert_eq!(p.best.as_ref().unwrap().preset, p.preset);
    }
    assert!(rec.presets.iter().any(|p| p.front));
    for t in &rec.trials {
        assert!(t.done);
        let min = t.min_score.unwrap();
        assert_eq!(
            t.passed,
            t.scores.iter().all(|s| s.is_some()) && min >= 90.0
        );
        if t.passed {
            assert!(t.size_ratio.unwrap() > 0.0 && t.bitrate_bps.unwrap() > 0.0);
        }
    }
    assert!(!rec.candidates.is_empty());
    for c in &rec.candidates {
        assert!(c.min_score >= 90.0);
        assert_eq!(c.scores.len(), 2);
    }
    for (name, trials) in [("ultrafast", &first), ("superfast", &second)] {
        let cands: Vec<_> = rec
            .candidates
            .iter()
            .filter(|c| c.preset.as_deref() == Some(name))
            .collect();
        assert!(!cands.is_empty() && cands.len() <= 3, "{name}");
        assert!(cands
            .windows(2)
            .all(|p| p[0].video_bytes <= p[1].video_bytes));
        // The cheapest candidate is the largest quality that passed.
        let best = trials.iter().filter(|t| t.passed).map(|t| t.quality).max();
        assert_eq!(Some(cands[0].quality), best, "{name}");
    }
    // Candidates come preset by preset.
    assert_eq!(rec.candidates[0].preset.as_deref(), Some("ultrafast"));

    // Progress went through every stage; scores arrived sample by sample.
    let stages: Vec<_> = run.progress.iter().map(|p| p.stage).collect();
    for s in [
        RecommendStage::Analyzing,
        RecommendStage::Sampling,
        RecommendStage::Encoding,
        RecommendStage::Scoring,
    ] {
        assert!(stages.contains(&s), "{s:?}");
    }
    assert!(run.progress.iter().all(|p| p.percent <= 99.0));
    assert!(run.updates.iter().any(|u| u
        .trials
        .first()
        .is_some_and(|t| !t.done && t.scores.iter().any(Option::is_some))));
    assert_eq!(run.updates.last().unwrap(), rec);
}

#[test]
fn scene_analysis_and_ready_made_samples() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    let dir = tempfile::tempdir().unwrap();
    let src = source(&tools, dir.path());
    let snapshot = snapshot(&tools, &scorer);

    let request = RecommendRequest {
        target_vmaf: 90.0,
        samples: SampleSpec::Auto {
            method: AutoMethod::Scene,
            count: 2,
            secs: 3.0,
        },
        presets: Some(vec!["ultrafast".into()]),
    };
    let run1 = run(
        &tools,
        &snapshot,
        &src,
        &request,
        &dir.path().join("w1"),
        None,
    );
    let rec = finished(&run1);
    assert_eq!(rec.samples.len(), 2);
    assert!(rec.samples.iter().all(|s| !s.reason.is_empty()));
    assert!(run1
        .progress
        .iter()
        .any(|p| p.stage == RecommendStage::Analyzing && p.percent > 0.0));

    // A ready-made sample with the source's format.
    let clip = dir.path().join("clip.mp4");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=24:d=2",
            "-f",
            "lavfi",
            "-i",
            "sine=d=2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            clip.to_str().unwrap(),
        ],
    );
    let request = RecommendRequest {
        target_vmaf: 90.0,
        samples: SampleSpec::Files {
            paths: vec![clip.clone()],
        },
        presets: Some(vec!["ultrafast".into()]),
    };
    let run2 = run(
        &tools,
        &snapshot,
        &src,
        &request,
        &dir.path().join("w2"),
        None,
    );
    let rec = finished(&run2);
    assert_eq!(rec.samples[0].kind, SampleKind::File);
    assert_eq!(rec.samples[0].path, clip);
    assert_eq!(rec.samples[0].frames, 48);
    assert!(clip.exists(), "ready-made samples are left alone");

    // A sample of another size is refused with the reason.
    let small = dir.path().join("small.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x120:r=24:d=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            small.to_str().unwrap(),
        ],
    );
    let request = RecommendRequest {
        target_vmaf: 90.0,
        samples: SampleSpec::Files { paths: vec![small] },
        presets: Some(vec!["ultrafast".into()]),
    };
    let run3 = run(
        &tools,
        &snapshot,
        &src,
        &request,
        &dir.path().join("w3"),
        None,
    );
    let err = run3.outcome.unwrap_err().to_string();
    assert!(err.contains("分辨率 160×120 与原片 320×240 不同"), "{err}");
}

#[test]
fn unsupported_sources_are_skipped_and_cancel_stops() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    let dir = tempfile::tempdir().unwrap();
    let snapshot = snapshot(&tools, &scorer);

    let hdr = dir.path().join("hdr.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=24:d=2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p10le",
            "-x264-params",
            "colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc",
            hdr.to_str().unwrap(),
        ],
    );
    let request = RecommendRequest::default();
    let r = run(
        &tools,
        &snapshot,
        &hdr,
        &request,
        &dir.path().join("w"),
        None,
    );
    match r.outcome {
        Ok(RecommendOutcome::Skipped { reason }) => assert_eq!(reason.code, SkipCode::HdrEncoder),
        other => panic!("{other:?}"),
    }

    let src = source(&tools, dir.path());
    let cancel = CancelToken::default();
    cancel.cancel();
    let workdir = dir.path().join("cancelled");
    let r = run(&tools, &snapshot, &src, &request, &workdir, Some(&cancel));
    let err = r.outcome.unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert!(!workdir.exists());
}

#[test]
fn hdr10_is_scored_through_the_display_proxy() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    if let Err(e) = recommend::probe_display_proxy(&scorer) {
        eprintln!("skipped: no display proxy ({e})");
        return;
    }
    assert!(recommend::probe_display_proxy(Path::new("no-such-ffmpeg")).is_err());
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("hdr10.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=24:d=12,setparams=color_primaries=bt2020:\
             color_trc=smpte2084:colorspace=bt2020nc:range=tv",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p10le",
            "-x265-params",
            "log-level=error:keyint=24:master-display=G(13250,34500)B(7500,3000)\
             R(34000,16000)WP(15635,16450)L(10000000,50):max-cll=1000,400",
            src.to_str().unwrap(),
        ],
    );
    let mut s = snapshot(&tools, &scorer);
    s.profile.encoder = Encoder::Libx265;
    s.profile.quality = 28;
    let request = RecommendRequest {
        target_vmaf: 60.0,
        samples: SampleSpec::Auto {
            method: AutoMethod::Bitrate,
            count: 2,
            secs: 2.0,
        },
        presets: Some(vec!["ultrafast".into()]),
    };
    let r = run(&tools, &s, &src, &request, &dir.path().join("w"), None);
    let rec = finished(&r);
    assert_eq!(rec.scoring, recommend::Scoring::HdrProxy);
    assert_eq!(rec.samples.len(), 2);
    let first = &rec.trials[0];
    assert!(
        first.scores.iter().all(|s| s.is_some_and(|s| s > 60.0)),
        "{first:?}"
    );
    assert!(!rec.candidates.is_empty());
}

#[test]
fn samples_get_parameter_sets_that_are_only_at_the_start() {
    let tools = require_tools!();
    let scorer = require_scorer!(tools);
    let dir = tempfile::tempdir().unwrap();
    let src = common::hevc_with_parameter_sets_only_at_the_start(&tools, dir.path(), 12);
    let mut s = snapshot(&tools, &scorer);
    s.profile.encoder = Encoder::Libx265;
    s.profile.quality = 28;
    let request = RecommendRequest {
        target_vmaf: 60.0,
        samples: SampleSpec::Auto {
            method: AutoMethod::Bitrate,
            count: 2,
            secs: 2.0,
        },
        presets: Some(vec!["ultrafast".into()]),
    };
    let r = run(&tools, &s, &src, &request, &dir.path().join("w"), None);
    let rec = finished(&r);
    assert_eq!(rec.samples.len(), 2, "{:?}", rec.samples);
    assert!(
        rec.samples
            .iter()
            .all(|s| s.start_s.is_some_and(|t| t > 0.5)),
        "{:?}",
        rec.samples
    );
    let first = &rec.trials[0];
    assert!(
        first.scores.iter().all(|s| s.is_some_and(|s| s > 60.0)),
        "{first:?}"
    );
}
