//! Dolby Vision routes end to end. Sources are built from lavfi pictures
//! (with B-frames) and RPUs that differ on every frame, so a frame that gets
//! another frame's RPU shows up. Needs FFmpeg and dovi_tool.

mod common;

use std::path::{Path, PathBuf};

use fncompress_core::{
    dovi::{self, mkv},
    encoder::Encoder,
    pipeline::{self, Candidate, Job, Outcome},
    probe::{DoviConfig, MediaInfo, ProbeDepth, Prober},
    recommend,
    route::SkipCode,
    settings::{ContainerChoice, Profile, SettingsSnapshot},
    verify::{RpuCheck, Severity, Verdict},
};

const FRAMES: usize = 48;

struct Env {
    tools: common::Tools,
    dovi: PathBuf,
    dir: tempfile::TempDir,
}

impl Env {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn ffmpeg(&self, args: &[&str]) {
        common::ffmpeg(&self.tools, args);
    }

    fn snapshot(&self, container: ContainerChoice) -> SettingsSnapshot {
        let mut s = SettingsSnapshot {
            profile: Profile {
                id: "dv".into(),
                encoder: Encoder::Libx265,
                quality: 30,
                preset: Some("ultrafast".into()),
                container,
                ..Default::default()
            },
            ..Default::default()
        };
        s.global.ffmpeg = self.tools.ffmpeg.clone();
        s.global.ffprobe = self.tools.ffprobe.clone();
        s.global.dovi_tool = Some(self.dovi.clone());
        s.global.min_savings_pct = f64::MIN;
        s
    }

    fn probe(&self, path: &Path) -> MediaInfo {
        Prober::new(&self.tools.ffprobe)
            .probe(path, ProbeDepth::Full, None)
            .unwrap()
    }

    /// The RPUs of `path`, as dovi_tool reads them, in display order.
    fn rpus_of(&self, path: &Path, to_p81: bool) -> Vec<Vec<u8>> {
        let out = self.path(&format!(
            "{}.check.rpu",
            path.file_name().unwrap().to_string_lossy()
        ));
        let video = self.probe(path).primary_video.unwrap();
        dovi::DoviTool::new(&self.dovi)
            .extract(
                &self.tools.ffmpeg,
                path,
                video,
                to_p81,
                &out,
                std::time::Duration::from_secs(120),
                None,
            )
            .unwrap();
        dovi::read_rpus(&out).unwrap()
    }

    fn transcode(&self, src: &Path, container: ContainerChoice) -> Candidate {
        let s = self.snapshot(container);
        let job = Job {
            source: src,
            snapshot: &s,
            force: false,
            available: None,
            publish: false,
            cancel: None,
        };
        match pipeline::transcode_file(&Prober::new(&self.tools.ffprobe), &job, |_| {}).unwrap() {
            Outcome::Verified { candidate } => candidate,
            other => panic!("expected a verified candidate, got {other:#?}"),
        }
    }
}

fn env() -> Option<Env> {
    let tools = common::tools()?;
    let dovi = common::dovi_tool()?;
    Some(Env {
        tools,
        dovi,
        dir: tempfile::tempdir().unwrap(),
    })
}

macro_rules! require_env {
    () => {
        match env() {
            Some(e) => e,
            None => {
                eprintln!("skipped: needs ffmpeg/ffprobe and dovi_tool (FNCOMPRESS_DOVI_TOOL)");
                return;
            }
        }
    };
}

fn config(profile: u8, compat: u8) -> DoviConfig {
    DoviConfig {
        version_major: 1,
        version_minor: 0,
        profile,
        level: 1,
        rpu_present: true,
        el_present: profile == 7,
        bl_present: true,
        bl_compat_id: compat,
        md_compression: 0,
    }
}

/// `FRAMES` RPUs of `profile` (`8.1`, `8.4`, `5`), each with its own L2 trim.
fn generated_rpus(e: &Env, profile: &str) -> PathBuf {
    let shots: Vec<_> = (0..FRAMES)
        .map(|i| {
            serde_json::json!({
                "start": i, "duration": 1,
                "metadata_blocks": [{"Level2": {
                    "target_max_pq": 2081, "trim_slope": 2048 + i, "trim_offset": 2048,
                    "trim_power": 2048, "trim_chroma_weight": 2048,
                    "trim_saturation_gain": 2048, "ms_weight": 2048
                }}]
            })
        })
        .collect();
    let json = serde_json::json!({
        "cm_version": "V40", "profile": profile, "length": FRAMES,
        "level6": {
            "max_display_mastering_luminance": 1000, "min_display_mastering_luminance": 1,
            "max_content_light_level": 1000, "max_frame_average_light_level": 400
        },
        "shots": shots,
    });
    let config = e.path(&format!("gen-{profile}.json"));
    std::fs::write(&config, json.to_string()).unwrap();
    let out = e.path(&format!("gen-{profile}.rpu"));
    common::run(
        &e.dovi,
        &[
            "generate",
            "-j",
            config.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
    );
    out
}

fn lavfi_video(transfer: &str, size: &str) -> String {
    format!(
        "testsrc2=s={size}:r=24:d=2,setparams=color_primaries=bt2020:color_trc={transfer}:\
         colorspace=bt2020nc:range=tv"
    )
}

/// Adds AAC and writes `name` (mkv or mp4) from a video-only DV Matroska file.
fn with_audio(e: &Env, video: &Path, name: &str, mp4_tag: &str) -> PathBuf {
    let out = e.path(name);
    let mut args = vec![
        "-i",
        video.to_str().unwrap(),
        "-f",
        "lavfi",
        "-i",
        "sine=d=2:sample_rate=48000",
        "-map",
        "0",
        "-map",
        "1",
        "-c:v",
        "copy",
        "-c:a",
        "aac",
        // The mux takes every stream's tags from the source.
        "-metadata:s:a:0",
        "language=kor",
    ];
    if name.ends_with(".mp4") {
        args.extend(["-strict", "unofficial", "-tag:v", mp4_tag]);
    }
    args.push(out.to_str().unwrap());
    e.ffmpeg(&args);
    out
}

/// A P5 or P8 source: 10-bit HEVC with B-frames, the RPUs of
/// [`generated_rpus`] and the configuration, plus AAC. Returns the source
/// and its RPU file.
fn dv_source(e: &Env, profile: &str, name: &str) -> (PathBuf, PathBuf) {
    dv_source_with(e, profile, name, None)
}

/// [`dv_source`], with these HDR10+ messages on the frames as well.
fn dv_source_with(
    e: &Env,
    profile: &str,
    name: &str,
    hdr10_plus: Option<&[Vec<u8>]>,
) -> (PathBuf, PathBuf) {
    let rpu = generated_rpus(e, profile);
    let transfer = if profile == "8.4" {
        "arib-std-b67"
    } else {
        "smpte2084"
    };
    let bl = e.path(&format!("bl-{name}.mkv"));
    e.ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &lavfi_video(transfer, "320x180"),
        "-c:v",
        "libx265",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p10le",
        "-x265-params",
        "log-level=error",
        bl.to_str().unwrap(),
    ]);
    let cfg = match profile {
        "8.1" => config(8, 1),
        "8.4" => config(8, 4),
        _ => config(5, 0),
    };
    let dv = e.path(&format!("dv-{name}.mkv"));
    let rpus = dovi::read_rpus(&rpu).unwrap();
    assert_eq!(rpus.len(), FRAMES);
    let metadata = mkv::FrameMetadata {
        rpus: Some((&rpus, &cfg)),
        hdr10_plus,
    };
    mkv::inject_frames(&bl, &metadata, &dv).unwrap();
    let tag = if profile == "5" { "dvh1" } else { "hvc1" };
    (with_audio(e, &dv, name, tag), rpu)
}

/// Single-frame P7 RPUs from dovi_tool's test assets (fixtures/dovi).
const P7_MEL: &[u8] = include_bytes!("fixtures/dovi/mel_orig.bin");
const P7_FEL: &[u8] = include_bytes!("fixtures/dovi/fel_orig.bin");

/// A P7 dual-layer source (single track): base layer and a quarter-size
/// enhancement layer carrying the RPU `one` on every frame. No B-frames:
/// the layers are muxed as raw HEVC, which carries no timestamps.
fn p7_source(e: &Env, one: &[u8], name: &str) -> PathBuf {
    let rpu = e.path(&format!("{name}.rpu"));
    std::fs::write(&rpu, one.repeat(FRAMES)).unwrap();
    let layer = |size: &str, out: &str| {
        let path = e.path(out);
        e.ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &lavfi_video("smpte2084", size),
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p10le",
            "-x265-params",
            "log-level=error:bframes=0",
            "-f",
            "hevc",
            path.to_str().unwrap(),
        ]);
        path
    };
    let (bl, el) = (
        layer("320x180", "p7-bl.hevc"),
        layer("160x90", "p7-el.hevc"),
    );
    let el_rpu = e.path("p7-el-rpu.hevc");
    let muxed = e.path("p7.hevc");
    let s = |p: &Path| p.to_str().unwrap().to_owned();
    common::run(
        &e.dovi,
        &[
            "inject-rpu",
            "-i",
            &s(&el),
            "--rpu-in",
            &s(&rpu),
            "-o",
            &s(&el_rpu),
        ],
    );
    common::run(
        &e.dovi,
        &[
            "mux",
            "--bl",
            &s(&bl),
            "--el",
            &s(&el_rpu),
            "-o",
            &s(&muxed),
        ],
    );
    let plain = e.path("p7-plain.mkv");
    e.ffmpeg(&["-r", "24", "-i", &s(&muxed), "-c", "copy", &s(&plain)]);
    let dv = e.path("p7-dv.mkv");
    mkv::set_config(&plain, &config(7, 6), &dv).unwrap();
    with_audio(e, &dv, name, "hvc1")
}

fn assert_clean(c: &Candidate) {
    assert_ne!(
        c.report.verdict,
        Verdict::Rejected,
        "{:#?}",
        c.report.findings
    );
    let hard: Vec<_> = c
        .report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Hard)
        .collect();
    assert!(hard.is_empty(), "{hard:#?}");
    assert_eq!(
        c.report.inspection.rpu,
        Some(RpuCheck {
            expected: FRAMES as u64,
            found: FRAMES as u64,
            first_mismatch: None
        })
    );
}

fn note(c: &Candidate) -> &str {
    &c.report
        .findings
        .iter()
        .find(|f| f.check == "dolby_vision")
        .expect("a dolby_vision note")
        .message
}

#[test]
fn p5_and_p8_keep_their_rpu_frame_for_frame() {
    let e = require_env!();
    for (profile, name, container, cfg, tag) in [
        ("8.1", "p81.mkv", ContainerChoice::Keep, config(8, 1), None),
        ("8.4", "p84.mkv", ContainerChoice::Keep, config(8, 4), None),
        (
            "5",
            "p5.mp4",
            ContainerChoice::Keep,
            config(5, 0),
            Some("dvh1"),
        ),
        (
            "8.1",
            "p81-to-mp4.mkv",
            ContainerChoice::Mp4,
            config(8, 1),
            Some("hvc1"),
        ),
    ] {
        let (src, rpu) = dv_source(&e, profile, name);
        let generated = dovi::read_rpus(&rpu).unwrap();
        // The fixture itself: dovi_tool reads back the generated RPUs, in
        // display order, and FFmpeg sees the configuration.
        assert_eq!(e.rpus_of(&src, false), generated, "{name}");
        let info = e.probe(&src);
        assert_eq!(info.hdr.dolby_vision, Some(cfg), "{name}");
        assert!(info.hdr.dovi_rpu, "{name}");

        let c = e.transcode(&src, container);
        assert_eq!(c.plan.dynamic_range.dolby_vision(), Some((&cfg, &cfg)));
        assert_clean(&c);
        let out = e.probe(&c.path);
        assert_eq!(out.hdr.dolby_vision, Some(cfg), "{name}");
        assert!(out.hdr.dovi_rpu, "{name}");
        assert_eq!(e.rpus_of(&c.path, false), generated, "{name}");
        let v = out.primary_video_stream().unwrap();
        assert_eq!(v.codec_name, "hevc");
        assert_eq!(v.codec_tag.as_deref(), tag, "{name}");
        assert_eq!(out.streams.len(), 2, "{name}: audio kept");
        assert_eq!(out.streams[1].language.as_deref(), Some("kor"), "{name}");
        assert!(note(&c).contains(&cfg.label()), "{}", note(&c));
    }
}

/// Streaming releases often carry HDR10+ next to a P8.1 RPU; both stay.
#[test]
fn p81_with_hdr10_plus_keeps_both() {
    let e = require_env!();
    let messages: Vec<_> = (0..FRAMES as u32).map(common::hdr10_plus_message).collect();
    let (src, rpu) = dv_source_with(&e, "8.1", "both.mkv", Some(&messages));
    assert!(e.probe(&src).hdr.has_hdr10_plus());
    for container in [ContainerChoice::Keep, ContainerChoice::Mp4] {
        let c = e.transcode(&src, container);
        let range = c.plan.dynamic_range;
        assert!(
            range.hdr10_plus() && range.dolby_vision().is_some(),
            "{range:?}"
        );
        assert_clean(&c);
        let found = c.report.inspection.hdr10_plus.as_ref().map(|h| h.found);
        assert_eq!(found, Some(FRAMES as u64), "{container:?}");
        assert_eq!(e.rpus_of(&c.path, false), dovi::read_rpus(&rpu).unwrap());
        assert!(e.probe(&c.path).hdr.has_hdr10_plus());
        std::fs::remove_file(&c.path).unwrap();
    }
}

#[test]
fn p7_becomes_p81_without_the_enhancement_layer() {
    let e = require_env!();
    for (fixture, kind) in [(P7_MEL, "MEL"), (P7_FEL, "FEL")] {
        let src = p7_source(&e, fixture, &format!("p7-{kind}.mkv"));
        assert_eq!(e.probe(&src).hdr.dolby_vision, Some(config(7, 6)));
        let c = e.transcode(&src, ContainerChoice::Keep);
        let p81 = DoviConfig {
            level: 1,
            ..config(8, 1)
        };
        assert_eq!(
            c.plan.dynamic_range.dolby_vision(),
            Some((&config(7, 6), &p81))
        );
        assert!(
            c.plan.warnings.iter().any(|w| w.contains("P7 转为 P8.1")),
            "{:?}",
            c.plan.warnings
        );
        assert_clean(&c);
        let out = e.probe(&c.path);
        assert_eq!(out.hdr.dolby_vision, Some(p81));
        let v = out.primary_video_stream().unwrap().video.as_ref().unwrap();
        assert_eq!((v.width, v.height), (320, 180), "base layer only");
        assert_eq!(e.rpus_of(&c.path, false), e.rpus_of(&src, true));
        assert!(note(&c).contains(kind), "{}", note(&c));
    }
}

#[test]
fn candidates_without_the_rpu_are_rejected() {
    let e = require_env!();
    let (src, _) = dv_source(&e, "8.1", "strip.mkv");
    let c = e.transcode(&src, ContainerChoice::Keep);
    let stripped = e.path("stripped.mkv");
    e.ffmpeg(&[
        "-i",
        c.path.to_str().unwrap(),
        "-map",
        "0",
        "-c",
        "copy",
        "-bsf:v",
        "filter_units=remove_types=62",
        stripped.to_str().unwrap(),
    ]);
    let s = e.snapshot(ContainerChoice::Keep);
    let judged = pipeline::assess(&Prober::new(&e.tools.ffprobe), &src, &stripped, &s)
        .unwrap()
        .unwrap();
    let hard: Vec<_> = judged
        .report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Hard)
        .map(|f| f.check.as_str())
        .collect();
    assert!(hard.contains(&"dovi_rpu"), "{:#?}", judged.report.findings);
    assert_eq!(judged.report.verdict, Verdict::Rejected);
}

#[test]
fn without_dovi_tool_the_route_is_skipped() {
    let e = require_env!();
    let (src, _) = dv_source(&e, "8.1", "no-tool.mkv");
    let mut s = e.snapshot(ContainerChoice::Keep);
    s.global.dovi_tool = Some(e.path("no-such-dovi_tool"));
    let job = Job {
        source: &src,
        snapshot: &s,
        force: false,
        available: None,
        publish: false,
        cancel: None,
    };
    match pipeline::transcode_file(&Prober::new(&e.tools.ffprobe), &job, |_| {}).unwrap() {
        Outcome::Skipped { reason } => assert_eq!(reason.code, SkipCode::DoviToolUnavailable),
        other => panic!("{other:#?}"),
    }
}

/// 样片推荐 of P8.1 (RPU applied on both sides, trials get the sample's RPU)
/// and P7 (HDR10 proxy of the base layer).
#[test]
fn recommendations_score_through_the_display_proxy() {
    let e = require_env!();
    let scorer = std::env::var_os("FNCOMPRESS_SCORING_FFMPEG")
        .map_or_else(|| e.tools.ffmpeg.clone(), PathBuf::from);
    let usable = recommend::probe_scorer(&scorer)
        .map(drop)
        .and_then(|()| recommend::probe_display_proxy(&scorer));
    if let Err(why) = usable {
        eprintln!("skipped: no scorer with libvmaf and libplacebo ({why})");
        return;
    }
    let (p81, _) = dv_source(&e, "8.1", "rec-p81.mkv");
    let p7 = p7_source(&e, P7_MEL, "rec-p7.mkv");
    for (src, scoring) in [
        (p81, recommend::Scoring::DolbyVisionProxy),
        (p7, recommend::Scoring::HdrProxy),
    ] {
        let mut s = e.snapshot(ContainerChoice::Keep);
        s.global.scoring_ffmpeg = Some(scorer.clone());
        let request = recommend::RecommendRequest {
            target_vmaf: 50.0,
            ..Default::default()
        };
        let workdir = e.path("rec-work");
        let job = recommend::RecommendJob {
            source: &src,
            snapshot: &s,
            request: &request,
            workdir: &workdir,
            available: None,
            cancel: None,
        };
        let outcome = recommend::run(&Prober::new(&e.tools.ffprobe), &job, |_| {}, |_| {}).unwrap();
        let rec = match outcome {
            recommend::RecommendOutcome::Finished { recommendation } => recommendation,
            other => panic!("{other:?}"),
        };
        assert_eq!(rec.scoring, scoring, "{}", src.display());
        // The 2 s clip is one sample; a trial at the base quality scores it.
        let first = &rec.trials[0];
        assert!(
            first.scores.iter().all(|s| s.is_some_and(|s| s > 50.0)),
            "{first:?}"
        );
        assert!(!rec.candidates.is_empty());
    }
}
