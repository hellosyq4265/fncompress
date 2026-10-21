//! HDR10 and HLG routes end to end: the candidate keeps the color
//! description and the static metadata, and verification checks both.

mod common;

use std::path::{Path, PathBuf};

use fncompress_core::{
    encoder::Encoder,
    pipeline::{self, Candidate, Job, Outcome},
    probe::{ContentLight, DynamicRange, MediaInfo, ProbeDepth, Prober},
    route::RangeRoute,
    settings::{ContainerChoice, Profile, Resize, SettingsSnapshot},
    verify::Verdict,
};

const MASTER_DISPLAY: &str =
    "G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50)";

fn snapshot(
    tools: &common::Tools,
    encoder: Encoder,
    container: ContainerChoice,
) -> SettingsSnapshot {
    let preset = match encoder {
        Encoder::Libsvtav1 => "12",
        _ => "ultrafast",
    };
    let mut s = SettingsSnapshot {
        profile: Profile {
            id: "hdr".into(),
            encoder,
            quality: 30,
            preset: Some(preset.into()),
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

fn run(tools: &common::Tools, s: &SettingsSnapshot, src: &Path) -> Candidate {
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

fn probe(tools: &common::Tools, path: &Path) -> MediaInfo {
    Prober::new(&tools.ffprobe)
        .probe(path, ProbeDepth::Full, None)
        .unwrap()
}

/// 2 s of 10-bit HEVC with AAC. `transfer` is `smpte2084` (with static
/// metadata in the SEI) or `arib-std-b67` (none, as HLG usually has).
fn source(tools: &common::Tools, dir: &Path, name: &str, transfer: &str) -> PathBuf {
    let src = dir.join(name);
    let params = if transfer == "smpte2084" {
        format!("log-level=error:master-display={MASTER_DISPLAY}:max-cll=1000,400:hdr10=1")
    } else {
        "log-level=error".to_owned()
    };
    // Frames from lavfi carry no color description, and FFmpeg takes the
    // encoder's from the frames; set it there.
    let video = format!(
        "testsrc2=s=320x240:r=24:d=2,setparams=color_primaries=bt2020:\
         color_trc={transfer}:colorspace=bt2020nc:range=tv"
    );
    common::ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            &video,
            "-f",
            "lavfi",
            "-i",
            "sine=d=2:sample_rate=48000",
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p10le",
            "-x265-params",
            &params,
            "-c:a",
            "aac",
            src.to_str().unwrap(),
        ],
    );
    src
}

fn assert_hdr10(info: &MediaInfo) {
    assert_eq!(info.dynamic_range(), DynamicRange::Hdr10);
    let md = info
        .hdr
        .mastering_display
        .as_ref()
        .expect("mastering display");
    assert!(md.contains_key("max_luminance"), "{md:?}");
    assert_eq!(
        info.hdr.content_light,
        Some(ContentLight {
            max_content: 1000,
            max_average: 400
        })
    );
}

fn passed(c: &Candidate) {
    assert_ne!(
        c.report.verdict,
        Verdict::Rejected,
        "{:#?}",
        c.report.findings
    );
    assert!(
        !c.report
            .findings
            .iter()
            .any(|f| f.check == "hdr_metadata" || f.check == "color"),
        "{:#?}",
        c.report.findings
    );
}

#[test]
fn hdr10_keeps_static_metadata() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = source(&tools, dir.path(), "hdr10.mkv", "smpte2084");
    assert_hdr10(&probe(&tools, &src));

    for (encoder, container) in [
        (Encoder::Libx265, ContainerChoice::Keep),
        (Encoder::Libx265, ContainerChoice::Mp4),
        (Encoder::Libsvtav1, ContainerChoice::Keep),
    ] {
        let s = snapshot(&tools, encoder, container);
        let c = run(&tools, &s, &src);
        assert_eq!(c.plan.dynamic_range, RangeRoute::Hdr10);
        passed(&c);
        let out = probe(&tools, &c.path);
        assert_hdr10(&out);
        let v = out.primary_video_stream().unwrap().video.as_ref().unwrap();
        assert_eq!(v.bit_depth, Some(10), "{encoder:?}");
        assert_eq!(v.color.primaries.as_deref(), Some("bt2020"));
        assert_eq!(v.color.space.as_deref(), Some("bt2020nc"));
        std::fs::remove_file(&c.path).unwrap();
    }
}

#[test]
fn hdr10_metadata_only_in_the_container_is_kept() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    // A re-encode puts the metadata in the bitstream and the Matroska Colour
    // element. x265 also repeats its SEI in the hvcC header, so go through
    // AV1, whose metadata OBUs a bitstream filter can drop.
    let hevc = source(&tools, dir.path(), "hevc.mkv", "smpte2084");
    let av1 = run(
        &tools,
        &snapshot(&tools, Encoder::Libsvtav1, ContainerChoice::Keep),
        &hevc,
    );
    let src = dir.path().join("container-only.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-i",
            av1.path.to_str().unwrap(),
            "-map",
            "0",
            // Drops our processed marker too.
            "-map_metadata",
            "-1",
            "-c",
            "copy",
            "-bsf:v",
            "filter_units=remove_types=5",
            src.to_str().unwrap(),
        ],
    );
    let info = probe(&tools, &src);
    let video = info.primary_video_stream().unwrap();
    assert!(
        video.side_data.contains("Mastering display metadata"),
        "{:?}",
        video.side_data
    );
    assert_hdr10(&info);

    let s = snapshot(&tools, Encoder::Libx265, ContainerChoice::Keep);
    let c = run(&tools, &s, &src);
    passed(&c);
    assert_hdr10(&probe(&tools, &c.path));
}

#[test]
fn hlg_mp4_keeps_its_transfer() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = source(&tools, dir.path(), "hlg.mp4", "arib-std-b67");
    let s = SettingsSnapshot {
        profile: Profile {
            resize: Resize::MaxHeight(180),
            ..snapshot(&tools, Encoder::Libx265, ContainerChoice::Keep).profile
        },
        ..snapshot(&tools, Encoder::Libx265, ContainerChoice::Keep)
    };
    let c = run(&tools, &s, &src);
    assert_eq!(c.plan.dynamic_range, RangeRoute::Hlg);
    passed(&c);
    let out = probe(&tools, &c.path);
    assert_eq!(out.dynamic_range(), DynamicRange::Hlg);
    let v = out.primary_video_stream().unwrap().video.as_ref().unwrap();
    assert_eq!((v.width, v.height, v.bit_depth), (240, 180, Some(10)));
}
