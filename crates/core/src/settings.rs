//! Encoding profile and global settings. A task runs against a frozen
//! [`SettingsSnapshot`] taken when it was queued (ADR 0003); every threshold
//! from ADR 0004 lives here so it can be tuned without a rebuild.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::encoder::Encoder;

/// Bumped when the command builder changes what it produces for the same
/// profile, so files that failed under the old commands may be retried.
/// 2: M5 opens the HDR10/HLG and Dolby Vision routes.
/// 3: M5 opens HDR10+; Dolby Vision muxes keep every stream's tags.
/// 4: QSV into Matroska clamps the dts jump at the end of a stream.
/// 5: Into Matroska, the lead-in an MP4 edit list hides is dropped.
/// 6: MP4/MOV chapter tracks are left out and their chapters copied as
///    chapters; such files were skipped before.
pub const COMMAND_REVISION: u32 = 6;

/// Output container choice of a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ContainerChoice {
    /// Same container and extension as the source.
    #[default]
    Keep,
    Mkv,
    Mp4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Resize {
    #[default]
    Keep,
    /// Scale down (never up) to this height, keeping the aspect ratio.
    MaxHeight(u32),
}

/// Encoding profile (编码方案).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    /// Written into the processed marker.
    pub id: String,
    pub encoder: Encoder,
    /// CRF for x264/x265/SVT-AV1, `global_quality` for QSV, QP for VAAPI.
    pub quality: u8,
    /// Encoder preset; `None` uses the encoder's default from [`Profile::preset_or_default`].
    pub preset: Option<String>,
    pub container: ContainerChoice,
    pub resize: Resize,
    /// Encoder thread limit; `None` lets FFmpeg decide. Does not affect output.
    pub threads: Option<u32>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            id: "default".into(),
            encoder: Encoder::Libx265,
            quality: 24,
            preset: None,
            container: ContainerChoice::Keep,
            resize: Resize::Keep,
            threads: None,
        }
    }
}

impl Profile {
    pub fn preset_or_default(&self) -> Option<String> {
        if let Some(p) = &self.preset {
            return Some(p.clone());
        }
        match self.encoder {
            Encoder::Libx264
            | Encoder::Libx265
            | Encoder::H264Qsv
            | Encoder::HevcQsv
            | Encoder::Av1Qsv => Some("medium".into()),
            Encoder::Libsvtav1 => Some("8".into()),
            Encoder::H264Vaapi | Encoder::HevcVaapi | Encoder::Av1Vaapi => None,
        }
    }

    /// Settings fingerprint (设置指纹): a stable hash of the fields that change
    /// the output. The id and thread count do not.
    pub fn fingerprint(&self) -> String {
        let canonical = serde_json::json!({
            "rev": COMMAND_REVISION,
            "encoder": self.encoder,
            "quality": self.quality,
            "preset": self.preset_or_default(),
            "container": self.container,
            "resize": self.resize,
        });
        format!("{:016x}", fnv1a64(canonical.to_string().as_bytes()))
    }
}

/// Verification settings of ADR 0004. Timing thresholds come from
/// docs/research/duration-tolerance.md.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct VerifyThresholds {
    /// Change in audio-to-video start offset above which a warning is raised.
    pub av_offset_soft_ms: f64,
    /// ... and above which the candidate is rejected.
    pub av_offset_hard_ms: f64,
    /// Change in any stream's end time above which a warning is raised.
    pub end_soft_ms: f64,
    /// ... and above which the candidate is rejected.
    pub end_hard_ms: f64,
    /// Decode the whole candidate video and reject on any decoder error.
    pub decode_check: bool,
    /// Try VAAPI decoding first for the decode check (falls back to software).
    pub hw_decode: bool,
    /// Picture spot checks: this many evenly spaced points, skipping the
    /// first and last 5%; 0 disables them.
    pub spot_checks: u32,
    /// Length of each spot check, seconds.
    pub spot_check_secs: f64,
    /// Spot-check SSIM below which the candidate is rejected (severe damage only).
    pub ssim_hard: f64,
    /// ... and below which a warning is raised.
    pub ssim_soft: f64,
    /// Hash every copied stream in source and candidate and require equality.
    /// Only when both use the same container; reads both files in full.
    pub copied_stream_hash: bool,
}

impl Default for VerifyThresholds {
    fn default() -> Self {
        Self {
            av_offset_soft_ms: 15.0,
            av_offset_hard_ms: 45.0,
            end_soft_ms: 50.0,
            end_hard_ms: 1000.0,
            decode_check: true,
            hw_decode: true,
            spot_checks: 8,
            spot_check_secs: 2.0,
            ssim_hard: 0.80,
            ssim_soft: 0.90,
            copied_stream_hash: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct GlobalSettings {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub ffmpeg: PathBuf,
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub ffprobe: PathBuf,
    /// FFmpeg with libvmaf for 样片推荐 scoring (the scoring runtime, ADR
    /// 0006); `None` uses `ffmpeg`. Libraries next to it are put on the
    /// library path.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub scoring_ffmpeg: Option<PathBuf>,
    /// dovi_tool for Dolby Vision routes (ADR 0011); `None` looks next to
    /// `scoring_ffmpeg` (the scoring runtime ships one), then on `PATH`.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub dovi_tool: Option<PathBuf>,
    /// Render node for QSV and VAAPI.
    pub render_device: String,
    /// QSV and VAAPI transcodes decode the source on the GPU first; a
    /// failed or erroring run is redone with software decoding.
    pub hw_decode: bool,
    /// Below this saving the candidate is "insufficient gain" (收益不足).
    pub min_savings_pct: f64,
    /// `None`: `.fncompress-recycle` next to each source, which keeps the
    /// recycle on the source's filesystem.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub recycle_dir: Option<PathBuf>,
    pub recycle_retention_days: u32,
    pub pending_retention_days: u32,
    /// Encode timeout = source duration x factor + 30 minutes.
    pub encode_timeout_factor: f64,
    pub verify: VerifyThresholds,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".into(),
            ffprobe: "ffprobe".into(),
            scoring_ffmpeg: None,
            dovi_tool: None,
            render_device: "/dev/dri/renderD128".into(),
            hw_decode: true,
            min_savings_pct: 5.0,
            recycle_dir: None,
            recycle_retention_days: 14,
            pending_retention_days: 14,
            encode_timeout_factor: 30.0,
            verify: VerifyThresholds::default(),
        }
    }
}

impl GlobalSettings {
    /// The dovi_tool to run: the setting, else the one shipped next to the
    /// scoring FFmpeg, else `dovi_tool` from `PATH`.
    pub fn dovi_tool_path(&self) -> PathBuf {
        if let Some(p) = &self.dovi_tool {
            return p.clone();
        }
        let name = format!("dovi_tool{}", std::env::consts::EXE_SUFFIX);
        self.scoring_ffmpeg
            .as_ref()
            .map(|f| f.with_file_name(&name))
            .filter(|p| p.is_file())
            .unwrap_or_else(|| name.into())
    }
}

/// What a task runs against: the profile and global settings frozen at enqueue.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(default, deny_unknown_fields)]
pub struct SettingsSnapshot {
    pub profile: Profile,
    pub global: GlobalSettings,
}

impl SettingsSnapshot {
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_fills_defaults_and_rejects_unknown_keys() {
        let s = SettingsSnapshot::from_toml(
            r#"
            [profile]
            id = "movies"
            encoder = "hevc_qsv"
            quality = 22
            container = "mkv"
            resize = { max_height = 1080 }

            [global]
            min_savings_pct = 8
            [global.verify]
            av_offset_hard_ms = 60
            "#,
        )
        .unwrap();
        assert_eq!(s.profile.encoder, Encoder::HevcQsv);
        assert_eq!(s.profile.resize, Resize::MaxHeight(1080));
        assert_eq!(s.profile.container, ContainerChoice::Mkv);
        assert_eq!(s.global.min_savings_pct, 8.0);
        assert_eq!(s.global.verify.av_offset_hard_ms, 60.0);
        assert_eq!(s.global.verify.av_offset_soft_ms, 15.0);
        assert_eq!(s.global.recycle_retention_days, 14);

        assert!(SettingsSnapshot::from_toml("[profile]\ncrf = 20\n").is_err());
        assert_eq!(
            SettingsSnapshot::from_toml("").unwrap(),
            SettingsSnapshot::default()
        );
    }

    #[test]
    fn fingerprint_tracks_output_affecting_fields_only() {
        let base = Profile::default();
        let fp = base.fingerprint();
        assert_eq!(fp.len(), 16);

        let renamed = Profile {
            id: "other".into(),
            threads: Some(4),
            ..base.clone()
        };
        assert_eq!(renamed.fingerprint(), fp);

        // Spelling out the default preset is the same profile.
        let explicit = Profile {
            preset: Some("medium".into()),
            ..base.clone()
        };
        assert_eq!(explicit.fingerprint(), fp);

        for changed in [
            Profile {
                quality: 23,
                ..base.clone()
            },
            Profile {
                encoder: Encoder::HevcQsv,
                ..base.clone()
            },
            Profile {
                container: ContainerChoice::Mp4,
                ..base.clone()
            },
            Profile {
                resize: Resize::MaxHeight(720),
                ..base.clone()
            },
        ] {
            assert_ne!(changed.fingerprint(), fp, "{changed:?}");
        }
    }

    #[test]
    fn fingerprint_is_stable_across_builds() {
        // Changing this value invalidates every attempt record; bump
        // COMMAND_REVISION instead when that is intended.
        assert_eq!(Profile::default().fingerprint(), FINGERPRINT_OF_DEFAULT);
    }

    const FINGERPRINT_OF_DEFAULT: &str = "fce7d09c15c396e0";
}
