//! 转码前预判收益 (backlog 16) through the pipeline: a transcode the
//! estimate says cannot pay off is skipped before it starts; one that can
//! runs and reports the estimate.

mod common;

use std::path::Path;

use fncompress_core::{
    encoder::Encoder,
    pipeline::{self, Job, Outcome},
    predict,
    probe::Prober,
    settings::{Profile, SettingsSnapshot},
};

/// Long enough for the samples to stay under a tenth of the film.
const SECS: u32 = 420;

fn snapshot(tools: &common::Tools, encoder: Encoder, quality: u8) -> SettingsSnapshot {
    let mut s = SettingsSnapshot {
        profile: Profile {
            id: "predict".into(),
            encoder,
            quality,
            preset: Some("ultrafast".into()),
            predict_savings: true,
            ..Default::default()
        },
        ..Default::default()
    };
    s.global.ffmpeg = tools.ffmpeg.clone();
    s.global.ffprobe = tools.ffprobe.clone();
    s
}

fn run(tools: &common::Tools, s: &SettingsSnapshot, src: &Path) -> Outcome {
    let job = Job {
        source: src,
        snapshot: s,
        force: false,
        available: None,
        publish: false,
        cancel: None,
    };
    pipeline::transcode_file(&Prober::new(&tools.ffprobe), &job, |_| {}).unwrap()
}

/// Small H.264 with audio; `crf` sets how much a re-encode can save.
fn source(tools: &common::Tools, path: &Path, crf: &str) {
    common::ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=s=160x120:r=10:d={SECS}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=d={SECS}:sample_rate=48000"),
            "-c:v",
            "libx264",
            "-crf",
            crf,
            "-preset",
            "ultrafast",
            "-g",
            "20",
            "-c:a",
            "aac",
            path.to_str().unwrap(),
        ],
    );
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".fncompress-"))
        .collect()
}

#[test]
fn a_transcode_predicted_to_grow_is_skipped() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("lean.mkv");
    source(&tools, &src, "35");
    let before = std::fs::read(&src).unwrap();
    // CRF 0 is lossless: far larger than the source.
    let s = snapshot(&tools, Encoder::Libx264, 0);
    match run(&tools, &s, &src) {
        Outcome::PredictedNoGain {
            estimate,
            min_savings_pct,
        } => {
            assert_eq!(estimate.samples, predict::SAMPLES);
            assert!(estimate.saved_pct < 0.0, "{estimate:?}");
            assert!(estimate.no_gain(min_savings_pct));
            assert_eq!(estimate.source_size, before.len() as u64);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(&src).unwrap(), before);
    assert!(
        leftovers(dir.path()).is_empty(),
        "{:?}",
        leftovers(dir.path())
    );
}

#[test]
fn a_promising_transcode_runs_and_reports_the_estimate() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("fat.mkv");
    source(&tools, &src, "8");
    let s = snapshot(&tools, Encoder::Libx264, 30);
    let candidate = match run(&tools, &s, &src) {
        Outcome::Verified { candidate } => candidate,
        other => panic!("{other:?}"),
    };
    let note = candidate
        .report
        .findings
        .iter()
        .find(|f| f.check == "predict_savings")
        .expect("the estimate is in the report");
    assert!(note.message.contains("预判节省约"), "{}", note.message);
    // The picture is the same all through, so the estimate is close.
    let estimated: f64 = note
        .message
        .split("约 ")
        .nth(1)
        .and_then(|m| m.split('%').next())
        .and_then(|n| n.parse().ok())
        .unwrap();
    let saved = candidate.savings.saved_pct;
    assert!(
        (estimated - saved).abs() < 10.0,
        "estimated {estimated}, saved {saved}"
    );
    std::fs::remove_file(&candidate.path).unwrap();
    assert!(
        leftovers(dir.path()).is_empty(),
        "{:?}",
        leftovers(dir.path())
    );
}

#[test]
fn samples_of_hevc_with_parameter_sets_only_at_the_start_decode() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = common::hevc_with_parameter_sets_only_at_the_start(&tools, dir.path(), SECS);
    let s = snapshot(&tools, Encoder::Libx264, 0);
    match run(&tools, &s, &src) {
        Outcome::PredictedNoGain { estimate, .. } => {
            assert_eq!(estimate.samples, predict::SAMPLES);
            assert!(estimate.video_ratio > 1.0, "{estimate:?}");
        }
        other => panic!("{other:?}"),
    }
    assert!(
        leftovers(dir.path()).is_empty(),
        "{:?}",
        leftovers(dir.path())
    );
}

#[test]
fn short_films_are_transcoded_without_an_estimate() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("short.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x120:r=10:d=20",
            "-c:v",
            "libx264",
            "-crf",
            "8",
            src.to_str().unwrap(),
        ],
    );
    let s = snapshot(&tools, Encoder::Libx264, 30);
    let candidate = match run(&tools, &s, &src) {
        Outcome::Verified { candidate } => candidate,
        other => panic!("{other:?}"),
    };
    let note = candidate
        .report
        .findings
        .iter()
        .find(|f| f.check == "predict_savings")
        .unwrap();
    assert!(note.message.contains("没有预判"), "{}", note.message);
    std::fs::remove_file(&candidate.path).unwrap();
}
