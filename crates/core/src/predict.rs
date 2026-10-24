//! 转码前预判收益 (backlog 16): before a transcode, encode a few short
//! samples with the profile and estimate the output size, so a source the
//! profile cannot shrink (a low-bitrate WEB-DL with QSV, say) is not
//! encoded in full only to end as 收益不足.
//!
//! Samples are spread evenly over the film within the margins and cut by
//! stream copy from the keyframe before each point, as 样片推荐 cuts its
//! samples; each is encoded with [`command::build_trial`], the video
//! options of the real transcode (GPU decoding decided the same way). The
//! trials' video over the samples' video gives the ratio for the whole
//! video stream; the other streams are copied and keep their size.
//!
//! The estimate has an error, so a transcode is skipped only when the
//! estimate falls [`MARGIN_PCT`] below the threshold: a wrong skip leaves a
//! file uncompressed without anyone noticing, which is worse than one
//! wasted encode. Near the threshold the transcode runs and the savings
//! check after it decides.

use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    command::{self, Progress},
    dovi::WorkFiles,
    hevc,
    probe::{self, ffmpeg_input, MediaInfo},
    route::RoutePlan,
    settings::SettingsSnapshot,
    tool::{self, CancelToken, ToolError},
};

/// Samples encoded for an estimate.
pub const SAMPLES: u32 = 5;
/// Length of each sample, seconds.
pub const SAMPLE_SECS: f64 = 8.0;
/// A transcode is skipped only when the estimated saving is this many
/// points below `min_savings_pct`.
pub const MARGIN_PCT: f64 = 5.0;
/// Samples stay clear of the first and last 5% (logos, credits).
const MARGIN: f64 = 0.05;
/// No estimate when the samples would be more than this share of the
/// film: encoding it whole costs little more.
const MAX_SHARE: f64 = 0.1;
const CUT_TIMEOUT: Duration = Duration::from_secs(600);
const PACKETS_TIMEOUT: Duration = Duration::from_secs(600);

/// The estimated output of a transcode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Estimate {
    /// Samples encoded.
    pub samples: u32,
    /// Their total length, seconds.
    pub sample_secs: f64,
    /// Trial video bytes over sample video bytes.
    pub video_ratio: f64,
    pub source_size: u64,
    pub estimated_size: u64,
    /// Estimated saving relative to the source, in percent.
    pub saved_pct: f64,
    /// Wall time the estimate took, seconds.
    pub secs: f64,
}

impl Estimate {
    /// Whether the transcode is skipped: the estimate is [`MARGIN_PCT`]
    /// below `min_savings_pct`.
    pub fn no_gain(&self, min_savings_pct: f64) -> bool {
        self.saved_pct < min_savings_pct - MARGIN_PCT
    }
}

/// Progress of an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PredictProgress {
    /// 1-based sample being encoded.
    pub sample: u32,
    pub samples: u32,
    pub percent: f64,
}

/// Estimates the transcode of `source` as `plan` would make it. `None`
/// when no estimate is worth making: the film is too short, or its video
/// size is unknown.
pub fn estimate(
    info: &MediaInfo,
    plan: &RoutePlan,
    snapshot: &SettingsSnapshot,
    source: &Path,
    cancel: Option<&CancelToken>,
    mut on_progress: impl FnMut(PredictProgress),
) -> Result<Option<Estimate>, ToolError> {
    let started = Instant::now();
    let Some(duration) = info.format.duration_s.filter(|d| *d > 0.0) else {
        return Ok(None);
    };
    let video_bytes = info
        .stream(plan.video_index)
        .and_then(|s| s.packets.as_ref())
        .map_or(0, |p| p.bytes);
    if video_bytes == 0 {
        return Ok(None);
    }
    let points = sample_points(duration, SAMPLES, SAMPLE_SECS);
    if points.is_empty() {
        return Ok(None);
    }
    let global = &snapshot.global;
    let video = plan.video_index;
    let parameter_sets = if hevc::lacks_parameter_sets(&global.ffprobe, source, video, cancel)? {
        let scratch = command::work_path(source, "ps.hevc");
        Some(hevc::parameter_sets(
            &global.ffmpeg,
            source,
            video,
            &scratch,
            cancel,
        )?)
    } else {
        None
    };

    let samples = points.len() as u32;
    let mut work = WorkFiles::default();
    let (mut sample_bytes, mut trial_bytes, mut sample_secs) = (0u64, 0u64, 0.0);
    for (i, &at) in points.iter().enumerate() {
        let report = |part: f64| PredictProgress {
            sample: i as u32 + 1,
            samples,
            percent: ((i as f64 + part.clamp(0.0, 1.0)) / f64::from(samples) * 100.0).min(99.0),
        };
        on_progress(report(0.0));
        let file = work.add(command::work_path(source, &format!("predict-{i}.mkv")));
        cut(
            &global.ffmpeg,
            source,
            video,
            at,
            SAMPLE_SECS,
            &file,
            cancel,
        )?;
        if let Some(nals) = &parameter_sets {
            hevc::prepend(&file, nals)?;
        }
        let (frames, bytes) =
            probe::stream_packets(&global.ffprobe, &file, 0, PACKETS_TIMEOUT, cancel)?;
        if frames == 0 {
            continue;
        }
        let out = work.add(command::work_path(
            source,
            &format!("predict-{i}-trial.mkv"),
        ));
        let cmd = command::build_trial(info, plan, snapshot, &file, 0, &out);
        tracing::debug!(command = %cmd.display(), "savings prediction trial");
        let secs = frames as f64 / frame_rate(info, video).unwrap_or(24.0);
        command::run(
            &cmd,
            command::encode_timeout(snapshot, Some(secs)),
            cancel,
            |p: Progress| on_progress(report(p.out_time_s / secs)),
        )?;
        let (_, encoded) =
            probe::stream_packets(&global.ffprobe, &out, 0, PACKETS_TIMEOUT, cancel)?;
        sample_bytes += bytes;
        trial_bytes += encoded;
        sample_secs += secs;
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&out);
    }
    if sample_bytes == 0 {
        return Ok(None);
    }
    let video_ratio = trial_bytes as f64 / sample_bytes as f64;
    let source_size = info.identity.size;
    let estimated_size = (source_size.saturating_sub(video_bytes) as f64
        + video_bytes as f64 * video_ratio)
        .round() as u64;
    let saved_pct = crate::publish::Savings::new(source_size, estimated_size).saved_pct;
    Ok(Some(Estimate {
        samples,
        sample_secs,
        video_ratio,
        source_size,
        estimated_size,
        saved_pct,
        secs: started.elapsed().as_secs_f64(),
    }))
}

/// Where the samples start, seconds from the container start: `count`
/// evenly spaced windows of `secs` within the margins. Empty when they
/// would cover more than [`MAX_SHARE`] of the film.
pub fn sample_points(duration: f64, count: u32, secs: f64) -> Vec<f64> {
    if count == 0 || f64::from(count) * secs > duration * MAX_SHARE {
        return Vec::new();
    }
    let span = duration * (1.0 - 2.0 * MARGIN);
    (0..count)
        .map(|k| {
            let centre = duration * MARGIN + span * (f64::from(k) + 0.5) / f64::from(count);
            (centre - secs / 2.0).max(0.0)
        })
        .collect()
}

fn frame_rate(info: &MediaInfo, video: u32) -> Option<f64> {
    let rate = info
        .stream(video)?
        .video
        .as_ref()?
        .avg_frame_rate
        .as_deref()?;
    let (n, d) = rate.split_once('/').unwrap_or((rate, "1"));
    let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
    (n > 0.0 && d > 0.0).then(|| n / d)
}

/// Cuts about `len` seconds of the video from `at` (relative to the
/// container start) by stream copy. Input seeking lands on the keyframe
/// before `at` and stream copy keeps the frames from there, so the cut
/// starts on a keyframe and decodes on its own.
fn cut(
    ffmpeg: &Path,
    source: &Path,
    video: u32,
    at: f64,
    len: f64,
    out: &Path,
    cancel: Option<&CancelToken>,
) -> Result<(), ToolError> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "error"])
        .args(["-ss", &format!("{at:.3}")])
        .arg("-i")
        .arg(ffmpeg_input(source))
        .args(["-map", &format!("0:{video}"), "-c", "copy"])
        .args(["-t", &format!("{len:.3}")])
        .args(["-map_metadata", "-1", "-map_chapters", "-1"])
        .args(["-avoid_negative_ts", "make_zero"])
        .args(["-f", "matroska", "-n"])
        .arg(ffmpeg_input(out));
    tool::run_lines(&mut cmd, CUT_TIMEOUT, cancel, |_| {}).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_spread_evenly_inside_the_margins() {
        let points = sample_points(1000.0, 5, 8.0);
        assert_eq!(points.len(), 5);
        // Centres at 5% + 90% x (k + 0.5) / 5: 140, 320, 500, 680, 860.
        for (p, centre) in points.iter().zip([140.0, 320.0, 500.0, 680.0, 860.0]) {
            assert!((p - (centre - 4.0)).abs() < 1e-9, "{p}");
        }
    }

    #[test]
    fn short_films_get_no_estimate() {
        // 40 s of samples is more than a tenth of 399 s.
        assert!(sample_points(399.0, 5, 8.0).is_empty());
        assert_eq!(sample_points(400.0, 5, 8.0).len(), 5);
        assert!(sample_points(7200.0, 0, 8.0).is_empty());
    }

    #[test]
    fn skips_only_well_below_the_threshold() {
        let e = |saved_pct| Estimate {
            samples: 5,
            sample_secs: 40.0,
            video_ratio: 1.0,
            source_size: 100,
            estimated_size: 100,
            saved_pct,
            secs: 1.0,
        };
        assert!(e(-0.1).no_gain(5.0));
        assert!(!e(0.0).no_gain(5.0));
        assert!(!e(3.0).no_gain(5.0));
        assert!(e(4.9).no_gain(10.0));
    }
}
