//! HDR10+ routes end to end: the candidate carries the source's HDR10+
//! messages frame for frame, in Matroska and MP4, and verification rejects a
//! candidate without them. Sources get their messages from our own injector
//! (see `common::hdr10_plus_source`), so any libx265 build will do.

mod common;

use std::path::{Path, PathBuf};

use fncompress_core::{
    encoder::Encoder,
    hdr10plus,
    pipeline::{self, Candidate, Job, Outcome},
    probe::{DynamicRange, MediaInfo, ProbeDepth, Prober},
    recommend::{self, AutoMethod, RecommendJob, RecommendOutcome, RecommendRequest, SampleSpec},
    route::RangeRoute,
    settings::{ContainerChoice, Profile, Resize, SettingsSnapshot},
    verify::{Severity, Verdict},
};

fn snapshot(tools: &common::Tools, container: ContainerChoice) -> SettingsSnapshot {
    let mut s = SettingsSnapshot {
        profile: Profile {
            id: "hdr10plus".into(),
            encoder: Encoder::Libx265,
            quality: 30,
            preset: Some("ultrafast".into()),
            container,
            ..Default::default()
        },
        ..Default::default()
    };
    s.global.ffmpeg = tools.ffmpeg.clone();
    s.global.ffprobe = tools.ffprobe.clone();
    s.global.min_savings_pct = f64::MIN;
    s
}

fn probe(tools: &common::Tools, path: &Path) -> MediaInfo {
    Prober::new(&tools.ffprobe)
        .probe(path, ProbeDepth::Full, None)
        .unwrap()
}

/// The HDR10+ messages of `path`, in display order (`None`: a frame without).
fn messages(tools: &common::Tools, path: &Path) -> Vec<Option<Vec<u8>>> {
    let scratch = path.with_extension("sei.mkv");
    hdr10plus::extract(
        &tools.ffmpeg,
        path,
        0,
        &scratch,
        std::time::Duration::from_secs(120),
        None,
    )
    .unwrap()
}

fn transcode(tools: &common::Tools, s: &SettingsSnapshot, src: &Path) -> Candidate {
    let job = Job {
        source: src,
        snapshot: s,
        force: false,
        available: None,
        publish: false,
        cancel: None,
    };
    match pipeline::transcode_file(&Prober::new(&tools.ffprobe), &job, |_| {}).unwrap() {
        Outcome::Verified { candidate } => candidate,
        other => panic!("expected a verified candidate, got {other:#?}"),
    }
}

fn hard(c: &Candidate) -> Vec<&str> {
    c.report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Hard)
        .map(|f| f.check.as_str())
        .collect()
}

#[test]
fn hdr10_plus_is_kept_frame_for_frame() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    for name in ["plus.mkv", "plus.mp4"] {
        // Its own directory: the MKV candidate of the MP4 needs the name.
        let dir = dir.path().join(name.replace('.', "-"));
        std::fs::create_dir(&dir).unwrap();
        let (src, sent) = common::hdr10_plus_source(&tools, &dir, name, 2);
        // The fixture itself: FFmpeg sees HDR10+ and the static metadata,
        // and the messages read back in display order.
        let info = probe(&tools, &src);
        assert_eq!(info.dynamic_range(), DynamicRange::DynamicHdr, "{name}");
        assert!(info.hdr.has_hdr10_plus(), "{name}");
        assert!(info.hdr.mastering_display.is_some(), "{name}");
        let want: Vec<_> = sent.iter().cloned().map(Some).collect();
        assert_eq!(messages(&tools, &src), want, "{name}");

        for (container, resize) in [
            (ContainerChoice::Keep, Resize::Keep),
            (ContainerChoice::Mkv, Resize::MaxHeight(180)),
        ] {
            let mut s = snapshot(&tools, container);
            s.profile.resize = resize;
            let c = transcode(&tools, &s, &src);
            assert_eq!(c.plan.dynamic_range, RangeRoute::Hdr10Plus);
            assert_ne!(
                c.report.verdict,
                Verdict::Rejected,
                "{:#?}",
                c.report.findings
            );
            assert!(hard(&c).is_empty(), "{:#?}", c.report.findings);
            let note = c
                .report
                .findings
                .iter()
                .find(|f| f.check == "hdr10_plus")
                .expect("an hdr10_plus note");
            assert_eq!(note.severity, Severity::Info);
            assert_eq!(c.report.inspection.hdr10_plus.as_ref().unwrap().found, 48);
            assert_eq!(messages(&tools, &c.path), want, "{name} {container:?}");
            let out = probe(&tools, &c.path);
            assert!(out.hdr.has_hdr10_plus());
            assert_eq!(out.hdr.content_light, info.hdr.content_light);
            assert_eq!(out.streams.len(), 2, "audio kept");
            std::fs::remove_file(&c.path).unwrap();
        }
    }
}

#[test]
fn candidates_without_hdr10_plus_are_rejected() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let (src, _) = common::hdr10_plus_source(&tools, dir.path(), "strip.mkv", 2);
    let s = snapshot(&tools, ContainerChoice::Keep);
    let c = transcode(&tools, &s, &src);
    let stripped = dir.path().join("stripped.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-i",
            c.path.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            "-bsf:v",
            "filter_units=remove_types=39",
            stripped.to_str().unwrap(),
        ],
    );
    let judged = pipeline::assess(&Prober::new(&tools.ffprobe), &src, &stripped, &s)
        .unwrap()
        .unwrap();
    assert!(
        hard(&judged).contains(&"hdr10_plus"),
        "{:#?}",
        judged.report.findings
    );
    assert_eq!(judged.report.verdict, Verdict::Rejected);
}

/// 样片推荐 of an HDR10+ source: HDR display proxy, the trials carry their
/// sample's messages.
#[test]
fn recommendations_inject_the_samples_messages() {
    let tools = require_tools!();
    let scorer = std::env::var_os("FNCOMPRESS_SCORING_FFMPEG")
        .map_or_else(|| tools.ffmpeg.clone(), PathBuf::from);
    let usable = recommend::probe_scorer(&scorer)
        .map(drop)
        .and_then(|()| recommend::probe_display_proxy(&scorer));
    if let Err(why) = usable {
        eprintln!("skipped: no scorer with libvmaf and libplacebo ({why})");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let (src, _) = common::hdr10_plus_source(&tools, dir.path(), "rec.mkv", 8);
    let mut s = snapshot(&tools, ContainerChoice::Keep);
    s.global.scoring_ffmpeg = Some(scorer);
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
    let workdir = dir.path().join("w");
    let job = RecommendJob {
        source: &src,
        snapshot: &s,
        request: &request,
        workdir: &workdir,
        available: None,
        cancel: None,
    };
    let outcome = recommend::run(&Prober::new(&tools.ffprobe), &job, |_| {}, |_| {}).unwrap();
    let rec = match outcome {
        RecommendOutcome::Finished { recommendation } => recommendation,
        other => panic!("{other:?}"),
    };
    assert_eq!(rec.scoring, recommend::Scoring::HdrProxy);
    let first = &rec.trials[0];
    assert!(
        first.scores.iter().all(|s| s.is_some_and(|s| s > 60.0)),
        "{first:?}"
    );
    assert!(!rec.candidates.is_empty());
}
