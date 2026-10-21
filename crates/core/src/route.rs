//! Route decision before encoding: either a [`RoutePlan`] the command builder
//! follows, or a protective skip (保护性跳过) with a reason the UI can show
//! when the transcode is created. Unsupported routes are never encoded and
//! then rejected (ADR 0004).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    dovi,
    encoder::Encoder,
    probe::{Chroma, DoviConfig, DynamicRange, MediaInfo, StreamInfo, StreamKind},
    settings::{ContainerChoice, Profile, Resize},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Container {
    Mkv,
    Mp4,
}

impl Container {
    /// FFmpeg muxer name for `-f`.
    pub fn muxer(self) -> &'static str {
        match self {
            Container::Mkv => "matroska",
            Container::Mp4 => "mp4",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Container::Mkv => "mkv",
            Container::Mp4 => "mp4",
        }
    }

    fn of_source(info: &MediaInfo, path: &Path) -> Option<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        let format = info.format.format_name.as_str();
        match ext.as_deref() {
            Some("mkv") if format.starts_with("matroska") => Some(Container::Mkv),
            Some("mp4" | "m4v") if format.starts_with("mov,mp4") => Some(Container::Mp4),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum SkipCode {
    AlreadyProcessed,
    NoVideo,
    /// Dolby Vision source the route cannot carry (configuration, codec,
    /// downscaling).
    DolbyVision,
    DolbyVisionAv1,
    /// Dolby Vision source with a non-HEVC encoder.
    DolbyVisionEncoder,
    /// dovi_tool, which Dolby Vision routes need, does not run.
    DoviToolUnavailable,
    /// Dynamic metadata other than HDR10+, or HDR10+ the route cannot carry
    /// (not HEVC, not PQ).
    DynamicHdr,
    /// HDR10+ source with a non-HEVC encoder.
    Hdr10PlusEncoder,
    /// HDR source the route cannot carry (e.g. not 10-bit).
    Hdr,
    /// HDR source with an encoder that cannot keep HDR (H.264).
    HdrEncoder,
    Interlaced,
    UnsupportedPixelFormat,
    EncoderCannotKeepFormat,
    EncoderUnavailable,
    SourceContainerUnsupported,
    StreamNotSupportedInContainer,
    TargetExists,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[error("{message}")]
pub struct SkipReason {
    pub code: SkipCode,
    pub message: String,
}

fn skip(code: SkipCode, message: impl Into<String>) -> SkipReason {
    SkipReason {
        code,
        message: message.into(),
    }
}

/// A track property the target container cannot store (换封装预期丢失).
/// Verification compares track properties against the source minus these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ExpectedLoss {
    pub stream: u32,
    /// `disposition:<flag>`.
    pub property: String,
    pub reason: String,
}

/// A copied stream whose hidden lead-in is cut down: Matroska has no edit
/// list, so the packets an MP4 hides before the first shown one would play.
/// The first `packets` are dropped; the last hidden one stays, so the first
/// shown audio frame still decodes from its predecessor (ADR 0012).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LeadInTrim {
    pub stream: u32,
    pub packets: u64,
}

impl ExpectedLoss {
    /// The disposition flag this loss removes, if it is one.
    pub fn disposition(&self) -> Option<&str> {
        self.property.strip_prefix("disposition:")
    }
}

/// Disposition flags each container stores, measured with FFmpeg 8.1
/// (MKV <-> MP4 copies of every flag on audio and subtitle tracks). Flags
/// outside these lists are lost when converting into that container.
/// `attached_pic` is structural and travels with the stream kind.
fn container_keeps(container: Container, kind: StreamKind, flag: &str) -> bool {
    match container {
        Container::Mkv => match flag {
            "default" | "forced" | "hearing_impaired" | "visual_impaired" | "original"
            | "comment" | "dub" | "attached_pic" => true,
            "descriptions" => kind == StreamKind::Subtitle,
            _ => false,
        },
        Container::Mp4 => matches!(
            flag,
            "default"
                | "forced"
                | "hearing_impaired"
                | "captions"
                | "visual_impaired"
                | "descriptions"
                | "comment"
                | "dub"
                | "attached_pic"
        ),
    }
}

fn expected_losses(
    info: &MediaInfo,
    source: Option<Container>,
    target: Container,
) -> Vec<ExpectedLoss> {
    if source == Some(target) {
        return vec![];
    }
    let name = target.extension().to_ascii_uppercase();
    let mut losses = Vec::new();
    for s in &info.streams {
        for flag in &s.disposition {
            if !container_keeps(target, s.kind, flag) {
                losses.push(ExpectedLoss {
                    stream: s.index,
                    property: format!("disposition:{flag}"),
                    reason: format!("{name} 无法写入第 {} 条流的 {flag} 标志", s.index),
                });
            }
        }
    }
    losses
}

/// How the plan carries the source's dynamic range (显示元数据). HDR routes
/// encode 10-bit, state the color description and keep the static metadata
/// the decoder attaches to every frame; Dolby Vision routes also carry the
/// RPU over (see [`crate::dovi`]), HDR10+ routes each frame's HDR10+ message
/// (see [`crate::hdr10plus`]). Verification checks all of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RangeRoute {
    #[default]
    Sdr,
    Hdr10,
    /// HDR10 with HDR10+ dynamic metadata on every frame.
    Hdr10Plus,
    Hlg,
    DolbyVision {
        source: DoviConfig,
        /// What the candidate carries: the source's configuration, or P8.1
        /// for a P7 source (enhancement layer dropped).
        output: DoviConfig,
        /// The source also carries HDR10+, which is kept too.
        #[serde(default)]
        hdr10_plus: bool,
    },
}

impl RangeRoute {
    pub fn dolby_vision(&self) -> Option<(&DoviConfig, &DoviConfig)> {
        match self {
            RangeRoute::DolbyVision { source, output, .. } => Some((source, output)),
            _ => None,
        }
    }

    /// Whether the candidate carries the source's HDR10+ messages.
    pub fn hdr10_plus(&self) -> bool {
        matches!(
            self,
            RangeRoute::Hdr10Plus
                | RangeRoute::DolbyVision {
                    hdr10_plus: true,
                    ..
                }
        )
    }

    /// Whether metadata is put back into the re-encoded video (an
    /// intermediate file, then the final mux).
    pub fn injects(&self) -> bool {
        self.hdr10_plus() || self.dolby_vision().is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RoutePlan {
    pub encoder: Encoder,
    /// Stream index of the video that is re-encoded; everything else is copied.
    pub video_index: u32,
    pub container: Container,
    /// Where publication puts the result: the source path, or the source
    /// path with the new extension when the container changes.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub target: PathBuf,
    /// Software pixel format the encoder is fed.
    pub pix_fmt: String,
    pub full_range: bool,
    /// Plans stored before M5 are SDR.
    #[serde(default)]
    pub dynamic_range: RangeRoute,
    /// Output picture size; differs from the source only when downscaling.
    pub width: u32,
    pub height: u32,
    /// The container changes (MKV <-> MP4, or TS etc. into either).
    pub container_changed: bool,
    /// Track properties the target container cannot store.
    pub expected_losses: Vec<ExpectedLoss>,
    /// QuickTime chapter tracks left out of the mapping; their chapters are
    /// carried as container chapters. Always the last streams.
    #[serde(default)]
    pub chapter_tracks: Vec<u32>,
    /// Things the user should know that do not stop the transcode.
    pub warnings: Vec<String>,
    /// Hidden lead-ins the output cannot hide; plans stored before M6 have none.
    #[serde(default)]
    pub lead_in_trims: Vec<LeadInTrim>,
}

impl RoutePlan {
    pub fn scales(&self, info: &MediaInfo) -> bool {
        let v = info
            .stream(self.video_index)
            .and_then(|s| s.video.as_ref())
            .expect("plan refers to a video stream");
        (v.width, v.height) != (self.width, self.height)
    }
}

#[derive(Debug, Clone, Default)]
pub struct RouteOptions<'a> {
    /// Manual transcode of a file that already carries our marker.
    pub force: bool,
    /// Encoders the capability probe found working; `None` skips the check.
    pub available: Option<&'a [Encoder]>,
    /// Why dovi_tool does not run, when the startup check found so.
    pub dovi_unavailable: Option<&'a str>,
}

pub fn plan(
    info: &MediaInfo,
    source: &Path,
    profile: &Profile,
    opts: &RouteOptions,
) -> Result<RoutePlan, SkipReason> {
    let encoder = profile.encoder;
    let mut warnings = Vec::new();

    if let Some(marker) = &info.marker {
        if !opts.force {
            return Err(skip(
                SkipCode::AlreadyProcessed,
                format!("已被 fncompress 处理过（方案 {}）", marker.profile),
            ));
        }
        warnings.push(format!(
            "源片已带已处理标记（方案 {}），按手动要求强制处理",
            marker.profile
        ));
    }
    if let Some(available) = opts.available {
        if !available.contains(&encoder) {
            return Err(skip(
                SkipCode::EncoderUnavailable,
                format!("{} 在本机试编码未通过", encoder.ffmpeg_name()),
            ));
        }
    }

    let stream = info
        .primary_video_stream()
        .ok_or_else(|| skip(SkipCode::NoVideo, "没有可压缩的视频流"))?;
    let video = stream
        .video
        .as_ref()
        .expect("video stream has video params");

    let dynamic_range = match info.dynamic_range() {
        DynamicRange::DolbyVision if encoder.is_av1() => {
            return Err(skip(
                SkipCode::DolbyVisionAv1,
                "杜比视界源片不能用 AV1 方案（无法保留 RPU）",
            ))
        }
        DynamicRange::DolbyVision => dolby_vision_route(info, stream, encoder, opts)?,
        DynamicRange::DynamicHdr => {
            only_hdr10_plus(info, "")?;
            hdr10_plus_route(info, stream, encoder)?
        }
        DynamicRange::Hdr10 | DynamicRange::Hlg if !encoder.keeps_hdr() => {
            return Err(skip(
                SkipCode::HdrEncoder,
                format!(
                    "HDR 源片请用 HEVC 或 AV1 方案（{} 不能可靠保留 HDR）",
                    encoder.ffmpeg_name()
                ),
            ))
        }
        DynamicRange::Hdr10 => RangeRoute::Hdr10,
        DynamicRange::Hlg => RangeRoute::Hlg,
        DynamicRange::Sdr => RangeRoute::Sdr,
    };
    if video.is_interlaced() {
        return Err(skip(SkipCode::Interlaced, "隔行扫描源片暂不支持"));
    }

    let (depth, chroma) = match (video.bit_depth, video.chroma) {
        (Some(d @ (8 | 10)), Some(c @ (Chroma::Yuv420 | Chroma::Yuv422 | Chroma::Yuv444))) => {
            (d, c)
        }
        _ => {
            return Err(skip(
                SkipCode::UnsupportedPixelFormat,
                format!(
                    "像素格式 {} 暂不支持",
                    video.pix_fmt.as_deref().unwrap_or("未知")
                ),
            ))
        }
    };
    if dynamic_range != RangeRoute::Sdr && depth != 10 {
        return Err(skip(
            SkipCode::Hdr,
            format!("HDR 源片只支持 10-bit（这个是 {depth}-bit）"),
        ));
    }
    if dynamic_range.dolby_vision().is_some() && chroma != Chroma::Yuv420 {
        return Err(skip(
            SkipCode::DolbyVision,
            format!("杜比视界源片应为 4:2:0（这个是 {}）", chroma_name(chroma)),
        ));
    }
    let pix_fmt = encoder_pix_fmt(encoder, depth, chroma).ok_or_else(|| {
        skip(
            SkipCode::EncoderCannotKeepFormat,
            format!(
                "{} 无法保持 {depth}-bit {} 的源片格式",
                encoder.ffmpeg_name(),
                chroma_name(chroma)
            ),
        )
    })?;
    let full_range = video.color.range.as_deref() == Some("pc")
        || video
            .pix_fmt
            .as_deref()
            .is_some_and(|p| p.starts_with("yuvj"));

    let source_container = Container::of_source(info, source);
    let container = match profile.container {
        ContainerChoice::Keep => source_container.ok_or_else(|| {
            skip(
                SkipCode::SourceContainerUnsupported,
                format!(
                    "源片封装（{}）不能原样保持，请选择换封装为 MKV 或 MP4",
                    info.format.format_name
                ),
            )
        })?,
        ContainerChoice::Mkv => Container::Mkv,
        ContainerChoice::Mp4 => Container::Mp4,
    };
    let chapter_tracks = chapter_tracks(info)?;
    for s in &info.streams {
        if s.index != stream.index && !chapter_tracks.contains(&s.index) {
            check_carriable(s, container)?;
        }
    }

    let target = if source_container == Some(container) {
        source.to_owned()
    } else {
        source.with_extension(container.extension())
    };
    if target != source && target.exists() {
        return Err(skip(
            SkipCode::TargetExists,
            format!("目标文件已存在：{}", target.display()),
        ));
    }

    if info
        .frame_side_data
        .iter()
        .any(|t| t.contains("User Data Unregistered"))
    {
        warnings.push("源片带编码器 SEI（如 x264 参数），重编码后不保留".into());
    }

    let expected_losses = expected_losses(info, source_container, container);
    warnings.extend(
        expected_losses
            .iter()
            .map(|l| format!("换封装后：{}", l.reason)),
    );

    let lead_in_trims = lead_in_trims(info, stream.index, container);
    warnings.extend(lead_in_trims.iter().map(|t| {
        format!(
            "第 {} 条流开头有 {} 个被 MP4 编辑列表藏起来的包，MKV 藏不住，舍弃",
            t.stream, t.packets
        )
    }));

    let (width, height) = output_size(video.width, video.height, profile.resize);
    if let Some((source, _)) = dynamic_range.dolby_vision() {
        if (width, height) != (video.width, video.height) {
            return Err(skip(
                SkipCode::DolbyVision,
                format!(
                    "杜比视界源片不能缩小分辨率（RPU 与 {}×{} 绑定），请用保持分辨率的方案",
                    video.width, video.height
                ),
            ));
        }
        if source.profile == 7 {
            warnings.push(
                "杜比视界 P7 转为 P8.1：舍弃增强层，保留 RPU；FEL 片源的增强层细节会丢失".into(),
            );
        }
    }
    Ok(RoutePlan {
        encoder,
        video_index: stream.index,
        container,
        target,
        pix_fmt: pix_fmt.into(),
        full_range,
        dynamic_range,
        width,
        height,
        container_changed: source_container != Some(container),
        expected_losses,
        chapter_tracks,
        warnings,
        lead_in_trims,
    })
}

/// Into Matroska, every copied stream that hides more than one packet at
/// its start. One is the AAC priming of any MP4 and stays as it always has.
fn lead_in_trims(info: &MediaInfo, video: u32, container: Container) -> Vec<LeadInTrim> {
    if container != Container::Mkv {
        return Vec::new();
    }
    info.streams
        .iter()
        .filter(|s| s.index != video)
        .filter_map(|s| {
            let hidden = s.packets.as_ref()?.lead_in?.packets;
            (hidden > 1).then_some(LeadInTrim {
                stream: s.index,
                packets: hidden - 1,
            })
        })
        .collect()
}

/// QuickTime chapter tracks, which are not copied: the chapters go through
/// `-map_chapters`, into Matroska chapters or the chapter track the MP4
/// muxer writes itself (ADR 0013). Copied as a stream, the track fails the
/// Matroska muxer and becomes a stray `gpmd` track in MP4.
///
/// The command, inspection and verification address copied streams by their
/// source index, which the candidate keeps only when no stream before them
/// is dropped; a chapter track before another stream is skipped.
fn chapter_tracks(info: &MediaInfo) -> Result<Vec<u32>, SkipReason> {
    let tracks: Vec<u32> = info
        .streams
        .iter()
        .filter(|s| info.is_chapter_track(s))
        .map(|s| s.index)
        .collect();
    let first = tracks.first().copied();
    let later = first.and_then(|first| {
        info.streams
            .iter()
            .find(|s| s.index > first && !tracks.contains(&s.index))
    });
    if let (Some(first), Some(later)) = (first, later) {
        return Err(skip(
            SkipCode::StreamNotSupportedInContainer,
            format!(
                "章节文本轨（第 {first} 条流）排在第 {} 条流之前，暂不支持",
                later.index
            ),
        ));
    }
    Ok(tracks)
}

/// Dynamic metadata other than HDR10+ has no route. `what` names the source
/// in the message ("杜比视界源片同时" or nothing).
fn only_hdr10_plus(info: &MediaInfo, what: &str) -> Result<(), SkipReason> {
    let other: Vec<_> = info
        .hdr
        .dynamic_metadata
        .iter()
        .filter(|t| !(t.contains("SMPTE2094-40") || t.contains("HDR10+")))
        .cloned()
        .collect();
    if other.is_empty() {
        return Ok(());
    }
    Err(skip(
        SkipCode::DynamicHdr,
        format!("{what}带动态 HDR 元数据（{}），暂不支持", other.join("、")),
    ))
}

/// HDR10+ messages travel as HEVC SEI: HEVC sources and encoders only.
fn hdr10_plus_route(
    info: &MediaInfo,
    stream: &StreamInfo,
    encoder: Encoder,
) -> Result<RangeRoute, SkipReason> {
    let transfer = stream
        .video
        .as_ref()
        .and_then(|v| v.color.transfer.as_deref());
    if transfer != Some("smpte2084") {
        return Err(skip(
            SkipCode::DynamicHdr,
            format!(
                "HDR10+ 源片的传递特性应为 PQ（这个是 {}）",
                transfer.unwrap_or("未标注")
            ),
        ));
    }
    if stream.codec_name != "hevc" {
        return Err(skip(
            SkipCode::DynamicHdr,
            format!(
                "只支持 HEVC 编码的 HDR10+ 源片（这个是 {}）",
                stream.codec_name
            ),
        ));
    }
    if !encoder.is_hevc() {
        return Err(skip(
            SkipCode::Hdr10PlusEncoder,
            format!(
                "HDR10+ 源片请用 HEVC 方案（x265、hevc_qsv、hevc_vaapi），{} 无法保留 HDR10+",
                encoder.ffmpeg_name()
            ),
        ));
    }
    debug_assert!(info.hdr.has_hdr10_plus());
    Ok(RangeRoute::Hdr10Plus)
}

fn dolby_vision_route(
    info: &MediaInfo,
    stream: &StreamInfo,
    encoder: Encoder,
    opts: &RouteOptions,
) -> Result<RangeRoute, SkipReason> {
    only_hdr10_plus(info, "杜比视界源片同时")?;
    let source = info
        .hdr
        .dolby_vision
        .ok_or_else(|| skip(SkipCode::DolbyVision, "源片有杜比视界 RPU，但没有配置记录"))?;
    dovi::supported(&source).map_err(|m| skip(SkipCode::DolbyVision, m))?;
    if stream.codec_name != "hevc" {
        return Err(skip(
            SkipCode::DolbyVision,
            format!(
                "只支持 HEVC 编码的杜比视界源片（这个是 {}）",
                stream.codec_name
            ),
        ));
    }
    if !encoder.is_hevc() {
        return Err(skip(
            SkipCode::DolbyVisionEncoder,
            format!(
                "杜比视界源片请用 HEVC 方案（x265、hevc_qsv、hevc_vaapi），{} 无法保留 RPU",
                encoder.ffmpeg_name()
            ),
        ));
    }
    if let Some(why) = opts.dovi_unavailable {
        return Err(skip(
            SkipCode::DoviToolUnavailable,
            format!("dovi_tool 不可用：{why}"),
        ));
    }
    Ok(RangeRoute::DolbyVision {
        source,
        output: dovi::output_config(&source),
        hdr10_plus: info.hdr.has_hdr10_plus(),
    })
}

/// Software format fed to the encoder that keeps the source's depth and
/// chroma, or `None` when the encoder cannot.
fn encoder_pix_fmt(encoder: Encoder, depth: u8, chroma: Chroma) -> Option<&'static str> {
    use Chroma::*;
    match encoder {
        Encoder::Libx264 | Encoder::Libx265 => Some(match (depth, chroma) {
            (8, Yuv420) => "yuv420p",
            (8, Yuv422) => "yuv422p",
            (8, Yuv444) => "yuv444p",
            (10, Yuv420) => "yuv420p10le",
            (10, Yuv422) => "yuv422p10le",
            (10, Yuv444) => "yuv444p10le",
            _ => return None,
        }),
        Encoder::Libsvtav1 => match (depth, chroma) {
            (8, Yuv420) => Some("yuv420p"),
            (10, Yuv420) => Some("yuv420p10le"),
            _ => None,
        },
        Encoder::H264Qsv | Encoder::H264Vaapi => (depth, chroma).eq(&(8, Yuv420)).then_some("nv12"),
        Encoder::HevcQsv | Encoder::HevcVaapi | Encoder::Av1Qsv | Encoder::Av1Vaapi => {
            match (depth, chroma) {
                (8, Yuv420) => Some("nv12"),
                (10, Yuv420) => Some("p010le"),
                _ => None,
            }
        }
    }
}

fn chroma_name(c: Chroma) -> &'static str {
    match c {
        Chroma::Yuv420 => "4:2:0",
        Chroma::Yuv422 => "4:2:2",
        Chroma::Yuv444 => "4:4:4",
        Chroma::Yuv440 => "4:4:0",
        Chroma::Yuv411 => "4:1:1",
        Chroma::Yuv410 => "4:1:0",
        Chroma::Rgb => "RGB",
        Chroma::Gray => "灰度",
    }
}

/// Codecs a copied stream may have in each container. Anything else is a
/// protective skip: FFmpeg would fail, or silently need a conversion.
fn check_carriable(s: &StreamInfo, container: Container) -> Result<(), SkipReason> {
    let codec = s.codec_name.as_str();
    let ok = match (container, s.kind) {
        (Container::Mkv, StreamKind::Video | StreamKind::Audio) => !codec.is_empty(),
        (Container::Mkv, StreamKind::Subtitle) => !matches!(codec, "" | "mov_text"),
        (Container::Mkv, StreamKind::Attachment) => true,
        (Container::Mp4, StreamKind::Video) => matches!(
            codec,
            "h264" | "hevc" | "av1" | "mpeg4" | "mjpeg" | "png" | "vp9"
        ),
        (Container::Mp4, StreamKind::Audio) => matches!(
            codec,
            "aac" | "ac3" | "eac3" | "mp3" | "alac" | "flac" | "opus"
        ),
        (Container::Mp4, StreamKind::Subtitle) => codec == "mov_text",
        _ => false,
    };
    if ok {
        return Ok(());
    }
    let what = match s.kind {
        StreamKind::Video => "视频流",
        StreamKind::Audio => "音轨",
        StreamKind::Subtitle => "字幕",
        StreamKind::Attachment => "附件",
        StreamKind::Data => "数据流",
        StreamKind::Unknown => "未知流",
    };
    let codec = if codec.is_empty() {
        "未知编码"
    } else {
        codec
    };
    Err(skip(
        SkipCode::StreamNotSupportedInContainer,
        format!(
            "{} 封装装不下第 {} 条流（{what}，{codec}）",
            container.extension().to_ascii_uppercase(),
            s.index
        ),
    ))
}

/// Downscale only, to even dimensions, keeping the aspect ratio.
fn output_size(width: u32, height: u32, resize: Resize) -> (u32, u32) {
    match resize {
        Resize::MaxHeight(max) if height > max && max > 0 => {
            let w = (u64::from(width) * u64::from(max) + u64::from(height) / 2) / u64::from(height);
            ((w as u32 + 1) & !1, max & !1)
        }
        _ => (width, height),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::{
        identity::FileIdentity,
        marker::ProcessedMarker,
        probe::{ColorTags, DoviConfig, FormatInfo, HdrInfo, VideoParams},
    };

    fn stream(index: u32, kind: StreamKind, codec: &str) -> StreamInfo {
        StreamInfo {
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
        }
    }

    fn video(pix_fmt: &str) -> StreamInfo {
        let (depth, chroma) = crate::probe::pix_fmt_layout(pix_fmt).unzip();
        StreamInfo {
            video: Some(VideoParams {
                width: 1920,
                height: 1080,
                pix_fmt: Some(pix_fmt.into()),
                bit_depth: depth,
                chroma,
                color: ColorTags {
                    primaries: Some("bt709".into()),
                    transfer: Some("bt709".into()),
                    space: Some("bt709".into()),
                    range: Some("tv".into()),
                },
                field_order: Some("progressive".into()),
                avg_frame_rate: Some("24000/1001".into()),
            }),
            ..stream(0, StreamKind::Video, "h264")
        }
    }

    fn mkv(streams: Vec<StreamInfo>) -> MediaInfo {
        MediaInfo {
            identity: FileIdentity {
                dev: 0,
                ino: 0,
                size: 0,
                mtime_ns: 0,
            },
            depth: crate::probe::ProbeDepth::Full,
            format: FormatInfo {
                format_name: "matroska,webm".into(),
                duration_s: Some(60.0),
                size: None,
                bit_rate: None,
                tags: BTreeMap::new(),
            },
            primary_video: streams
                .iter()
                .find(|s| s.kind == StreamKind::Video)
                .map(|s| s.index),
            streams,
            chapters: vec![],
            marker: None,
            hdr: HdrInfo::default(),
            frame_side_data: BTreeSet::new(),
        }
    }

    fn sdr_mkv() -> MediaInfo {
        mkv(vec![
            video("yuv420p"),
            stream(1, StreamKind::Audio, "aac"),
            stream(2, StreamKind::Subtitle, "ass"),
            stream(3, StreamKind::Attachment, "ttf"),
        ])
    }

    fn profile(encoder: Encoder) -> Profile {
        Profile {
            encoder,
            ..Profile::default()
        }
    }

    fn code(r: Result<RoutePlan, SkipReason>) -> SkipCode {
        r.unwrap_err().code
    }

    const SRC: &str = "/media/a.mkv";

    #[test]
    fn plain_sdr_mkv_keeps_container_and_path() {
        let plan = plan(
            &sdr_mkv(),
            Path::new(SRC),
            &profile(Encoder::HevcQsv),
            &RouteOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.container, Container::Mkv);
        assert_eq!(plan.target, Path::new(SRC));
        assert_eq!(plan.pix_fmt, "nv12");
        assert_eq!((plan.width, plan.height), (1920, 1080));
        assert!(!plan.full_range);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn marker_skips_unless_forced() {
        let mut info = sdr_mkv();
        info.marker = Some(ProcessedMarker {
            version: "0.1.0".into(),
            profile: "movies".into(),
        });
        let p = profile(Encoder::Libx265);
        let src = Path::new(SRC);
        assert_eq!(
            code(plan(&info, src, &p, &RouteOptions::default())),
            SkipCode::AlreadyProcessed
        );
        let forced = RouteOptions {
            force: true,
            ..Default::default()
        };
        assert_eq!(plan(&info, src, &p, &forced).unwrap().warnings.len(), 1);
    }

    #[test]
    fn hdr10_plus_needs_hevc_and_pq_and_other_dynamic_metadata_is_skipped() {
        let src = Path::new(SRC);
        let opts = RouteOptions::default();
        let plus = |codec: &str, transfer: &str| {
            let mut info = hdr_video("yuv420p10le", transfer);
            info.streams[0].codec_name = codec.into();
            info.hdr
                .dynamic_metadata
                .insert("HDR Dynamic Metadata SMPTE2094-40 (HDR10+)".into());
            info
        };
        for e in [Encoder::Libx265, Encoder::HevcQsv, Encoder::HevcVaapi] {
            let p = plan(&plus("hevc", "smpte2084"), src, &profile(e), &opts).unwrap();
            assert_eq!(p.dynamic_range, RangeRoute::Hdr10Plus);
            assert!(p.dynamic_range.hdr10_plus() && p.dynamic_range.injects());
        }
        // Downscaling is fine: one processing window covers any size.
        let smaller = Profile {
            resize: Resize::MaxHeight(720),
            ..profile(Encoder::Libx265)
        };
        assert!(plan(&plus("hevc", "smpte2084"), src, &smaller, &opts).is_ok());
        let skipped = |info: &MediaInfo, e| code(plan(info, src, &profile(e), &opts));
        let ok = plus("hevc", "smpte2084");
        assert_eq!(skipped(&ok, Encoder::Libsvtav1), SkipCode::Hdr10PlusEncoder);
        assert_eq!(skipped(&ok, Encoder::Libx264), SkipCode::Hdr10PlusEncoder);
        assert_eq!(
            skipped(&plus("av1", "smpte2084"), Encoder::Libx265),
            SkipCode::DynamicHdr
        );
        assert_eq!(
            skipped(&plus("hevc", "arib-std-b67"), Encoder::Libx265),
            SkipCode::DynamicHdr
        );
        let mut vivid = ok.clone();
        vivid
            .hdr
            .dynamic_metadata
            .insert("HDR Dynamic Metadata CUVA 005.1 2021 (Vivid)".into());
        assert_eq!(skipped(&vivid, Encoder::Libx265), SkipCode::DynamicHdr);
        assert!(!RangeRoute::Hdr10.injects());
    }

    fn dv_config(profile: u8, compat: u8) -> DoviConfig {
        DoviConfig {
            version_major: 1,
            version_minor: 0,
            profile,
            level: 6,
            rpu_present: true,
            el_present: profile == 7,
            bl_present: true,
            bl_compat_id: compat,
            md_compression: 0,
        }
    }

    fn dv_video(profile: u8, compat: u8) -> MediaInfo {
        let mut info = hdr_video("yuv420p10le", "smpte2084");
        info.streams[0].codec_name = "hevc".into();
        info.hdr.dolby_vision = Some(dv_config(profile, compat));
        info.hdr.dovi_rpu = true;
        info
    }

    #[test]
    fn dolby_vision_needs_hevc_and_a_known_configuration() {
        let src = Path::new(SRC);
        let opts = RouteOptions::default();
        for (p, c) in [(5, 0), (8, 1), (8, 4)] {
            let plan = plan(&dv_video(p, c), src, &profile(Encoder::HevcQsv), &opts).unwrap();
            let (source, output) = plan.dynamic_range.dolby_vision().unwrap();
            assert_eq!(source, output);
            assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        }
        let p7 = plan(&dv_video(7, 6), src, &profile(Encoder::Libx265), &opts).unwrap();
        let (_, output) = p7.dynamic_range.dolby_vision().unwrap();
        assert_eq!(
            (output.profile, output.bl_compat_id, output.el_present),
            (8, 1, false)
        );
        assert_eq!(p7.warnings.len(), 1);

        let skipped =
            |info: &MediaInfo, p: &Profile, opts: &RouteOptions| code(plan(info, src, p, opts));
        let x265 = profile(Encoder::Libx265);
        assert_eq!(
            skipped(&dv_video(8, 1), &profile(Encoder::Libsvtav1), &opts),
            SkipCode::DolbyVisionAv1
        );
        assert_eq!(
            skipped(&dv_video(8, 1), &profile(Encoder::H264Qsv), &opts),
            SkipCode::DolbyVisionEncoder
        );
        assert_eq!(
            skipped(&dv_video(8, 2), &x265, &opts),
            SkipCode::DolbyVision
        );
        let mut avc = dv_video(8, 1);
        avc.streams[0].codec_name = "h264".into();
        assert_eq!(skipped(&avc, &x265, &opts), SkipCode::DolbyVision);
        let mut rpu_only = dv_video(8, 1);
        rpu_only.hdr.dolby_vision = None;
        assert_eq!(skipped(&rpu_only, &x265, &opts), SkipCode::DolbyVision);
        let mut plus = dv_video(8, 1);
        plus.hdr.dynamic_metadata.insert("HDR10+".into());
        let both = plan(&plus, src, &x265, &opts).unwrap().dynamic_range;
        assert!(both.hdr10_plus() && both.dolby_vision().is_some());
        assert!(!plan(&dv_video(8, 1), src, &x265, &opts)
            .unwrap()
            .dynamic_range
            .hdr10_plus());
        plus.hdr.dynamic_metadata.insert("Vivid".into());
        assert_eq!(skipped(&plus, &x265, &opts), SkipCode::DynamicHdr);
        let smaller = Profile {
            resize: Resize::MaxHeight(720),
            ..x265.clone()
        };
        assert_eq!(
            skipped(&dv_video(8, 1), &smaller, &opts),
            SkipCode::DolbyVision
        );
        let mut four_two_two = dv_video(5, 0);
        let color = four_two_two.streams[0]
            .video
            .as_ref()
            .unwrap()
            .color
            .clone();
        four_two_two.streams[0] = StreamInfo {
            codec_name: "hevc".into(),
            ..video("yuv422p10le")
        };
        four_two_two.streams[0].video.as_mut().unwrap().color = color;
        assert_eq!(skipped(&four_two_two, &x265, &opts), SkipCode::DolbyVision);
        let no_tool = RouteOptions {
            dovi_unavailable: Some("not found"),
            ..Default::default()
        };
        assert_eq!(
            skipped(&dv_video(8, 1), &x265, &no_tool),
            SkipCode::DoviToolUnavailable
        );
    }

    fn hdr_video(pix_fmt: &str, transfer: &str) -> MediaInfo {
        let mut info = mkv(vec![video(pix_fmt), stream(1, StreamKind::Audio, "eac3")]);
        info.streams[0].video.as_mut().unwrap().color = ColorTags {
            primaries: Some("bt2020".into()),
            transfer: Some(transfer.into()),
            space: Some("bt2020nc".into()),
            range: Some("tv".into()),
        };
        info
    }

    #[test]
    fn hdr10_and_hlg_need_10_bit_hevc_or_av1() {
        let src = Path::new(SRC);
        let opts = RouteOptions::default();
        for (transfer, kind) in [
            ("smpte2084", RangeRoute::Hdr10),
            ("arib-std-b67", RangeRoute::Hlg),
        ] {
            let info = hdr_video("yuv420p10le", transfer);
            for e in [
                Encoder::Libx265,
                Encoder::HevcQsv,
                Encoder::HevcVaapi,
                Encoder::Libsvtav1,
            ] {
                let p = plan(&info, src, &profile(e), &opts).unwrap();
                assert_eq!(p.dynamic_range, kind, "{e:?}");
                assert!(p.warnings.is_empty(), "{e:?}: {:?}", p.warnings);
            }
            for e in [Encoder::Libx264, Encoder::H264Qsv, Encoder::H264Vaapi] {
                assert_eq!(
                    code(plan(&info, src, &profile(e), &opts)),
                    SkipCode::HdrEncoder,
                    "{e:?}"
                );
            }
            let eight = hdr_video("yuv420p", transfer);
            assert_eq!(
                code(plan(&eight, src, &profile(Encoder::Libx265), &opts)),
                SkipCode::Hdr
            );
        }
        // Downscaling and changing the container keep the route.
        let p = plan(
            &hdr_video("yuv420p10le", "smpte2084"),
            src,
            &Profile {
                container: ContainerChoice::Mp4,
                resize: Resize::MaxHeight(720),
                ..profile(Encoder::HevcQsv)
            },
            &opts,
        )
        .unwrap();
        assert_eq!(
            (p.dynamic_range, p.pix_fmt.as_str(), p.height),
            (RangeRoute::Hdr10, "p010le", 720)
        );
        assert_eq!(
            plan(&sdr_mkv(), src, &profile(Encoder::Libx265), &opts)
                .unwrap()
                .dynamic_range,
            RangeRoute::Sdr
        );
    }

    #[test]
    fn encoder_must_keep_depth_and_chroma() {
        let src = Path::new(SRC);
        let opts = RouteOptions::default();
        let with = |pix: &str| mkv(vec![video(pix)]);
        let pix = |info: &MediaInfo, e| plan(info, src, &profile(e), &opts).map(|p| p.pix_fmt);

        assert_eq!(
            pix(&with("yuv420p10le"), Encoder::HevcVaapi).unwrap(),
            "p010le"
        );
        assert_eq!(
            pix(&with("yuv420p10le"), Encoder::Libx264).unwrap(),
            "yuv420p10le"
        );
        assert_eq!(pix(&with("yuv422p"), Encoder::Libx265).unwrap(), "yuv422p");
        assert_eq!(
            pix(&with("yuv420p10le"), Encoder::H264Qsv)
                .unwrap_err()
                .code,
            SkipCode::EncoderCannotKeepFormat
        );
        assert_eq!(
            pix(&with("yuv422p10le"), Encoder::HevcQsv)
                .unwrap_err()
                .code,
            SkipCode::EncoderCannotKeepFormat
        );
        assert_eq!(
            pix(&with("yuv444p"), Encoder::Libsvtav1).unwrap_err().code,
            SkipCode::EncoderCannotKeepFormat
        );
        for odd in ["yuv420p12le", "gbrp", "gray", "pal8"] {
            assert_eq!(
                pix(&with(odd), Encoder::Libx265).unwrap_err().code,
                SkipCode::UnsupportedPixelFormat,
                "{odd}"
            );
        }
        let full = pix(&with("yuvj420p"), Encoder::Libx264).unwrap();
        assert_eq!(full, "yuv420p");
        assert!(
            plan(&with("yuvj420p"), src, &profile(Encoder::Libx264), &opts)
                .unwrap()
                .full_range
        );
    }

    #[test]
    fn interlaced_and_no_video_are_skipped() {
        let src = Path::new(SRC);
        let opts = RouteOptions::default();
        let mut info = sdr_mkv();
        info.streams[0].video.as_mut().unwrap().field_order = Some("tt".into());
        assert_eq!(
            code(plan(&info, src, &profile(Encoder::Libx265), &opts)),
            SkipCode::Interlaced
        );

        let audio_only = mkv(vec![stream(0, StreamKind::Audio, "flac")]);
        assert_eq!(
            code(plan(&audio_only, src, &profile(Encoder::Libx265), &opts)),
            SkipCode::NoVideo
        );
    }

    #[test]
    fn mp4_cannot_carry_mkv_extras() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.mkv");
        let mp4 = Profile {
            container: ContainerChoice::Mp4,
            ..profile(Encoder::Libx265)
        };
        let opts = RouteOptions::default();
        let err = plan(&sdr_mkv(), &src, &mp4, &opts).unwrap_err();
        assert_eq!(err.code, SkipCode::StreamNotSupportedInContainer);
        assert!(err.message.contains("第 2 条流"), "{}", err.message);

        let simple = mkv(vec![video("yuv420p"), stream(1, StreamKind::Audio, "eac3")]);
        let ok = plan(&simple, &src, &mp4, &opts).unwrap();
        assert_eq!(ok.container, Container::Mp4);
        assert_eq!(ok.target, dir.path().join("a.mp4"));

        std::fs::write(dir.path().join("a.mp4"), b"someone else's").unwrap();
        assert_eq!(
            code(plan(&simple, &src, &mp4, &opts)),
            SkipCode::TargetExists
        );

        let truehd = mkv(vec![
            video("yuv420p"),
            stream(1, StreamKind::Audio, "truehd"),
        ]);
        assert_eq!(
            code(plan(&truehd, &dir.path().join("b.mkv"), &mp4, &opts)),
            SkipCode::StreamNotSupportedInContainer
        );
    }

    #[test]
    fn container_change_lists_expected_losses() {
        let flagged = |kind, codec: &str, flags: &[&str]| StreamInfo {
            disposition: flags.iter().map(|f| f.to_string()).collect(),
            ..stream(1, kind, codec)
        };
        let opts = RouteOptions::default();
        let to = |c| Profile {
            container: c,
            ..profile(Encoder::Libx265)
        };

        // MKV -> MP4: `original` has no MP4 equivalent; forced survives.
        let info = mkv(vec![
            video("yuv420p"),
            flagged(StreamKind::Audio, "aac", &["default", "original", "forced"]),
        ]);
        let p = plan(
            &info,
            Path::new("/m/a.mkv"),
            &to(ContainerChoice::Mp4),
            &opts,
        )
        .unwrap();
        assert!(p.container_changed);
        let lost: Vec<_> = p
            .expected_losses
            .iter()
            .map(|l| (l.stream, l.property.as_str()))
            .collect();
        assert_eq!(lost, [(1, "disposition:original")]);
        assert_eq!(p.warnings.len(), 1);

        // MP4 -> MKV: captions and audio descriptions cannot be written.
        let mut info = mkv(vec![
            video("yuv420p"),
            flagged(
                StreamKind::Audio,
                "aac",
                &[
                    "default",
                    "hearing_impaired",
                    "captions",
                    "visual_impaired",
                    "descriptions",
                ],
            ),
        ]);
        info.format.format_name = "mov,mp4,m4a,3gp,3g2,mj2".into();
        let p = plan(
            &info,
            Path::new("/m/a.mp4"),
            &to(ContainerChoice::Mkv),
            &opts,
        )
        .unwrap();
        let lost: BTreeSet<_> = p
            .expected_losses
            .iter()
            .filter_map(|l| l.disposition())
            .collect();
        assert_eq!(lost, BTreeSet::from(["captions", "descriptions"]));

        // Same container: nothing is expected to be lost.
        let p = plan(
            &info,
            Path::new("/m/a.mp4"),
            &profile(Encoder::Libx265),
            &opts,
        )
        .unwrap();
        assert!(!p.container_changed);
        assert!(p.expected_losses.is_empty());
    }

    #[test]
    fn mkv_cannot_carry_mov_text_or_data() {
        let mut info = mkv(vec![
            video("yuv420p"),
            stream(1, StreamKind::Subtitle, "mov_text"),
        ]);
        info.format.format_name = "mov,mp4,m4a,3gp,3g2,mj2".into();
        let to_mkv = Profile {
            container: ContainerChoice::Mkv,
            ..profile(Encoder::Libx265)
        };
        let opts = RouteOptions::default();
        assert_eq!(
            code(plan(&info, Path::new("/m/a.mp4"), &to_mkv, &opts)),
            SkipCode::StreamNotSupportedInContainer
        );
        // Keeping MP4 carries mov_text fine.
        assert!(plan(
            &info,
            Path::new("/m/a.mp4"),
            &profile(Encoder::Libx265),
            &opts
        )
        .is_ok());

        // A data stream that is not a chapter track (no chapters were read).
        info.streams[1] = stream(1, StreamKind::Data, "bin_data");
        assert_eq!(
            code(plan(
                &info,
                Path::new("/m/a.mp4"),
                &profile(Encoder::Libx265),
                &opts
            )),
            SkipCode::StreamNotSupportedInContainer
        );
    }

    #[test]
    fn chapter_tracks_are_left_out() {
        let chapter_track = |index| StreamInfo {
            codec_tag: Some("text".into()),
            ..stream(index, StreamKind::Data, "bin_data")
        };
        let mut info = mkv(vec![
            video("yuv420p"),
            stream(1, StreamKind::Audio, "aac"),
            chapter_track(2),
        ]);
        info.format.format_name = "mov,mp4,m4a,3gp,3g2,mj2".into();
        info.chapters = vec![crate::probe::Chapter {
            start_s: 0.0,
            end_s: 1.0,
            title: Some("Opening".into()),
        }];
        let opts = RouteOptions::default();
        for (container, path) in [
            (ContainerChoice::Keep, "/m/a.mp4"),
            (ContainerChoice::Keep, "/m/a.m4v"),
            (ContainerChoice::Mkv, "/m/a.mp4"),
            (ContainerChoice::Mkv, "/m/a.mov"),
            (ContainerChoice::Mp4, "/m/a.mov"),
        ] {
            let p = Profile {
                container,
                ..profile(Encoder::Libx265)
            };
            let p = plan(&info, Path::new(path), &p, &opts).unwrap();
            assert_eq!(p.chapter_tracks, [2], "{path} {container:?}");
            assert!(p.warnings.is_empty());
        }

        let keep = profile(Encoder::Libx265);
        let skipped = |info: &MediaInfo| plan(info, Path::new("/m/a.mp4"), &keep, &opts);
        // Copied streams keep their index only when the track comes last.
        let mut first = info.clone();
        first.streams.swap(1, 2);
        first.streams[1].index = 1;
        first.streams[2].index = 2;
        let r = skipped(&first).unwrap_err();
        assert_eq!(r.code, SkipCode::StreamNotSupportedInContainer);
        assert!(r.message.contains("章节文本轨"), "{}", r.message);
        // Without chapters, or as another kind of data, it is not one.
        let mut no_chapters = info.clone();
        no_chapters.chapters.clear();
        assert_eq!(
            code(skipped(&no_chapters)),
            SkipCode::StreamNotSupportedInContainer
        );
        let mut gpmd = info.clone();
        gpmd.streams[2].codec_tag = Some("gpmd".into());
        assert_eq!(
            code(skipped(&gpmd)),
            SkipCode::StreamNotSupportedInContainer
        );
    }

    #[test]
    fn keep_needs_a_known_source_container() {
        let mut info = sdr_mkv();
        info.format.format_name = "mpegts".into();
        let opts = RouteOptions::default();
        assert_eq!(
            code(plan(
                &info,
                Path::new("/m/a.ts"),
                &profile(Encoder::Libx265),
                &opts
            )),
            SkipCode::SourceContainerUnsupported
        );
        let to_mkv = Profile {
            container: ContainerChoice::Mkv,
            ..profile(Encoder::Libx265)
        };
        let p = plan(&info, Path::new("/m/a.ts"), &to_mkv, &opts).unwrap();
        assert_eq!(p.target, Path::new("/m/a.mkv"));
    }

    #[test]
    fn unavailable_encoder_is_skipped() {
        let opts = RouteOptions {
            available: Some(&[Encoder::Libx265]),
            ..Default::default()
        };
        let src = Path::new(SRC);
        assert_eq!(
            code(plan(&sdr_mkv(), src, &profile(Encoder::Av1Qsv), &opts)),
            SkipCode::EncoderUnavailable
        );
        assert!(plan(&sdr_mkv(), src, &profile(Encoder::Libx265), &opts).is_ok());
    }

    #[test]
    fn sei_is_only_a_warning() {
        let mut info = sdr_mkv();
        info.frame_side_data
            .insert("H.26[45] User Data Unregistered SEI message".into());
        let p = plan(
            &info,
            Path::new(SRC),
            &profile(Encoder::Libx265),
            &RouteOptions::default(),
        )
        .unwrap();
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn downscale_keeps_aspect_and_even_sizes() {
        assert_eq!(
            output_size(3840, 2160, Resize::MaxHeight(1080)),
            (1920, 1080)
        );
        assert_eq!(output_size(1920, 800, Resize::MaxHeight(720)), (1728, 720));
        assert_eq!(output_size(1440, 1080, Resize::MaxHeight(721)), (962, 720));
        assert_eq!(output_size(1280, 720, Resize::MaxHeight(1080)), (1280, 720));
        assert_eq!(output_size(1280, 720, Resize::Keep), (1280, 720));
    }
}
