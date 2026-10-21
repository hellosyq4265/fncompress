//! Encoder capability probe: a real one-second trial encode per encoder.
//! `ffmpeg -encoders` is not enough: on the NAS av1_qsv/av1_vaapi open fine but
//! never emit a packet (m0-findings), so an encoder only counts as available
//! when the trial produced packets.

use std::{path::Path, process::Command, time::Duration};

use serde::Serialize;

use crate::{
    encoder::Encoder,
    tool::{self, ToolError},
};

const TRIAL_SOURCE: &str = "testsrc2=s=640x360:r=25:d=1";
const TRIAL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EncoderProbe {
    pub encoder: Encoder,
    pub available: bool,
    pub packets: u64,
    /// Why the encoder is unavailable (last lines of FFmpeg's stderr).
    pub error: Option<String>,
}

pub fn probe_encoder(ffmpeg: &Path, encoder: Encoder, render_device: &str) -> EncoderProbe {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-v", "error"])
        .args(encoder.device_args(render_device))
        .args(["-f", "lavfi", "-i", TRIAL_SOURCE])
        .args(["-vf", &encoder.upload_filter("nv12")])
        .args(["-c:v", encoder.ffmpeg_name(), "-f", "framecrc", "-"]);

    // framecrc prints one line per packet after a `#` header.
    let mut packets = 0;
    let result = tool::run_lines(&mut cmd, TRIAL_TIMEOUT, None, |line| {
        if !line.is_empty() && !line.starts_with('#') {
            packets += 1;
        }
    });
    let error = match result {
        Ok(_) if packets > 0 => None,
        Ok(_) => Some("编码器已打开，但没有输出任何数据包".to_owned()),
        Err(ToolError::Failed { stderr, .. }) => Some(last_lines(&stderr, 3)),
        Err(e) => Some(e.to_string()),
    };
    EncoderProbe {
        encoder,
        available: error.is_none(),
        packets,
        error,
    }
}

pub fn probe_all(ffmpeg: &Path, render_device: &str) -> Vec<EncoderProbe> {
    Encoder::ALL
        .iter()
        .map(|&e| probe_encoder(ffmpeg, e, render_device))
        .collect()
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
