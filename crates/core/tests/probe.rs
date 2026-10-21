mod common;

use std::path::PathBuf;

use fncompress_core::probe::{DynamicRange, JsonDirCache, ProbeDepth, Prober, StreamKind};

#[test]
fn full_probe_of_a_generated_mkv() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let chapters = common::write(dir.path(), "chapters.txt", common::CHAPTERS);
    let srt = common::write(dir.path(), "s.srt", common::SRT);
    let src = dir.path().join("src.mkv");
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
            "sine=d=2.3:sample_rate=48000",
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
            "-map_chapters",
            "3",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-c:s",
            "srt",
            "-metadata:s:a:0",
            "language=jpn",
            "-disposition:s:0",
            "forced",
            "-metadata",
            "FNCOMPRESS=1;v=0.1.0;profile=movies",
            src.to_str().unwrap(),
        ],
    );

    let cache_dir = dir.path().join("cache");
    let prober = Prober::new(&tools.ffprobe).with_cache(JsonDirCache::new(&cache_dir));
    let info = prober.probe(&src, ProbeDepth::Full, None).unwrap();

    assert_eq!(info.depth, ProbeDepth::Full);
    assert_eq!(info.primary_video, Some(0));
    assert_eq!(info.dynamic_range(), DynamicRange::Sdr);
    assert_eq!(info.marker.as_ref().unwrap().profile, "movies");
    assert_eq!(info.chapters.len(), 2);
    assert!(
        info.frame_side_data
            .iter()
            .any(|t| t.contains("User Data Unregistered")),
        "x264 writes its settings as SEI"
    );

    let kinds: Vec<_> = info.streams.iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        [StreamKind::Video, StreamKind::Audio, StreamKind::Subtitle]
    );
    let video = &info.streams[0];
    assert_eq!(video.packets.as_ref().unwrap().count, 50);
    assert_eq!(video.video.as_ref().unwrap().bit_depth, Some(8));
    let end = video.packets.as_ref().unwrap().end_s.unwrap();
    assert!((end - 2.0).abs() < 0.001, "video ends at {end}");
    let audio = &info.streams[1];
    assert_eq!(audio.language.as_deref(), Some("jpn"));
    let audio_end = audio.packets.as_ref().unwrap().end_s.unwrap();
    assert!((audio_end - 2.3).abs() < 0.05, "audio ends at {audio_end}");
    assert!(info.streams[2].has_disposition("forced"));
    assert_eq!(info.streams[2].packets.as_ref().unwrap().count, 1);

    // A second probe is served from the cache, even with a broken ffprobe.
    let cached = Prober::new(PathBuf::from("no-such-ffprobe"))
        .with_cache(JsonDirCache::new(&cache_dir))
        .probe(&src, ProbeDepth::Quick, None)
        .unwrap();
    assert_eq!(cached, info);
}

#[test]
fn mp4_cover_art_is_not_the_primary_video() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
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
    let src = dir.path().join("src.mp4");
    common::ffmpeg(
        &tools,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=s=320x240:r=25:d=1",
            "-i",
            cover.to_str().unwrap(),
            "-map",
            "0",
            "-map",
            "1",
            "-c:v:0",
            "libx264",
            "-c:v:1",
            "png",
            "-disposition:v:1",
            "attached_pic",
            "-metadata",
            "keywords=FNCOMPRESS=1;v=0.1.0;profile=tv",
            src.to_str().unwrap(),
        ],
    );
    let info = Prober::new(&tools.ffprobe)
        .probe(&src, ProbeDepth::Quick, None)
        .unwrap();
    assert_eq!(info.primary_video, Some(0));
    assert!(info.streams[1].is_cover_art());
    assert_eq!(info.marker.as_ref().map(|m| m.profile.as_str()), Some("tv"));
}
