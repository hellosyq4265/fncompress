//! Dolby Vision routes (ADR 0011). The source's RPU travels to the
//! candidate frame for frame:
//!
//! 1. dovi_tool extracts it in display order ([`DoviTool::extract`]),
//!    converting P7 to P8.1 on the way;
//! 2. the re-encoded video goes to an intermediate Matroska file;
//! 3. [`mkv::inject`] appends each frame's RPU NAL unit and writes the
//!    configuration record;
//! 4. FFmpeg muxes that video with the source's other streams.
//!
//! Re-encoding keeps B-frames and any frame timing, unlike pushing a raw
//! HEVC stream through dovi_tool's `inject-rpu`, which loses timestamps.

pub mod mkv;

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    probe::{ffmpeg_input, DoviConfig},
    tool::{self, CancelToken, ToolError},
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// The configuration a Dolby Vision source is re-encoded to: P5 and P8 stay,
/// P7 becomes P8.1 without the enhancement layer.
pub fn output_config(source: &DoviConfig) -> DoviConfig {
    if source.profile == 7 {
        DoviConfig {
            profile: 8,
            bl_compat_id: 1,
            el_present: false,
            ..*source
        }
    } else {
        *source
    }
}

/// Configurations with a route: P5, P7 (dual layer, HDR10 base), P8.1
/// (HDR10 base) and P8.4 (HLG base), each with a base layer and an RPU.
pub fn supported(c: &DoviConfig) -> Result<(), String> {
    let known = matches!(
        (c.profile, c.bl_compat_id, c.el_present),
        (5, 0, false) | (7, 6, true) | (8, 1 | 4, false)
    );
    if !known {
        return Err(format!(
            "不支持的杜比视界配置 {}（兼容 ID {}{}），只支持 P5、P7、P8.1、P8.4",
            c.label(),
            c.bl_compat_id,
            if c.el_present { "，带增强层" } else { "" }
        ));
    }
    if !(c.rpu_present && c.bl_present) {
        return Err(format!("杜比视界 {} 缺少基础层或 RPU", c.label()));
    }
    Ok(())
}

/// The 24-byte decoder configuration record (ETSI GS CCM 001, the payload of
/// the dvcC/dvvC box and of Matroska's BlockAddIDExtraData).
pub fn config_record(c: &DoviConfig) -> [u8; 24] {
    let mut b = [0u8; 24];
    b[0] = c.version_major;
    b[1] = c.version_minor;
    let bits = (u16::from(c.profile & 0x7f) << 9)
        | (u16::from(c.level & 0x3f) << 3)
        | (u16::from(c.rpu_present) << 2)
        | (u16::from(c.el_present) << 1)
        | u16::from(c.bl_present);
    b[2..4].copy_from_slice(&bits.to_be_bytes());
    b[4] = ((c.bl_compat_id & 0x0f) << 4) | ((c.md_compression & 0x03) << 2);
    b
}

/// Box type of the record: `dvcC` up to profile 7, `dvvC` for 8–10.
pub fn config_fourcc(c: &DoviConfig) -> [u8; 4] {
    match c.profile {
        0..=7 => *b"dvcC",
        8..=10 => *b"dvvC",
        _ => *b"dvwC",
    }
}

/// RPUs of a dovi_tool `.bin` file, in display order. Each entry is the NAL
/// unit payload after the two-byte header (type 62), emulation prevention
/// included, as dovi_tool writes it after a 4-byte start code.
pub fn read_rpus(path: &Path) -> std::io::Result<Vec<Vec<u8>>> {
    Ok(split_annex_b(&std::fs::read(path)?))
}

/// NAL units of an Annex B file (FFmpeg's raw HEVC output), headers included.
pub fn read_nal_units(path: &Path) -> std::io::Result<Vec<Vec<u8>>> {
    Ok(split_annex_b(&std::fs::read(path)?))
}

fn split_annex_b(data: &[u8]) -> Vec<Vec<u8>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(n, &s)| {
            let end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
            let mut nal = &data[s..end];
            // The next 4-byte start code begins with a zero byte.
            while let [rest @ .., 0] = nal {
                nal = rest;
            }
            nal.to_vec()
        })
        .collect()
}

/// Enhancement layer type of a P7 source, from dovi_tool's summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum EnhancementLayer {
    /// Minimal: carries next to nothing; dropping it loses nothing visible.
    Mel,
    /// Full: a real residual; dropping it keeps only the base layer's picture.
    Fel,
}

/// `dovi_tool info --summary` of an RPU file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RpuSummary {
    pub frames: u64,
    pub profile: u8,
    pub el: Option<EnhancementLayer>,
}

fn parse_summary(text: &str) -> Option<RpuSummary> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(name).map(str::trim))
    };
    let frames = field("Frames:")?.parse().ok()?;
    let profile_line = field("Profile:")?;
    let profile = profile_line.split_whitespace().next()?.parse().ok()?;
    let el = if profile_line.contains("FEL") {
        Some(EnhancementLayer::Fel)
    } else if profile_line.contains("MEL") {
        Some(EnhancementLayer::Mel)
    } else {
        None
    };
    Some(RpuSummary {
        frames,
        profile,
        el,
    })
}

/// Intermediate files of a Dolby Vision route, removed when dropped.
#[derive(Debug, Default)]
pub struct WorkFiles(Vec<PathBuf>);

impl WorkFiles {
    /// Registers `path` for removal and returns it.
    pub fn add(&mut self, path: PathBuf) -> PathBuf {
        self.0.push(path.clone());
        path
    }
}

impl Drop for WorkFiles {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[derive(Debug, Clone)]
pub struct DoviTool {
    pub path: PathBuf,
}

impl DoviTool {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn command(&self) -> Command {
        Command::new(&self.path)
    }

    /// The version string, e.g. `2.3.4`, when the tool runs.
    pub fn check(&self) -> Result<String, String> {
        let out = tool::run_capture(self.command().arg("--version"), CHECK_TIMEOUT, None)
            .map_err(|e| e.to_string())?;
        out.split_whitespace()
            .nth(1)
            .map(str::to_owned)
            .ok_or_else(|| format!("无法识别 dovi_tool 的版本：{}", out.trim()))
    }

    /// Writes the RPUs of stream `video` of `input` (any container FFmpeg
    /// reads) to `out`, in display order. `to_p81` converts a P7 RPU to
    /// P8.1 (dovi_tool mode 2, which drops the enhancement layer's mapping).
    #[allow(clippy::too_many_arguments)]
    pub fn extract(
        &self,
        ffmpeg: &Path,
        input: &Path,
        video: u32,
        to_p81: bool,
        out: &Path,
        timeout: Duration,
        cancel: Option<&CancelToken>,
    ) -> Result<(), ToolError> {
        let mut demux = Command::new(ffmpeg);
        demux
            .args(["-hide_banner", "-nostdin", "-nostats", "-v", "error", "-i"])
            .arg(ffmpeg_input(input))
            .args(["-map", &format!("0:{video}"), "-c:v", "copy"])
            .args(["-bsf:v", "hevc_mp4toannexb", "-f", "hevc", "-"]);
        let mut extract = self.command();
        if to_p81 {
            extract.args(["-m", "2"]);
        }
        extract.args(["extract-rpu", "-", "-o"]).arg(out);
        let _ = std::fs::remove_file(out);
        tool::run_piped(&mut demux, &mut extract, timeout, cancel)?;
        if !out.is_file() {
            return Err(ToolError::Unexpected {
                program: self.path.display().to_string(),
                message: "没有写出 RPU 文件（片源里没有 RPU？）".into(),
            });
        }
        Ok(())
    }

    /// Converts the P7 RPUs in `input` to P8.1 (mode 2: the enhancement
    /// layer's mapping is dropped), writing `output`.
    pub fn to_p81(
        &self,
        input: &Path,
        output: &Path,
        cancel: Option<&CancelToken>,
    ) -> Result<(), ToolError> {
        let edit = output.with_extension("edit.json");
        std::fs::write(&edit, r#"{"mode":2}"#).map_err(|source| ToolError::Io {
            program: self.path.display().to_string(),
            source,
        })?;
        let mut cmd = self.command();
        cmd.args(["editor", "-i"])
            .arg(input)
            .arg("-j")
            .arg(&edit)
            .arg("-o")
            .arg(output);
        let result = tool::run_capture(&mut cmd, Duration::from_secs(600), cancel);
        let _ = std::fs::remove_file(&edit);
        result.map(drop)
    }

    pub fn summary(
        &self,
        rpu: &Path,
        cancel: Option<&CancelToken>,
    ) -> Result<RpuSummary, ToolError> {
        let mut cmd = self.command();
        cmd.args(["info", "--summary", "-i"]).arg(rpu);
        let out = tool::run_capture(&mut cmd, Duration::from_secs(600), cancel)?;
        parse_summary(&out).ok_or_else(|| ToolError::Unexpected {
            program: self.path.display().to_string(),
            message: format!("无法解析 RPU 摘要：{}", out.trim()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(profile: u8, compat: u8, el: bool) -> DoviConfig {
        DoviConfig {
            version_major: 1,
            version_minor: 0,
            profile,
            level: 6,
            rpu_present: true,
            el_present: el,
            bl_present: true,
            bl_compat_id: compat,
            md_compression: 0,
        }
    }

    #[test]
    fn record_matches_the_bit_layout() {
        // P8.1 level 6: profile 8 << 9 | 6 << 3 | rpu | bl = 0x1035.
        let r = config_record(&config(8, 1, false));
        assert_eq!(r[..5], [1, 0, 0x10, 0x35, 0x10]);
        assert!(r[5..].iter().all(|&b| b == 0));
        let p7 = config_record(&DoviConfig {
            md_compression: 1,
            ..config(7, 6, true)
        });
        assert_eq!(p7[..5], [1, 0, 0x0e, 0x37, 0x64]);
        assert_eq!(&config_fourcc(&config(5, 0, false)), b"dvcC");
        assert_eq!(&config_fourcc(&config(8, 4, false)), b"dvvC");
    }

    #[test]
    fn p7_becomes_p81_and_others_stay() {
        let out = output_config(&config(7, 6, true));
        assert_eq!(
            (out.profile, out.bl_compat_id, out.el_present, out.level),
            (8, 1, false, 6)
        );
        assert_eq!(output_config(&config(5, 0, false)), config(5, 0, false));
        for ok in [
            config(5, 0, false),
            config(7, 6, true),
            config(8, 1, false),
            config(8, 4, false),
        ] {
            assert!(supported(&ok).is_ok(), "{ok:?}");
        }
        for bad in [
            config(8, 2, false),
            config(7, 6, false),
            config(4, 2, true),
            config(10, 1, false),
        ] {
            assert!(supported(&bad).is_err(), "{bad:?}");
        }
        assert!(supported(&DoviConfig {
            rpu_present: false,
            ..config(8, 1, false)
        })
        .is_err());
    }

    #[test]
    fn rpu_file_splits_on_start_codes() {
        let data = [
            0, 0, 0, 1, 0x19, 8, 0, 0, 3, 1, 0x80, //
            0, 0, 0, 1, 0x19, 9, 0x80,
        ];
        assert_eq!(
            split_annex_b(&data),
            [vec![0x19, 8, 0, 0, 3, 1, 0x80], vec![0x19, 9, 0x80]]
        );
        assert!(split_annex_b(&[]).is_empty());
    }

    #[test]
    fn summary_is_parsed() {
        let text = "Parsing RPU file...\n\nSummary:\n  Frames: 48\n  Profile: 7 (FEL)\n  DM version: 1 (CM v2.9)\n";
        assert_eq!(
            parse_summary(text),
            Some(RpuSummary {
                frames: 48,
                profile: 7,
                el: Some(EnhancementLayer::Fel)
            })
        );
        let p8 = parse_summary("Summary:\n  Frames: 1\n  Profile: 8\n").unwrap();
        assert_eq!((p8.profile, p8.el), (8, None));
        assert_eq!(parse_summary("nothing"), None);
    }
}
