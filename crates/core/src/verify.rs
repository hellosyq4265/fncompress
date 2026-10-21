//! Tiered verification (ADR 0004). Hard checks reject the candidate; soft
//! checks count as warnings; info entries are only recorded. A rejected
//! candidate becomes a pending output awaiting manual release instead of
//! being deleted.
//!
//! [`verify`] itself is pure: checks that need FFmpeg (decoding, SSIM, frame
//! counting, hashing) run in [`crate::inspect`] and arrive as an [`Inspection`].
//!
//! Every check has a stable id and a fixed tier:
//!
//! | id | tier | what |
//! |---|---|---|
//! | `probe_depth` | hard | both files probed with packet tables |
//! | `stream_layout` | hard | same streams, same order, same kinds; QuickTime chapter tracks are compared under `chapters` |
//! | `video_codec`, `video_size`, `video_format` | hard | encoded video is what the plan asked for, same depth and chroma |
//! | `color` | hard (changed) / soft (added) | color description |
//! | `frame_count` | hard | video frames; when packet counts differ, decoded frame counts decide (field-coded sources) |
//! | `packet_count` | hard | every copied stream's packets |
//! | `stream_codec` | hard | copied streams keep codec, sample rate, channels |
//! | `stream_language`, `stream_title`, `stream_disposition` | hard | track properties, against the source minus the plan's expected losses |
//! | `decode` | hard | the candidate video decodes without any error |
//! | `spot_check_ssim` | soft < 0.90, hard < 0.80 | SSIM of picture spot checks; catches severe damage, not quality |
//! | `stream_hash` | hard | copied streams hash equal (optional, same container only) |
//! | `timeline` | soft | with equal frame counts, frames at 10 checkpoints shown within one frame duration of the source (relative to the video start) |
//! | `av_offset` | soft > 15 ms, hard > 45 ms | audio start relative to video start (its first decoded frame) |
//! | `stream_end` | soft > 50 ms, hard > 1 s | each stream's end time (relative to the video start) |
//! | `chapters` | hard (count, titles) / tiered (times) | chapters |
//! | `hdr_metadata` | hard | HDR static and dynamic metadata; the Dolby Vision configuration the plan expects (P8.1 for P7) |
//! | `dovi_rpu` | hard | Dolby Vision routes: the candidate's RPUs are the source's, frame for frame |
//! | `hdr10_plus` | hard / info | HDR10+ routes: the candidate's HDR10+ messages are the source's, frame for frame; info when they are |
//! | `marker` | hard | the processed marker; without it monitoring would re-encode the output forever |
//! | `stream_tags`, `global_tags` | soft | user tag contents changed |
//! | `inspection` | soft | an FFmpeg-based check could not run |
//! | `time_base`, `muxer_tags`, `encoder_sei`, `container_duration` | info | representation the muxer or encoder decides |
//! | `chapter_track` | info | the source's chapter tracks the plan leaves out (ADR 0013) |
//! | `dolby_vision` | info | what a Dolby Vision route did (P7 to P8.1, RPU frames); added by the pipeline |

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    encoder::Encoder,
    marker::{ProcessedMarker, METADATA_KEY, MP4_TAG},
    probe::{DoviConfig, MediaInfo, PacketStats, ProbeDepth, StreamInfo, StreamKind, VideoParams},
    route::{ExpectedLoss, LeadInTrim, RoutePlan},
    settings::VerifyThresholds,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Rejects: track loss, content change, damaged picture, HDR/DV metadata loss.
    Hard,
    /// Warning: timing within tolerance, tag contents, mild SSIM drop.
    Soft,
    /// Recorded only; does not affect the verdict. Differences nearly every
    /// file shows (time base, muxer tags, encoder SEI, container duration).
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Finding {
    pub check: String,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Passed,
    PassedWithWarnings,
    Rejected,
}

pub fn verdict(findings: &[Finding]) -> Verdict {
    if findings.iter().any(|f| f.severity == Severity::Hard) {
        Verdict::Rejected
    } else if findings.iter().any(|f| f.severity == Severity::Soft) {
        Verdict::PassedWithWarnings
    } else {
        Verdict::Passed
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Report {
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    /// Also recorded as a `container_duration` info finding.
    pub container_duration_delta_ms: Option<f64>,
    /// Raw results of the FFmpeg-based checks.
    pub inspection: Inspection,
}

/// Results of the checks that need FFmpeg; `None` means not run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Inspection {
    pub decode: Option<DecodeResult>,
    pub spot_checks: Option<Vec<SpotCheck>>,
    /// Decoded video frames (source, candidate); only counted when the
    /// packet counts differ.
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Vec<u64>>))]
    pub decoded_frames: Option<(u64, u64)>,
    pub stream_hashes: Option<Vec<StreamHash>>,
    /// Dolby Vision routes: the candidate's RPUs against the source's.
    #[serde(default)]
    pub rpu: Option<RpuCheck>,
    /// HDR10+ routes: the candidate's HDR10+ messages against the source's.
    #[serde(default)]
    pub hdr10_plus: Option<RpuCheck>,
    /// (check, why) for checks that could not run.
    #[cfg_attr(feature = "openapi", schema(value_type = Vec<Vec<String>>))]
    pub errors: Vec<(String, String)>,
}

/// Per-frame metadata extracted from the candidate, compared with what was
/// injected, in display order: RPUs (the source's, converted to P8.1 for
/// P7) or HDR10+ messages. `found` counts the frames that carry one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RpuCheck {
    pub expected: u64,
    pub found: u64,
    /// First frame whose metadata differs.
    pub first_mismatch: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DecodeResult {
    /// Whether the deciding decode ran on the GPU.
    pub hardware: bool,
    /// Decoder errors; empty means the whole video decoded cleanly.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SpotCheck {
    /// Seconds from the start of the video.
    pub at_s: f64,
    pub frames: u32,
    /// Mean SSIM (All) over the compared frames.
    pub ssim: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct StreamHash {
    pub index: u32,
    pub source: String,
    pub candidate: String,
}

/// What the candidate should look like, from the route plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Expectation {
    pub video_index: u32,
    /// ffprobe codec name of the encoded video.
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub marker: ProcessedMarker,
    pub container_changed: bool,
    pub losses: Vec<ExpectedLoss>,
    /// Dolby Vision configuration the candidate must carry.
    pub dovi: Option<DoviConfig>,
    /// The candidate must carry the source's HDR10+ messages.
    #[serde(default)]
    pub hdr10_plus: bool,
    /// Hidden lead-in packets the command drops (into Matroska).
    #[serde(default)]
    pub lead_in_trims: Vec<LeadInTrim>,
    /// Source chapter tracks the plan does not copy.
    #[serde(default)]
    pub chapter_tracks: Vec<u32>,
}

impl Expectation {
    pub fn new(plan: &RoutePlan, marker: ProcessedMarker) -> Self {
        Self {
            video_index: plan.video_index,
            codec: codec_of(plan.encoder).into(),
            width: plan.width,
            height: plan.height,
            marker,
            container_changed: plan.container_changed,
            losses: plan.expected_losses.clone(),
            dovi: plan.dynamic_range.dolby_vision().map(|(_, out)| *out),
            hdr10_plus: plan.dynamic_range.hdr10_plus(),
            lead_in_trims: plan.lead_in_trims.clone(),
            chapter_tracks: plan.chapter_tracks.clone(),
        }
    }

    /// Packets dropped from the start of stream `index`.
    fn trimmed(&self, index: u32) -> u64 {
        self.lead_in_trims
            .iter()
            .find(|t| t.stream == index)
            .map_or(0, |t| t.packets)
    }

    fn loses_flag(&self, stream: u32, flag: &str) -> bool {
        self.losses
            .iter()
            .any(|l| l.stream == stream && l.disposition() == Some(flag))
    }
}

fn codec_of(encoder: Encoder) -> &'static str {
    match encoder {
        Encoder::Libx264 | Encoder::H264Qsv | Encoder::H264Vaapi => "h264",
        Encoder::Libx265 | Encoder::HevcQsv | Encoder::HevcVaapi => "hevc",
        Encoder::Av1Qsv | Encoder::Av1Vaapi | Encoder::Libsvtav1 => "av1",
    }
}

#[derive(Default)]
struct Findings(Vec<Finding>);

impl Findings {
    fn add(&mut self, check: &str, severity: Severity, message: impl Into<String>) {
        self.0.push(Finding {
            check: check.into(),
            severity,
            message: message.into(),
        });
    }

    fn hard(&mut self, check: &str, message: impl Into<String>) {
        self.add(check, Severity::Hard, message);
    }

    fn soft(&mut self, check: &str, message: impl Into<String>) {
        self.add(check, Severity::Soft, message);
    }

    fn info(&mut self, check: &str, message: impl Into<String>) {
        self.add(check, Severity::Info, message);
    }

    /// Hard above `hard_ms`, soft above `soft_ms`.
    fn tiered(&mut self, check: &str, delta_ms: f64, soft_ms: f64, hard_ms: f64, what: &str) {
        if delta_ms > hard_ms {
            self.hard(
                check,
                format!("{what}变化 {delta_ms:.1} ms（> {hard_ms} ms）"),
            );
        } else if delta_ms > soft_ms {
            self.soft(
                check,
                format!("{what}变化 {delta_ms:.1} ms（> {soft_ms} ms）"),
            );
        }
    }
}

pub fn verify(
    src: &MediaInfo,
    out: &MediaInfo,
    exp: &Expectation,
    t: &VerifyThresholds,
    inspection: &Inspection,
) -> Report {
    let mut f = Findings::default();
    let duration_delta = src
        .format
        .duration_s
        .zip(out.format.duration_s)
        .map(|(a, b)| (b - a) * 1000.0);
    let report = |f: Findings| Report {
        verdict: verdict(&f.0),
        findings: f.0,
        container_duration_delta_ms: duration_delta,
        inspection: inspection.clone(),
    };

    if src.depth != ProbeDepth::Full || out.depth != ProbeDepth::Full {
        f.hard("probe_depth", "缺少包级探测数据，无法校验");
        return report(f);
    }

    // Chapter tracks are compared as chapters: the source's that the plan
    // leaves out, and the one FFmpeg's MP4 muxer writes for the chapters.
    let src_streams: Vec<_> = src
        .streams
        .iter()
        .filter(|s| !exp.chapter_tracks.contains(&s.index))
        .collect();
    let out_streams: Vec<_> = out
        .streams
        .iter()
        .filter(|s| !out.is_chapter_track(s))
        .collect();
    let same_layout = src_streams.len() == out_streams.len()
        && src_streams
            .iter()
            .zip(&out_streams)
            .all(|(a, b)| a.index == b.index && a.kind == b.kind);
    let pairs: Vec<_> = src_streams.into_iter().zip(out_streams).collect();
    if !exp.chapter_tracks.is_empty() {
        let tracks: Vec<_> = exp.chapter_tracks.iter().map(u32::to_string).collect();
        f.info(
            "chapter_track",
            format!(
                "第 {} 条流是章节文本轨，不复制，章节按容器章节保留",
                tracks.join("、")
            ),
        );
    }
    if same_layout {
        check_streams(&mut f, src, out, &pairs, exp, t, inspection);
    } else {
        f.hard(
            "stream_layout",
            format!(
                "轨道布局不同：源片 [{}]，成品 [{}]",
                layout(src),
                layout(out)
            ),
        );
    }

    check_inspection(&mut f, t, inspection);
    check_chapters(&mut f, src, out, t);
    check_hdr(&mut f, src, out, exp, inspection);
    if out.marker.as_ref() != Some(&exp.marker) {
        f.hard(
            "marker",
            format!("已处理标记应为 {}，实际 {:?}", exp.marker, out.marker),
        );
    }
    check_global_tags(&mut f, src, out);
    let has_sei = |i: &MediaInfo| {
        i.frame_side_data
            .iter()
            .any(|t| t.contains("User Data Unregistered"))
    };
    if has_sei(src) && !has_sei(out) {
        f.info("encoder_sei", "源片的编码器 SEI 没有保留");
    }
    if let Some(delta) = duration_delta.filter(|d| *d != 0.0) {
        f.info(
            "container_duration",
            format!("容器总时长变化 {delta:+.1} ms"),
        );
    }
    report(f)
}

fn check_inspection(f: &mut Findings, t: &VerifyThresholds, i: &Inspection) {
    if let Some(d) = &i.decode {
        if !d.errors.is_empty() {
            let shown: Vec<_> = d.errors.iter().take(3).cloned().collect();
            f.hard(
                "decode",
                format!(
                    "成品视频解码出错（{} 条）：{}",
                    d.errors.len(),
                    shown.join("；")
                ),
            );
        }
    }
    if let Some(worst) = i
        .spot_checks
        .iter()
        .flatten()
        .min_by(|a, b| a.ssim.total_cmp(&b.ssim))
    {
        let what = format!("{:.1}s 处画面抽检 SSIM {:.3}", worst.at_s, worst.ssim);
        if worst.ssim < t.ssim_hard {
            f.hard(
                "spot_check_ssim",
                format!("{what}（< {}），画面可能严重损坏", t.ssim_hard),
            );
        } else if worst.ssim < t.ssim_soft {
            f.soft("spot_check_ssim", format!("{what}（< {}）", t.ssim_soft));
        }
    }
    for h in i.stream_hashes.iter().flatten() {
        if h.source != h.candidate {
            f.hard(
                "stream_hash",
                format!("第 {} 条复制流的数据哈希不一致", h.index),
            );
        }
    }
    for (check, why) in &i.errors {
        f.soft("inspection", format!("{check} 未能执行：{why}"));
    }
}

fn layout(info: &MediaInfo) -> String {
    info.streams
        .iter()
        .map(|s| format!("{}:{:?}", s.index, s.kind).to_lowercase())
        .collect::<Vec<_>>()
        .join(", ")
}

fn check_streams(
    f: &mut Findings,
    src: &MediaInfo,
    out: &MediaInfo,
    pairs: &[(&StreamInfo, &StreamInfo)],
    exp: &Expectation,
    t: &VerifyThresholds,
    inspection: &Inspection,
) {
    let mut time_base_changed = Vec::new();
    let mut tags_changed = Vec::new();
    let mut muxer_tags_changed = Vec::new();

    let start = |info: &MediaInfo, index: u32| {
        info.stream(index)
            .and_then(|s| s.packets.as_ref())
            .and_then(|p| p.start_s)
    };
    // The video starts at its first decoded frame: a recording cut at an
    // open-GOP keyframe begins with leading frames the decoder drops.
    let video_start = |info: &MediaInfo| {
        info.stream(exp.video_index)
            .and_then(|s| s.packets.as_ref())
            .and_then(PacketStats::shown_start_s)
    };
    // Ends are measured from the video start: a container change can move
    // the whole timeline (MPEG-TS starts near 1.4 s, the MKV made from it at
    // 0), and that changes no stream.
    let origin = |info: &MediaInfo| video_start(info).unwrap_or(0.0);
    let (oa, ob) = (origin(src), origin(out));

    for &(a, b) in pairs {
        let n = a.index;
        if a.index == exp.video_index {
            check_video(f, a, b, exp, inspection);
        } else {
            let params = |s: &StreamInfo| s.audio.as_ref().map(|p| (p.sample_rate, p.channels));
            if a.codec_name != b.codec_name || params(a) != params(b) {
                f.hard(
                    "stream_codec",
                    format!(
                        "第 {n} 条流的编码或参数变了：{} → {}",
                        a.codec_name, b.codec_name
                    ),
                );
            }
            let trimmed = exp.trimmed(n);
            let (pa, pb) = (packets(a).saturating_sub(trimmed), packets(b));
            if trimmed > 0 {
                f.info(
                    "lead_in",
                    format!("第 {n} 条流舍弃了开头 {trimmed} 个被编辑列表藏起来的包"),
                );
            }
            if pa != pb {
                f.hard(
                    "packet_count",
                    format!("第 {n} 条流的包数变了：{pa} → {pb}"),
                );
            }
        }

        if a.language != b.language {
            f.hard(
                "stream_language",
                format!("第 {n} 条流的语言变了：{:?} → {:?}", a.language, b.language),
            );
        }
        if a.title != b.title {
            f.hard(
                "stream_title",
                format!("第 {n} 条流的标题变了：{:?} → {:?}", a.title, b.title),
            );
        }
        let mut want = effective_disposition(src, a);
        want.retain(|flag| !exp.loses_flag(n, flag));
        let (want, got) = if exp.container_changed {
            (
                merge_roles(want),
                merge_roles(effective_disposition(out, b)),
            )
        } else {
            (want, effective_disposition(out, b))
        };
        if want != got {
            f.hard(
                "stream_disposition",
                format!("第 {n} 条流的标志与预期不符：应为 {want:?}，实际 {got:?}"),
            );
        }
        if a.time_base != b.time_base {
            time_base_changed.push(n);
        }
        let (ua, ma) = split_stream_tags(a);
        let (ub, mb) = split_stream_tags(b);
        if ua != ub {
            tags_changed.push(n);
        }
        if ma != mb {
            muxer_tags_changed.push(n);
        }

        let end = |s: &StreamInfo| s.packets.as_ref().and_then(|p| p.end_s);
        if let (Some(ea), Some(eb)) = (end(a), end(b)) {
            let delta = ((eb - ob) - (ea - oa)).abs() * 1000.0;
            f.tiered(
                "stream_end",
                delta,
                t.end_soft_ms,
                t.end_hard_ms,
                &format!("第 {n} 条流的结束时间"),
            );
        }
    }

    // Audio start relative to video start, before and after.
    if let (Some(va), Some(vb)) = (video_start(src), video_start(out)) {
        for a in src.streams.iter().filter(|s| s.kind == StreamKind::Audio) {
            // A trimmed lead-in now starts at its last hidden packet.
            let aa = if exp.trimmed(a.index) > 0 {
                a.packets
                    .as_ref()
                    .and_then(|p| p.lead_in)
                    .and_then(|l| l.last_start_s)
            } else {
                start(src, a.index)
            };
            if let (Some(aa), Some(ab)) = (aa, start(out, a.index)) {
                let delta = ((ab - vb) - (aa - va)).abs() * 1000.0;
                f.tiered(
                    "av_offset",
                    delta,
                    t.av_offset_soft_ms,
                    t.av_offset_hard_ms,
                    &format!("第 {} 条音轨相对视频的起点偏移", a.index),
                );
            }
        }
    }

    if !time_base_changed.is_empty() {
        f.info(
            "time_base",
            format!("time base 表示变化的流：{time_base_changed:?}"),
        );
    }
    if !tags_changed.is_empty() {
        f.soft(
            "stream_tags",
            format!("标签内容有变化的流：{tags_changed:?}"),
        );
    }
    if !muxer_tags_changed.is_empty() {
        f.info(
            "muxer_tags",
            format!("封装器写入的标签有变化的流：{muxer_tags_changed:?}"),
        );
    }
}

/// Disposition as players read it: when no stream of a kind is marked default,
/// the first one is. MP4 cannot express "none is default" and marks the first
/// track of each kind, so comparing raw flags would reject every MKV -> MP4.
fn effective_disposition(info: &MediaInfo, s: &StreamInfo) -> BTreeSet<String> {
    let mut d = s.disposition.clone();
    let peers = || {
        info.streams
            .iter()
            .filter(|p| p.kind == s.kind && !p.is_cover_art())
    };
    if !s.is_cover_art()
        && !peers().any(|p| p.has_disposition("default"))
        && peers().next().map(|p| p.index) == Some(s.index)
    {
        d.insert("default".into());
    }
    d
}

/// MP4 stores one accessibility role per track and FFmpeg reports it as two
/// flags (`hearing_impaired` + `captions`, `visual_impaired` + `descriptions`).
/// Across a container change, compare the role, not its spelling.
fn merge_roles(flags: BTreeSet<String>) -> BTreeSet<String> {
    flags
        .into_iter()
        .map(|f| match f.as_str() {
            "captions" => "hearing_impaired".to_owned(),
            "descriptions" => "visual_impaired".to_owned(),
            _ => f,
        })
        .collect()
}

fn check_video(
    f: &mut Findings,
    a: &StreamInfo,
    b: &StreamInfo,
    exp: &Expectation,
    inspection: &Inspection,
) {
    let n = a.index;
    if b.codec_name != exp.codec {
        f.hard(
            "video_codec",
            format!("视频编码应为 {}，实际 {}", exp.codec, b.codec_name),
        );
    }
    let (Some(va), Some(vb)) = (&a.video, &b.video) else {
        return;
    };
    if (vb.width, vb.height) != (exp.width, exp.height) {
        f.hard(
            "video_size",
            format!(
                "分辨率应为 {}x{}，实际 {}x{}",
                exp.width, exp.height, vb.width, vb.height
            ),
        );
    }
    if (va.bit_depth, va.chroma) != (vb.bit_depth, vb.chroma) {
        f.hard(
            "video_format",
            format!("位深或色度采样变了：{:?} → {:?}", va.pix_fmt, vb.pix_fmt),
        );
    }
    let (ca, cb) = (&va.color, &vb.color);
    // Unspecified range on YUV video means limited ("tv") to every player,
    // and MP4 or x264 often leave it unspecified; only `pc` must survive.
    let range = |r: &Option<String>| r.clone().or_else(|| Some("tv".into()));
    let (ra, rb) = (range(&ca.range), range(&cb.range));
    for (what, x, y) in [
        ("primaries", &ca.primaries, &cb.primaries),
        ("transfer", &ca.transfer, &cb.transfer),
        ("matrix", &ca.space, &cb.space),
        ("range", &ra, &rb),
    ] {
        match (x, y) {
            (Some(x), y) if Some(x) != y.as_ref() => {
                f.hard("color", format!("色彩标记 {what} 变了：{x} → {y:?}"))
            }
            (None, Some(y)) => f.soft("color", format!("新增了色彩标记 {what}={y}")),
            _ => {}
        }
    }
    let (pa, pb) = (packets(a), packets(b));
    if pa != pb {
        // Field-coded sources carry one packet per field; the re-encode has
        // one per frame. Decoded frame counts decide.
        match inspection.decoded_frames {
            Some((fa, fb)) if fa == fb => f.info(
                "frame_count",
                format!("第 {n} 条流（视频）包数 {pa} → {pb}，解码帧数一致（{fa}），按场编码处理"),
            ),
            Some((fa, fb)) => f.hard(
                "frame_count",
                format!("第 {n} 条流（视频）的解码帧数变了：{fa} → {fb}"),
            ),
            None => f.hard(
                "frame_count",
                format!("第 {n} 条流（视频）的帧数变了：{pa} → {pb}"),
            ),
        }
    } else {
        check_timeline(f, a, b, va);
    }
}

/// Timeline checkpoints (ADR 0004): with equal frame counts, the k-th frame
/// should be shown at the same time relative to the video start. A shift of
/// more than one frame duration is drift that start and end checks miss.
/// Soft until the observation mode has data to justify hard.
fn check_timeline(f: &mut Findings, a: &StreamInfo, b: &StreamInfo, va: &VideoParams) {
    let (Some(pa), Some(pb)) = (&a.packets, &b.packets) else {
        return;
    };
    let (Some(sa), Some(sb)) = (pa.shown_start_s(), pb.shown_start_s()) else {
        return;
    };
    let frame_s = va
        .avg_frame_rate
        .as_deref()
        .and_then(frame_duration)
        .or_else(|| {
            let span = pa.end_s? - sa;
            (pa.count > 0 && span > 0.0).then(|| span / pa.count as f64)
        });
    let Some(frame_s) = frame_s else { return };
    let worst = pa
        .checkpoints
        .iter()
        .zip(&pb.checkpoints)
        .filter(|(x, y)| x.frame == y.frame)
        .map(|(x, y)| (x.frame, ((y.pts_s - sb) - (x.pts_s - sa)).abs()))
        .max_by(|x, y| x.1.total_cmp(&y.1));
    if let Some((frame, shift)) = worst {
        // Rounding between time bases is far below a frame; the epsilon
        // only absorbs float noise.
        if shift > frame_s + 1e-6 {
            f.soft(
                "timeline",
                format!(
                    "第 {frame} 帧的显示时间偏移 {:.1} ms，超过 1 帧（{:.1} ms）",
                    shift * 1000.0,
                    frame_s * 1000.0
                ),
            );
        }
    }
}

/// Seconds per frame from an `avg_frame_rate` such as `24000/1001`.
fn frame_duration(rate: &str) -> Option<f64> {
    let (n, d) = rate.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (n > 0.0 && d > 0.0).then(|| d / n)
}

fn packets(s: &StreamInfo) -> u64 {
    let p = s.packets.as_ref();
    let count = p.map_or(0, |p| p.count);
    // FFmpeg's MP4 muxer fills the gaps between subtitles with empty
    // samples, more on every remux; only the subtitles themselves count.
    if s.codec_name == "mov_text" {
        count - p.map_or(0, |p| p.empty)
    } else {
        count
    }
}

/// Stream tags that muxers write themselves.
const MUXER_STREAM_TAGS: &[&str] = &[
    "DURATION",
    "ENCODER",
    "HANDLER_NAME",
    "VENDOR_ID",
    "BPS",
    "NUMBER_OF_",
    "_STATISTICS_",
    "CREATION_TIME",
];
/// Tags compared as track properties instead.
const PROPERTY_TAGS: &[&str] = &["LANGUAGE", "TITLE", "NAME"];

/// (user tags, muxer tags) with upper-cased keys, because MKV and MP4 spell
/// them differently.
fn split_stream_tags(s: &StreamInfo) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
    let all = upper_tags(&s.tags, PROPERTY_TAGS);
    all.into_iter()
        .partition(|(k, _)| !MUXER_STREAM_TAGS.iter().any(|p| k.starts_with(p)))
}

fn upper_tags(tags: &BTreeMap<String, String>, ignored: &[&str]) -> BTreeMap<String, String> {
    tags.iter()
        .map(|(k, v)| (k.to_ascii_uppercase(), v.clone()))
        .filter(|(k, _)| !ignored.iter().any(|p| k.starts_with(p)))
        .collect()
}

fn check_global_tags(f: &mut Findings, src: &MediaInfo, out: &MediaInfo) {
    const MUXER: &[&str] = &[
        "ENCODER",
        "MAJOR_BRAND",
        "MINOR_VERSION",
        "COMPATIBLE_BRANDS",
    ];
    let split = |info: &MediaInfo| {
        let mut user = upper_tags(&info.format.tags, &[METADATA_KEY, "KEYWORDS"]);
        let muxer: BTreeMap<_, _> = MUXER
            .iter()
            .filter_map(|k| user.remove(*k).map(|v| (k.to_string(), v)))
            .collect();
        // MP4 keywords carry the marker; compare what is left of them.
        let kw = info
            .format
            .tags
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(MP4_TAG))
            .map(|(_, v)| v.as_str())
            .map(|v| v.find("FNCOMPRESS=").map_or(v, |at| &v[..at]).trim())
            .filter(|v| !v.is_empty());
        if let Some(kw) = kw {
            user.insert("KEYWORDS".into(), kw.into());
        }
        (user, muxer)
    };
    let changed = |a: &BTreeMap<String, String>, b: &BTreeMap<String, String>| -> Vec<String> {
        a.keys()
            .chain(b.keys())
            .filter(|k| a.get(*k) != b.get(*k))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    let ((ua, ma), (ub, mb)) = (split(src), split(out));
    let user = changed(&ua, &ub);
    if !user.is_empty() {
        f.soft("global_tags", format!("全局标签内容有变化：{user:?}"));
    }
    let muxer = changed(&ma, &mb);
    if !muxer.is_empty() {
        f.info(
            "muxer_tags",
            format!("封装器写入的全局标签有变化：{muxer:?}"),
        );
    }
}

fn check_chapters(f: &mut Findings, src: &MediaInfo, out: &MediaInfo, t: &VerifyThresholds) {
    let (a, b) = (&src.chapters, &out.chapters);
    if a.len() != b.len() {
        f.hard("chapters", format!("章节数变了：{} → {}", a.len(), b.len()));
        return;
    }
    let mut worst: f64 = 0.0;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        if x.title != y.title {
            f.hard(
                "chapters",
                format!(
                    "第 {} 个章节的标题变了：{:?} → {:?}",
                    i + 1,
                    x.title,
                    y.title
                ),
            );
        }
        worst = worst
            .max((x.start_s - y.start_s).abs())
            .max((x.end_s - y.end_s).abs());
    }
    f.tiered(
        "chapters",
        worst * 1000.0,
        t.end_soft_ms,
        t.end_hard_ms,
        "章节时间",
    );
}

fn check_hdr(
    f: &mut Findings,
    src: &MediaInfo,
    out: &MediaInfo,
    exp: &Expectation,
    inspection: &Inspection,
) {
    let (a, b) = (&src.hdr, &out.hdr);
    if let Some(md) = &a.mastering_display {
        if !b
            .mastering_display
            .as_ref()
            .is_some_and(|o| same_rationals(md, o))
        {
            f.hard("hdr_metadata", "母版显示元数据丢失或不一致");
        }
    }
    if a.content_light.is_some() && a.content_light != b.content_light {
        f.hard(
            "hdr_metadata",
            format!(
                "内容亮度元数据变了：{:?} → {:?}",
                a.content_light, b.content_light
            ),
        );
    }
    let missing: Vec<_> = a.dynamic_metadata.difference(&b.dynamic_metadata).collect();
    if !missing.is_empty() {
        f.hard("hdr_metadata", format!("动态 HDR 元数据丢失：{missing:?}"));
    }
    // A route converts P7 to P8.1; without one the source's must survive.
    if let Some(dv) = exp.dovi.as_ref().or(a.dolby_vision.as_ref()) {
        let same = |o: &DoviConfig| {
            (o.profile, o.bl_compat_id, o.level) == (dv.profile, dv.bl_compat_id, dv.level)
                && (o.rpu_present, o.el_present, o.bl_present)
                    == (dv.rpu_present, dv.el_present, dv.bl_present)
        };
        if !b.dolby_vision.as_ref().is_some_and(same) {
            f.hard(
                "hdr_metadata",
                format!(
                    "杜比视界配置应为 {}，实际 {}",
                    describe(Some(dv)),
                    describe(b.dolby_vision.as_ref())
                ),
            );
        }
    }
    if a.dovi_rpu && !b.dovi_rpu {
        f.hard("hdr_metadata", "杜比视界 RPU 丢失");
    }
    if exp.dovi.is_some() {
        match &inspection.rpu {
            Some(c) if c.found != c.expected => f.hard(
                "dovi_rpu",
                format!("成品有 {} 个 RPU，应为 {} 个", c.found, c.expected),
            ),
            Some(RpuCheck {
                first_mismatch: Some(n),
                ..
            }) => f.hard("dovi_rpu", format!("第 {} 帧的 RPU 与源片不同", n + 1)),
            Some(_) => {}
            None => f.hard("dovi_rpu", "没能核对成品的 RPU"),
        }
    }
    if exp.hdr10_plus {
        match &inspection.hdr10_plus {
            Some(c) if c.found != c.expected => f.hard(
                "hdr10_plus",
                format!("成品有 {} 帧带 HDR10+，应为 {} 帧", c.found, c.expected),
            ),
            Some(RpuCheck {
                first_mismatch: Some(n),
                ..
            }) => f.hard(
                "hdr10_plus",
                format!("第 {} 帧的 HDR10+ 元数据与源片不同", n + 1),
            ),
            Some(c) => f.info(
                "hdr10_plus",
                format!("保留了 {} 帧 HDR10+ 动态元数据", c.found),
            ),
            None => f.hard("hdr10_plus", "没能核对成品的 HDR10+ 元数据"),
        }
    }
}

fn describe(c: Option<&DoviConfig>) -> String {
    match c {
        None => "无".into(),
        Some(c) => format!(
            "{} level {}{}",
            c.label(),
            c.level,
            if c.el_present {
                "（带增强层）"
            } else {
                ""
            }
        ),
    }
}

/// Mastering display values are rationals whose denominators may differ
/// between container boxes and SEI; compare their values.
fn same_rationals(a: &BTreeMap<String, String>, b: &BTreeMap<String, String>) -> bool {
    let value = |s: &str| -> Option<f64> {
        match s.split_once('/') {
            Some((n, d)) => Some(n.trim().parse::<f64>().ok()? / d.trim().parse::<f64>().ok()?),
            None => s.trim().parse().ok(),
        }
    };
    a.len() == b.len()
        && a.iter().all(|(k, x)| {
            b.get(k)
                .and_then(|y| Some((value(x)?, value(y)?)))
                .is_some_and(|(x, y)| (x - y).abs() <= 1e-4 * x.abs().max(1e-6))
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{
        identity::FileIdentity,
        probe::{
            AudioParams, Chapter, ColorTags, ContentLight, FormatInfo, HdrInfo, PacketStats,
            VideoParams,
        },
    };

    fn marker() -> ProcessedMarker {
        ProcessedMarker {
            version: "0.1.0".into(),
            profile: "movies".into(),
        }
    }

    fn exp() -> Expectation {
        Expectation {
            video_index: 0,
            codec: "hevc".into(),
            width: 1920,
            height: 1080,
            marker: marker(),
            container_changed: false,
            losses: vec![],
            dovi: None,
            hdr10_plus: false,
            lead_in_trims: vec![],
            chapter_tracks: vec![],
        }
    }

    fn stream(
        index: u32,
        kind: StreamKind,
        codec: &str,
        count: u64,
        start: f64,
        end: f64,
    ) -> StreamInfo {
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
            packets: Some(PacketStats {
                count,
                empty: 0,
                start_s: Some(start),
                end_s: Some(end),
                checkpoints: vec![],
                lead_in: None,
                first_frame_s: None,
            }),
        }
    }

    /// Source: h264 video, jpn AAC, chi subtitle, two chapters.
    fn source() -> MediaInfo {
        let video = StreamInfo {
            video: Some(VideoParams {
                width: 1920,
                height: 1080,
                pix_fmt: Some("yuv420p".into()),
                bit_depth: Some(8),
                chroma: Some(crate::probe::Chroma::Yuv420),
                color: ColorTags {
                    primaries: Some("bt709".into()),
                    transfer: Some("bt709".into()),
                    space: Some("bt709".into()),
                    range: Some("tv".into()),
                },
                field_order: Some("progressive".into()),
                avg_frame_rate: None,
            }),
            disposition: BTreeSet::from(["default".to_owned()]),
            ..stream(0, StreamKind::Video, "h264", 1440, 0.0, 60.06)
        };
        let audio = StreamInfo {
            language: Some("jpn".into()),
            audio: Some(AudioParams {
                sample_rate: Some(48000),
                channels: Some(2),
                channel_layout: Some("stereo".into()),
            }),
            ..stream(1, StreamKind::Audio, "aac", 2814, -0.021, 60.43)
        };
        let subs = StreamInfo {
            language: Some("chi".into()),
            ..stream(2, StreamKind::Subtitle, "subrip", 12, 1.0, 58.0)
        };
        MediaInfo {
            identity: FileIdentity {
                dev: 0,
                ino: 0,
                size: 100,
                mtime_ns: 0,
            },
            depth: ProbeDepth::Full,
            format: FormatInfo {
                format_name: "matroska,webm".into(),
                duration_s: Some(60.43),
                size: Some(100),
                bit_rate: None,
                tags: BTreeMap::from([
                    ("title".into(), "Movie".into()),
                    ("ENCODER".into(), "Lavf61".into()),
                ]),
            },
            streams: vec![video, audio, subs],
            chapters: vec![
                Chapter {
                    start_s: 0.0,
                    end_s: 30.0,
                    title: Some("A".into()),
                },
                Chapter {
                    start_s: 30.0,
                    end_s: 60.0,
                    title: Some("B".into()),
                },
            ],
            marker: None,
            primary_video: Some(0),
            hdr: HdrInfo::default(),
            frame_side_data: BTreeSet::new(),
        }
    }

    /// What a correct candidate of `source()` looks like.
    fn candidate() -> MediaInfo {
        let mut out = source();
        out.streams[0].codec_name = "hevc".into();
        out.format.duration_s = Some(60.443);
        out.format.tags.insert("ENCODER".into(), "Lavf62".into());
        out.format
            .tags
            .insert(METADATA_KEY.into(), marker().to_value());
        out.marker = Some(marker());
        out
    }

    fn run(out: &MediaInfo) -> Report {
        verify(
            &source(),
            out,
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        )
    }

    /// The (check, severity) pairs of a report.
    fn checks(r: &Report) -> Vec<(&str, Severity)> {
        r.findings
            .iter()
            .map(|f| (f.check.as_str(), f.severity))
            .collect()
    }

    type Mutation = Box<dyn Fn(&mut MediaInfo)>;

    fn set_packets(s: &mut StreamInfo, f: impl FnOnce(&mut PacketStats)) {
        f(s.packets.as_mut().unwrap());
    }

    #[test]
    fn correct_candidate_passes_and_records_duration() {
        let r = run(&candidate());
        assert_eq!(r.verdict, Verdict::Passed, "{:?}", r.findings);
        assert!((r.container_duration_delta_ms.unwrap() - 13.0).abs() < 1e-6);
    }

    #[test]
    fn timeline_drift_beyond_one_frame_is_soft() {
        let with_checkpoints = |info: &mut MediaInfo, start: f64, shift_at_900: f64| {
            let v = &mut info.streams[0];
            v.video.as_mut().unwrap().avg_frame_rate = Some("24/1".into());
            set_packets(v, |p| {
                p.start_s = Some(start);
                p.checkpoints = (1..=10)
                    .map(|i| {
                        let frame = i * 130;
                        let shift = if frame == 910 { shift_at_900 } else { 0.0 };
                        crate::probe::Checkpoint {
                            frame,
                            pts_s: start + frame as f64 / 24.0 + shift,
                        }
                    })
                    .collect();
            });
        };
        let judge = |out_start: f64, shift: f64| {
            let mut src = source();
            with_checkpoints(&mut src, 0.0, 0.0);
            let mut out = candidate();
            with_checkpoints(&mut out, out_start, shift);
            let r = verify(
                &src,
                &out,
                &exp(),
                &VerifyThresholds::default(),
                &Inspection::default(),
            );
            r.findings
                .into_iter()
                .filter(|f| f.check == "timeline")
                .map(|f| f.severity)
                .collect::<Vec<_>>()
        };
        // A different start is not drift; within a frame (41.7 ms) is fine.
        assert!(judge(0.5, 0.0).is_empty());
        assert!(judge(0.0, 0.040).is_empty());
        assert_eq!(judge(0.5, 0.050), [Severity::Soft]);
        assert_eq!(judge(0.0, -0.2), [Severity::Soft]);
    }

    #[test]
    fn container_duration_is_never_judged() {
        let mut out = candidate();
        out.format.duration_s = Some(75.0);
        assert_eq!(run(&out).verdict, Verdict::Passed);
    }

    #[test]
    fn chapter_tracks_are_compared_as_chapters() {
        let chapter_track = |index| StreamInfo {
            codec_tag: Some("text".into()),
            packets: Some(PacketStats {
                count: 0,
                empty: 0,
                start_s: None,
                end_s: None,
                checkpoints: vec![],
                lead_in: None,
                first_frame_s: None,
            }),
            ..stream(index, StreamKind::Data, "bin_data", 0, 0.0, 0.0)
        };
        let mut src = source();
        src.format.format_name = "mov,mp4,m4a,3gp,3g2,mj2".into();
        src.streams.push(chapter_track(3));
        let e = Expectation {
            chapter_tracks: vec![3],
            ..exp()
        };
        let judge = |out: &MediaInfo, e: &Expectation| {
            let r = verify(
                &src,
                out,
                e,
                &VerifyThresholds::default(),
                &Inspection::default(),
            );
            checks(&r)
                .into_iter()
                .filter(|&(c, s)| s != Severity::Info || c == "chapter_track")
                .map(|(c, s)| (c.to_owned(), s))
                .collect::<Vec<_>>()
        };
        let left_out = ("chapter_track".to_owned(), Severity::Info);

        // Into Matroska: no chapter track, chapters as Matroska chapters.
        let mkv = candidate();
        assert_eq!(judge(&mkv, &e), std::slice::from_ref(&left_out));
        // Kept MP4: the muxer writes its own chapter track.
        let mut mp4 = candidate();
        mp4.format.format_name = src.format.format_name.clone();
        mp4.streams.push(chapter_track(3));
        assert_eq!(judge(&mp4, &e), [left_out]);
        // The chapters themselves are still checked.
        let mut lost = mp4.clone();
        lost.chapters.pop();
        assert!(judge(&lost, &e).contains(&("chapters".to_owned(), Severity::Hard)));

        // Without the plan leaving it out, a missing track is a layout change.
        let plain = Expectation {
            chapter_tracks: vec![],
            ..e.clone()
        };
        assert!(judge(&mkv, &plain).contains(&("stream_layout".to_owned(), Severity::Hard)));
        // An MKV source with chapters into MP4 gains the muxer's chapter track.
        let r = verify(
            &source(),
            &mp4,
            &plain,
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        assert_eq!(r.verdict, Verdict::Passed, "{:?}", r.findings);
    }

    #[test]
    fn content_changes_are_hard() {
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "frame_count",
                Box::new(|o| set_packets(&mut o.streams[0], |p| p.count -= 1)),
            ),
            (
                "packet_count",
                Box::new(|o| set_packets(&mut o.streams[1], |p| p.count += 1)),
            ),
            (
                "stream_layout",
                Box::new(|o| {
                    o.streams.pop();
                }),
            ),
            (
                "stream_layout",
                Box::new(|o| o.streams[2].kind = StreamKind::Data),
            ),
            (
                "video_codec",
                Box::new(|o| o.streams[0].codec_name = "h264".into()),
            ),
            (
                "video_size",
                Box::new(|o| o.streams[0].video.as_mut().unwrap().height = 1088),
            ),
            (
                "video_format",
                Box::new(|o| o.streams[0].video.as_mut().unwrap().bit_depth = Some(10)),
            ),
            (
                "color",
                Box::new(|o| o.streams[0].video.as_mut().unwrap().color.range = Some("pc".into())),
            ),
            (
                "color",
                Box::new(|o| o.streams[0].video.as_mut().unwrap().color.primaries = None),
            ),
            (
                "stream_codec",
                Box::new(|o| o.streams[1].codec_name = "opus".into()),
            ),
            (
                "stream_codec",
                Box::new(|o| o.streams[1].audio.as_mut().unwrap().channels = Some(6)),
            ),
            (
                "stream_language",
                Box::new(|o| o.streams[2].language = None),
            ),
            (
                "stream_title",
                Box::new(|o| o.streams[1].title = Some("Commentary".into())),
            ),
            (
                "stream_disposition",
                Box::new(|o| {
                    o.streams[2].disposition.insert("forced".into());
                }),
            ),
            (
                "chapters",
                Box::new(|o| {
                    o.chapters.pop();
                }),
            ),
            ("chapters", Box::new(|o| o.chapters[1].title = None)),
            ("marker", Box::new(|o| o.marker = None)),
            (
                "marker",
                Box::new(|o| o.marker.as_mut().unwrap().profile = "other".into()),
            ),
        ];
        for (check, mutate) in cases {
            let mut out = candidate();
            mutate(&mut out);
            let r = run(&out);
            assert_eq!(r.verdict, Verdict::Rejected, "{check}: {:?}", r.findings);
            assert!(
                checks(&r).contains(&(check, Severity::Hard)),
                "{check}: {:?}",
                r.findings
            );
        }
    }

    #[test]
    fn default_flags_compare_as_players_read_them() {
        let second_audio = |info: &mut MediaInfo| {
            let mut a = info.streams[1].clone();
            a.index = 3;
            a.language = Some("eng".into());
            info.streams.push(a);
        };
        let mut src = source();
        second_audio(&mut src);

        // MKV -> MP4: no default audio becomes "first audio enabled".
        let mut out = candidate();
        second_audio(&mut out);
        out.streams[1].disposition.insert("default".into());
        let r = verify(
            &src,
            &out,
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        assert_eq!(r.verdict, Verdict::Passed, "{:?}", r.findings);

        // Moving the default to the second audio track is a real change.
        let mut out = candidate();
        second_audio(&mut out);
        out.streams[3].disposition.insert("default".into());
        let r = verify(
            &src,
            &out,
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        let hard: Vec<_> = r
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Hard)
            .collect();
        assert_eq!(hard.len(), 2, "{:?}", r.findings);
        assert!(hard.iter().all(|f| f.check == "stream_disposition"));
    }

    #[test]
    fn av_offset_tiers_follow_thresholds() {
        for (shift_ms, want) in [
            (10.0, None),
            (20.0, Some(Severity::Soft)),
            (50.0, Some(Severity::Hard)),
        ] {
            let mut out = candidate();
            set_packets(&mut out.streams[1], |p| {
                p.start_s = Some(-0.021 + shift_ms / 1000.0)
            });
            let r = run(&out);
            let got = r
                .findings
                .iter()
                .find(|f| f.check == "av_offset")
                .map(|f| f.severity);
            assert_eq!(got, want, "{shift_ms} ms: {:?}", r.findings);
        }
    }

    #[test]
    fn av_offset_is_measured_from_the_first_decoded_frame() {
        // A TS cut at an open-GOP keyframe: three leading B frames (125 ms)
        // come before it and the decoder drops them, so the candidate's
        // video starts at the keyframe.
        let mut src = source();
        set_packets(&mut src.streams[0], |p| {
            p.first_frame_s = p.start_s.map(|v| v + 0.125)
        });
        let mut out = candidate();
        set_packets(&mut out.streams[0], |p| {
            p.start_s = p.start_s.map(|v| v + 0.125)
        });
        let judge = |out: &MediaInfo| {
            verify(
                &src,
                out,
                &exp(),
                &VerifyThresholds::default(),
                &Inspection::default(),
            )
        };
        let r = judge(&out);
        assert!(
            !r.findings
                .iter()
                .any(|f| f.check == "av_offset" || f.check == "stream_end"),
            "{:?}",
            r.findings
        );
        // A real shift of the audio is still caught.
        set_packets(&mut out.streams[1], |p| {
            p.start_s = p.start_s.map(|v| v + 0.05)
        });
        let r = judge(&out);
        assert!(
            r.findings
                .iter()
                .any(|f| f.check == "av_offset" && f.severity == Severity::Hard),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn whole_file_shift_keeps_av_offset() {
        // mp4 -> mkv shifts every stream by the AAC priming (~21 ms).
        let mut out = candidate();
        for s in &mut out.streams {
            set_packets(s, |p| {
                p.start_s = p.start_s.map(|v| v + 0.021);
                p.end_s = p.end_s.map(|v| v + 0.021);
            });
        }
        let r = run(&out);
        assert_eq!(r.verdict, Verdict::Passed, "{:?}", r.findings);
    }

    #[test]
    fn timeline_moved_to_zero_keeps_stream_ends() {
        // MPEG-TS -> MKV as measured: the source starts at 1.442 s, the
        // candidate at 0.
        let mut out = candidate();
        for s in &mut out.streams {
            set_packets(s, |p| {
                p.start_s = p.start_s.map(|v| v - 1.442);
                p.end_s = p.end_s.map(|v| v - 1.442);
            });
        }
        let r = run(&out);
        assert_eq!(r.verdict, Verdict::Passed, "{:?}", r.findings);
    }

    #[test]
    fn shorter_video_is_caught_from_its_start() {
        let mut out = candidate();
        set_packets(&mut out.streams[0], |p| p.end_s = p.end_s.map(|v| v - 2.0));
        let r = run(&out);
        assert!(
            r.findings
                .iter()
                .any(|f| f.check == "stream_end" && f.severity == Severity::Hard),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn stream_end_tiers_follow_thresholds() {
        for (shift, want) in [
            (0.03, None),
            (0.2, Some(Severity::Soft)),
            (1.5, Some(Severity::Hard)),
        ] {
            let mut out = candidate();
            set_packets(&mut out.streams[2], |p| p.end_s = Some(58.0 + shift));
            let r = run(&out);
            let got = r
                .findings
                .iter()
                .find(|f| f.check == "stream_end")
                .map(|f| f.severity);
            assert_eq!(got, want, "{shift}: {:?}", r.findings);
        }
    }

    #[test]
    fn thresholds_come_from_settings() {
        let mut out = candidate();
        set_packets(&mut out.streams[1], |p| p.start_s = Some(-0.021 + 0.05));
        let lenient = VerifyThresholds {
            av_offset_hard_ms: 100.0,
            ..Default::default()
        };
        let r = verify(&source(), &out, &exp(), &lenient, &Inspection::default());
        assert_eq!(r.verdict, Verdict::PassedWithWarnings, "{:?}", r.findings);
    }

    #[test]
    fn representation_is_info_and_content_is_soft() {
        let mut out = candidate();
        for s in &mut out.streams {
            s.time_base = "1/90000".into();
            s.tags.insert("handler_name".into(), "SoundHandler".into());
        }
        let mut src = source();
        src.frame_side_data
            .insert("H.26[45] User Data Unregistered SEI message".into());
        let r = verify(
            &src,
            &out,
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        assert_eq!(
            r.verdict,
            Verdict::Passed,
            "info never warns: {:?}",
            r.findings
        );
        assert_eq!(
            checks(&r).into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([
                ("time_base", Severity::Info),
                ("muxer_tags", Severity::Info),
                ("encoder_sei", Severity::Info),
                ("container_duration", Severity::Info),
            ])
        );

        // Tag contents and new color tags are warnings.
        out.streams[1].tags.insert("comment".into(), "x".into());
        out.format.tags.insert("comment".into(), "new".into());
        out.streams[0].video.as_mut().unwrap().color.space = Some("bt709".into());
        src.streams[0].video.as_mut().unwrap().color.space = None;
        let r = verify(
            &src,
            &out,
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        assert_eq!(r.verdict, Verdict::PassedWithWarnings, "{:?}", r.findings);
        let soft: BTreeSet<_> = checks(&r)
            .into_iter()
            .filter(|(_, s)| *s == Severity::Soft)
            .map(|(c, _)| c)
            .collect();
        assert_eq!(
            soft,
            BTreeSet::from(["stream_tags", "global_tags", "color"])
        );
    }

    #[test]
    fn expected_losses_are_not_failures_but_others_are() {
        let mut src = source();
        src.streams[1].disposition = BTreeSet::from(["default".to_owned(), "original".to_owned()]);
        src.streams[2].disposition = BTreeSet::from(["hearing_impaired".to_owned()]);
        // MKV -> MP4 as measured: `original` dropped, `captions` added.
        let mut out = candidate();
        out.streams[1].disposition = BTreeSet::from(["default".to_owned()]);
        out.streams[2].disposition =
            BTreeSet::from(["hearing_impaired".to_owned(), "captions".to_owned()]);
        let mut e = exp();
        e.container_changed = true;
        e.losses = vec![ExpectedLoss {
            stream: 1,
            property: "disposition:original".into(),
            reason: "MP4 无法写入".into(),
        }];
        let t = VerifyThresholds::default();
        let r = verify(&src, &out, &e, &t, &Inspection::default());
        assert_ne!(r.verdict, Verdict::Rejected, "{:?}", r.findings);

        // Without the expected loss, the same output is rejected.
        let r = verify(&src, &out, &exp(), &t, &Inspection::default());
        assert!(checks(&r).contains(&("stream_disposition", Severity::Hard)));

        // A loss outside the list still rejects.
        out.streams[2].disposition.clear();
        let r = verify(&src, &out, &e, &t, &Inspection::default());
        assert!(
            checks(&r).contains(&("stream_disposition", Severity::Hard)),
            "{:?}",
            r.findings
        );
    }

    #[test]
    fn inspection_results_are_judged() {
        let t = VerifyThresholds::default();
        let judge = |i: Inspection| {
            let r = verify(&source(), &candidate(), &exp(), &t, &i);
            checks(&r)
                .into_iter()
                .filter(|(_, s)| *s != Severity::Info)
                .map(|(c, s)| (c.to_owned(), s))
                .collect::<Vec<_>>()
        };
        let spot = |ssim| SpotCheck {
            at_s: 10.0,
            frames: 48,
            ssim,
        };

        let clean = Inspection {
            decode: Some(DecodeResult {
                hardware: false,
                errors: vec![],
            }),
            spot_checks: Some(vec![spot(0.99), spot(0.97)]),
            ..Default::default()
        };
        assert!(judge(clean).is_empty());

        let broken = Inspection {
            decode: Some(DecodeResult {
                hardware: true,
                errors: vec!["[hevc] Invalid NAL unit size".into()],
            }),
            ..Default::default()
        };
        assert_eq!(judge(broken), [("decode".to_owned(), Severity::Hard)]);

        for (ssim, want) in [(0.85, Severity::Soft), (0.67, Severity::Hard)] {
            let i = Inspection {
                spot_checks: Some(vec![spot(0.99), spot(ssim)]),
                ..Default::default()
            };
            assert_eq!(judge(i), [("spot_check_ssim".to_owned(), want)], "{ssim}");
        }

        let hashes = Inspection {
            stream_hashes: Some(vec![
                StreamHash {
                    index: 1,
                    source: "a".into(),
                    candidate: "a".into(),
                },
                StreamHash {
                    index: 2,
                    source: "b".into(),
                    candidate: "c".into(),
                },
            ]),
            ..Default::default()
        };
        assert_eq!(judge(hashes), [("stream_hash".to_owned(), Severity::Hard)]);

        let failed = Inspection {
            errors: vec![("spot_check_ssim".into(), "ffmpeg 超时".into())],
            ..Default::default()
        };
        assert_eq!(judge(failed), [("inspection".to_owned(), Severity::Soft)]);
    }

    #[test]
    fn unspecified_range_means_limited() {
        let t = VerifyThresholds::default();
        let mut out = candidate();
        out.streams[0].video.as_mut().unwrap().color.range = None;
        let r = verify(&source(), &out, &exp(), &t, &Inspection::default());
        assert_eq!(
            r.verdict,
            Verdict::Passed,
            "tv -> unspecified: {:?}",
            r.findings
        );

        let mut src = source();
        src.streams[0].video.as_mut().unwrap().color.range = Some("pc".into());
        let r = verify(&src, &out, &exp(), &t, &Inspection::default());
        assert!(
            checks(&r).contains(&("color", Severity::Hard)),
            "pc -> unspecified"
        );
    }

    #[test]
    fn decoded_frames_decide_when_packet_counts_differ() {
        let mut out = candidate();
        // Field-coded source: 2880 field packets for 1440 frames.
        let mut src = source();
        set_packets(&mut src.streams[0], |p| p.count = 2880);
        set_packets(&mut out.streams[0], |p| p.count = 1440);
        let t = VerifyThresholds::default();
        let run = |frames| {
            let i = Inspection {
                decoded_frames: frames,
                ..Default::default()
            };
            verify(&src, &out, &exp(), &t, &i)
        };
        let same = run(Some((1440, 1440)));
        assert_eq!(same.verdict, Verdict::Passed, "{:?}", same.findings);
        assert!(checks(&same).contains(&("frame_count", Severity::Info)));
        assert!(checks(&run(Some((1440, 1439)))).contains(&("frame_count", Severity::Hard)));
        assert!(checks(&run(None)).contains(&("frame_count", Severity::Hard)));
    }

    #[test]
    fn mov_text_empty_samples_do_not_count() {
        let mut src = source();
        let mut out = candidate();
        for info in [&mut src, &mut out] {
            info.streams[2].codec_name = "mov_text".into();
        }
        set_packets(&mut src.streams[2], |p| (p.count, p.empty) = (39, 29));
        set_packets(&mut out.streams[2], |p| (p.count, p.empty) = (76, 66));
        let t = VerifyThresholds::default();
        let i = Inspection::default();
        let r = verify(&src, &out, &exp(), &t, &i);
        assert_eq!(r.verdict, Verdict::Passed, "{:?}", r.findings);
        set_packets(&mut out.streams[2], |p| p.empty = 67);
        let r = verify(&src, &out, &exp(), &t, &i);
        assert!(checks(&r).contains(&("packet_count", Severity::Hard)));
    }

    #[test]
    fn mp4_keywords_marker_is_not_a_tag_change() {
        let mut src = source();
        src.format.tags.insert("keywords".into(), "anime".into());
        let mut out = candidate();
        out.format.tags.remove(METADATA_KEY);
        out.format
            .tags
            .insert("keywords".into(), marker().mp4_keywords(Some("anime")));
        let r = verify(
            &src,
            &out,
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        assert_eq!(r.verdict, Verdict::Passed, "{:?}", r.findings);
    }

    #[test]
    fn hdr_metadata_loss_is_hard() {
        let mut src = source();
        src.hdr.mastering_display = Some(BTreeMap::from([
            ("red_x".to_owned(), "34000/50000".to_owned()),
            ("max_luminance".to_owned(), "10000000/10000".to_owned()),
        ]));
        src.hdr.content_light = Some(ContentLight {
            max_content: 1000,
            max_average: 400,
        });
        let mut out = candidate();
        // Same values, different denominators (container box vs SEI).
        out.hdr.mastering_display = Some(BTreeMap::from([
            ("red_x".to_owned(), "0.68".to_owned()),
            ("max_luminance".to_owned(), "1000/1".to_owned()),
        ]));
        out.hdr.content_light = src.hdr.content_light;
        let ok = verify(
            &src,
            &out,
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        assert_eq!(ok.verdict, Verdict::Passed, "{:?}", ok.findings);

        out.hdr.content_light = None;
        let r = verify(
            &src,
            &out,
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        assert!(checks(&r).contains(&("hdr_metadata", Severity::Hard)));

        let mut dv_src = source();
        dv_src.hdr.dovi_rpu = true;
        let r = verify(
            &dv_src,
            &candidate(),
            &exp(),
            &VerifyThresholds::default(),
            &Inspection::default(),
        );
        assert!(checks(&r).contains(&("hdr_metadata", Severity::Hard)));
    }

    #[test]
    fn dolby_vision_follows_the_plan() {
        let cfg = |profile, compat, el| DoviConfig {
            version_major: 1,
            version_minor: 0,
            profile,
            level: 6,
            rpu_present: true,
            el_present: el,
            bl_present: true,
            bl_compat_id: compat,
            md_compression: 0,
        };
        let mut src = source();
        src.hdr.dolby_vision = Some(cfg(7, 6, true));
        src.hdr.dovi_rpu = true;
        let mut out = candidate();
        out.hdr.dolby_vision = Some(cfg(8, 1, false));
        out.hdr.dovi_rpu = true;
        let e = Expectation {
            dovi: Some(cfg(8, 1, false)),
            ..exp()
        };
        let rpu = |found, first_mismatch| Inspection {
            rpu: Some(RpuCheck {
                expected: 48,
                found,
                first_mismatch,
            }),
            ..Default::default()
        };
        let judge = |out: &MediaInfo, i: &Inspection| -> Vec<(String, Severity)> {
            let r = verify(&src, out, &e, &VerifyThresholds::default(), i);
            checks(&r)
                .into_iter()
                .filter(|(_, s)| *s != Severity::Info)
                .map(|(c, s)| (c.to_owned(), s))
                .collect()
        };
        let rpu_hard = vec![("dovi_rpu".to_owned(), Severity::Hard)];
        assert_eq!(judge(&out, &rpu(48, None)), []);
        assert_eq!(judge(&out, &rpu(47, None)), rpu_hard);
        assert_eq!(judge(&out, &rpu(48, Some(3))), rpu_hard);
        assert_eq!(judge(&out, &Inspection::default()), rpu_hard);
        // The P7 configuration surviving unconverted is as wrong as losing it.
        for wrong in [Some(cfg(7, 6, true)), None] {
            let mut o = out.clone();
            o.hdr.dolby_vision = wrong;
            assert_eq!(
                judge(&o, &rpu(48, None)),
                [("hdr_metadata".to_owned(), Severity::Hard)]
            );
        }
    }

    #[test]
    fn hdr10_plus_is_checked_frame_for_frame() {
        let plus = "HDR Dynamic Metadata SMPTE2094-40 (HDR10+)".to_owned();
        let mut src = source();
        src.hdr.dynamic_metadata.insert(plus.clone());
        let mut out = candidate();
        out.hdr.dynamic_metadata.insert(plus);
        let e = Expectation {
            hdr10_plus: true,
            ..exp()
        };
        let judge = |found: Option<u64>, first_mismatch: Option<u64>| {
            let i = Inspection {
                hdr10_plus: found.map(|found| RpuCheck {
                    expected: 48,
                    found,
                    first_mismatch,
                }),
                ..Default::default()
            };
            checks(&verify(&src, &out, &e, &VerifyThresholds::default(), &i))
                .into_iter()
                .filter(|(c, _)| *c == "hdr10_plus")
                .map(|(_, s)| s)
                .collect::<Vec<_>>()
        };
        assert_eq!(judge(Some(48), None), [Severity::Info]);
        assert_eq!(judge(Some(47), Some(47)), [Severity::Hard]);
        assert_eq!(judge(Some(48), Some(3)), [Severity::Hard]);
        assert_eq!(judge(None, None), [Severity::Hard]);
    }

    #[test]
    fn quick_probes_cannot_be_verified() {
        let mut out = candidate();
        out.depth = ProbeDepth::Quick;
        assert_eq!(checks(&run(&out)), [("probe_depth", Severity::Hard)]);
    }

    #[test]
    fn verdict_from_findings() {
        let finding = |severity| Finding {
            check: "c".into(),
            severity,
            message: "m".into(),
        };
        assert_eq!(verdict(&[]), Verdict::Passed);
        assert_eq!(
            verdict(&[finding(Severity::Soft)]),
            Verdict::PassedWithWarnings
        );
        assert_eq!(
            verdict(&[finding(Severity::Soft), finding(Severity::Hard)]),
            Verdict::Rejected
        );
    }
}
