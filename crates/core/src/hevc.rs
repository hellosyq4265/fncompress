//! HEVC sources whose parameter sets (VPS, SPS, PPS) are only in-band in
//! front of the first frame, with an hvcC that has none: some `hev1`/`dvhe`
//! web releases. Decoding the whole file works, but anything that seeks into
//! the middle (spot checks, samples) has nothing to decode with. Such a part
//! is cut by stream copy and gets the parameter sets in front of its first
//! frame ([`prepend`]).

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use crate::{
    dovi::{self, mkv},
    probe::ffmpeg_input,
    tool::{self, CancelToken, ToolError},
};

const TIMEOUT: Duration = Duration::from_secs(600);
/// An hvcC without arrays is its 23-byte header alone.
const BARE_HVCC: usize = 23;
const SPS_NAL_TYPE: u8 = 33;

/// Whether stream `video` of `path` is HEVC whose codec configuration has
/// no parameter sets.
pub fn lacks_parameter_sets(
    ffprobe: &Path,
    path: &Path,
    video: u32,
    cancel: Option<&CancelToken>,
) -> Result<bool, ToolError> {
    let out = tool::run_capture(
        Command::new(ffprobe)
            .args(["-v", "error", "-select_streams", &video.to_string()])
            .args(["-show_entries", "stream=codec_name,extradata_size"])
            .args(["-of", "csv=p=0"])
            .arg(ffmpeg_input(path)),
        TIMEOUT,
        cancel,
    )?;
    let mut fields = out.trim().split(',');
    Ok(fields.next() == Some("hevc")
        && fields
            .next()
            .and_then(|n| n.trim().parse::<usize>().ok())
            .is_some_and(|n| n <= BARE_HVCC))
}

/// The VPS, SPS and PPS in front of the first frame of stream `video`,
/// read through `scratch` (removed afterwards).
pub fn parameter_sets(
    ffmpeg: &Path,
    path: &Path,
    video: u32,
    scratch: &Path,
    cancel: Option<&CancelToken>,
) -> Result<Vec<Vec<u8>>, ToolError> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "error", "-i"])
        .arg(ffmpeg_input(path))
        .args([
            "-map",
            &format!("0:{video}"),
            "-c",
            "copy",
            "-frames:v",
            "1",
        ])
        .args(["-bsf:v", "hevc_mp4toannexb,filter_units=pass_types=32-34"])
        .args(["-f", "hevc", "-y"])
        .arg(ffmpeg_input(scratch));
    tool::run_lines(&mut cmd, TIMEOUT, cancel, |_| {})?;
    let nals = dovi::read_nal_units(scratch);
    let _ = std::fs::remove_file(scratch);
    let nals = nals.map_err(|source| ToolError::Io {
        program: ffmpeg.display().to_string(),
        source,
    })?;
    if !nals
        .iter()
        .any(|n| n.first().is_some_and(|b| (b >> 1) & 0x3f == SPS_NAL_TYPE))
    {
        return Err(ToolError::Unexpected {
            program: ffmpeg.display().to_string(),
            message: "源片的 hvcC 不带参数集，第一帧前面也没有 SPS".into(),
        });
    }
    Ok(nals)
}

/// Rewrites `file` (a video-only HEVC Matroska cut) with `nals` in front of
/// its first frame.
pub fn prepend(file: &Path, nals: &[Vec<u8>]) -> Result<(), ToolError> {
    let unexpected = |message: String| ToolError::Unexpected {
        program: "fncompress".into(),
        message,
    };
    let mut whole = file.as_os_str().to_owned();
    whole.push(".ps");
    let whole = PathBuf::from(whole);
    if let Err(e) = mkv::prepend(file, nals, &whole) {
        let _ = std::fs::remove_file(&whole);
        return Err(unexpected(format!("给切出的片段补参数集失败：{e}")));
    }
    std::fs::rename(&whole, file).map_err(|e| unexpected(format!("替换切出的片段失败：{e}")))
}
