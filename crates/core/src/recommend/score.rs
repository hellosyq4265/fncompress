//! VMAF with the scoring runtime's FFmpeg (ADR 0006): a startup check that
//! libvmaf works, and the score of one trial against its sample. HDR and
//! Dolby Vision samples are scored through a display proxy (ADR 0011): both
//! sides are first mapped to SDR BT.709 by libplacebo, the same way.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::RecommendError;
use crate::{
    probe::ffmpeg_input,
    tool::{self, CancelToken},
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(60);

/// The scoring FFmpeg. Shared libraries shipped next to it (the runtime
/// bundles its Vulkan loader for libplacebo) go on the library path.
pub(super) struct Scorer {
    pub ffmpeg: PathBuf,
}

impl Scorer {
    pub fn new(ffmpeg: PathBuf) -> Self {
        Self { ffmpeg }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.ffmpeg);
        if let Some(dir) = self.ffmpeg.parent() {
            if dir.join("libvulkan.so.1").is_file() {
                cmd.env("LD_LIBRARY_PATH", dir);
            }
        }
        cmd.args(["-hide_banner", "-nostdin", "-nostats"]);
        cmd
    }
}

/// What the startup check found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ScorerInfo {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub ffmpeg: PathBuf,
    /// FFmpeg version string, e.g. `n8.1.3-17-gc389840894-20261010`.
    pub version: String,
}

/// Checks that `ffmpeg` runs and scores a tiny clip with libvmaf.
pub fn probe_scorer(ffmpeg: &Path) -> Result<ScorerInfo, String> {
    let scorer = Scorer::new(ffmpeg.to_owned());
    let out = tool::run_capture(scorer.command().arg("-version"), CHECK_TIMEOUT, None)
        .map_err(|e| e.to_string())?;
    let version = out
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("ffmpeg version "))
        .and_then(|l| l.split_whitespace().next())
        .unwrap_or("未知")
        .to_owned();
    let clip = "testsrc2=s=64x64:r=10:d=0.3";
    let stderr = tool::run_lines(
        scorer
            .command()
            .args([
                "-v", "info", "-f", "lavfi", "-i", clip, "-f", "lavfi", "-i", clip,
            ])
            .args(["-lavfi", "[0][1]libvmaf=model=version=vmaf_v0.6.1"])
            .args(["-f", "null", "-"]),
        CHECK_TIMEOUT,
        None,
        |_| {},
    )
    .map_err(|e| match e {
        tool::ToolError::Failed { ref stderr, .. } if stderr.contains("libvmaf") => {
            "这个 FFmpeg 没有 libvmaf，请指定评分运行时的 FFmpeg".to_owned()
        }
        e => e.to_string(),
    })?;
    if !stderr.contains("VMAF score") {
        return Err("libvmaf 没有输出分数".into());
    }
    Ok(ScorerInfo {
        ffmpeg: ffmpeg.to_owned(),
        version,
    })
}

/// libplacebo options of the display proxy: BT.2390 tone mapping to BT.709
/// SDR, 10-bit, without the frame-dependent peak detection and dithering,
/// so both sides are mapped identically.
const PROXY: &str = "colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:\
                     format=yuv420p10le:tonemapping=bt.2390:peak_detect=0:dithering=none";

/// Checks that `ffmpeg` can run the display proxy: libplacebo on a Vulkan
/// device (on the NAS without the render group that is the software
/// llvmpipe, which is slow but works).
pub fn probe_display_proxy(ffmpeg: &Path) -> Result<(), String> {
    let scorer = Scorer::new(ffmpeg.to_owned());
    let clip = "testsrc2=s=64x64:r=10:d=0.3,format=yuv420p10le,\
                setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=tv";
    tool::run_lines(
        scorer
            .command()
            .args(["-v", "error"])
            .args(VULKAN)
            .args(["-f", "lavfi", "-i", clip])
            .args(["-vf", &format!("libplacebo=w=64:h=64:{PROXY}")])
            .args(["-f", "null", "-"]),
        CHECK_TIMEOUT,
        None,
        |_| {},
    )
    .map(drop)
    .map_err(|e| match e {
        tool::ToolError::Failed { ref stderr, .. } if stderr.contains("No such filter") => {
            "这个 FFmpeg 没有 libplacebo".to_owned()
        }
        tool::ToolError::Failed { ref stderr, .. } if stderr.contains("Vulkan") => {
            format!(
                "没有可用的 Vulkan 设备：{}",
                stderr.lines().last().unwrap_or("")
            )
        }
        e => e.to_string(),
    })
}

/// libplacebo gets the device FFmpeg opens; on its own it skips software
/// devices.
const VULKAN: [&str; 4] = ["-init_hw_device", "vulkan=vk", "-filter_hw_device", "vk"];

/// How HDR and Dolby Vision samples are mapped before VMAF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DisplayProxy {
    /// Apply the RPU's reshaping and mapping (P5, P8); P7 and HDR10/HLG use
    /// the HDR10/HLG base picture.
    pub dolby_vision: bool,
}

pub(super) struct VmafSpec<'a> {
    /// Output picture size, where both sides are compared.
    pub width: u32,
    pub height: u32,
    /// Planar pixel format both sides are converted to.
    pub format: &'a str,
    pub full_range: bool,
    pub model: &'a str,
    pub threads: usize,
    pub proxy: Option<DisplayProxy>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct VmafScore {
    pub mean: f64,
    pub frames: u64,
}

/// Scores `distorted` (stream 0) against stream `reference_stream` of
/// `reference`. Frames are paired by their order, not their timestamps
/// (`setpts=N/TB` on both sides, as the spot checks do), so container time
/// bases cannot shift the pairing.
#[allow(clippy::too_many_arguments)]
pub(super) fn vmaf(
    scorer: &Scorer,
    reference: &Path,
    reference_stream: u32,
    distorted: &Path,
    spec: &VmafSpec,
    workdir: &Path,
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> Result<VmafScore, RecommendError> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // A bare file name in the working directory: nothing to escape.
    let log = format!("vmaf-{nonce:x}.json");
    let range = if spec.full_range {
        ":in_range=full:out_range=full"
    } else {
        ""
    };
    let side = |input: &str, label: &str| match spec.proxy {
        None => format!(
            "[{input}]setpts=N/TB,scale={}:{}:flags=bicubic{range},format={}[{label}]",
            spec.width, spec.height, spec.format
        ),
        Some(p) => format!(
            "[{input}]setpts=N/TB,libplacebo=w={}:h={}:{PROXY}:apply_dolbyvision={}[{label}]",
            spec.width,
            spec.height,
            u8::from(p.dolby_vision)
        ),
    };
    let filter = format!(
        "{};{};[dist][ref]libvmaf=model=version={}:n_threads={}:log_fmt=json:log_path={log}",
        side(&format!("0:{reference_stream}"), "ref"),
        side("1:0", "dist"),
        spec.model,
        spec.threads,
    );
    let mut cmd = scorer.command();
    cmd.current_dir(workdir).args(["-v", "error"]);
    if spec.proxy.is_some() {
        cmd.args(VULKAN);
    }
    cmd.arg("-i")
        .arg(ffmpeg_input(reference))
        .arg("-i")
        .arg(ffmpeg_input(distorted))
        .args(["-filter_complex", &filter])
        .args(["-an", "-sn", "-dn", "-f", "null", "-"]);
    let log_path = workdir.join(&log);
    let result = tool::run_lines(&mut cmd, timeout, cancel, |_| {});
    let text = std::fs::read_to_string(&log_path);
    let _ = std::fs::remove_file(&log_path);
    result.map_err(RecommendError::tool("VMAF 评分"))?;
    let text = text.map_err(|e| RecommendError::Score(format!("读不到评分日志：{e}")))?;
    parse_log(&text).map_err(RecommendError::Score)
}

/// Mean of the per-frame scores in a libvmaf JSON log, checked against the
/// pooled mean libvmaf reports itself.
pub(super) fn parse_log(text: &str) -> Result<VmafScore, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("评分日志不是 JSON：{e}"))?;
    let frames = v["frames"].as_array().ok_or("评分日志没有逐帧结果")?;
    if frames.is_empty() {
        return Err("没有评分任何帧".into());
    }
    let scores: Vec<f64> = frames
        .iter()
        .map(|f| f["metrics"]["vmaf"].as_f64().filter(|s| s.is_finite()))
        .collect::<Option<_>>()
        .ok_or("有的帧缺少 VMAF 分数")?;
    let mean = scores.iter().sum::<f64>() / scores.len() as f64;
    if !(0.0..=100.0).contains(&mean) {
        return Err(format!("平均分 {mean} 超出 0–100"));
    }
    if let Some(pooled) = v["pooled_metrics"]["vmaf"]["mean"].as_f64() {
        if (pooled - mean).abs() > 0.02 {
            return Err(format!("逐帧平均 {mean:.3} 与汇总 {pooled:.3} 不一致"));
        }
    }
    Ok(VmafScore {
        mean,
        frames: scores.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_libvmaf_json_logs() {
        let log = r#"{"version":"86da14d","frames":[
            {"frameNum":0,"metrics":{"vmaf":96.0}},
            {"frameNum":1,"metrics":{"vmaf":94.0}}],
            "pooled_metrics":{"vmaf":{"min":94.0,"max":96.0,"mean":95.0}}}"#;
        assert_eq!(
            parse_log(log),
            Ok(VmafScore {
                mean: 95.0,
                frames: 2
            })
        );
        assert!(parse_log("{}").is_err());
        assert!(parse_log(r#"{"frames":[]}"#).is_err());
        assert!(parse_log(r#"{"frames":[{"metrics":{}}]}"#).is_err());
        let inconsistent = log.replace("\"mean\":95.0", "\"mean\":90.0");
        assert!(parse_log(&inconsistent).unwrap_err().contains("不一致"));
    }
}
