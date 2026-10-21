//! Shared helpers for integration tests that need FFmpeg. Tools come from
//! `FNCOMPRESS_FFMPEG` / `FNCOMPRESS_FFPROBE` or `PATH`; without them the tests
//! print a note and pass, so `cargo test` still works on a bare machine.

#![allow(dead_code)]

use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

pub fn tools() -> Option<Tools> {
    let find = |var: &str, name: &str| -> Option<PathBuf> {
        let path = std::env::var_os(var).map_or_else(|| PathBuf::from(name), PathBuf::from);
        let ok = Command::new(&path)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success());
        ok.then_some(path)
    };
    let tools = Tools {
        ffmpeg: find("FNCOMPRESS_FFMPEG", "ffmpeg")?,
        ffprobe: find("FNCOMPRESS_FFPROBE", "ffprobe")?,
    };
    Some(tools)
}

/// Returns early from a test when FFmpeg is not available.
#[macro_export]
macro_rules! require_tools {
    () => {
        match common::tools() {
            Some(t) => t,
            None => {
                eprintln!(
                    "skipped: ffmpeg/ffprobe not found (set FNCOMPRESS_FFMPEG/FNCOMPRESS_FFPROBE)"
                );
                return;
            }
        }
    };
}

/// Runs ffmpeg with `args` after the usual quiet, non-interactive options.
pub fn ffmpeg(tools: &Tools, args: &[&str]) {
    let out = Command::new(&tools.ffmpeg)
        .args(["-hide_banner", "-nostdin", "-v", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn write(dir: &Path, name: &str, contents: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path.to_str().unwrap().to_owned()
}

/// Two chapters for `-i <this> -map_chapters N`.
pub const CHAPTERS: &str = ";FFMETADATA1
[CHAPTER]
TIMEBASE=1/1000
START=0
END=1000
title=Opening
[CHAPTER]
TIMEBASE=1/1000
START=1000
END=2000
title=Main
";

pub const SRT: &str = "1
00:00:00,500 --> 00:00:01,500
hello
";

/// dovi_tool from `FNCOMPRESS_DOVI_TOOL` or `PATH`, if it runs.
pub fn dovi_tool() -> Option<PathBuf> {
    let path = std::env::var_os("FNCOMPRESS_DOVI_TOOL")
        .map_or_else(|| PathBuf::from("dovi_tool"), PathBuf::from);
    let ok = Command::new(&path)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    ok.then_some(path)
}

/// Returns early from a test when dovi_tool is not available.
#[macro_export]
macro_rules! require_dovi_tool {
    () => {
        match common::dovi_tool() {
            Some(t) => t,
            None => {
                eprintln!("skipped: dovi_tool not found (set FNCOMPRESS_DOVI_TOOL)");
                return;
            }
        }
    };
}

/// Runs `program` with `args` and asserts it succeeded.
pub fn run(program: &Path, args: &[&str]) {
    let out = Command::new(program).args(args).output().expect("run tool");
    assert!(
        out.status.success(),
        "{} {args:?} failed: {}{}",
        program.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// HEVC whose parameter sets are only in front of the first frame and whose
/// hvcC has none, like some `dvhe`/`hev1` web releases: `secs` at 24 fps,
/// a keyframe every second, no B-frames (it goes through a raw stream).
pub fn hevc_with_parameter_sets_only_at_the_start(tools: &Tools, dir: &Path, secs: u32) -> PathBuf {
    let raw = dir.join("raw.hevc");
    ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=s=320x240:r=24:d={secs}"),
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-x265-params",
            "log-level=error:keyint=24:bframes=0",
            "-f",
            "hevc",
            raw.to_str().unwrap(),
        ],
    );
    // libx265 repeats VPS/SPS/PPS before every keyframe; keep the first ones.
    let data = std::fs::read(&raw).unwrap();
    // Start codes are 3 or 4 bytes; a NAL unit runs from its start code to
    // the next one.
    let codes: Vec<usize> = (0..data.len().saturating_sub(3))
        .filter(|&i| data[i..i + 3] == [0, 0, 1])
        .collect();
    let begin = |i: usize| if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
    let (mut once, mut seen) = (Vec::new(), 0);
    for (n, &c) in codes.iter().enumerate() {
        let end = codes.get(n + 1).map_or(data.len(), |&next| begin(next));
        let nal = &data[begin(c)..end];
        if (32..=34).contains(&((data[c + 3] >> 1) & 0x3f)) {
            seen += 1;
            if seen > 3 {
                continue;
            }
        }
        once.extend_from_slice(nal);
    }
    assert!(seen > 3, "libx265 did not repeat the parameter sets");
    let once_path = dir.join("once.hevc");
    std::fs::write(&once_path, once).unwrap();
    let src = dir.join("in-band.mkv");
    ffmpeg(
        tools,
        &[
            "-r",
            "24",
            "-i",
            once_path.to_str().unwrap(),
            "-c",
            "copy",
            src.to_str().unwrap(),
        ],
    );
    // Cut the CodecPrivate hvcC down to its 23-byte header with no arrays
    // and fill the rest of the element's room with an EBML Void.
    let mut mkv = std::fs::read(&src).unwrap();
    let at = mkv.windows(2).position(|w| w == [0x63, 0xA2]).unwrap();
    let size = usize::from(mkv[at + 2] & 0x7f);
    assert!(mkv[at + 2] & 0x80 != 0 && size > 25 && size < 127, "{size}");
    let body = at + 3;
    mkv[at + 2] = 0x80 | 23;
    mkv[body + 22] = 0;
    mkv[body + 23] = 0xEC;
    mkv[body + 24] = 0x80 | (size - 25) as u8;
    mkv[body + 25..body + size].fill(0);
    std::fs::write(&src, mkv).unwrap();
    src
}

/// A well-formed HDR10+ message (ST 2094-40 application version 1, one
/// processing window) whose maxSCL and average maxRGB depend on `n`, so a
/// frame that gets another frame's message shows up.
pub fn hdr10_plus_message(n: u32) -> Vec<u8> {
    let mut bits: Vec<bool> = Vec::new();
    let mut put = |value: u32, width: u32| {
        bits.extend((0..width).rev().map(|i| value >> i & 1 == 1));
    };
    // T.35: country, terminal provider, provider oriented code, application.
    for (value, width) in [(0xB5, 8), (0x003C, 16), (0x0001, 16), (4, 8), (1, 8)] {
        put(value, width);
    }
    put(1, 2); // num_windows
    put(400, 27); // targeted_system_display_maximum_luminance
    put(0, 1); // targeted_system_display_actual_peak_luminance_flag
    for maxscl in [5000 + n, 4000, 3000] {
        put(maxscl, 17);
    }
    put(1000 + n, 17); // average_maxrgb
    let percentiles = [
        (1, 10),
        (5, 50),
        (10, 100),
        (25, 500),
        (50, 1000),
        (75, 5000),
        (90, 20000),
        (95, 40000),
        (99, 90000),
    ];
    put(percentiles.len() as u32, 4);
    for (percentage, percentile) in percentiles {
        put(percentage, 7);
        put(percentile, 17);
    }
    put(0, 10); // fraction_bright_pixels
    put(0, 1); // mastering_display_actual_peak_luminance_flag
    put(0, 1); // tone_mapping_flag
    put(0, 1); // color_saturation_mapping_flag
    bits.chunks(8)
        .map(|byte| {
            byte.iter()
                .chain(std::iter::repeat(&false))
                .take(8)
                .fold(0u8, |b, &bit| b << 1 | u8::from(bit))
        })
        .collect()
}

/// HDR10 (static metadata) 10-bit HEVC with B-frames, `secs` at 24 fps, a
/// keyframe every second, each frame with its own HDR10+ message
/// ([`hdr10_plus_message`] of its display position), plus AAC. `name`
/// picks the container. Returns the file and the messages in display order.
/// The messages go in with our own injector, so libx265 needs no HDR10+
/// support.
pub fn hdr10_plus_source(
    tools: &Tools,
    dir: &Path,
    name: &str,
    secs: u32,
) -> (PathBuf, Vec<Vec<u8>>) {
    let video = dir.join(format!("{name}.video.mkv"));
    ffmpeg(
        tools,
        &[
            "-f",
            "lavfi",
            "-i",
            &format!(
                "testsrc2=s=320x240:r=24:d={secs},setparams=color_primaries=bt2020:\
                 color_trc=smpte2084:colorspace=bt2020nc:range=tv"
            ),
            "-c:v",
            "libx265",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p10le",
            "-x265-params",
            "log-level=error:keyint=24:bframes=3:hdr10=1:max-cll=1000,400:\
             master-display=G(13250,34500)B(7500,3000)R(34000,16000)WP(15635,16450)L(10000000,50)",
            video.to_str().unwrap(),
        ],
    );
    let messages: Vec<_> = (0..secs * 24).map(hdr10_plus_message).collect();
    let plus = dir.join(format!("{name}.plus.mkv"));
    let metadata = fncompress_core::dovi::mkv::FrameMetadata {
        rpus: None,
        hdr10_plus: Some(&messages),
    };
    fncompress_core::dovi::mkv::inject_frames(&video, &metadata, &plus).unwrap();
    let src = dir.join(name);
    ffmpeg(
        tools,
        &[
            "-i",
            plus.to_str().unwrap(),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=d={secs}:sample_rate=48000"),
            "-map",
            "0",
            "-map",
            "1",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            src.to_str().unwrap(),
        ],
    );
    (src, messages)
}
