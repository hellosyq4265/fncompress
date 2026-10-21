mod common;

use std::path::Path;

use fncompress_core::{
    encoder::Encoder,
    inspect,
    pipeline::{self, Candidate, Job, Outcome},
    probe::{ProbeDepth, Prober},
    settings::{ContainerChoice, Profile, SettingsSnapshot},
    verify::{Inspection, Severity, Verdict},
};

fn snapshot(tools: &common::Tools, container: ContainerChoice) -> SettingsSnapshot {
    let mut s = SettingsSnapshot {
        profile: Profile {
            id: "verify".into(),
            encoder: Encoder::Libx264,
            quality: 30,
            preset: Some("ultrafast".into()),
            container,
            ..Default::default()
        },
        ..Default::default()
    };
    s.global.ffmpeg = tools.ffmpeg.clone();
    s.global.ffprobe = tools.ffprobe.clone();
    // Tiny samples can grow when re-encoded; savings are not under test here.
    s.global.min_savings_pct = f64::MIN;
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

fn verified(outcome: Outcome) -> Candidate {
    match outcome {
        Outcome::Verified { candidate } => candidate,
        other => panic!("expected a verified candidate, got {other:#?}"),
    }
}

/// 3 s H.264 + two audio tracks (AAC, AC-3) + SRT, audio longer than video.
fn source(tools: &common::Tools, dir: &Path, name: &str) -> std::path::PathBuf {
    let srt = common::write(dir, "s.srt", common::SRT);
    let src = dir.join(name);
    let mp4 = name.ends_with(".mp4");
    let mut args = vec![
        "-f",
        "lavfi",
        "-i",
        "testsrc2=s=320x240:r=30000/1001:d=3",
        "-f",
        "lavfi",
        "-i",
        "sine=d=3.37:sample_rate=48000",
        "-f",
        "lavfi",
        "-i",
        "sine=f=660:d=3.37:sample_rate=48000",
    ];
    if !mp4 {
        args.extend(["-i", &srt]);
    }
    args.extend([
        "-map",
        "0",
        "-map",
        "1",
        "-map",
        "2",
        "-c:v",
        "libx264",
        "-bf",
        "3",
        "-c:a:0",
        "aac",
        "-c:a:1",
        "ac3",
        "-metadata:s:a:0",
        "language=jpn",
        "-metadata:s:a:1",
        "language=eng",
    ]);
    if !mp4 {
        args.extend(["-map", "3", "-c:s", "srt"]);
    }
    args.push(src.to_str().unwrap());
    common::ffmpeg(tools, &args);
    src
}

#[test]
fn same_container_passes() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = source(&tools, dir.path(), "a.mkv");
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Keep), &src));
    assert_eq!(c.target, src);
    assert_eq!(c.report.verdict, Verdict::Passed, "{:?}", c.report.findings);
    let i = &c.report.inspection;
    assert_eq!(
        i.decode.as_ref().map(|d| d.errors.len()),
        Some(0),
        "decoded cleanly"
    );
    let spots = i.spot_checks.as_ref().unwrap();
    assert_eq!(spots.len(), 8);
    assert!(
        spots.iter().all(|p| p.frames > 0 && p.ssim > 0.9),
        "{spots:?}"
    );
    assert!(i.stream_hashes.is_none(), "hashing is off by default");
}

/// An MP4's AAC priming shifts a Matroska copy by ~21 ms, half a frame at
/// 24 fps; the decode check must not read the rounding as decoder errors.
#[test]
fn mp4_at_24_fps_into_mkv_decodes_cleanly() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let mp4 = dir.path().join("film.mp4");
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
            "sine=d=2:sample_rate=48000",
            "-c:v",
            "libx264",
            "-bf",
            "3",
            "-c:a",
            "aac",
            mp4.to_str().unwrap(),
        ],
    );
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Mkv), &mp4));
    let decode = c.report.inspection.decode.as_ref().unwrap();
    assert!(decode.errors.is_empty(), "{:?}", decode.errors);
    assert_ne!(
        c.report.verdict,
        Verdict::Rejected,
        "{:#?}",
        c.report.findings
    );
}

/// An MP4 cut with stream copy starts at the keyframe before the cut and
/// hides the frames up to it with an edit list (packets flagged discard):
/// 2 s of video and about 1 s of audio here.
fn mp4_cut_with_a_hidden_lead_in(tools: &common::Tools, dir: &Path) -> std::path::PathBuf {
    let full = dir.join("full.mp4");
    common::ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=24:d=10",
            "-f",
            "lavfi",
            "-i",
            "sine=d=10:sample_rate=48000",
            "-c:v",
            "libx264",
            "-g",
            "120",
            "-c:a",
            "aac",
            full.to_str().unwrap(),
        ],
    );
    let cut = dir.join("cut.mp4");
    common::ffmpeg(
        tools,
        &[
            "-ss",
            "7",
            "-t",
            "3",
            "-i",
            full.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            cut.to_str().unwrap(),
        ],
    );
    cut
}

/// The decoder drops the hidden frames, so the candidate's video starts at
/// the cut while the copied audio keeps its own hidden lead-in, still hidden
/// by the MP4 edit list: nothing moved.
#[test]
fn mp4_cut_with_a_hidden_lead_in_passes() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let cut = mp4_cut_with_a_hidden_lead_in(&tools, dir.path());
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Keep), &cut));
    let moved: Vec<_> = c
        .report
        .findings
        .iter()
        .filter(|f| f.severity != Severity::Info)
        .collect();
    assert!(moved.is_empty(), "{moved:#?}");
    assert!(!c.report.findings.iter().any(|f| f.check == "lead_in"));
    let spots = c.report.inspection.spot_checks.as_ref().unwrap();
    assert!(
        spots.iter().all(|p| p.frames > 0 && p.ssim > 0.9),
        "{spots:?}"
    );
}

/// Matroska has no edit list: the audio's hidden lead-in would play for a
/// second before the first picture. All of it but the last packet is
/// dropped, so audio and video start together.
#[test]
fn mp4_cut_into_mkv_drops_the_hidden_lead_in() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let cut = mp4_cut_with_a_hidden_lead_in(&tools, dir.path());
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Mkv), &cut));
    let moved: Vec<_> = c
        .report
        .findings
        .iter()
        .filter(|f| f.severity != Severity::Info)
        .collect();
    assert!(moved.is_empty(), "{moved:#?}");
    assert!(
        c.report.findings.iter().any(|f| f.check == "lead_in"),
        "{:#?}",
        c.report.findings
    );
    let out = Prober::new(&tools.ffprobe)
        .probe(&c.path, ProbeDepth::Full, None)
        .unwrap();
    let start = |i: usize| out.streams[i].packets.as_ref().unwrap().start_s.unwrap();
    assert!(
        (start(1) - start(0)).abs() < 0.05,
        "{} {}",
        start(0),
        start(1)
    );
}

#[test]
fn container_changes_pass_within_tolerance() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();

    let mp4 = source(&tools, dir.path(), "b.mp4");
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Mkv), &mp4));
    assert_eq!(c.target, dir.path().join("b.mkv"));
    assert_ne!(c.report.verdict, Verdict::Rejected);

    // An MKV without subtitles fits in MP4.
    let mkv = dir.path().join("c.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-i",
            mp4.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            mkv.to_str().unwrap(),
        ],
    );
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Mp4), &mkv));
    assert_eq!(c.target, dir.path().join("c.mp4"));
}

/// MPEG-TS starts its timeline near 1.4 s; the Matroska made from it starts
/// at 0. Every stream moves together, so no stream ends earlier or later.
#[test]
fn mpegts_into_mkv_keeps_stream_ends() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let mp4 = source(&tools, dir.path(), "d.mp4");
    let ts = dir.path().join("d.ts");
    common::ffmpeg(
        &tools,
        &[
            "-i",
            mp4.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            ts.to_str().unwrap(),
        ],
    );
    let src = Prober::new(&tools.ffprobe)
        .probe(&ts, ProbeDepth::Full, None)
        .unwrap();
    let start = src.streams[0].packets.as_ref().and_then(|p| p.start_s);
    assert!(start.unwrap_or(0.0) > 1.0, "{start:?}");
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Mkv), &ts));
    assert_eq!(c.target, dir.path().join("d.mkv"));
    let ends: Vec<_> = c
        .report
        .findings
        .iter()
        .filter(|f| f.check == "stream_end" || f.check == "av_offset")
        .collect();
    assert!(ends.is_empty(), "{ends:#?}");
    assert_ne!(
        c.report.verdict,
        Verdict::Rejected,
        "{:#?}",
        c.report.findings
    );
}

/// A recording cut with stream copy at an open-GOP keyframe: the B frames
/// after it in decode order are shown before it and reference the GOP that
/// was cut away. The decoder drops them, so the video starts at the
/// keyframe, not at the earliest packet.
fn mpegts_with_open_gop_leading_frames(tools: &common::Tools, dir: &Path) -> std::path::PathBuf {
    let full = dir.join("full.ts");
    common::ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=24:d=10",
            "-f",
            "lavfi",
            "-i",
            "sine=d=10:sample_rate=48000",
            "-c:v",
            "libx264",
            "-bf",
            "3",
            "-x264-params",
            "open-gop=1:keyint=48:min-keyint=48:scenecut=0",
            "-c:a",
            "aac",
            full.to_str().unwrap(),
        ],
    );
    let cut = dir.join("cut.ts");
    common::ffmpeg(
        tools,
        &[
            "-ss",
            "4.5",
            "-t",
            "3",
            "-i",
            full.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            cut.to_str().unwrap(),
        ],
    );
    cut
}

#[test]
fn mpegts_open_gop_start_keeps_av_offset() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let ts = mpegts_with_open_gop_leading_frames(&tools, dir.path());
    for container in [ContainerChoice::Mkv, ContainerChoice::Mp4] {
        let c = verified(run(&tools, &snapshot(&tools, container), &ts));
        let moved: Vec<_> = c
            .report
            .findings
            .iter()
            .filter(|f| f.check == "av_offset" || f.check == "stream_end")
            .collect();
        assert!(moved.is_empty(), "{container:?}: {moved:#?}");
        assert_ne!(
            c.report.verdict,
            Verdict::Rejected,
            "{container:?}: {:#?}",
            c.report.findings
        );
        std::fs::remove_file(&c.path).unwrap();
    }
    let src = Prober::new(&tools.ffprobe)
        .probe(&ts, ProbeDepth::Full, None)
        .unwrap();
    let p = src.streams[0].packets.as_ref().unwrap();
    let (first, shown) = (p.start_s.unwrap(), p.shown_start_s().unwrap());
    // Three leading B frames at 24 fps.
    assert!(
        (shown - first - 3.0 / 24.0).abs() < 0.002,
        "{first} {shown}"
    );
}

#[test]
fn tampered_candidates_are_rejected() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = source(&tools, dir.path(), "d.mkv");
    let s = snapshot(&tools, ContainerChoice::Keep);
    let c = verified(run(&tools, &s, &src));
    let prober = Prober::new(&tools.ffprobe);
    let src_info = prober.probe(&src, ProbeDepth::Full, None).unwrap();

    let candidate = c.path.to_str().unwrap();
    let cases: [(&str, &[&str]); 3] = [
        ("stream_layout", &["-map", "0", "-map", "-0:a:1"]),
        ("frame_count", &["-map", "0", "-t", "2"]),
        (
            "stream_language",
            &["-map", "0", "-metadata:s:a:0", "language=chi"],
        ),
    ];
    for (i, (check, extra)) in cases.iter().enumerate() {
        let bad = dir.path().join(format!("bad{i}.mkv"));
        let mut args = vec!["-i", candidate];
        args.extend_from_slice(extra);
        args.extend(["-c", "copy", bad.to_str().unwrap()]);
        common::ffmpeg(&tools, &args);
        let out = prober.probe(&bad, ProbeDepth::Full, None).unwrap();
        let report =
            pipeline::verify_against_plan(&src_info, &out, &c.plan, &s, &Inspection::default());
        assert_eq!(
            report.verdict,
            Verdict::Rejected,
            "{check}: {:?}",
            report.findings
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.check == *check && f.severity == Severity::Hard),
            "{check}: {:?}",
            report.findings
        );
    }
}

/// Re-runs inspection and verification on a modified copy of the candidate.
fn judge(
    tools: &common::Tools,
    s: &SettingsSnapshot,
    src: &Path,
    c: &Candidate,
    bad: &Path,
) -> fncompress_core::verify::Report {
    let prober = Prober::new(&tools.ffprobe);
    let src_info = prober.probe(src, ProbeDepth::Full, None).unwrap();
    let out = prober.probe(bad, ProbeDepth::Full, None).unwrap();
    let i = inspect::inspect(
        src,
        &src_info,
        bad,
        &out,
        &c.plan,
        s,
        &Default::default(),
        None,
    )
    .unwrap();
    pipeline::verify_against_plan(&src_info, &out, &c.plan, s, &i)
}

fn hard(report: &fncompress_core::verify::Report, check: &str) -> bool {
    report
        .findings
        .iter()
        .any(|f| f.check == check && f.severity == Severity::Hard)
}

#[test]
fn damaged_pictures_and_bytes_are_rejected() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = source(&tools, dir.path(), "e.mkv");
    let s = snapshot(&tools, ContainerChoice::Keep);
    let c = verified(run(&tools, &s, &src));

    // Green screen with every count intact: only the spot checks see it.
    let green = dir.path().join("green.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-i",
            c.path.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            "-vf",
            "drawbox=c=green:t=fill",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-fps_mode",
            "passthrough",
            "-disposition:0",
            "0",
            green.to_str().unwrap(),
        ],
    );
    let r = judge(&tools, &s, &src, &c, &green);
    assert!(hard(&r, "spot_check_ssim"), "{:?}", r.findings);

    // Garbage in the middle of the file: the decoder notices.
    let mut bytes = std::fs::read(&c.path).unwrap();
    let mid = bytes.len() / 2;
    for b in &mut bytes[mid..mid + 4096] {
        *b = 0x5a;
    }
    let corrupt = dir.path().join("corrupt.mkv");
    std::fs::write(&corrupt, bytes).unwrap();
    let r = judge(&tools, &s, &src, &c, &corrupt);
    assert!(hard(&r, "decode"), "{:?}", r.findings);
}

#[test]
fn optional_stream_hash_catches_rewritten_audio() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = source(&tools, dir.path(), "f.mkv");
    let mut s = snapshot(&tools, ContainerChoice::Keep);
    s.global.verify.copied_stream_hash = true;
    let c = verified(run(&tools, &s, &src));
    let hashes = c.report.inspection.stream_hashes.as_ref().unwrap();
    assert_eq!(hashes.len(), 3, "two audio tracks and the subtitle");
    assert!(hashes.iter().all(|h| h.source == h.candidate));

    // Same codec, same packet count, different bytes.
    let redone = dir.path().join("redone.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-i",
            c.path.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            "-c:a:1",
            "ac3",
            "-b:a:1",
            "96k",
            "-disposition:0",
            "0",
            redone.to_str().unwrap(),
        ],
    );
    let r = judge(&tools, &s, &src, &c, &redone);
    assert!(hard(&r, "stream_hash"), "{:?}", r.findings);
}

#[test]
fn container_change_follows_measured_expectations() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();

    // MKV -> MP4: `original` cannot be written; titles survive as `name`.
    let mkv = dir.path().join("g.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=25:d=3",
            "-f",
            "lavfi",
            "-i",
            "sine=d=3",
            "-f",
            "lavfi",
            "-i",
            "sine=f=500:d=3",
            "-map",
            "0",
            "-map",
            "1",
            "-map",
            "2",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-disposition:a:0",
            "default+original",
            "-disposition:a:1",
            "comment",
            "-metadata:s:a:1",
            "title=Commentary",
            "-metadata:s:a:1",
            "language=eng",
            mkv.to_str().unwrap(),
        ],
    );
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Mp4), &mkv));
    let lost: Vec<_> = c
        .plan
        .expected_losses
        .iter()
        .map(|l| (l.stream, l.property.as_str()))
        .collect();
    assert_eq!(lost, [(1, "disposition:original")]);
    assert_ne!(
        c.report.verdict,
        Verdict::Rejected,
        "{:?}",
        c.report.findings
    );

    // MP4 -> MKV: the `name` tag becomes a real MKV track title.
    let mp4 = dir.path().join("h.mp4");
    common::ffmpeg(
        &tools,
        &[
            "-i",
            mkv.to_str().unwrap(),
            "-map",
            "0",
            "-c",
            "copy",
            mp4.to_str().unwrap(),
        ],
    );
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Mkv), &mp4));
    assert_ne!(
        c.report.verdict,
        Verdict::Rejected,
        "{:?}",
        c.report.findings
    );
    let out = Prober::new(&tools.ffprobe)
        .probe(&c.path, ProbeDepth::Quick, None)
        .unwrap();
    let title_tag = out.streams[2]
        .tags
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("title"));
    assert_eq!(title_tag.map(|(_, v)| v.as_str()), Some("Commentary"));
}

#[test]
fn spot_checks_run_when_parameter_sets_are_only_at_the_start() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let src = common::hevc_with_parameter_sets_only_at_the_start(&tools, dir.path(), 30);
    let mut s = snapshot(&tools, ContainerChoice::Keep);
    s.profile.encoder = Encoder::Libx265;
    let c = verified(run(&tools, &s, &src));
    let i = &c.report.inspection;
    assert!(i.errors.is_empty(), "{:?}", i.errors);
    let spots = i.spot_checks.as_ref().unwrap();
    assert_eq!(spots.len(), s.global.verify.spot_checks as usize);
    assert!(
        spots.iter().all(|p| p.frames > 0 && p.ssim > 0.9),
        "{spots:?}"
    );
    assert_eq!(c.report.verdict, Verdict::Passed, "{:?}", c.report.findings);
}

/// What iTunes and HandBrake write: chapters in a QuickTime chapter track,
/// which FFmpeg probes as a `bin_data` data stream. The track is left out
/// and the chapters are carried as chapters (ADR 0013).
#[test]
fn chapter_track_mp4_keeps_its_chapters() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let chapters = common::write(
        dir.path(),
        "chapters.txt",
        ";FFMETADATA1
[CHAPTER]
TIMEBASE=1/1000
START=0
END=1000
title=Opening
[CHAPTER]
TIMEBASE=1/1000
START=1000
END=2300
title=第二章
[CHAPTER]
TIMEBASE=1/1000
START=2300
END=3000
title=End
",
    );
    let make = |name: &str| {
        let path = dir.path().join(name);
        common::ffmpeg(
            &tools,
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:r=24:d=3",
                "-f",
                "lavfi",
                "-i",
                "sine=d=3:sample_rate=48000",
                "-i",
                &chapters,
                "-map",
                "0",
                "-map",
                "1",
                "-map_chapters",
                "2",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                path.to_str().unwrap(),
            ],
        );
        path
    };
    let prober = Prober::new(&tools.ffprobe);
    let probe = |p: &Path| prober.probe(p, ProbeDepth::Full, None).unwrap();

    let cases = [
        ("a.mp4", ContainerChoice::Keep, "a.mp4"),
        ("b.mp4", ContainerChoice::Mkv, "b.mkv"),
        ("c.m4v", ContainerChoice::Keep, "c.m4v"),
        ("d.mov", ContainerChoice::Mkv, "d.mkv"),
        ("e.mov", ContainerChoice::Mp4, "e.mp4"),
    ];
    for (name, container, target) in cases {
        let src = make(name);
        let info = probe(&src);
        let track = &info.streams[2];
        assert_eq!(
            (track.codec_name.as_str(), track.codec_tag.as_deref()),
            ("bin_data", Some("text")),
            "{name}: FFmpeg probes the chapter track as data"
        );
        assert!(info.is_chapter_track(track));
        assert_eq!(info.chapters.len(), 3);

        let c = verified(run(&tools, &snapshot(&tools, container), &src));
        assert_eq!(c.target, dir.path().join(target));
        assert_eq!(c.plan.chapter_tracks, [2], "{name}");
        let not_info: Vec<_> = c
            .report
            .findings
            .iter()
            .filter(|f| f.severity != Severity::Info)
            .collect();
        assert!(not_info.is_empty(), "{name}: {not_info:#?}");
        assert!(c.report.findings.iter().any(|f| f.check == "chapter_track"));

        let out = probe(&c.path);
        assert_eq!(out.chapters.len(), 3, "{name}");
        for (a, b) in info.chapters.iter().zip(&out.chapters) {
            assert_eq!(a.title, b.title, "{name}");
            assert!((a.start_s - b.start_s).abs() < 0.001, "{name}: {a:?} {b:?}");
            assert!((a.end_s - b.end_s).abs() < 0.001, "{name}: {a:?} {b:?}");
        }
        // MP4 players read chapters from the chapter track the muxer writes.
        let tracks = out.streams.iter().filter(|s| out.is_chapter_track(s));
        let mp4 = target.ends_with(".mp4") || target.ends_with(".m4v");
        assert_eq!(tracks.count(), usize::from(mp4), "{name}");
        assert_eq!(out.streams.len(), 2 + usize::from(mp4), "{name}");
    }

    // Matroska chapters into MP4 gain the muxer's chapter track.
    let mkv = dir.path().join("f.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-i",
            make("f-source.mp4").to_str().unwrap(),
            "-map",
            "0:v",
            "-map",
            "0:a",
            "-c",
            "copy",
            mkv.to_str().unwrap(),
        ],
    );
    assert_eq!(probe(&mkv).chapters.len(), 3);
    let c = verified(run(&tools, &snapshot(&tools, ContainerChoice::Mp4), &mkv));
    assert!(c.plan.chapter_tracks.is_empty());
    assert_ne!(
        c.report.verdict,
        Verdict::Rejected,
        "{:#?}",
        c.report.findings
    );
}
