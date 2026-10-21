//! Samples: picking windows of the source (by bitrate or by decoded scene
//! measurements), cutting them out by stream copy, and checking ready-made
//! sample files against the source.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use super::{
    AutoMethod, RecommendError, RecommendJob, RecommendStage, Sample, SampleKind, SampleSpec,
};
use crate::{
    hevc,
    probe::{ffmpeg_input, DynamicRange, MediaInfo, ProbeDepth, Prober, VideoParams},
    route::RoutePlan,
    tool::{self, CancelToken},
};

/// Auto samples stay clear of the first and last 5% (logos, credits).
const MARGIN: f64 = 0.05;
/// Scene analysis measures at most this many frames spread over the film.
const ANALYSIS_POINTS: usize = 600;
const PACKETS_TIMEOUT: Duration = Duration::from_secs(3600);
const CUT_TIMEOUT: Duration = Duration::from_secs(1800);

/// A sample ready for trials: the file to encode and its video stream.
pub(super) struct Prepared {
    pub sample: Sample,
    pub file: PathBuf,
    pub stream: u32,
}

/// Builds the samples. `report(stage, done, total)`: for
/// [`RecommendStage::Analyzing`], `done` is the analysed fraction (0–1);
/// for [`RecommendStage::Sampling`], the number of samples finished.
pub(super) fn prepare(
    prober: &Prober,
    job: &RecommendJob,
    info: &MediaInfo,
    plan: &RoutePlan,
    mut report: impl FnMut(RecommendStage, f64, u32),
) -> Result<Vec<Prepared>, RecommendError> {
    let ffprobe = &job.snapshot.global.ffprobe;
    let fps = info
        .stream(plan.video_index)
        .and_then(|s| s.video.as_ref())
        .and_then(|v| v.avg_frame_rate.as_deref())
        .and_then(parse_rate);
    match &job.request.samples {
        SampleSpec::Files { paths } => {
            let total = paths.len() as u32;
            let source = video_params(info, plan.video_index);
            let mut out = Vec::new();
            for (i, path) in paths.iter().enumerate() {
                report(RecommendStage::Sampling, i as f64, total);
                let s = prober.probe(path, ProbeDepth::Quick, job.cancel)?;
                let stream = s
                    .primary_video
                    .ok_or_else(|| invalid(i, path, "没有视频流"))?;
                check_matches(source, video_params(&s, stream), info, &s)
                    .map_err(|why| invalid(i, path, &why))?;
                let (frames, bytes) = video_packets(ffprobe, path, stream, job.cancel)?;
                let rate = video_params(&s, stream)
                    .avg_frame_rate
                    .as_deref()
                    .and_then(parse_rate);
                let duration = duration_of(frames, rate, s.format.duration_s);
                out.push(Prepared {
                    sample: Sample {
                        kind: SampleKind::File,
                        path: path.clone(),
                        start_s: None,
                        duration_s: duration,
                        frames,
                        video_bytes: bytes,
                        bitrate_bps: bitrate(bytes, duration),
                        reason: "现成样片".into(),
                    },
                    file: path.clone(),
                    stream,
                });
            }
            report(RecommendStage::Sampling, f64::from(total), total);
            Ok(out)
        }
        SampleSpec::Auto {
            method,
            count,
            secs,
        } => {
            report(RecommendStage::Analyzing, 0.0, *count);
            let list = list_packets(ffprobe, job.source, plan.video_index, job.cancel)?;
            if list.packets.is_empty() {
                return Err(RecommendError::Invalid("源片没有视频数据包".into()));
            }
            let windows = match method {
                AutoMethod::Bitrate => pick_by_bitrate(&list.packets, *count, *secs),
                AutoMethod::Scene => {
                    let metrics = analyze_scenes(job, plan, &list, &mut |f| {
                        report(RecommendStage::Analyzing, f, *count)
                    })?;
                    pick_by_scene(&list.packets, &metrics, *count, *secs)
                }
            };
            let total = windows.len() as u32;
            let first = list.packets[0].t;
            // Samples of a source with its parameter sets only at the start
            // get them in front of their first frame.
            let parameter_sets =
                if hevc::lacks_parameter_sets(ffprobe, job.source, plan.video_index, job.cancel)
                    .map_err(RecommendError::tool("检查源片的参数集"))?
                {
                    Some(
                        hevc::parameter_sets(
                            &job.snapshot.global.ffmpeg,
                            job.source,
                            plan.video_index,
                            &job.workdir.join("parameter-sets.hevc"),
                            job.cancel,
                        )
                        .map_err(RecommendError::tool("读取源片的参数集"))?,
                    )
                } else {
                    None
                };
            let mut out = Vec::new();
            for (i, w) in windows.iter().enumerate() {
                report(RecommendStage::Sampling, i as f64, total);
                let file = job.workdir.join(format!("sample-{i}.mkv"));
                cut(job, plan.video_index, w.start - list.start, w.len, &file)?;
                if let Some(nals) = &parameter_sets {
                    hevc::prepend(&file, nals).map_err(RecommendError::tool("切取样片"))?;
                }
                let (frames, bytes) = video_packets(ffprobe, &file, 0, job.cancel)?;
                if frames == 0 {
                    return Err(RecommendError::Invalid(format!(
                        "样片 {} 切出来是空的",
                        i + 1
                    )));
                }
                let duration = duration_of(frames, fps, None);
                out.push(Prepared {
                    sample: Sample {
                        kind: SampleKind::Auto,
                        path: job.source.to_owned(),
                        start_s: Some(w.start - first),
                        duration_s: duration,
                        frames,
                        video_bytes: bytes,
                        bitrate_bps: bitrate(bytes, duration),
                        reason: w.reason.clone(),
                    },
                    file,
                    stream: 0,
                });
            }
            report(RecommendStage::Sampling, f64::from(total), total);
            Ok(out)
        }
    }
}

fn invalid(i: usize, path: &Path, why: &str) -> RecommendError {
    RecommendError::Invalid(format!("样片 {}（{}）{why}", i + 1, path.display()))
}

fn video_params(info: &MediaInfo, index: u32) -> &VideoParams {
    info.stream(index)
        .and_then(|s| s.video.as_ref())
        .expect("primary video has video params")
}

/// A ready-made sample must look like the source to the encoder: same
/// size, depth and chroma, the same dynamic range and, for Dolby Vision,
/// the same configuration.
fn check_matches(
    source: &VideoParams,
    sample: &VideoParams,
    source_info: &MediaInfo,
    sample_info: &MediaInfo,
) -> Result<(), String> {
    if (sample.width, sample.height) != (source.width, source.height) {
        return Err(format!(
            "分辨率 {}×{} 与原片 {}×{} 不同",
            sample.width, sample.height, source.width, source.height
        ));
    }
    if (sample.bit_depth, sample.chroma) != (source.bit_depth, source.chroma) {
        return Err(format!(
            "像素格式 {} 与原片 {} 不同",
            sample.pix_fmt.as_deref().unwrap_or("未知"),
            source.pix_fmt.as_deref().unwrap_or("未知")
        ));
    }
    let (a, b) = (source_info.dynamic_range(), sample_info.dynamic_range());
    if a != b {
        return Err(format!(
            "动态范围是 {}，原片是 {}",
            range_name(b),
            range_name(a)
        ));
    }
    let profile = |i: &MediaInfo| i.hdr.dolby_vision.map(|c| (c.profile, c.bl_compat_id));
    if profile(source_info) != profile(sample_info) {
        return Err("杜比视界配置与原片不同".into());
    }
    if sample.is_interlaced() {
        return Err("是隔行扫描".into());
    }
    Ok(())
}

fn range_name(r: DynamicRange) -> &'static str {
    match r {
        DynamicRange::Sdr => "SDR",
        DynamicRange::Hlg => "HLG",
        DynamicRange::Hdr10 => "HDR10",
        DynamicRange::DynamicHdr => "动态 HDR",
        DynamicRange::DolbyVision => "杜比视界",
    }
}

fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
    (n > 0.0 && d > 0.0).then(|| n / d)
}

fn duration_of(frames: u64, fps: Option<f64>, fallback: Option<f64>) -> f64 {
    match fps {
        Some(fps) => frames as f64 / fps,
        None => fallback.unwrap_or(frames as f64 / 24.0),
    }
}

fn bitrate(bytes: u64, secs: f64) -> f64 {
    if secs > 0.0 {
        bytes as f64 * 8.0 / secs
    } else {
        0.0
    }
}

/// One packet of the primary video: presentation time (seconds, as in the
/// file), size, keyframe flag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Packet {
    pub t: f64,
    pub size: u64,
    pub key: bool,
}

struct PacketList {
    /// The container's start time, which `-ss` is relative to.
    start: f64,
    /// Sorted by time.
    packets: Vec<Packet>,
}

fn ffprobe(ffprobe: &Path) -> Command {
    let mut cmd = Command::new(ffprobe);
    cmd.args(["-v", "error", "-hide_banner"]);
    cmd
}

fn list_packets(
    ffprobe_path: &Path,
    path: &Path,
    stream: u32,
    cancel: Option<&CancelToken>,
) -> Result<PacketList, RecommendError> {
    let mut start = 0.0;
    let mut packets = Vec::new();
    tool::run_lines(
        ffprobe(ffprobe_path)
            .args(["-select_streams", &stream.to_string()])
            .args([
                "-show_entries",
                "packet=pts_time,dts_time,size,flags:format=start_time",
            ])
            .args(["-of", "compact=p=1:nk=0"])
            .arg(ffmpeg_input(path)),
        PACKETS_TIMEOUT,
        cancel,
        |line| {
            let mut fields = line.split('|');
            let section = fields.next();
            let get = |key: &str| {
                line.split('|')
                    .find_map(|f| f.strip_prefix(key)?.strip_prefix('='))
                    .and_then(|v| v.parse::<f64>().ok())
            };
            match section {
                Some("format") => start = get("start_time").unwrap_or(0.0),
                Some("packet") => {
                    let Some(t) = get("pts_time").or_else(|| get("dts_time")) else {
                        return;
                    };
                    let size = get("size").unwrap_or(0.0) as u64;
                    let key = fields.any(|f| f.starts_with("flags=K"));
                    packets.push(Packet { t, size, key });
                }
                _ => {}
            }
        },
    )
    .map_err(RecommendError::tool("读取源片数据包"))?;
    packets.sort_by(|a, b| a.t.total_cmp(&b.t));
    Ok(PacketList { start, packets })
}

/// Frames (packets) and bytes of one video stream.
pub(super) fn video_packets(
    ffprobe_path: &Path,
    path: &Path,
    stream: u32,
    cancel: Option<&CancelToken>,
) -> Result<(u64, u64), RecommendError> {
    let (mut frames, mut bytes) = (0u64, 0u64);
    tool::run_lines(
        ffprobe(ffprobe_path)
            .args(["-select_streams", &stream.to_string()])
            .args(["-show_entries", "packet=size", "-of", "csv=p=0"])
            .arg(ffmpeg_input(path)),
        PACKETS_TIMEOUT,
        cancel,
        |line| {
            if let Ok(size) = line.trim().trim_end_matches(',').parse::<u64>() {
                frames += 1;
                bytes += size;
            }
        },
    )
    .map_err(RecommendError::tool("统计视频数据包"))?;
    Ok((frames, bytes))
}

/// Cuts `len` seconds of the video from `at` (relative to the container
/// start, on a keyframe) by stream copy.
fn cut(
    job: &RecommendJob,
    video: u32,
    at: f64,
    len: f64,
    out: &Path,
) -> Result<(), RecommendError> {
    let mut cmd = Command::new(&job.snapshot.global.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "error"])
        // Matroska seeks to the cue point before, and stream copy would keep
        // that pre-roll: seek a hair before the keyframe and drop whatever
        // precedes it (`-copypriorss 0`, judged by pts), so the sample starts
        // exactly on the keyframe.
        .args(["-ss", &format!("{:.6}", (at - 0.0005).max(0.0))])
        .arg("-i")
        .arg(ffmpeg_input(job.source))
        .args([
            "-map",
            &format!("0:{video}"),
            "-c",
            "copy",
            "-copypriorss",
            "0",
        ])
        .args(["-t", &format!("{len:.6}")])
        .args(["-map_metadata", "-1", "-map_chapters", "-1"])
        .args(["-avoid_negative_ts", "make_zero"])
        .args(["-f", "matroska", "-n"])
        .arg(ffmpeg_input(out));
    tool::run_lines(&mut cmd, CUT_TIMEOUT, job.cancel, |_| {})
        .map(drop)
        .map_err(RecommendError::tool("切取样片"))
}

/// A part of the source to sample: from a keyframe, `len` seconds.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Window {
    pub start: f64,
    pub len: f64,
    pub bytes: u64,
    pub reason: String,
}

/// Keyframe-aligned windows of `secs` within the margins, at least half a
/// window apart. A film shorter than one and a half windows is one window.
pub(super) fn candidate_windows(packets: &[Packet], secs: f64) -> Vec<Window> {
    let (Some(first), Some(last)) = (packets.first(), packets.last()) else {
        return vec![];
    };
    let span = last.t - first.t;
    if span <= secs * 1.5 {
        return vec![Window {
            start: first.t,
            len: span + 1.0,
            bytes: packets.iter().map(|p| p.size).sum(),
            reason: "片长较短，整段作为样片".into(),
        }];
    }
    let (mut lo, mut hi) = (first.t + span * MARGIN, last.t - span * MARGIN - secs);
    if hi < lo {
        (lo, hi) = (first.t, last.t - secs);
    }
    let mut starts: Vec<f64> = Vec::new();
    for p in packets.iter().filter(|p| p.key && p.t >= lo && p.t <= hi) {
        if starts.last().is_none_or(|s| p.t - s >= secs / 2.0) {
            starts.push(p.t);
        }
    }
    if starts.is_empty() {
        // Sparse keyframes: the last one before the range, or the first.
        let before = packets.iter().rev().find(|p| p.key && p.t <= hi);
        starts.push(before.or(packets.iter().find(|p| p.key)).unwrap_or(first).t);
    }
    // Prefix sums over time-sorted packets give each window's bytes.
    let mut sums = Vec::with_capacity(packets.len() + 1);
    sums.push(0u64);
    for p in packets {
        sums.push(sums.last().unwrap() + p.size);
    }
    let at = |t: f64| packets.partition_point(|p| p.t < t);
    starts
        .into_iter()
        .map(|start| Window {
            start,
            len: secs,
            bytes: sums[at(start + secs)] - sums[at(start)],
            reason: String::new(),
        })
        .collect()
}

/// The whole film as one window (too short to choose from).
fn is_whole(windows: &[Window]) -> bool {
    windows.len() == 1 && !windows[0].reason.is_empty()
}

/// By bitrate: half the samples (rounded up) from the highest-bitrate
/// windows, which are the hardest to compress; the rest from the middle
/// towards the low end (often dark or static scenes, where banding shows).
pub(super) fn pick_by_bitrate(packets: &[Packet], count: u32, secs: f64) -> Vec<Window> {
    let windows = candidate_windows(packets, secs);
    if is_whole(&windows) {
        return windows;
    }
    let count = count as usize;
    let mut by_bytes: Vec<&Window> = windows.iter().collect();
    by_bytes.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.start.total_cmp(&b.start)));
    let high = count.div_ceil(2);
    let rest = count - high;
    // Where each slot aims in the descending order (0 is the highest
    // bitrate): the top, then from the median (0.5) down to the low end (0.8).
    let mut targets: Vec<f64> = vec![0.0; high];
    targets.extend((0..rest).map(|k| {
        if rest == 1 {
            0.5
        } else {
            0.5 + 0.3 * k as f64 / (rest - 1) as f64
        }
    }));
    // Prefer a whole window of gap between samples; settle for no overlap.
    let mut chosen: Vec<Window> = Vec::new();
    let mut filled = vec![false; targets.len()];
    for gap in [2.0 * secs, secs] {
        for (k, &q) in targets.iter().enumerate() {
            if filled[k] {
                continue;
            }
            let aim = (q * (by_bytes.len() - 1) as f64).round() as usize;
            let mut order: Vec<usize> = (0..by_bytes.len()).collect();
            order.sort_by_key(|&i| (i.abs_diff(aim), i));
            let free = |w: &Window| chosen.iter().all(|c| (c.start - w.start).abs() >= gap);
            if let Some(&i) = order.iter().find(|&&i| free(by_bytes[i])) {
                chosen.push(by_bytes[i].clone());
                filled[k] = true;
            }
        }
    }
    // Name each pick by where its bitrate actually ranks: in a short film a
    // slot may have settled for a window far from its aim.
    let last = (by_bytes.len().max(2) - 1) as f64;
    let rank = |w: &Window| {
        by_bytes
            .iter()
            .position(|b| b.start == w.start)
            .unwrap_or(0)
    };
    let top = chosen.iter().map(rank).min();
    for w in &mut chosen {
        let r = rank(w);
        let q = r as f64 / last;
        w.reason = if Some(r) == top && q < 1.0 / 3.0 {
            "码率最高的片段（最难压缩）"
        } else if q < 1.0 / 3.0 {
            "码率较高的片段"
        } else if q < 2.0 / 3.0 {
            "码率居中的片段"
        } else {
            "码率偏低的片段（暗场或静态画面）"
        }
        .into();
    }
    chosen.sort_by(|a, b| a.start.total_cmp(&b.start));
    chosen
}

/// Per-frame measurements of the scene analysis.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct FrameMetrics {
    /// Seconds, on the same clock as [`Packet::t`].
    pub t: f64,
    /// Spatial information (ITU-T P.910): detail.
    pub si: f64,
    /// Temporal information: motion.
    pub ti: f64,
    pub brightness: f64,
    pub contrast: f64,
}

/// Decodes the whole video at a small size and measures every `step`th
/// frame: spatial and temporal information, brightness, contrast.
///
/// With `hw_decode` on, the video is decoded and shrunk on the GPU (VAAPI)
/// first, whatever the encoder: only the small frames come back to memory,
/// so a long 4K film no longer takes the CPU for tens of minutes. A run that
/// fails, or prints errors, is redone in software, as transcodes do.
/// Full-range sources stay in software (only swscale is told the range).
fn analyze_scenes(
    job: &RecommendJob,
    plan: &RoutePlan,
    list: &PacketList,
    progress: &mut dyn FnMut(f64),
) -> Result<Vec<FrameMetrics>, RecommendError> {
    let packets = &list.packets;
    let step = packets.len().div_ceil(ANALYSIS_POINTS).max(1);
    let span = packets.last().map_or(0.0, |l| l.t - packets[0].t).max(1.0);
    let timeout = crate::command::encode_timeout(job.snapshot, Some(span));
    let global = &job.snapshot.global;
    let gpu = global.hw_decode && !plan.full_range && Path::new(&global.render_device).exists();
    let mut run = |gpu: bool| {
        let mut cmd = scene_command(job, plan, step, gpu);
        let mut out: Vec<FrameMetrics> = Vec::new();
        let mut parser = MetricsParser::default();
        let stderr = tool::run_lines(&mut cmd, timeout, job.cancel, |line| {
            if let Some(m) = parser.push(line) {
                progress((m.t / span).clamp(0.0, 1.0));
                out.push(m);
            }
        })?;
        out.extend(parser.finish());
        Ok::<_, tool::ToolError>((out, stderr))
    };
    let gpu_run = match gpu {
        true => match run(true) {
            Ok((out, stderr)) => {
                let errors = crate::command::error_lines(&stderr);
                if errors.is_empty() && !out.is_empty() {
                    Some(out)
                } else {
                    tracing::info!(
                        ?errors,
                        "GPU-decoded scene analysis reported errors; redoing it in software"
                    );
                    None
                }
            }
            Err(tool::ToolError::Failed { stderr, status, .. }) => {
                tracing::info!(
                    %status,
                    stderr = stderr.trim(),
                    "GPU-decoded scene analysis failed; redoing it in software"
                );
                None
            }
            Err(e) => return Err(RecommendError::tool("场景分析")(e)),
        },
        false => None,
    };
    let mut out = match gpu_run {
        Some(out) => out,
        None => run(false).map_err(RecommendError::tool("场景分析"))?.0,
    };
    if out.is_empty() {
        return Err(RecommendError::Invalid("场景分析没有得到任何测量值".into()));
    }
    // FFmpeg starts the clock at the container start; packets use file times.
    for m in &mut out {
        m.t += list.start;
    }
    Ok(out)
}

/// The scene analysis run: the video shrunk to 320 wide in 8-bit 4:2:0,
/// then measured. On the GPU, `scale_vaapi` shrinks and converts to NV12
/// before the download.
fn scene_command(job: &RecommendJob, plan: &RoutePlan, step: usize, gpu: bool) -> Command {
    let global = &job.snapshot.global;
    let shrink = match gpu {
        true => "scale_vaapi=w=320:h=-2:format=nv12,hwdownload,format=nv12",
        false => "scale=w=320:h=-2:flags=fast_bilinear",
    };
    let filter = format!(
        "{shrink},format=yuv420p,siti,\
         select='not(mod(n\\,{step}))',signalstats,metadata=mode=print:file='pipe\\:1'"
    );
    let mut cmd = Command::new(&global.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "error"]);
    if gpu {
        crate::command::quiet_libva(&mut cmd);
        cmd.args([
            "-hwaccel",
            "vaapi",
            "-hwaccel_device",
            &global.render_device,
        ])
        .args(["-hwaccel_output_format", "vaapi"]);
    }
    cmd.arg("-i")
        .arg(ffmpeg_input(job.source))
        .args(["-map", &format!("0:{}", plan.video_index)])
        .args(["-an", "-sn", "-dn", "-vf", &filter, "-f", "null", "-"]);
    cmd
}

/// Parses `metadata=mode=print` output: a `frame:N pts:P pts_time:T` line,
/// then `key=value` lines.
#[derive(Default)]
struct MetricsParser {
    current: Option<FrameMetrics>,
    low: f64,
}

impl MetricsParser {
    /// Returns the previous frame when a new one starts.
    fn push(&mut self, line: &str) -> Option<FrameMetrics> {
        if line.starts_with("frame:") {
            let t = line
                .split_whitespace()
                .find_map(|f| f.strip_prefix("pts_time:"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.0);
            let done = self.current.take();
            self.current = Some(FrameMetrics {
                t,
                ..Default::default()
            });
            return done;
        }
        let (Some(m), Some((key, value))) = (self.current.as_mut(), line.split_once('=')) else {
            return None;
        };
        let Ok(v) = value.trim().parse::<f64>() else {
            return None;
        };
        match key {
            "lavfi.siti.si" => m.si = v,
            "lavfi.siti.ti" => m.ti = v,
            "lavfi.signalstats.YAVG" => m.brightness = v,
            "lavfi.signalstats.YLOW" => self.low = v,
            "lavfi.signalstats.YHIGH" => m.contrast = (v - self.low).max(0.0),
            _ => {}
        }
        None
    }

    fn finish(&mut self) -> Option<FrameMetrics> {
        self.current.take()
    }
}

/// By scene: each window's mean detail, peak motion, mean contrast and
/// brightness, normalised over all windows. The first pick is the most
/// complex window; later ones weigh complexity against how different they
/// are from what is already chosen (ported from fpk-converter).
pub(super) fn pick_by_scene(
    packets: &[Packet],
    metrics: &[FrameMetrics],
    count: u32,
    secs: f64,
) -> Vec<Window> {
    let windows = candidate_windows(packets, secs);
    if is_whole(&windows) {
        return windows;
    }
    struct Row {
        w: Window,
        raw: [f64; 4],
        norm: [f64; 4],
        extreme: f64,
        complexity: f64,
    }
    let mut rows: Vec<Row> = windows
        .into_iter()
        .filter_map(|w| {
            let inside: Vec<&FrameMetrics> = metrics
                .iter()
                .filter(|m| m.t >= w.start && m.t < w.start + w.len)
                .collect();
            let inside = if inside.is_empty() {
                vec![metrics
                    .iter()
                    .min_by(|a, b| (a.t - w.start).abs().total_cmp(&(b.t - w.start).abs()))?]
            } else {
                inside
            };
            let n = inside.len() as f64;
            let mean = |f: fn(&FrameMetrics) -> f64| inside.iter().map(|m| f(m)).sum::<f64>() / n;
            let raw = [
                mean(|m| m.si),
                inside.iter().map(|m| m.ti).fold(0.0, f64::max),
                mean(|m| m.contrast),
                mean(|m| m.brightness),
            ];
            Some(Row {
                w,
                raw,
                norm: [0.0; 4],
                extreme: 0.0,
                complexity: 0.0,
            })
        })
        .collect();
    for k in 0..4 {
        let (lo, hi) = rows
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), r| {
                (lo.min(r.raw[k]), hi.max(r.raw[k]))
            });
        for r in &mut rows {
            r.norm[k] = if hi > lo {
                (r.raw[k] - lo) / (hi - lo)
            } else {
                0.5
            };
        }
    }
    for r in &mut rows {
        r.extreme = (r.norm[3] - 0.5).abs() * 2.0;
        r.complexity = 0.32 * r.norm[0] + 0.26 * r.norm[1] + 0.26 * r.norm[2] + 0.16 * r.extreme;
    }
    let mut chosen: Vec<usize> = Vec::new();
    while chosen.len() < count as usize {
        let score = |i: usize| -> Option<f64> {
            let r = &rows[i];
            if chosen.contains(&i)
                || chosen
                    .iter()
                    .any(|&c| (rows[c].w.start - r.w.start).abs() < secs)
            {
                return None;
            }
            if chosen.is_empty() {
                return Some(r.complexity);
            }
            let diversity = chosen
                .iter()
                .map(|&c| {
                    rows[c]
                        .norm
                        .iter()
                        .zip(&r.norm)
                        .map(|(a, b)| (a - b).powi(2))
                        .sum::<f64>()
                        .sqrt()
                })
                .fold(f64::INFINITY, f64::min);
            Some(0.58 * r.complexity + 0.42 * diversity / 2.0)
        };
        let best = (0..rows.len())
            .filter_map(|i| score(i).map(|s| (i, s)))
            // Ties go to the earlier window.
            .max_by(|a, b| a.1.total_cmp(&b.1).then(b.0.cmp(&a.0)));
        match best {
            Some((i, _)) => chosen.push(i),
            None => break,
        }
    }
    let mut out: Vec<Window> = chosen
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let r = &rows[i];
            let mut traits = Vec::new();
            if r.norm[0] >= 0.65 {
                traits.push("细节较多");
            }
            if r.norm[1] >= 0.65 {
                traits.push("运动或场景变化明显");
            }
            if r.norm[2] >= 0.65 {
                traits.push("明暗反差较大");
            }
            if r.extreme >= 0.65 {
                traits.push("画面较亮或较暗");
            }
            if k > 0 {
                traits.push("与已选片段差异较大");
            }
            if traits.is_empty() {
                traits.push("补充代表性场景");
            }
            Window {
                reason: traits.join("；"),
                ..r.w.clone()
            }
        })
        .collect();
    out.sort_by(|a, b| a.start.total_cmp(&b.start));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `secs` seconds at 10 fps, keyframe every second; `size(t)` per packet.
    fn packets(secs: u32, size: impl Fn(f64) -> u64) -> Vec<Packet> {
        (0..secs * 10)
            .map(|i| {
                let t = f64::from(i) / 10.0;
                Packet {
                    t,
                    size: size(t),
                    key: i % 10 == 0,
                }
            })
            .collect()
    }

    #[test]
    fn windows_start_on_keyframes_inside_the_margins() {
        let p = packets(200, |_| 100);
        let w = candidate_windows(&p, 10.0);
        // 5% of ~200 s is ~10 s; starts every 5 s (half a window).
        assert_eq!(w.first().unwrap().start, 10.0);
        assert!(w.last().unwrap().start <= 200.0 - 10.0 - 10.0);
        assert!(w.windows(2).all(|p| p[1].start - p[0].start == 5.0));
        assert!(w.iter().all(|w| w.bytes == 100 * 100));
        // A short film is one window covering it all.
        let w = candidate_windows(&packets(12, |_| 1), 10.0);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].start, 0.0);
        assert_eq!(w[0].bytes, 120);
    }

    #[test]
    fn bitrate_picks_the_peaks_then_the_middle_and_low_end() {
        // Bitrate rises with time, with a spike at 100–110 s.
        let p = packets(300, |t| {
            if (100.0..110.0).contains(&t) {
                10_000
            } else {
                100 + t as u64
            }
        });
        let w = pick_by_bitrate(&p, 4, 10.0);
        assert_eq!(w.len(), 4);
        let starts: Vec<f64> = w.iter().map(|w| w.start).collect();
        assert!(starts.contains(&100.0), "{starts:?}");
        assert!(
            w.windows(2).all(|p| p[1].start - p[0].start >= 20.0),
            "{starts:?}"
        );
        let reasons: Vec<&str> = w.iter().map(|w| w.reason.as_str()).collect();
        assert!(reasons.contains(&"码率最高的片段（最难压缩）"));
        assert!(reasons.contains(&"码率较高的片段"));
        assert!(reasons.contains(&"码率居中的片段"));
        assert!(reasons.contains(&"码率偏低的片段（暗场或静态画面）"));
        // The highest-bitrate one besides the spike is late in the film.
        let high = w.iter().find(|w| w.reason == "码率较高的片段").unwrap();
        assert!(high.start > 250.0, "{starts:?}");
        let low = w.iter().find(|w| w.reason.starts_with("码率偏低")).unwrap();
        assert!(low.start < 100.0, "{starts:?}");
    }

    #[test]
    fn bitrate_takes_fewer_when_the_film_is_short() {
        let p = packets(40, |t| t as u64);
        let w = pick_by_bitrate(&p, 4, 10.0);
        assert!(!w.is_empty() && w.len() < 4, "{w:?}");
        // Labels follow the actual ranks, not the slot that was aimed at:
        // rising bitrate, so the earliest window is the lowest.
        let reasons: Vec<&str> = w.iter().map(|w| w.reason.as_str()).collect();
        assert_eq!(reasons.last(), Some(&"码率最高的片段（最难压缩）"), "{w:?}");
        assert_eq!(reasons[0], "码率偏低的片段（暗场或静态画面）", "{w:?}");
        for pair in w.windows(2) {
            assert!(pair[1].start - pair[0].start >= 10.0);
        }
        assert_eq!(pick_by_bitrate(&packets(8, |_| 1), 4, 10.0).len(), 1);
    }

    #[test]
    fn scene_picks_complex_then_different_windows() {
        let p = packets(300, |_| 100);
        let metrics: Vec<FrameMetrics> = (0..3000)
            .step_by(5)
            .map(|i| {
                let t = f64::from(i) / 10.0;
                let busy = (150.0..160.0).contains(&t);
                let dark = (60.0..70.0).contains(&t);
                FrameMetrics {
                    t,
                    si: if busy { 80.0 } else { 20.0 },
                    ti: if busy { 40.0 } else { 2.0 },
                    brightness: if dark { 16.0 } else { 120.0 },
                    contrast: if busy { 200.0 } else { 100.0 },
                }
            })
            .collect();
        let w = pick_by_scene(&p, &metrics, 3, 10.0);
        assert_eq!(w.len(), 3);
        let busy = w.iter().find(|w| w.start == 150.0).expect("busy window");
        assert!(busy.reason.contains("细节较多"), "{}", busy.reason);
        let dark = w.iter().find(|w| w.start == 60.0).expect("dark window");
        assert!(dark.reason.contains("画面较亮或较暗"), "{}", dark.reason);
        assert!(w.windows(2).all(|p| p[1].start - p[0].start >= 10.0));
    }

    #[test]
    fn parses_metadata_print_output() {
        let mut p = MetricsParser::default();
        let lines = [
            "frame:0    pts:0       pts_time:0",
            "lavfi.siti.si=84.50",
            "lavfi.siti.ti=0.00",
            "lavfi.signalstats.YLOW=41",
            "lavfi.signalstats.YAVG=125.219",
            "lavfi.signalstats.YHIGH=210",
            "frame:1    pts:24      pts_time:1.001",
            "lavfi.siti.si=80",
        ];
        let out: Vec<_> = lines.iter().filter_map(|l| p.push(l)).collect();
        assert_eq!(
            out,
            [FrameMetrics {
                t: 0.0,
                si: 84.5,
                ti: 0.0,
                brightness: 125.219,
                contrast: 169.0,
            }]
        );
        let last = p.finish().unwrap();
        assert_eq!((last.t, last.si), (1.001, 80.0));
    }

    #[test]
    fn frame_rates_and_durations() {
        assert_eq!(parse_rate("24000/1001"), Some(24000.0 / 1001.0));
        assert_eq!(parse_rate("25"), Some(25.0));
        assert_eq!(parse_rate("0/0"), None);
        assert_eq!(duration_of(240, Some(24.0), None), 10.0);
        assert_eq!(duration_of(240, None, Some(9.5)), 9.5);
        assert_eq!(bitrate(1000, 2.0), 4000.0);
    }
}
