mod common;

use std::path::Path;

use fncompress_core::{
    encoder::Encoder,
    pipeline::{self, Job, Outcome},
    probe::{ProbeDepth, Prober},
    recycle,
    route::SkipCode,
    settings::{ContainerChoice, Profile, SettingsSnapshot},
};

fn snapshot(
    tools: &common::Tools,
    encoder: Encoder,
    quality: u8,
    container: ContainerChoice,
) -> SettingsSnapshot {
    let mut s = SettingsSnapshot {
        profile: Profile {
            id: "pub".into(),
            encoder,
            quality,
            preset: Some("ultrafast".into()),
            container,
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
        publish: true,
        cancel: None,
    };
    pipeline::transcode_file(&Prober::new(&tools.ffprobe), &job, |_| {}).unwrap()
}

/// A deliberately fat H.264 source so a CRF 30 re-encode saves plenty.
fn fat_source(tools: &common::Tools, path: &Path) {
    common::ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=640x360:r=25:d=3",
            "-f",
            "lavfi",
            "-i",
            "sine=d=3:sample_rate=48000",
            "-c:v",
            "libx264",
            "-crf",
            "8",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            path.to_str().unwrap(),
        ],
    );
}

fn hidden_leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".fncompress-") && n != recycle::DEFAULT_DIR)
        .collect()
}

#[test]
fn publishes_in_place_and_skips_the_result_next_time() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.mkv");
    fat_source(&tools, &src);
    let original = std::fs::read(&src).unwrap();
    let s = snapshot(&tools, Encoder::Libx265, 30, ContainerChoice::Keep);

    let Outcome::Published {
        published, savings, ..
    } = run(&tools, &s, &src)
    else {
        panic!("not published");
    };
    assert_eq!(published.target, src);
    assert!(savings.saved_pct > 5.0, "{savings:?}");
    assert_eq!(std::fs::read(&published.recycled.file).unwrap(), original);
    assert_eq!(
        recycle::list(&dir.path().join(recycle::DEFAULT_DIR))
            .unwrap()
            .len(),
        1
    );
    assert!(hidden_leftovers(dir.path()).is_empty());

    let now = Prober::new(&tools.ffprobe)
        .probe(&src, ProbeDepth::Quick, None)
        .unwrap();
    assert_eq!(now.marker.unwrap().profile, "pub");
    match run(&tools, &s, &src) {
        Outcome::Skipped { reason } => assert_eq!(reason.code, SkipCode::AlreadyProcessed),
        other => panic!("{other:?}"),
    }
}

#[test]
fn container_change_publishes_new_extension() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("b.mp4");
    fat_source(&tools, &src);
    let s = snapshot(&tools, Encoder::Libx265, 30, ContainerChoice::Mkv);

    let Outcome::Published { published, .. } = run(&tools, &s, &src) else {
        panic!("not published");
    };
    assert_eq!(published.target, dir.path().join("b.mkv"));
    assert!(published.target.exists());
    assert!(!src.exists(), "the source moved to the recycle");
    assert_eq!(published.recycled.original_path, src);
}

#[test]
fn insufficient_savings_keeps_the_source() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("c.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=25:d=2",
            "-c:v",
            "libx264",
            "-crf",
            "35",
            src.to_str().unwrap(),
        ],
    );
    let before = std::fs::read(&src).unwrap();
    // CRF 0 is lossless: far larger than the source.
    let s = snapshot(&tools, Encoder::Libx264, 0, ContainerChoice::Keep);
    match run(&tools, &s, &src) {
        Outcome::InsufficientSavings {
            savings,
            min_savings_pct,
            ..
        } => {
            assert!(savings.saved_pct < min_savings_pct);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read(&src).unwrap(), before);
    assert!(
        hidden_leftovers(dir.path()).is_empty(),
        "the candidate was removed"
    );
    assert!(!dir.path().join(recycle::DEFAULT_DIR).exists());
}
