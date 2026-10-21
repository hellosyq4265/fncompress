//! FFmpeg command for a [`RoutePlan`]: re-encode the primary video, copy every
//! other stream, chapter and tag, and write the processed marker. QuickTime
//! chapter tracks are left out; their chapters are copied as chapters.
//!
//! The base is the command validated in docs/research/duration-tolerance.md:
//! `-map 0 -c copy -c:<video> <encoder> -fps_mode passthrough`, which keeps
//! the video frame count and every copied stream's packet count exact.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::Serialize;

use crate::{
    encoder::{Encoder, Hw},
    marker::{ProcessedMarker, MP4_TAG},
    probe::{ffmpeg_input, DoviConfig, MediaInfo},
    route::{Container, RoutePlan},
    settings::{Profile, SettingsSnapshot},
    tool::{self, CancelToken, ToolError},
};

#[derive(Debug, Clone, Serialize)]
pub struct EncodeCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub output: PathBuf,
    /// The same command decoding the source in software, when `args` decode
    /// it on the GPU; [`run`] falls back to it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_decode: Option<Vec<OsString>>,
}

impl EncodeCommand {
    pub fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        quiet_libva(&mut cmd);
        cmd
    }

    /// Whether the source is decoded on the GPU.
    pub fn hw_decode(&self) -> bool {
        self.software_decode.is_some()
    }

    /// The software-decode variant of a GPU-decoding command.
    fn with_software_decode(&self) -> Option<EncodeCommand> {
        self.software_decode.as_ref().map(|args| EncodeCommand {
            program: self.program.clone(),
            args: args.clone(),
            output: self.output.clone(),
            software_decode: None,
        })
    }

    /// Shell-like rendering for logs and `--dry-run`.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_os_str())
            .chain(self.args.iter().map(OsString::as_os_str))
            .map(|a| {
                let a = a.to_string_lossy();
                if a.is_empty() || a.contains([' ', ';', '\'', '"', '(', ')', ',', '|']) {
                    format!("'{}'", a.replace('\'', r"'\''"))
                } else {
                    a.into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The processed marker this build writes for `snapshot`.
pub fn marker_for(snapshot: &SettingsSnapshot) -> ProcessedMarker {
    ProcessedMarker {
        version: env!("CARGO_PKG_VERSION").into(),
        profile: snapshot.profile.id.clone(),
    }
}

pub fn build(
    info: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    input: &Path,
    output: &Path,
) -> EncodeCommand {
    with_decode_fallback(snapshot, plan, |hw| {
        build_with(info, plan, snapshot, input, None, hw, output)
    })
}

/// The last step of a Dolby Vision or HDR10+ route: the source's streams
/// with the primary video taken from `video`, the re-encode with the RPU or
/// HDR10+ messages injected (see [`crate::dovi`], [`crate::hdr10plus`]).
/// Everything is copied; the rest is as [`build`].
pub fn build_injected_mux(
    info: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    input: &Path,
    video: &Path,
    output: &Path,
) -> EncodeCommand {
    // Nothing is decoded: the video is copied from `video`.
    build_with(info, plan, snapshot, input, Some(video), false, output)
}

/// `build(hw_decode)`, decoding on the GPU when the settings and encoder
/// allow it, with the software-decode command kept for [`run`]. A full-range
/// source stays in software: only swscale is told the range it converts.
fn with_decode_fallback(
    snapshot: &SettingsSnapshot,
    plan: &RoutePlan,
    build: impl Fn(bool) -> EncodeCommand,
) -> EncodeCommand {
    if !snapshot.global.hw_decode || plan.encoder.hw().is_none() || plan.full_range {
        return build(false);
    }
    EncodeCommand {
        software_decode: Some(build(false).args),
        ..build(true)
    }
}

fn build_with(
    info: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    input: &Path,
    video: Option<&Path>,
    hw_decode: bool,
    output: &Path,
) -> EncodeCommand {
    let profile = &snapshot.profile;
    let encoder = plan.encoder;
    let v = plan.video_index;

    let mut a: Vec<OsString> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    push(&["-hide_banner", "-nostdin", "-nostats", "-v", "error"]);
    push(&["-progress", "pipe:1"]);
    let device = &snapshot.global.render_device;
    let input_args = match (video, hw_decode) {
        (Some(_), _) => vec![],
        (None, true) => encoder.gpu_decode_args(device),
        (None, false) => encoder.device_args(device),
    };
    for arg in input_args {
        push(&[&arg]);
    }
    a.push("-i".into());
    a.push(ffmpeg_input(input));
    if let Some(video) = video {
        a.push("-i".into());
        a.push(ffmpeg_input(video));
    }

    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    let copied = || {
        info.streams
            .iter()
            .filter(|s| !plan.chapter_tracks.contains(&s.index))
    };
    match video {
        None => {
            push(&["-map", "0"]);
            for i in &plan.chapter_tracks {
                push(&["-map", &format!("-0:{i}")]);
            }
        }
        Some(_) => {
            // Stream by stream, so the video keeps its place.
            for s in copied() {
                let from = if s.index == v {
                    "1:0".to_owned()
                } else {
                    format!("0:{}", s.index)
                };
                push(&["-map", &from]);
            }
            // Any stream -map_metadata turns off FFmpeg's own per-stream
            // copy, so every stream's tags (language, title) are named.
            for s in copied() {
                let i = s.index;
                push(&[&format!("-map_metadata:s:{i}"), &format!("0:s:{i}")]);
            }
        }
    }
    push(&["-map_metadata", "0", "-map_chapters", "0", "-c", "copy"]);
    for t in &plan.lead_in_trims {
        // Packets in demux order; `n` does not depend on timestamp offsets.
        push(&[
            &format!("-bsf:{}", t.stream),
            &format!(r"noise=drop=lt(n\,{})", t.packets),
        ]);
    }
    if video.is_none() {
        let matroska = plan.container == Container::Mkv;
        a.extend(video_args(info, plan, profile, v, v, matroska, hw_decode));
    }

    let spec = |opt: &str| format!("-{opt}:{v}");
    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    // With no manual disposition, FFmpeg marks the first stream of a type as
    // default when none is, changing the source's track flags. Restating the
    // video's own flags counts as manual and turns that off.
    let flags = info
        .stream(v)
        .map(|s| &s.disposition)
        .filter(|d| !d.is_empty());
    let flags = flags.map_or("0".to_owned(), |d| {
        d.iter().map(String::as_str).collect::<Vec<_>>().join("+")
    });
    push(&[&spec("disposition"), &flags]);

    // MP4 keeps track titles as `name`; copied into MKV that stays a plain
    // NAME tag, which players do not show as the track name.
    if plan.container == Container::Mkv && plan.container_changed {
        for s in copied() {
            let tag = |key: &str| s.tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(key));
            if let (Some((_, name)), None) = (tag("name"), tag("title")) {
                push(&[
                    &format!("-metadata:s:{}", s.index),
                    &format!("title={name}"),
                ]);
            }
        }
    }

    let marker = marker_for(snapshot);
    match plan.container {
        Container::Mkv => push(&["-metadata", &format!("{marker}")]),
        Container::Mp4 => {
            let existing = info
                .format
                .tags
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(MP4_TAG))
                .map(|(_, v)| v.as_str())
                // Re-processing replaces our old marker instead of stacking it.
                .map(|kw| kw.find("FNCOMPRESS=").map_or(kw, |at| &kw[..at]));
            let keywords = marker.mp4_keywords(existing);
            push(&["-metadata", &format!("{MP4_TAG}={keywords}")]);
            push(&["-movflags", "+faststart"]);
            if let Some((_, dv)) = plan.dynamic_range.dolby_vision() {
                // FFmpeg writes the dvcC/dvvC box only when told to.
                push(&["-strict", "unofficial"]);
                push(&[&spec("tag"), mp4_dolby_vision_tag(info, v, dv)]);
            }
        }
    }
    push(&["-max_muxing_queue_size", "4096"]);
    push(&["-f", plan.container.muxer(), "-n"]);
    a.push(ffmpeg_input(output));

    EncodeCommand {
        program: snapshot.global.ffmpeg.clone(),
        args: a,
        output: output.to_owned(),
        software_decode: None,
    }
}

/// Sample entry of a Dolby Vision video in MP4: the source's own when it was
/// an MP4 HEVC one, else `dvh1` for P5 (no compatible base layer) and
/// `hvc1` for P8.
fn mp4_dolby_vision_tag(info: &MediaInfo, video: u32, dv: &DoviConfig) -> &'static str {
    let source = info.stream(video).and_then(|s| s.codec_tag.as_deref());
    match source {
        Some("hvc1") => "hvc1",
        Some("hev1") => "hev1",
        Some("dvh1") => "dvh1",
        Some("dvhe") => "dvhe",
        _ if dv.profile == 5 => "dvh1",
        _ => "hvc1",
    }
}

/// 样片推荐 trial encode: only the video of `sample` (stream `sample_video`),
/// encoded as the transcode of the source in `info`/`plan` would be, into a
/// Matroska file. The sample must match the source's picture format; its
/// frames are kept one for one. A Dolby Vision route encodes its whole
/// source this way before the RPU goes in.
pub fn build_trial(
    info: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    sample: &Path,
    sample_video: u32,
    output: &Path,
) -> EncodeCommand {
    with_decode_fallback(snapshot, plan, |hw| {
        trial_with(info, plan, snapshot, sample, sample_video, hw, output)
    })
}

fn trial_with(
    info: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    sample: &Path,
    sample_video: u32,
    hw_decode: bool,
    output: &Path,
) -> EncodeCommand {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    push(&["-hide_banner", "-nostdin", "-nostats", "-v", "error"]);
    push(&["-progress", "pipe:1"]);
    let device = &snapshot.global.render_device;
    let input_args = if hw_decode {
        plan.encoder.gpu_decode_args(device)
    } else {
        plan.encoder.device_args(device)
    };
    for arg in input_args {
        push(&[&arg]);
    }
    a.push("-i".into());
    a.push(ffmpeg_input(sample));
    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    push(&["-map", &format!("0:{sample_video}")]);
    push(&["-map_metadata", "-1", "-map_chapters", "-1"]);
    a.extend(video_args(
        info,
        plan,
        &snapshot.profile,
        plan.video_index,
        0,
        true,
        hw_decode,
    ));
    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    push(&["-an", "-sn", "-dn", "-f", "matroska", "-n"]);
    a.push(ffmpeg_input(output));
    EncodeCommand {
        program: snapshot.global.ffmpeg.clone(),
        args: a,
        output: output.to_owned(),
        software_decode: None,
    }
}

/// Output options that decide how the video is encoded: encoder, quality,
/// preset, filters, frame timing and color description. Shared by the full
/// transcode and the 样片推荐 trial encodes, so a trial measures exactly what a
/// transcode with the same profile would produce. `input` is the source
/// stream the plan was made for, `out` the output stream index, `matroska`
/// whether the output is MKV, `gpu` whether the frames arrive decoded on the
/// GPU.
fn video_args(
    info: &MediaInfo,
    plan: &RoutePlan,
    profile: &Profile,
    input: u32,
    out: u32,
    matroska: bool,
    gpu: bool,
) -> Vec<OsString> {
    let encoder = plan.encoder;
    let video = info
        .stream(input)
        .and_then(|s| s.video.as_ref())
        .expect("plan refers to a video stream");
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(OsString::from));
    let spec = |opt: &str| format!("-{opt}:{out}");
    push(&[&spec("c"), encoder.ffmpeg_name()]);

    let quality = profile.quality.to_string();
    let preset = profile.preset_or_default();
    match encoder {
        Encoder::Libx264 | Encoder::Libx265 | Encoder::Libsvtav1 => {
            push(&[&spec("crf"), &quality]);
        }
        Encoder::H264Qsv | Encoder::HevcQsv | Encoder::Av1Qsv => {
            push(&[&spec("global_quality"), &quality]);
        }
        Encoder::H264Vaapi | Encoder::HevcVaapi | Encoder::Av1Vaapi => {
            push(&[&spec("rc_mode"), "CQP", &spec("qp"), &quality]);
        }
    }
    if let Some(preset) = &preset {
        push(&[&spec("preset"), preset]);
    }
    if encoder == Encoder::Libx265 {
        push(&[&spec("x265-params"), "log-level=error"]);
        if plan.dynamic_range.dolby_vision().is_some() {
            // libx265 would write RPUs of its own from the decoded frames'
            // metadata; the source's RPU goes in afterwards, unchanged.
            push(&[&spec("dolbyvision"), "0"]);
        }
    }
    if let Some(threads) = profile.threads {
        push(&["-threads", &threads.to_string()]);
    }

    let size = plan.scales(info).then_some((plan.width, plan.height));
    let mut filters = Vec::new();
    let range = if plan.full_range {
        ":in_range=full:out_range=full"
    } else {
        ""
    };
    if gpu {
        filters.push(encoder.gpu_filter(size, &plan.pix_fmt));
    } else {
        if let Some((w, h)) = size {
            filters.push(format!("scale={w}:{h}{range}"));
        } else if plan.full_range {
            // Name the range explicitly whenever swscale converts the format.
            filters.push(format!("scale=iw:ih{range}"));
        }
        filters.push(encoder.upload_filter(&plan.pix_fmt));
    }
    push(&[&spec("filter"), &filters.join(",")]);
    push(&[&spec("fps_mode"), "passthrough"]);
    if encoder.hw() == Some(Hw::Qsv) && matroska {
        // QSV gives the last frame of a stream a dts equal to its pts, ahead
        // of the frames flushed after it. FFmpeg then raises those frames'
        // dts and pts to it, so several frames end up with one pts. Matroska
        // stores pts only: pull such a jump back to just after the previous
        // dts, which keeps dts increasing and never past pts.
        push(&[&spec("bsf"), QSV_DTS_CLAMP]);
    }

    // Encoders do not reliably copy the color description; state it.
    let c = &video.color;
    for (opt, value) in [
        ("color_primaries", &c.primaries),
        ("color_trc", &c.transfer),
        ("colorspace", &c.space),
    ] {
        if let Some(value) = value {
            push(&[&spec(opt), value]);
        }
    }
    let range = if plan.full_range {
        Some("pc")
    } else {
        c.range.as_deref()
    };
    if let Some(range) = range {
        push(&[&spec("color_range"), range]);
    }

    a
}

/// `setts` expression for QSV into Matroska (see [`video_args`]). A dts more
/// than one and a half frames past the previous one becomes the previous + 1;
/// without packet durations every dts does, which Matroska does not mind.
pub const QSV_DTS_CLAMP: &str = concat!(
    r"setts=pts=PTS:dts=if(eq(N\,0)\,DTS\,",
    r"if(gt(DTS-PREV_OUTDTS\,1.5*DURATION)\,PREV_OUTDTS+1\,DTS))"
);

/// Unique hidden candidate path next to the source, so publication is a
/// same-filesystem rename.
pub fn candidate_path(source: &Path, container: Container) -> PathBuf {
    work_path(source, container.extension())
}

/// Unique hidden path next to the source ending in `.{suffix}`, for the
/// candidate and the intermediate files of Dolby Vision and HDR10+ routes.
pub fn work_path(source: &Path, suffix: &str) -> PathBuf {
    let name = source.file_name().unwrap_or_default().to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    source.with_file_name(format!(
        ".{name}.fncompress-{}-{nonce:x}.{suffix}",
        std::process::id()
    ))
}

/// Encode progress from `-progress pipe:1`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Progress {
    pub out_time_s: f64,
    /// Encode speed relative to real time, when FFmpeg reports it.
    pub speed: Option<f64>,
}

/// Runs the encode. On any failure the partial output is removed. A GPU
/// decode that fails, or prints any error while the run still succeeds, is
/// redone with software decoding: a GPU quirk costs time, never a file.
pub fn run(
    cmd: &EncodeCommand,
    timeout: Duration,
    cancel: Option<&CancelToken>,
    mut on_progress: impl FnMut(Progress),
) -> Result<(), ToolError> {
    let Some(software) = cmd.with_software_decode() else {
        return run_once(cmd, timeout, cancel, &mut on_progress).map(drop);
    };
    match run_once(cmd, timeout, cancel, &mut on_progress) {
        Ok(stderr) => {
            let errors = error_lines(&stderr);
            if errors.is_empty() {
                return Ok(());
            }
            let _ = std::fs::remove_file(&cmd.output);
            tracing::info!(
                ?errors,
                "hardware-decoded encode reported errors; redoing it with software decoding"
            );
        }
        Err(ToolError::Failed { stderr, status, .. }) => tracing::info!(
            %status,
            stderr = stderr.trim(),
            "hardware-decoded encode failed; redoing it with software decoding"
        ),
        Err(e) => return Err(e),
    }
    run_once(&software, timeout, cancel, &mut on_progress).map(drop)
}

/// One FFmpeg run; its stderr on success.
fn run_once(
    cmd: &EncodeCommand,
    timeout: Duration,
    cancel: Option<&CancelToken>,
    on_progress: &mut impl FnMut(Progress),
) -> Result<String, ToolError> {
    let mut current = Progress::default();
    let started = std::time::Instant::now();
    let result = tool::run_lines(&mut cmd.to_command(), timeout, cancel, |line| {
        let Some((key, value)) = line.split_once('=') else {
            return;
        };
        match key {
            "out_time_us" => {
                if let Ok(us) = value.parse::<i64>() {
                    current.out_time_s = us as f64 / 1e6;
                }
            }
            "speed" => current.speed = value.trim_end_matches('x').trim().parse().ok(),
            "progress" => on_progress(current),
            _ => {}
        }
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&cmd.output);
    }
    tracing::info!(
        secs = started.elapsed().as_secs_f64(),
        hw_decode = cmd.hw_decode(),
        ok = result.is_ok(),
        "ffmpeg run finished"
    );
    result
}

/// libva logs its driver loading at info level on stderr, whatever FFmpeg's
/// `-v`. Errors only, so stderr stays FFmpeg's own errors.
pub(crate) fn quiet_libva(cmd: &mut Command) {
    cmd.env("LIBVA_MESSAGING_LEVEL", "1");
}

/// FFmpeg's error lines in `stderr`, without blank lines and libva's info
/// lines (in case [`quiet_libva`] is not honoured).
pub(crate) fn error_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("libva info:"))
        .map(str::to_owned)
        .collect()
}

/// Encode timeout for a source of `duration_s` seconds.
pub fn encode_timeout(snapshot: &SettingsSnapshot, duration_s: Option<f64>) -> Duration {
    let media = duration_s.unwrap_or(3600.0).max(0.0);
    Duration::from_secs_f64(media * snapshot.global.encode_timeout_factor + 1800.0)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::{
        identity::FileIdentity,
        probe::{
            ColorTags, FormatInfo, HdrInfo, LeadIn, PacketStats, ProbeDepth, StreamInfo,
            StreamKind, VideoParams,
        },
        route::{self, RouteOptions},
        settings::{ContainerChoice, Profile, Resize},
    };

    fn info(format: &str, pix_fmt: &str, tags: &[(&str, &str)]) -> MediaInfo {
        let (depth, chroma) = crate::probe::pix_fmt_layout(pix_fmt).unzip();
        let s = |index, kind, codec: &str| StreamInfo {
            index,
            kind,
            codec_name: codec.into(),
            codec_tag: None,
            profile: None,
            time_base: "1/1000".into(),
            language: None,
            title: None,
            tags: BTreeMap::new(),
            disposition: BTreeSet::new(),
            video: None,
            audio: None,
            side_data: BTreeSet::new(),
            packets: None,
        };
        let cover = StreamInfo {
            disposition: BTreeSet::from(["attached_pic".to_owned()]),
            ..s(0, StreamKind::Video, "mjpeg")
        };
        let main = StreamInfo {
            video: Some(VideoParams {
                width: 3840,
                height: 2160,
                pix_fmt: Some(pix_fmt.into()),
                bit_depth: depth,
                chroma,
                color: ColorTags {
                    primaries: Some("bt709".into()),
                    transfer: Some("bt709".into()),
                    space: None,
                    range: Some("tv".into()),
                },
                field_order: None,
                avg_frame_rate: None,
            }),
            ..s(1, StreamKind::Video, "h264")
        };
        MediaInfo {
            identity: FileIdentity {
                dev: 0,
                ino: 0,
                size: 0,
                mtime_ns: 0,
            },
            depth: ProbeDepth::Full,
            format: FormatInfo {
                format_name: format.into(),
                duration_s: Some(10.0),
                size: None,
                bit_rate: None,
                tags: tags
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            },
            streams: vec![cover, main, s(2, StreamKind::Audio, "aac")],
            chapters: vec![],
            marker: None,
            primary_video: Some(1),
            hdr: HdrInfo::default(),
            frame_side_data: BTreeSet::new(),
        }
    }

    fn args(info: &MediaInfo, source: &str, profile: Profile) -> (Vec<String>, RoutePlan) {
        let snapshot = SettingsSnapshot {
            profile,
            ..Default::default()
        };
        let plan = route::plan(
            info,
            Path::new(source),
            &snapshot.profile,
            &RouteOptions::default(),
        )
        .unwrap();
        let cmd = build(
            info,
            &plan,
            &snapshot,
            Path::new(source),
            Path::new("/m/out.tmp"),
        );
        // The software-decode command (GPU decoding has its own test).
        let args = cmd
            .software_decode
            .as_ref()
            .unwrap_or(&cmd.args)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        (args, plan)
    }

    fn has(args: &[String], pair: &[&str]) -> bool {
        args.windows(pair.len()).any(|w| w == pair)
    }

    #[test]
    fn x265_into_mkv() {
        let profile = Profile {
            id: "movies".into(),
            encoder: Encoder::Libx265,
            quality: 22,
            ..Default::default()
        };
        let (a, _) = args(
            &info("matroska,webm", "yuv420p10le", &[]),
            "/m/a.mkv",
            profile,
        );
        assert!(has(&a, &["-i", "file:/m/a.mkv"]));
        assert!(has(
            &a,
            &[
                "-map",
                "0",
                "-map_metadata",
                "0",
                "-map_chapters",
                "0",
                "-c",
                "copy"
            ]
        ));
        assert!(
            has(&a, &["-c:1", "libx265"]),
            "cover art at index 0 is copied"
        );
        assert!(has(&a, &["-crf:1", "22"]));
        assert!(has(&a, &["-preset:1", "medium"]));
        assert!(has(&a, &["-filter:1", "format=yuv420p10le"]));
        assert!(has(&a, &["-fps_mode:1", "passthrough"]));
        assert!(
            has(&a, &["-disposition:1", "0"]),
            "disables automatic default flags"
        );
        assert!(has(&a, &["-color_primaries:1", "bt709"]));
        assert!(has(&a, &["-color_range:1", "tv"]));
        assert!(
            !a.iter().any(|x| x.starts_with("-colorspace")),
            "unknown matrix is not invented"
        );
        let version = env!("CARGO_PKG_VERSION");
        assert!(has(
            &a,
            &[
                "-metadata",
                &format!("FNCOMPRESS=1;v={version};profile=movies")
            ]
        ));
        assert!(has(&a, &["-f", "matroska", "-n", "file:/m/out.tmp"]));
        assert!(!a.iter().any(|x| x == "-init_hw_device" || x == "-movflags"));
    }

    #[test]
    fn chapter_track_is_not_mapped() {
        let mut info = info("mov,mp4,m4a,3gp,3g2,mj2", "yuv420p", &[]);
        info.streams.push(StreamInfo {
            index: 3,
            kind: StreamKind::Data,
            codec_name: "bin_data".into(),
            codec_tag: Some("text".into()),
            ..info.streams[2].clone()
        });
        info.chapters = vec![crate::probe::Chapter {
            start_s: 0.0,
            end_s: 10.0,
            title: Some("Opening".into()),
        }];
        for container in [ContainerChoice::Keep, ContainerChoice::Mkv] {
            let profile = Profile {
                container,
                ..Default::default()
            };
            let (a, plan) = args(&info, "/m/a.mp4", profile);
            assert!(has(&a, &["-map", "0", "-map", "-0:3"]), "{a:?}");
            assert!(has(&a, &["-map_chapters", "0"]));

            let snapshot = SettingsSnapshot::default();
            let mux = build_injected_mux(
                &info,
                &plan,
                &snapshot,
                Path::new("/m/a.mp4"),
                Path::new("/m/v.mkv"),
                Path::new("/m/out.tmp"),
            );
            let a: Vec<_> = mux.args.iter().map(|a| a.to_string_lossy()).collect();
            assert!(a.iter().any(|x| x == "0:2"));
            assert!(!a.iter().any(|x| x.ends_with(":3")), "{a:?}");
        }
    }

    #[test]
    fn vaapi_downscale_uploads_after_scaling() {
        let profile = Profile {
            encoder: Encoder::HevcVaapi,
            quality: 25,
            resize: Resize::MaxHeight(1080),
            ..Default::default()
        };
        let (a, plan) = args(&info("matroska,webm", "yuv420p", &[]), "/m/a.mkv", profile);
        assert_eq!((plan.width, plan.height), (1920, 1080));
        assert!(has(&a, &["-vaapi_device", "/dev/dri/renderD128"]));
        assert!(has(&a, &["-rc_mode:1", "CQP", "-qp:1", "25"]));
        assert!(has(
            &a,
            &["-filter:1", "scale=1920:1080,format=nv12,hwupload"]
        ));
        assert!(!a.iter().any(|x| x.starts_with("-preset")));
    }

    #[test]
    fn hardware_encoders_decode_on_the_gpu_with_a_software_fallback() {
        let strings = |v: &[OsString]| -> Vec<String> {
            v.iter().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        let qsv_in = [
            "-init_hw_device",
            "vaapi=va:/dev/dri/renderD128",
            "-init_hw_device",
            "qsv=qs@va",
            "-hwaccel",
            "vaapi",
            "-hwaccel_device",
            "va",
            "-hwaccel_output_format",
            "vaapi",
            "-i",
        ];
        let vaapi_in = [
            "-vaapi_device",
            "/dev/dri/renderD128",
            "-hwaccel",
            "vaapi",
            "-hwaccel_device",
            "/dev/dri/renderD128",
            "-hwaccel_output_format",
            "vaapi",
            "-i",
        ];
        let qsv_map = ",hwmap=derive_device=qsv,format=qsv";
        // (encoder, setting, range, max height, GPU input, GPU filter, software filter)
        type Case<'a> = (
            Encoder,
            bool,
            &'a str,
            Option<u32>,
            &'a [&'a str],
            String,
            &'a str,
        );
        let cases: [Case; 6] = [
            (
                Encoder::HevcQsv,
                true,
                "tv",
                None,
                &qsv_in,
                format!("scale_vaapi=format=nv12{qsv_map}"),
                "format=nv12",
            ),
            (
                Encoder::HevcQsv,
                true,
                "tv",
                Some(1080),
                &qsv_in,
                format!("scale_vaapi=w=1920:h=1080:format=nv12{qsv_map}"),
                "scale=1920:1080,format=nv12",
            ),
            (
                Encoder::HevcVaapi,
                true,
                "tv",
                Some(1080),
                &vaapi_in,
                "scale_vaapi=w=1920:h=1080:format=nv12".into(),
                "scale=1920:1080,format=nv12,hwupload",
            ),
            // Off, a software encoder, or a full-range source: software only.
            (
                Encoder::HevcVaapi,
                false,
                "tv",
                None,
                &[],
                String::new(),
                "format=nv12,hwupload",
            ),
            (
                Encoder::Libx265,
                true,
                "tv",
                None,
                &[],
                String::new(),
                "format=yuv420p",
            ),
            (
                Encoder::HevcQsv,
                true,
                "pc",
                None,
                &[],
                String::new(),
                "scale=iw:ih:in_range=full:out_range=full,format=nv12",
            ),
        ];
        for (encoder, setting, range, max_height, gpu_in, gpu_filter, sw_filter) in cases {
            let mut src = info("matroska,webm", "yuv420p", &[]);
            src.streams[1].video.as_mut().unwrap().color.range = Some(range.into());
            let mut snapshot = SettingsSnapshot {
                profile: Profile {
                    encoder,
                    resize: max_height.map_or(Resize::Keep, Resize::MaxHeight),
                    ..Default::default()
                },
                ..Default::default()
            };
            snapshot.global.hw_decode = setting;
            let plan = route::plan(
                &src,
                Path::new("/m/a.mkv"),
                &snapshot.profile,
                &RouteOptions::default(),
            )
            .unwrap();
            let full = build(
                &src,
                &plan,
                &snapshot,
                Path::new("/m/a.mkv"),
                Path::new("/m/o"),
            );
            let trial = build_trial(
                &src,
                &plan,
                &snapshot,
                Path::new("/m/s.mkv"),
                1,
                Path::new("/m/o"),
            );
            for (cmd, out) in [(full, 1), (trial, 0)] {
                let filter = format!("-filter:{out}");
                let a = strings(&cmd.args);
                let gpu = !gpu_in.is_empty();
                assert_eq!(cmd.hw_decode(), gpu, "{encoder:?} {a:?}");
                assert_eq!(a.iter().any(|x| x == "-hwaccel"), gpu, "{a:?}");
                let software = match &cmd.software_decode {
                    Some(sw) => {
                        assert!(has(&a, gpu_in), "{a:?}");
                        assert!(has(&a, &[&filter, &gpu_filter]), "{a:?}");
                        strings(sw)
                    }
                    None => a,
                };
                assert!(!software.iter().any(|x| x.starts_with("-hwaccel")));
                assert!(has(&software, &[&filter, sw_filter]), "{software:?}");
            }
        }

        // Muxing the injected video decodes nothing.
        let src = info("matroska,webm", "yuv420p", &[]);
        let mut snapshot = SettingsSnapshot::default();
        snapshot.profile.encoder = Encoder::HevcQsv;
        let plan = route::plan(
            &src,
            Path::new("/m/a.mkv"),
            &snapshot.profile,
            &RouteOptions::default(),
        )
        .unwrap();
        let mux = build_injected_mux(
            &src,
            &plan,
            &snapshot,
            Path::new("/m/a.mkv"),
            Path::new("/m/v.mkv"),
            Path::new("/m/o"),
        );
        assert!(!mux.hw_decode());
        assert!(!strings(&mux.args).iter().any(|x| x == "-hwaccel"));
    }

    #[test]
    fn trial_encodes_only_the_sample_video_like_the_transcode() {
        let profile = Profile {
            encoder: Encoder::HevcVaapi,
            quality: 25,
            resize: Resize::MaxHeight(1080),
            ..Default::default()
        };
        let info = info("matroska,webm", "yuv420p", &[]);
        let snapshot = SettingsSnapshot {
            profile: profile.clone(),
            ..Default::default()
        };
        let (full, plan) = args(&info, "/m/a.mkv", profile);
        let cmd = build_trial(
            &info,
            &plan,
            &snapshot,
            Path::new("/w/sample.mkv"),
            0,
            Path::new("/w/t25.mkv"),
        );
        let a: Vec<String> = cmd
            .software_decode
            .as_ref()
            .unwrap_or(&cmd.args)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(has(&a, &["-i", "file:/w/sample.mkv", "-map", "0:0"]));
        assert!(has(&a, &["-vaapi_device", "/dev/dri/renderD128"]));
        // Same encoder options as the transcode, on output stream 0.
        for (opt, value) in [
            ("rc_mode", "CQP"),
            ("qp", "25"),
            ("filter", "scale=1920:1080,format=nv12,hwupload"),
            ("fps_mode", "passthrough"),
            ("color_primaries", "bt709"),
        ] {
            assert!(has(&full, &[&format!("-{opt}:1"), value]), "{opt}");
            assert!(has(&a, &[&format!("-{opt}:0"), value]), "{opt}");
        }
        assert!(has(&a, &["-an", "-sn", "-dn", "-f", "matroska", "-n"]));
        assert!(!a
            .iter()
            .any(|x| x.starts_with("FNCOMPRESS") || x == "-disposition:0"));
    }

    #[test]
    fn qsv_into_mp4_writes_marker_in_keywords() {
        let profile = Profile {
            id: "tv".into(),
            encoder: Encoder::HevcQsv,
            quality: 24,
            container: ContainerChoice::Keep,
            ..Default::default()
        };
        let src = info(
            "mov,mp4,m4a,3gp,3g2,mj2",
            "yuv420p",
            &[("keywords", "anime FNCOMPRESS=1;v=0.0.1;profile=old")],
        );
        let (a, _) = args(&src, "/m/a.mp4", profile);
        assert!(has(&a, &["-init_hw_device", "qsv=hw:/dev/dri/renderD128"]));
        assert!(has(&a, &["-global_quality:1", "24"]));
        let version = env!("CARGO_PKG_VERSION");
        assert!(has(
            &a,
            &[
                "-metadata",
                &format!("keywords=anime FNCOMPRESS=1;v={version};profile=tv")
            ]
        ));
        assert!(has(&a, &["-movflags", "+faststart"]));
        assert!(has(&a, &["-f", "mp4", "-n"]));
        // MP4 keeps dts in its sample table: no clamp there.
        assert!(!a.iter().any(|x| x.starts_with("-bsf")));
    }

    #[test]
    fn qsv_into_matroska_clamps_the_dts_jump() {
        let profile = Profile {
            encoder: Encoder::HevcQsv,
            container: ContainerChoice::Keep,
            ..Default::default()
        };
        let src = info("matroska,webm", "yuv420p", &[]);
        let (a, plan) = args(&src, "/m/a.mkv", profile.clone());
        assert!(has(&a, &["-bsf:1", QSV_DTS_CLAMP]));
        let snapshot = SettingsSnapshot {
            profile,
            ..Default::default()
        };
        let trial = build_trial(
            &src,
            &plan,
            &snapshot,
            Path::new("/w/s.mp4"),
            0,
            Path::new("/w/t.mkv"),
        );
        let t: Vec<String> = trial
            .args
            .iter()
            .map(|x| x.to_string_lossy().into_owned())
            .collect();
        assert!(has(&t, &["-bsf:0", QSV_DTS_CLAMP]));
        let (x265, _) = args(&src, "/m/a.mkv", Profile::default());
        assert!(!x265.iter().any(|x| x.starts_with("-bsf")));
    }

    #[test]
    fn full_range_is_kept_through_swscale() {
        let profile = Profile {
            encoder: Encoder::Libx264,
            ..Default::default()
        };
        let (a, _) = args(&info("matroska,webm", "yuvj420p", &[]), "/m/a.mkv", profile);
        assert!(has(
            &a,
            &[
                "-filter:1",
                "scale=iw:ih:in_range=full:out_range=full,format=yuv420p"
            ]
        ));
        assert!(has(&a, &["-color_range:1", "pc"]));
    }

    #[test]
    fn candidate_is_hidden_next_to_source() {
        let p = candidate_path(Path::new("/m/Movie (2020).mkv"), Container::Mp4);
        assert_eq!(p.parent(), Some(Path::new("/m")));
        let name = p.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".Movie (2020).mkv.fncompress-"), "{name}");
        assert!(name.ends_with(".mp4"));
    }

    #[test]
    fn libva_info_lines_are_not_errors() {
        let stderr = "libva info: VA-API version 1.23.0

libva info: va_openDriver() returns 0
";
        assert!(error_lines(stderr).is_empty());
        let stderr = "libva info: VA-API version 1.23.0
[hevc @ 0x1] Invalid NAL unit size
";
        assert_eq!(error_lines(stderr), ["[hevc @ 0x1] Invalid NAL unit size"]);
    }

    #[test]
    fn display_quotes_awkward_arguments() {
        let cmd = EncodeCommand {
            program: "ffmpeg".into(),
            args: vec![
                "-metadata".into(),
                "FNCOMPRESS=1;v=1".into(),
                "file:/m/a b.mkv".into(),
            ],
            output: "/m/x".into(),
            software_decode: None,
        };
        assert_eq!(
            cmd.display(),
            "ffmpeg -metadata 'FNCOMPRESS=1;v=1' 'file:/m/a b.mkv'"
        );
    }

    #[test]
    fn hidden_lead_in_is_dropped_only_into_matroska() {
        let cut = |hidden| {
            let mut i = info("mov,mp4,m4a,3gp,3g2,mj2", "yuv420p", &[]);
            i.streams[2].packets = Some(PacketStats {
                count: 188,
                empty: 0,
                start_s: Some(-1.005),
                end_s: Some(2.984),
                checkpoints: vec![],
                lead_in: Some(LeadIn {
                    packets: hidden,
                    last_start_s: Some(-0.024),
                }),
                first_frame_s: None,
            });
            i
        };
        let into = |container| Profile {
            encoder: Encoder::Libx265,
            container,
            ..Default::default()
        };
        let noise = |a: &[String]| a.iter().any(|x| x.starts_with("noise="));

        let (a, plan) = args(&cut(47), "/m/a.mp4", into(ContainerChoice::Mkv));
        assert!(has(&a, &["-bsf:2", r"noise=drop=lt(n\,46)"]), "{a:?}");
        assert_eq!(plan.lead_in_trims.len(), 1);
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        // The MP4 edit list keeps hiding it.
        let (a, plan) = args(&cut(47), "/m/a.mp4", into(ContainerChoice::Keep));
        assert!(!noise(&a) && plan.lead_in_trims.is_empty());
        // One hidden packet is the AAC priming of any MP4.
        let (a, plan) = args(&cut(1), "/m/a.mp4", into(ContainerChoice::Mkv));
        assert!(!noise(&a) && plan.lead_in_trims.is_empty());
    }
}
