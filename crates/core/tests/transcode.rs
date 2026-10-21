mod common;

use std::path::Path;

use fncompress_core::{
    command,
    encoder::Encoder,
    probe::{MediaInfo, ProbeDepth, Prober, StreamKind},
    route::{self, RouteOptions, SkipCode},
    settings::{ContainerChoice, Profile, SettingsSnapshot},
};

fn snapshot(tools: &common::Tools, profile: Profile) -> SettingsSnapshot {
    let mut s = SettingsSnapshot {
        profile,
        ..Default::default()
    };
    s.global.ffmpeg = tools.ffmpeg.clone();
    s.global.ffprobe = tools.ffprobe.clone();
    s
}

/// Probes, plans and encodes; returns (source info, candidate info).
fn encode(
    tools: &common::Tools,
    snapshot: &SettingsSnapshot,
    src: &Path,
) -> (MediaInfo, MediaInfo) {
    let prober = Prober::new(&tools.ffprobe);
    let info = prober.probe(src, ProbeDepth::Full, None).unwrap();
    let plan = route::plan(&info, src, &snapshot.profile, &RouteOptions::default()).unwrap();
    let candidate = command::candidate_path(src, plan.container);
    let cmd = command::build(&info, &plan, snapshot, src, &candidate);
    let mut progress = 0;
    command::run(
        &cmd,
        command::encode_timeout(snapshot, Some(5.0)),
        None,
        |_| progress += 1,
    )
    .unwrap_or_else(|e| panic!("{e}\n{}", cmd.display()));
    assert!(progress > 0, "progress was reported");
    let out = prober.probe(&candidate, ProbeDepth::Full, None).unwrap();
    (info, out)
}

fn packets(info: &MediaInfo, index: usize) -> u64 {
    info.streams[index].packets.as_ref().unwrap().count
}

#[test]
fn mkv_keeps_every_stream_chapter_and_tag() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let chapters = common::write(dir.path(), "chapters.txt", common::CHAPTERS);
    let srt = common::write(dir.path(), "s.srt", common::SRT);
    let font = common::write(dir.path(), "font.ttf", "not really a font");
    let src = dir.path().join("Movie (2020).mkv");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=24000/1001:d=2",
            "-f",
            "lavfi",
            "-i",
            "sine=d=2.3:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=f=880:d=2.3:sample_rate=48000",
            "-i",
            &srt,
            "-i",
            &chapters,
            "-map",
            "0",
            "-map",
            "1",
            "-map",
            "2",
            "-map",
            "3",
            "-map_chapters",
            "4",
            "-c:v",
            "libx264",
            "-c:a:0",
            "aac",
            "-c:a:1",
            "flac",
            "-c:s",
            "srt",
            "-attach",
            &font,
            "-metadata:s:t:0",
            "mimetype=font/ttf",
            "-metadata:s:a:0",
            "language=jpn",
            "-metadata:s:a:1",
            "language=eng",
            "-metadata:s:a:1",
            "title=Commentary",
            "-disposition:a:1",
            "0",
            "-metadata",
            "title=Movie",
            src.to_str().unwrap(),
        ],
    );

    let s = snapshot(
        &tools,
        Profile {
            id: "movies".into(),
            encoder: Encoder::Libx265,
            quality: 30,
            preset: Some("ultrafast".into()),
            ..Default::default()
        },
    );
    let (info, out) = encode(&tools, &s, &src);

    assert_eq!(out.marker.as_ref().unwrap().profile, "movies");
    assert_eq!(out.streams.len(), info.streams.len());
    for (a, b) in info.streams.iter().zip(&out.streams) {
        assert_eq!(a.kind, b.kind);
        assert_eq!(a.language, b.language, "stream {}", a.index);
        assert_eq!(a.title, b.title, "stream {}", a.index);
        assert_eq!(a.disposition, b.disposition, "stream {}", a.index);
    }
    assert_eq!(out.streams[0].codec_name, "hevc");
    assert_eq!(packets(&out, 0), packets(&info, 0), "video frame count");
    for i in 1..=3 {
        assert_eq!(packets(&out, i), packets(&info, i), "copied stream {i}");
    }
    assert_eq!(out.streams[4].kind, StreamKind::Attachment);
    assert_eq!(out.chapters, info.chapters);
    assert_eq!(
        out.format.tags.get("title").map(String::as_str),
        Some("Movie")
    );
}

#[test]
fn mp4_keeps_cover_art_and_mov_text() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let srt = common::write(dir.path(), "s.srt", common::SRT);
    let cover = dir.path().join("cover.png");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=64x64:d=1",
            "-frames:v",
            "1",
            cover.to_str().unwrap(),
        ],
    );
    let src = dir.path().join("clip.mp4");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=25:d=2",
            "-f",
            "lavfi",
            "-i",
            "sine=d=2:sample_rate=44100",
            "-i",
            &srt,
            "-i",
            cover.to_str().unwrap(),
            "-map",
            "0",
            "-map",
            "1",
            "-map",
            "2",
            "-map",
            "3",
            "-c:v:0",
            "libx264",
            "-c:v:1",
            "png",
            "-disposition:v:1",
            "attached_pic",
            "-c:a",
            "aac",
            "-c:s",
            "mov_text",
            src.to_str().unwrap(),
        ],
    );
    let s = snapshot(
        &tools,
        Profile {
            id: "clips".into(),
            encoder: Encoder::Libx264,
            quality: 30,
            preset: Some("ultrafast".into()),
            ..Default::default()
        },
    );
    let (info, out) = encode(&tools, &s, &src);
    assert_eq!(out.marker.as_ref().unwrap().profile, "clips");
    let kinds = |i: &MediaInfo| {
        i.streams
            .iter()
            .map(|s| (s.kind, s.codec_name.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(kinds(&out), kinds(&info));
    assert!(
        out.streams.iter().any(|s| s.is_cover_art()),
        "cover art survives"
    );
    assert_eq!(packets(&out, 0), packets(&info, 0));
    assert_eq!(packets(&out, 1), packets(&info, 1));
    assert_eq!(packets(&out, 2), packets(&info, 2));
}

#[test]
fn mkv_with_srt_cannot_become_mp4() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let srt = common::write(dir.path(), "s.srt", common::SRT);
    let src = dir.path().join("a.mkv");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x120:r=25:d=1",
            "-i",
            &srt,
            "-map",
            "0",
            "-map",
            "1",
            "-c:v",
            "libx264",
            "-c:s",
            "srt",
            src.to_str().unwrap(),
        ],
    );
    let info = Prober::new(&tools.ffprobe)
        .probe(&src, ProbeDepth::Quick, None)
        .unwrap();
    let profile = Profile {
        container: ContainerChoice::Mp4,
        ..Default::default()
    };
    let skip = route::plan(&info, &src, &profile, &RouteOptions::default()).unwrap_err();
    assert_eq!(skip.code, SkipCode::StreamNotSupportedInContainer);
}

/// hevc_qsv ends a stream with one frame whose dts jumps to its pts, ahead of
/// the frames flushed after it; FFmpeg then raises their pts to it. The same
/// jump, made with `setts` on libx265: the clamp keeps every frame's pts.
#[test]
fn qsv_dts_clamp_keeps_the_pts() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let pts = |bsf: Option<&str>| {
        let out = dir.path().join("o.mkv");
        let mut args = vec![
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=160x90:r=24000/1001",
            "-frames:v",
            "48",
            "-c:v",
            "libx265",
            "-x265-params",
            "log-level=error:bframes=3",
        ];
        if let Some(bsf) = bsf {
            args.extend(["-bsf:v", bsf]);
        }
        args.push(out.to_str().unwrap());
        common::ffmpeg(&tools, &args);
        let probe = std::process::Command::new(&tools.ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "packet=pts",
                "-of",
                "csv=p=0",
            ])
            .arg(&out)
            .output()
            .unwrap();
        let mut pts: Vec<i64> = String::from_utf8(probe.stdout)
            .unwrap()
            .lines()
            .map(|l| l.trim().parse().unwrap())
            .collect();
        pts.sort_unstable();
        pts
    };
    // Packet 40 in decode order is the last frame shown.
    let jump = r"setts=pts=PTS:dts=if(eq(N\,40)\,PTS\,DTS)";
    let clean = pts(None);
    let broken = pts(Some(jump));
    assert!(broken.windows(2).any(|w| w[0] == w[1]), "{broken:?}");
    assert_eq!(pts(Some(command::QSV_DTS_CLAMP)), clean);
    assert_eq!(
        pts(Some(&format!("{jump},{}", command::QSV_DTS_CLAMP))),
        clean
    );
}
