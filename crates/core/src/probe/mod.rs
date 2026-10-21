//! Media probing: a structured view of what ffprobe reports, enough to decide
//! the route (protective skip or encode), build the command and verify the
//! candidate.
//!
//! Probing runs ffprobe up to three times:
//! 1. format, streams and chapters (JSON);
//! 2. side data on the first frames of the primary video (HDR static metadata,
//!    dynamic metadata, Dolby Vision RPU, encoder SEI);
//! 3. [`ProbeDepth::Full`] only: the packet table, giving each stream's packet
//!    count, first timestamp and end time without decoding, and the first
//!    frames of the primary video decoded to find when it starts to be shown.
//!
//! Results are cached by [`FileIdentity`].

mod cache;
mod parse;
mod pixfmt;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use serde::{Deserialize, Serialize};

pub use cache::{JsonDirCache, ProbeCache};
pub use pixfmt::{pix_fmt_layout, Chroma};

use crate::{
    identity::FileIdentity,
    marker::ProcessedMarker,
    tool::{self, CancelToken, ToolError},
};

/// Bumped whenever [`MediaInfo`] changes shape or meaning; old cache entries
/// are then ignored.
pub const PROBE_SCHEMA: u32 = 8;

/// Frames of the primary video inspected for side data.
const SIDE_DATA_FRAMES: u32 = 5;

/// Packets of the primary video decoded to find its first shown frame; more
/// than an open GOP's leading frames.
const FIRST_FRAME_PACKETS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeDepth {
    /// Metadata and first-frame side data; cheap enough for library scans.
    Quick,
    /// Also reads every packet header (the whole file, but no decoding).
    Full,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub identity: FileIdentity,
    pub depth: ProbeDepth,
    pub format: FormatInfo,
    pub streams: Vec<StreamInfo>,
    pub chapters: Vec<Chapter>,
    /// Our processed marker, if the container carries one.
    pub marker: Option<ProcessedMarker>,
    /// Index of the stream that gets re-encoded: the first video stream that is
    /// not cover art.
    pub primary_video: Option<u32>,
    pub hdr: HdrInfo,
    /// Distinct side data types seen on the primary video's first frames.
    pub frame_side_data: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormatInfo {
    pub format_name: String,
    pub duration_s: Option<f64>,
    pub size: Option<u64>,
    pub bit_rate: Option<u64>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    Video,
    Audio,
    Subtitle,
    Attachment,
    Data,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub index: u32,
    pub kind: StreamKind,
    /// Empty when ffprobe has no decoder for the stream.
    pub codec_name: String,
    pub codec_tag: Option<String>,
    pub profile: Option<String>,
    pub time_base: String,
    pub language: Option<String>,
    pub title: Option<String>,
    pub tags: BTreeMap<String, String>,
    /// Disposition flags that are set, e.g. `default`, `forced`, `attached_pic`.
    pub disposition: BTreeSet<String>,
    pub video: Option<VideoParams>,
    pub audio: Option<AudioParams>,
    /// Stream-level (container) side data types.
    pub side_data: BTreeSet<String>,
    /// Filled by [`ProbeDepth::Full`].
    pub packets: Option<PacketStats>,
}

impl StreamInfo {
    pub fn has_disposition(&self, flag: &str) -> bool {
        self.disposition.contains(flag)
    }

    pub fn is_cover_art(&self) -> bool {
        self.has_disposition("attached_pic")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    pub pix_fmt: Option<String>,
    pub bit_depth: Option<u8>,
    pub chroma: Option<Chroma>,
    pub color: ColorTags,
    /// `progressive`, `tt`, `bb`, `tb`, `bt`, or `None` when unknown.
    pub field_order: Option<String>,
    pub avg_frame_rate: Option<String>,
}

impl VideoParams {
    pub fn is_interlaced(&self) -> bool {
        matches!(self.field_order.as_deref(), Some("tt" | "bb" | "tb" | "bt"))
    }
}

/// Color description; unspecified values are `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorTags {
    pub primaries: Option<String>,
    pub transfer: Option<String>,
    pub space: Option<String>,
    pub range: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioParams {
    pub sample_rate: Option<u32>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacketStats {
    pub count: u64,
    /// Packets of at most 2 bytes: for `mov_text`, the empty samples that
    /// FFmpeg's MP4 muxer adds between subtitles, more on every remux.
    #[serde(default)]
    pub empty: u64,
    /// Smallest presentation timestamp, seconds.
    pub start_s: Option<f64>,
    /// Largest `pts + duration`, seconds.
    pub end_s: Option<f64>,
    /// Presentation times of frames at evenly spaced positions; primary video
    /// only. Verification compares them to catch drift mid-file (ADR 0004).
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
    /// Packets at the start that an MP4 edit list hides (flag D); streams
    /// other than the primary video, whose hidden frames are left out.
    #[serde(default)]
    pub lead_in: Option<LeadIn>,
    /// Presentation time of the first frame the decoder outputs; primary
    /// video only. Later than `start_s` when the file starts with frames
    /// that cannot be decoded: the leading B frames of an open GOP at the
    /// start of a cut recording, which reference the GOP cut away.
    #[serde(default)]
    pub first_frame_s: Option<f64>,
}

impl PacketStats {
    /// When the stream starts to be shown: the first decoded frame where
    /// known, else the first packet.
    pub fn shown_start_s(&self) -> Option<f64> {
        self.first_frame_s.or(self.start_s)
    }
}

/// The first `packets` packets of a stream are hidden; the last of them
/// starts at `last_start_s`. One hidden AAC packet is the encoder priming
/// every MP4 has; more come from a stream-copy cut.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LeadIn {
    pub packets: u64,
    pub last_start_s: Option<f64>,
}

/// The `frame`-th frame in presentation order is shown at `pts_s`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub frame: u64,
    pub pts_s: f64,
}

/// Number of timeline checkpoints per video.
pub const CHECKPOINTS: u64 = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub start_s: f64,
    pub end_s: f64,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HdrInfo {
    /// Mastering display primaries and luminance, as ffprobe prints them.
    pub mastering_display: Option<BTreeMap<String, String>>,
    pub content_light: Option<ContentLight>,
    /// Dynamic metadata side data types (HDR10+, HDR Vivid, ...).
    pub dynamic_metadata: BTreeSet<String>,
    /// Dolby Vision configuration record of the primary video.
    pub dolby_vision: Option<DoviConfig>,
    /// A Dolby Vision RPU was seen on the first frames.
    pub dovi_rpu: bool,
}

impl HdrInfo {
    pub fn has_hdr10_plus(&self) -> bool {
        self.dynamic_metadata
            .iter()
            .any(|t| t.contains("SMPTE2094-40") || t.contains("HDR10+"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentLight {
    pub max_content: u32,
    pub max_average: u32,
}

/// Dolby Vision decoder configuration record (dvcC/dvvC box, Matroska
/// BlockAdditionMapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DoviConfig {
    #[serde(default = "one")]
    pub version_major: u8,
    #[serde(default)]
    pub version_minor: u8,
    pub profile: u8,
    pub level: u8,
    pub rpu_present: bool,
    pub el_present: bool,
    pub bl_present: bool,
    pub bl_compat_id: u8,
    /// Metadata compression: 0 none, 1 limited, 2 reserved, 3 extended.
    #[serde(default)]
    pub md_compression: u8,
}

fn one() -> u8 {
    1
}

impl DoviConfig {
    /// `P8.1`, `P5`, `P7`.
    pub fn label(&self) -> String {
        match self.profile {
            8 => format!("P8.{}", self.bl_compat_id),
            p => format!("P{p}"),
        }
    }
}

/// How the primary video's picture is meant to be displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicRange {
    Sdr,
    Hlg,
    Hdr10,
    /// PQ or HLG with dynamic metadata other than Dolby Vision.
    DynamicHdr,
    DolbyVision,
}

impl MediaInfo {
    pub fn stream(&self, index: u32) -> Option<&StreamInfo> {
        self.streams.iter().find(|s| s.index == index)
    }

    pub fn primary_video_stream(&self) -> Option<&StreamInfo> {
        self.primary_video.and_then(|i| self.stream(i))
    }

    /// A QuickTime chapter track (章节文本轨) whose chapters FFmpeg read into
    /// [`MediaInfo::chapters`]. The MOV demuxer turns the text track a `chap`
    /// reference points to into a `bin_data` data stream and discards its
    /// packets; a text track that is not a chapter track is probed as a
    /// subtitle instead (ADR 0013).
    pub fn is_chapter_track(&self, s: &StreamInfo) -> bool {
        self.format.format_name.starts_with("mov,mp4")
            && !self.chapters.is_empty()
            && s.kind == StreamKind::Data
            && s.codec_name == "bin_data"
            && matches!(s.codec_tag.as_deref(), Some("text" | "tx3g"))
    }

    pub fn dynamic_range(&self) -> DynamicRange {
        if self.hdr.dolby_vision.is_some() || self.hdr.dovi_rpu {
            return DynamicRange::DolbyVision;
        }
        if !self.hdr.dynamic_metadata.is_empty() {
            return DynamicRange::DynamicHdr;
        }
        let transfer = self
            .primary_video_stream()
            .and_then(|s| s.video.as_ref())
            .and_then(|v| v.color.transfer.as_deref());
        match transfer {
            Some("smpte2084") => DynamicRange::Hdr10,
            Some("arib-std-b67") => DynamicRange::Hlg,
            _ => DynamicRange::Sdr,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("无法读取 {path}：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Tool(#[from] ToolError),
    #[error("无法解析 ffprobe 输出：{0}")]
    Parse(String),
    #[error("探测期间文件被改动：{0}")]
    Changed(PathBuf),
}

/// ffprobe wrapper with an optional cache.
pub struct Prober {
    pub ffprobe: PathBuf,
    pub quick_timeout: Duration,
    pub full_timeout: Duration,
    pub cache: Option<Box<dyn ProbeCache>>,
}

impl Prober {
    pub fn new(ffprobe: impl Into<PathBuf>) -> Self {
        Self {
            ffprobe: ffprobe.into(),
            quick_timeout: Duration::from_secs(120),
            full_timeout: Duration::from_secs(3600),
            cache: None,
        }
    }

    pub fn with_cache(mut self, cache: impl ProbeCache + 'static) -> Self {
        self.cache = Some(Box::new(cache));
        self
    }

    pub fn probe(
        &self,
        path: &Path,
        depth: ProbeDepth,
        cancel: Option<&CancelToken>,
    ) -> Result<MediaInfo, ProbeError> {
        let identity = identity_of(path)?;
        if let Some(hit) = self.cache.as_ref().and_then(|c| c.get(&identity, depth)) {
            tracing::debug!(path = %path.display(), "probe cache hit");
            return Ok(hit);
        }

        let input = ffmpeg_input(path);
        let json = tool::run_capture(
            self.ffprobe_cmd()
                .args([
                    "-of",
                    "json",
                    "-show_format",
                    "-show_streams",
                    "-show_chapters",
                ])
                .arg(&input),
            self.quick_timeout,
            cancel,
        )?;
        let mut info = parse::parse_metadata(&json, identity).map_err(ProbeError::Parse)?;

        if let Some(index) = info.primary_video {
            let frames = tool::run_capture(
                self.ffprobe_cmd()
                    .args(["-of", "json", "-select_streams", &index.to_string()])
                    .args(["-read_intervals", &format!("%+#{SIDE_DATA_FRAMES}")])
                    .args(["-show_entries", "frame_side_data_list"])
                    .arg(&input),
                self.quick_timeout,
                cancel,
            )?;
            parse::apply_frame_side_data(&mut info, &frames).map_err(ProbeError::Parse)?;
        }

        if depth == ProbeDepth::Full {
            let mut acc = parse::PacketAccumulator::new(&info.streams, info.primary_video);
            tool::run_lines(
                self.ffprobe_cmd()
                    .args(["-of", "compact=p=0:nk=0"])
                    .args([
                        "-show_entries",
                        "packet=stream_index,pts,dts,duration,size,flags",
                    ])
                    .arg(&input),
                self.full_timeout,
                cancel,
                |line| acc.push_line(line),
            )?;
            acc.finish(&mut info.streams);
            if let Some(index) = info.primary_video {
                let frames = tool::run_capture(
                    self.ffprobe_cmd()
                        .args(["-of", "csv=p=0", "-select_streams", &index.to_string()])
                        .args(["-read_intervals", &format!("%+#{FIRST_FRAME_PACKETS}")])
                        .args(["-show_entries", "frame=pts,best_effort_timestamp"])
                        .arg(&input),
                    self.full_timeout,
                    cancel,
                )?;
                parse::apply_first_frame(&mut info, index, &frames);
            }
            info.depth = ProbeDepth::Full;
        }

        if identity_of(path)? != identity {
            return Err(ProbeError::Changed(path.to_owned()));
        }
        if let Some(cache) = &self.cache {
            cache.put(&info);
        }
        Ok(info)
    }

    fn ffprobe_cmd(&self) -> Command {
        let mut cmd = Command::new(&self.ffprobe);
        cmd.args(["-v", "error", "-hide_banner"]);
        cmd
    }
}

fn identity_of(path: &Path) -> Result<FileIdentity, ProbeError> {
    FileIdentity::of(path).map_err(|source| ProbeError::Io {
        path: path.to_owned(),
        source,
    })
}

/// FFmpeg input/output argument for a local path. The `file:` prefix stops
/// FFmpeg from reading `name:rest` as a protocol.
pub fn ffmpeg_input(path: &Path) -> OsString {
    let mut s = OsString::from("file:");
    s.push(path.as_os_str());
    s
}
