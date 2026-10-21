//! The verification checks that need FFmpeg (ADR 0004): full decode of the
//! candidate video, picture spot checks with SSIM, decoded frame counts when
//! packet counts differ, optional hashes of copied streams, and the RPUs and
//! HDR10+ messages a Dolby Vision or HDR10+ route injected. Results go to
//! [`verify::verify`](crate::verify::verify) as an [`Inspection`]; judging
//! them is verify's job.

use std::{path::Path, process::Command, time::Duration};

use crate::{
    command,
    dovi::{self, DoviTool, WorkFiles},
    hdr10plus, hevc,
    probe::{ffmpeg_input, MediaInfo, PacketStats, StreamKind},
    route::RoutePlan,
    settings::SettingsSnapshot,
    tool::{self, CancelToken, ToolError},
    verify::{DecodeResult, Inspection, RpuCheck, SpotCheck, StreamHash},
};

/// Spot checks stay clear of the first and last 5% (logos, credits, black).
const SPOT_MARGIN: f64 = 0.05;
/// Seek this much before a spot check so the decoder settles.
const SPOT_PREROLL_S: f64 = 1.0;
const SPOT_TIMEOUT: Duration = Duration::from_secs(300);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3600);

/// What a Dolby Vision or HDR10+ route injected, for the candidate to be
/// checked against.
#[derive(Debug, Clone, Copy, Default)]
pub struct Expected<'a> {
    /// The RPU file.
    pub rpu: Option<&'a Path>,
    /// Each frame's HDR10+ message, in display order.
    pub hdr10_plus: Option<&'a [Vec<u8>]>,
}

/// Runs every enabled check. Only cancellation is an error; a check that
/// fails to run is recorded in [`Inspection::errors`].
#[allow(clippy::too_many_arguments)]
pub fn inspect(
    source: &Path,
    src: &MediaInfo,
    candidate: &Path,
    out: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    expected: &Expected,
    cancel: Option<&CancelToken>,
) -> Result<Inspection, ToolError> {
    let t = &snapshot.global.verify;
    let mut i = Inspection::default();
    let v = plan.video_index;

    if t.decode_check {
        let timeout = command::encode_timeout(snapshot, out.format.duration_s);
        match decode(snapshot, candidate, v, timeout, cancel) {
            Ok(d) => i.decode = Some(d),
            Err(e @ ToolError::Cancelled { .. }) => return Err(e),
            Err(e) => i.errors.push(("decode".into(), e.to_string())),
        }
    }

    let packets = |info: &MediaInfo| {
        info.stream(v)
            .and_then(|s| s.packets.as_ref())
            .map(|p| p.count)
    };
    if packets(src) != packets(out) {
        let count = |path: &Path| count_frames(snapshot, path, v, cancel);
        match (count(source), count(candidate)) {
            (Ok(a), Ok(b)) => i.decoded_frames = Some((a, b)),
            (Err(e @ ToolError::Cancelled { .. }), _)
            | (_, Err(e @ ToolError::Cancelled { .. })) => return Err(e),
            (Err(e), _) | (_, Err(e)) => i.errors.push(("frame_count".into(), e.to_string())),
        }
    }

    if t.spot_checks > 0 {
        match spot_checks(source, src, candidate, out, plan, snapshot, cancel) {
            Ok(points) => i.spot_checks = Some(points),
            Err(e @ ToolError::Cancelled { .. }) => return Err(e),
            Err(e) => i.errors.push(("spot_check_ssim".into(), e.to_string())),
        }
    }

    let timeout = command::encode_timeout(snapshot, out.format.duration_s);
    if let (Some(_), Some(rpu)) = (plan.dynamic_range.dolby_vision(), expected.rpu) {
        match rpu_check(snapshot, candidate, v, rpu, timeout, cancel) {
            Ok(c) => i.rpu = Some(c),
            Err(e @ ToolError::Cancelled { .. }) => return Err(e),
            Err(e) => i.errors.push(("dovi_rpu".into(), e.to_string())),
        }
    }
    if let (true, Some(messages)) = (plan.dynamic_range.hdr10_plus(), expected.hdr10_plus) {
        match hdr10_plus_check(snapshot, candidate, v, messages, timeout, cancel) {
            Ok(c) => i.hdr10_plus = Some(c),
            Err(e @ ToolError::Cancelled { .. }) => return Err(e),
            Err(e) => i.errors.push(("hdr10_plus".into(), e.to_string())),
        }
    }

    // Repackaging into another container legitimately rewrites some codec
    // headers, so hashes are only comparable within one container.
    if t.copied_stream_hash && !plan.container_changed {
        let hash = |path: &Path| stream_hashes(snapshot, path, src, v, cancel);
        match (hash(source), hash(candidate)) {
            (Ok(a), Ok(b)) if a.len() == b.len() => {
                i.stream_hashes = Some(
                    a.into_iter()
                        .zip(b)
                        .map(|((index, source), (_, candidate))| StreamHash {
                            index,
                            source,
                            candidate,
                        })
                        .collect(),
                )
            }
            (Ok(_), Ok(_)) => i
                .errors
                .push(("stream_hash".into(), "两边复制流的数量不同".into())),
            (Err(e @ ToolError::Cancelled { .. }), _)
            | (_, Err(e @ ToolError::Cancelled { .. })) => return Err(e),
            (Err(e), _) | (_, Err(e)) => i.errors.push(("stream_hash".into(), e.to_string())),
        }
    }
    Ok(i)
}

/// Extracts the candidate's RPUs and compares them with `expected`.
fn rpu_check(
    snapshot: &SettingsSnapshot,
    candidate: &Path,
    video: u32,
    expected: &Path,
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> Result<RpuCheck, ToolError> {
    let tool = DoviTool::new(snapshot.global.dovi_tool_path());
    let mut work = WorkFiles::default();
    let found = work.add(command::work_path(candidate, "rpu"));
    tool.extract(
        &snapshot.global.ffmpeg,
        candidate,
        video,
        false,
        &found,
        timeout,
        cancel,
    )?;
    let read = |p: &Path| {
        dovi::read_rpus(p).map_err(|source| ToolError::Io {
            program: tool.path.display().to_string(),
            source,
        })
    };
    let (want, got) = (read(expected)?, read(&found)?);
    Ok(RpuCheck {
        expected: want.len() as u64,
        found: got.len() as u64,
        first_mismatch: want
            .iter()
            .zip(&got)
            .position(|(a, b)| a != b)
            .map(|n| n as u64),
    })
}

/// Extracts the candidate's HDR10+ messages and compares them with
/// `expected`. `found` counts the frames that carry one.
fn hdr10_plus_check(
    snapshot: &SettingsSnapshot,
    candidate: &Path,
    video: u32,
    expected: &[Vec<u8>],
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> Result<RpuCheck, ToolError> {
    let scratch = command::work_path(candidate, "sei.mkv");
    let got = hdr10plus::extract(
        &snapshot.global.ffmpeg,
        candidate,
        video,
        &scratch,
        timeout,
        cancel,
    )?;
    let first_mismatch = (0..expected.len().max(got.len()))
        .find(|&n| expected.get(n) != got.get(n).and_then(Option::as_ref))
        .map(|n| n as u64);
    Ok(RpuCheck {
        expected: expected.len() as u64,
        found: got.iter().flatten().count() as u64,
        first_mismatch,
    })
}

fn ffmpeg(snapshot: &SettingsSnapshot) -> Command {
    let mut cmd = Command::new(&snapshot.global.ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "error"]);
    crate::command::quiet_libva(&mut cmd);
    cmd
}

/// Decodes the whole video stream; any line on stderr is a decoder error.
/// VAAPI goes first when enabled; if it reports anything, a software decode
/// decides, so a GPU quirk never rejects a good file.
fn decode(
    snapshot: &SettingsSnapshot,
    path: &Path,
    video: u32,
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> Result<DecodeResult, ToolError> {
    let device = &snapshot.global.render_device;
    let run = |hw: bool| -> Result<Vec<String>, ToolError> {
        let mut cmd = ffmpeg(snapshot);
        if hw {
            cmd.args(["-hwaccel", "vaapi", "-hwaccel_device", device])
                .args(["-hwaccel_output_format", "vaapi"]);
        }
        // The null muxer would otherwise get the frames in 1/frame-rate
        // ticks: Matroska's millisecond timestamps half a frame off (an MP4
        // source's AAC priming shifts everything by ~21 ms) round onto each
        // other and read as errors.
        cmd.arg("-i").arg(ffmpeg_input(path)).args([
            "-map",
            &format!("0:{video}"),
            "-enc_time_base:v",
            "demux",
            "-f",
            "null",
            "-",
        ]);
        match tool::run_lines(&mut cmd, timeout, cancel, |_| {}) {
            Ok(stderr) => Ok(crate::command::error_lines(&stderr)),
            Err(ToolError::Failed { stderr, status, .. }) => {
                let mut errs = crate::command::error_lines(&stderr);
                errs.push(format!("ffmpeg 退出：{status}"));
                Ok(errs)
            }
            Err(e) => Err(e),
        }
    };
    if snapshot.global.verify.hw_decode && Path::new(device).exists() {
        match run(true) {
            Ok(errors) if errors.is_empty() => {
                return Ok(DecodeResult {
                    hardware: true,
                    errors,
                })
            }
            Err(e @ ToolError::Cancelled { .. }) => return Err(e),
            Ok(errors) => tracing::info!(
                ?errors,
                "hardware decode reported errors; retrying in software"
            ),
            Err(e) => tracing::info!("hardware decode failed ({e}); retrying in software"),
        }
    }
    Ok(DecodeResult {
        hardware: false,
        errors: run(false)?,
    })
}

fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn count_frames(
    snapshot: &SettingsSnapshot,
    path: &Path,
    video: u32,
    cancel: Option<&CancelToken>,
) -> Result<u64, ToolError> {
    let mut cmd = Command::new(&snapshot.global.ffprobe);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        &video.to_string(),
        "-count_frames",
    ])
    .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
    .arg(ffmpeg_input(path));
    let out = tool::run_capture(&mut cmd, PROBE_TIMEOUT, cancel)?;
    // MPEG-TS lists the stream once under its program and once on its own.
    lines(&out)
        .first()
        .map_or("", |l| l.trim_end_matches(','))
        .parse()
        .map_err(|_| ToolError::Unexpected {
            program: "ffprobe".into(),
            message: format!("无法解析帧数：{out:?}"),
        })
}

/// Spot-check positions in seconds from the video start: `n` evenly spaced
/// windows of `len` inside the middle 90%.
pub fn spot_positions(duration: f64, n: u32, len: f64) -> Vec<f64> {
    if n == 0 || duration <= 0.0 {
        return vec![];
    }
    if duration <= len {
        return vec![0.0];
    }
    let lo = duration * SPOT_MARGIN;
    let span = (duration * (1.0 - 2.0 * SPOT_MARGIN) - len).max(0.0);
    (0..n)
        .map(|k| lo + span * (f64::from(k) + 0.5) / f64::from(n))
        .collect()
}

fn spot_checks(
    source: &Path,
    src: &MediaInfo,
    candidate: &Path,
    out: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    cancel: Option<&CancelToken>,
) -> Result<Vec<SpotCheck>, ToolError> {
    let t = &snapshot.global.verify;
    let v = plan.video_index;
    let span = |info: &MediaInfo| {
        let p = info.stream(v).and_then(|s| s.packets.as_ref());
        (
            p.and_then(PacketStats::shown_start_s).unwrap_or(0.0),
            p.and_then(|p| p.end_s),
        )
    };
    let (src_start, src_end) = span(src);
    let (out_start, _) = span(out);
    let duration = src_end.map_or(0.0, |e| e - src_start);
    let len = t.spot_check_secs;
    // A source with its parameter sets only at the start cannot be decoded
    // from a seek: each window of it is cut out and given them instead.
    let parameter_sets = if hevc::lacks_parameter_sets(&snapshot.global.ffprobe, source, v, cancel)?
    {
        Some(hevc::parameter_sets(
            &snapshot.global.ffmpeg,
            source,
            v,
            &command::work_path(candidate, "ps.hevc"),
            cancel,
        )?)
    } else {
        None
    };

    let mut points = Vec::new();
    for at in spot_positions(duration, t.spot_checks, len) {
        // Both files keep their own timestamps (-copyts); trimming at each
        // file's video start + `at` selects the same frames even when a
        // container change shifted every stream.
        let window = |start: f64| {
            let seek = (at - SPOT_PREROLL_S).max(0.0);
            (format!("{seek:.3}"), format!("{:.6}", start + at))
        };
        let (seek_a, from_a) = window(src_start);
        let (seek_b, from_b) = window(out_start);
        let input_len = format!("{:.3}", len + SPOT_PREROLL_S + 1.0);
        let branch = |input: &str, from: &str, label: &str| {
            format!(
                "[{input}]trim=start={from}:duration={len},setpts=N/TB,\
                 scale={}:{},format=yuv420p[{label}]",
                plan.width, plan.height
            )
        };
        let mut work = WorkFiles::default();
        let mut cmd = ffmpeg(snapshot);
        let source_video = match &parameter_sets {
            Some(nals) => {
                // Stream copy keeps the source's timestamps (-copyts), so
                // the trim below is the same as with a seek.
                let cut = work.add(command::work_path(candidate, "spot.mkv"));
                let mut c = ffmpeg(snapshot);
                c.args(["-copyts", "-ss", &seek_a, "-t", &input_len, "-i"])
                    .arg(ffmpeg_input(source))
                    .args(["-map", &format!("0:{v}"), "-c", "copy"])
                    .args(["-f", "matroska", "-y"])
                    .arg(ffmpeg_input(&cut));
                tool::run_lines(&mut c, SPOT_TIMEOUT, cancel, |_| {})?;
                hevc::prepend(&cut, nals)?;
                cmd.args(["-copyts", "-i"]).arg(ffmpeg_input(&cut));
                "0:0".to_string()
            }
            None => {
                cmd.args(["-copyts", "-ss", &seek_a, "-t", &input_len, "-i"])
                    .arg(ffmpeg_input(source));
                format!("0:{v}")
            }
        };
        let graph = format!(
            "{};{};[a][b]ssim=stats_file=-:shortest=1",
            branch(&source_video, &from_a, "a"),
            branch(&format!("1:{v}"), &from_b, "b")
        );
        cmd.args(["-ss", &seek_b, "-t", &input_len, "-i"])
            .arg(ffmpeg_input(candidate))
            .args(["-filter_complex", &graph, "-f", "null", "-"]);
        let (mut sum, mut frames) = (0.0, 0u32);
        tool::run_lines(&mut cmd, SPOT_TIMEOUT, cancel, |line| {
            if let Some(all) = parse_ssim_all(line) {
                sum += all;
                frames += 1;
            }
        })?;
        if frames == 0 {
            return Err(ToolError::Unexpected {
                program: "ffmpeg ssim".into(),
                message: format!("{at:.1}s 处没有可比较的帧"),
            });
        }
        points.push(SpotCheck {
            at_s: at,
            frames,
            ssim: sum / f64::from(frames),
        });
    }
    Ok(points)
}

/// `n:1 Y:0.991 U:0.993 V:0.992 All:0.991806 (20.866)` -> 0.991806
fn parse_ssim_all(line: &str) -> Option<f64> {
    let rest = &line[line.find("All:")? + 4..];
    rest.split_whitespace().next()?.parse().ok()
}

/// SHA-256 of each copied stream (everything but the encoded video,
/// attachments and data streams), as (stream index, hash) in stream order.
fn stream_hashes(
    snapshot: &SettingsSnapshot,
    path: &Path,
    src: &MediaInfo,
    video: u32,
    cancel: Option<&CancelToken>,
) -> Result<Vec<(u32, String)>, ToolError> {
    let indices: Vec<u32> = src
        .streams
        .iter()
        .filter(|s| {
            s.index != video && !matches!(s.kind, StreamKind::Attachment | StreamKind::Data)
        })
        .map(|s| s.index)
        .collect();
    if indices.is_empty() {
        return Ok(vec![]);
    }
    let mut cmd = ffmpeg(snapshot);
    cmd.arg("-i").arg(ffmpeg_input(path));
    for i in &indices {
        cmd.args(["-map", &format!("0:{i}")]);
    }
    cmd.args(["-c", "copy", "-f", "streamhash", "-hash", "sha256", "-"]);
    let mut hashes = Vec::new();
    tool::run_lines(&mut cmd, PROBE_TIMEOUT, cancel, |line| {
        // `0,a,SHA256=...`: output index, kind, hash.
        if let Some((_, hash)) = line.rsplit_once('=') {
            hashes.push(hash.to_owned());
        }
    })?;
    Ok(indices.into_iter().zip(hashes).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_positions_avoid_the_edges() {
        let p = spot_positions(100.0, 8, 2.0);
        assert_eq!(p.len(), 8);
        assert!(p.iter().all(|&x| (5.0..=93.0).contains(&x)), "{p:?}");
        assert!(p.windows(2).all(|w| w[1] > w[0]));
        assert_eq!(spot_positions(1.5, 8, 2.0), [0.0]);
        assert!(spot_positions(100.0, 0, 2.0).is_empty());
        assert!(spot_positions(0.0, 8, 2.0).is_empty());
    }

    #[test]
    fn parses_ssim_lines() {
        let line = "n:1 Y:0.991 U:0.993 V:0.992 All:0.991806 (20.866)";
        assert_eq!(parse_ssim_all(line), Some(0.991806));
        assert_eq!(parse_ssim_all("garbage"), None);
    }
}
