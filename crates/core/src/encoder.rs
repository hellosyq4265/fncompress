use serde::{Deserialize, Serialize};

/// Video encoders fncompress can drive. AV1 variants are only offered when the
/// startup probe proves they actually encode on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Encoder {
    Libx264,
    Libx265,
    H264Qsv,
    HevcQsv,
    H264Vaapi,
    HevcVaapi,
    Av1Qsv,
    Av1Vaapi,
    Libsvtav1,
}

/// Which concurrency quota a job consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Quota {
    Gpu,
    Cpu,
}

impl Encoder {
    pub const ALL: [Encoder; 9] = [
        Encoder::Libx264,
        Encoder::Libx265,
        Encoder::H264Qsv,
        Encoder::HevcQsv,
        Encoder::H264Vaapi,
        Encoder::HevcVaapi,
        Encoder::Av1Qsv,
        Encoder::Av1Vaapi,
        Encoder::Libsvtav1,
    ];

    /// Name passed to `ffmpeg -c:v`.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Encoder::Libx264 => "libx264",
            Encoder::Libx265 => "libx265",
            Encoder::H264Qsv => "h264_qsv",
            Encoder::HevcQsv => "hevc_qsv",
            Encoder::H264Vaapi => "h264_vaapi",
            Encoder::HevcVaapi => "hevc_vaapi",
            Encoder::Av1Qsv => "av1_qsv",
            Encoder::Av1Vaapi => "av1_vaapi",
            Encoder::Libsvtav1 => "libsvtav1",
        }
    }

    pub fn quota(self) -> Quota {
        match self {
            Encoder::Libx264 | Encoder::Libx265 | Encoder::Libsvtav1 => Quota::Cpu,
            _ => Quota::Gpu,
        }
    }

    /// Presets the encoder takes, fastest first; empty for VAAPI, which is
    /// not given one. The web profile editor keeps the same lists.
    pub fn presets(self) -> &'static [&'static str] {
        match self {
            Encoder::Libx264 | Encoder::Libx265 => &[
                "ultrafast",
                "superfast",
                "veryfast",
                "faster",
                "fast",
                "medium",
                "slow",
                "slower",
                "veryslow",
            ],
            Encoder::H264Qsv | Encoder::HevcQsv | Encoder::Av1Qsv => &[
                "veryfast", "faster", "fast", "medium", "slow", "slower", "veryslow",
            ],
            Encoder::Libsvtav1 => &[
                "13", "12", "11", "10", "9", "8", "7", "6", "5", "4", "3", "2", "1", "0",
            ],
            Encoder::H264Vaapi | Encoder::HevcVaapi | Encoder::Av1Vaapi => &[],
        }
    }

    /// Presets a 样片推荐 compares unless told otherwise, fastest first:
    /// a spread that leaves out the slowest ones, which take hours on a NAS.
    pub fn compared_presets(self) -> &'static [&'static str] {
        match self {
            Encoder::Libx264 | Encoder::Libx265 => &["fast", "medium", "slow"],
            Encoder::H264Qsv | Encoder::HevcQsv | Encoder::Av1Qsv => {
                &["veryfast", "medium", "slow", "veryslow"]
            }
            Encoder::Libsvtav1 => &["10", "8", "6"],
            Encoder::H264Vaapi | Encoder::HevcVaapi | Encoder::Av1Vaapi => &[],
        }
    }

    pub fn is_av1(self) -> bool {
        matches!(
            self,
            Encoder::Av1Qsv | Encoder::Av1Vaapi | Encoder::Libsvtav1
        )
    }

    pub fn is_hevc(self) -> bool {
        matches!(
            self,
            Encoder::Libx265 | Encoder::HevcQsv | Encoder::HevcVaapi
        )
    }

    /// Encoders whose 10-bit output carries HDR10/HLG: color description and
    /// static metadata (mastering display, content light) from the decoded
    /// frames. H.264 is left out; its HDR support in players is poor.
    pub fn keeps_hdr(self) -> bool {
        self.is_hevc() || self.is_av1()
    }

    pub fn hw(self) -> Option<Hw> {
        match self {
            Encoder::H264Qsv | Encoder::HevcQsv | Encoder::Av1Qsv => Some(Hw::Qsv),
            Encoder::H264Vaapi | Encoder::HevcVaapi | Encoder::Av1Vaapi => Some(Hw::Vaapi),
            _ => None,
        }
    }

    /// Global options that open the render device, placed before `-i`.
    pub fn device_args(self, render_device: &str) -> Vec<String> {
        match self.hw() {
            Some(Hw::Qsv) => vec!["-init_hw_device".into(), format!("qsv=hw:{render_device}")],
            Some(Hw::Vaapi) => vec!["-vaapi_device".into(), render_device.into()],
            None => vec![],
        }
    }

    /// Global and input options, in place of [`Self::device_args`] before
    /// the source's `-i`, that decode the source on the GPU through VAAPI
    /// and keep the frames there (zero copy). QSV maps them onto a QSV
    /// device derived from the same VAAPI one. None for software encoders:
    /// they stay off the GPU.
    pub fn gpu_decode_args(self, render_device: &str) -> Vec<String> {
        let decode = |device: &str| {
            ["-hwaccel", "vaapi", "-hwaccel_device", device]
                .into_iter()
                .chain(["-hwaccel_output_format", "vaapi"])
                .map(String::from)
                .collect::<Vec<_>>()
        };
        match self.hw() {
            Some(Hw::Qsv) => [
                "-init_hw_device".into(),
                format!("vaapi=va:{render_device}"),
                "-init_hw_device".into(),
                "qsv=qs@va".into(),
            ]
            .into_iter()
            .chain(decode("va"))
            .collect(),
            Some(Hw::Vaapi) => ["-vaapi_device".into(), render_device.into()]
                .into_iter()
                .chain(decode(render_device))
                .collect(),
            None => vec![],
        }
    }

    /// Video filters for GPU-decoded frames ([`Self::gpu_decode_args`]):
    /// scale (when `size` is given) and convert to `sw_format` on the GPU,
    /// then hand the frames to the encoder.
    pub fn gpu_filter(self, size: Option<(u32, u32)>, sw_format: &str) -> String {
        let scale = match size {
            Some((w, h)) => format!("scale_vaapi=w={w}:h={h}:format={sw_format}"),
            None => format!("scale_vaapi=format={sw_format}"),
        };
        match self.hw() {
            Some(Hw::Qsv) => format!("{scale},hwmap=derive_device=qsv,format=qsv"),
            _ => scale,
        }
    }

    /// Video filter tail that hands software frames in `sw_format` to the
    /// encoder. QSV takes system-memory frames directly; VAAPI needs an upload.
    pub fn upload_filter(self, sw_format: &str) -> String {
        match self.hw() {
            Some(Hw::Vaapi) => format!("format={sw_format},hwupload"),
            _ => format!("format={sw_format}"),
        }
    }
}

impl std::str::FromStr for Encoder {
    type Err = String;

    /// Parses the FFmpeg name, e.g. `hevc_qsv`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Encoder::ALL
            .into_iter()
            .find(|e| e.ffmpeg_name() == s)
            .ok_or_else(|| format!("未知编码器 {s}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hw {
    Qsv,
    Vaapi,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_encoders_use_gpu_quota() {
        for enc in Encoder::ALL {
            let hw = enc.ffmpeg_name().ends_with("_qsv") || enc.ffmpeg_name().ends_with("_vaapi");
            assert_eq!(enc.quota() == Quota::Gpu, hw, "{enc:?}");
        }
    }

    #[test]
    fn compared_presets_are_offered_and_include_the_default() {
        for enc in Encoder::ALL {
            let all = enc.presets();
            let compared = enc.compared_presets();
            assert!(compared.iter().all(|p| all.contains(p)), "{enc:?}");
            let at = |p: &&str| all.iter().position(|a| a == p).unwrap();
            assert!(
                compared.windows(2).all(|w| at(&w[0]) < at(&w[1])),
                "{enc:?}"
            );
            let default = crate::settings::Profile {
                encoder: enc,
                ..Default::default()
            }
            .preset_or_default();
            assert_eq!(
                default.as_deref().is_some_and(|d| compared.contains(&d)),
                !all.is_empty(),
                "{enc:?}"
            );
        }
    }
}
