//! Turning ffprobe output into [`MediaInfo`]. Kept free of process calls so
//! it can be tested against fixed output.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use super::{
    pix_fmt_layout, AudioParams, Chapter, Checkpoint, ColorTags, ContentLight, DoviConfig,
    FormatInfo, HdrInfo, LeadIn, MediaInfo, PacketStats, ProbeDepth, StreamInfo, StreamKind,
    VideoParams, CHECKPOINTS,
};
use crate::{identity::FileIdentity, marker::ProcessedMarker};

const DOVI_CONFIG: &str = "DOVI configuration record";
const MASTERING_DISPLAY: &str = "Mastering display metadata";
const CONTENT_LIGHT: &str = "Content light level metadata";

/// Parses `-show_format -show_streams -show_chapters` JSON.
pub fn parse_metadata(json: &str, identity: FileIdentity) -> Result<MediaInfo, String> {
    let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;

    let format = root.get("format").ok_or("缺少 format")?;
    let format_tags = tags(format);
    let marker = ProcessedMarker::find(&format_tags);
    let format = FormatInfo {
        format_name: str_field(format, "format_name").unwrap_or_default(),
        duration_s: num_field(format, "duration"),
        size: num_field(format, "size").map(|v| v as u64),
        bit_rate: num_field(format, "bit_rate").map(|v| v as u64),
        tags: format_tags,
    };

    let mut hdr = HdrInfo::default();
    let mut streams = Vec::new();
    for s in array(&root, "streams") {
        streams.push(parse_stream(s));
    }
    let primary_video = streams
        .iter()
        .find(|s| s.kind == StreamKind::Video && !s.is_cover_art())
        .map(|s| s.index);
    if let Some(primary) = primary_video {
        let raw = array(&root, "streams")
            .find(|s| int_field(s, "index") == Some(primary as i64))
            .expect("primary stream came from this list");
        for sd in array(raw, "side_data_list") {
            apply_side_data(&mut hdr, sd);
        }
    }

    let chapters = array(&root, "chapters")
        .map(|c| Chapter {
            start_s: num_field(c, "start_time").unwrap_or(0.0),
            end_s: num_field(c, "end_time").unwrap_or(0.0),
            title: lookup(&tags(c), "title").map(str::to_owned),
        })
        .collect();

    Ok(MediaInfo {
        identity,
        depth: ProbeDepth::Quick,
        format,
        streams,
        chapters,
        marker,
        primary_video,
        hdr,
        frame_side_data: BTreeSet::new(),
    })
}

fn parse_stream(s: &Value) -> StreamInfo {
    let kind = match str_field(s, "codec_type").as_deref() {
        Some("video") => StreamKind::Video,
        Some("audio") => StreamKind::Audio,
        Some("subtitle") => StreamKind::Subtitle,
        Some("attachment") => StreamKind::Attachment,
        Some("data") => StreamKind::Data,
        _ => StreamKind::Unknown,
    };
    let tags = tags(s);
    let disposition = s
        .get("disposition")
        .and_then(Value::as_object)
        .map(|d| {
            d.iter()
                .filter(|(_, v)| v.as_i64() == Some(1))
                .map(|(k, _)| k.clone())
                .collect()
        })
        .unwrap_or_default();

    let video = (kind == StreamKind::Video).then(|| {
        let pix_fmt = str_field(s, "pix_fmt");
        let layout = pix_fmt.as_deref().and_then(pix_fmt_layout);
        let raw_depth = num_field(s, "bits_per_raw_sample")
            .map(|d| d as u8)
            .filter(|d| *d > 0);
        VideoParams {
            width: int_field(s, "width").unwrap_or(0) as u32,
            height: int_field(s, "height").unwrap_or(0) as u32,
            bit_depth: layout.map(|(d, _)| d).or(raw_depth),
            chroma: layout.map(|(_, c)| c),
            pix_fmt,
            color: ColorTags {
                primaries: color_value(s, "color_primaries"),
                transfer: color_value(s, "color_transfer"),
                space: color_value(s, "color_space"),
                range: color_value(s, "color_range"),
            },
            field_order: str_field(s, "field_order").filter(|f| f != "unknown"),
            avg_frame_rate: str_field(s, "avg_frame_rate").filter(|r| r != "0/0"),
        }
    });
    let audio = (kind == StreamKind::Audio).then(|| AudioParams {
        sample_rate: num_field(s, "sample_rate").map(|v| v as u32),
        channels: int_field(s, "channels").map(|v| v as u32),
        channel_layout: str_field(s, "channel_layout"),
    });

    StreamInfo {
        index: int_field(s, "index").unwrap_or(0) as u32,
        kind,
        codec_name: str_field(s, "codec_name").unwrap_or_default(),
        codec_tag: str_field(s, "codec_tag_string").filter(|t| !t.starts_with("[0]")),
        profile: str_field(s, "profile"),
        time_base: str_field(s, "time_base").unwrap_or_default(),
        language: lookup(&tags, "language")
            .filter(|l| *l != "und")
            .map(str::to_owned),
        // MP4 stores a track title as `name`.
        title: lookup(&tags, "title")
            .or_else(|| lookup(&tags, "name"))
            .map(str::to_owned),
        tags,
        disposition,
        video,
        audio,
        side_data: array(s, "side_data_list")
            .filter_map(|sd| str_field(sd, "side_data_type"))
            .collect(),
        packets: None,
    }
}

/// Folds `-show_entries frame_side_data_list` JSON for the primary video into
/// `info`.
pub fn apply_frame_side_data(info: &mut MediaInfo, json: &str) -> Result<(), String> {
    let root: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    for frame in array(&root, "frames") {
        for sd in array(frame, "side_data_list") {
            if let Some(t) = str_field(sd, "side_data_type") {
                info.frame_side_data.insert(t);
            }
            apply_side_data(&mut info.hdr, sd);
        }
    }
    Ok(())
}

/// Stream-level side data (MKV/MP4 boxes) and frame-level SEI describe the
/// same things; the first one seen wins.
fn apply_side_data(hdr: &mut HdrInfo, sd: &Value) {
    let Some(kind) = str_field(sd, "side_data_type") else {
        return;
    };
    match kind.as_str() {
        DOVI_CONFIG => {
            let flag = |k| int_field(sd, k) == Some(1);
            let byte = |k| int_field(sd, k).unwrap_or(0) as u8;
            let compression = match str_field(sd, "dv_md_compression").as_deref() {
                Some("limited") => 1,
                Some("reserved") => 2,
                Some("extended") => 3,
                _ => byte("dv_md_compression"),
            };
            hdr.dolby_vision.get_or_insert(DoviConfig {
                version_major: int_field(sd, "dv_version_major").unwrap_or(1) as u8,
                version_minor: byte("dv_version_minor"),
                profile: byte("dv_profile"),
                level: byte("dv_level"),
                rpu_present: flag("rpu_present_flag"),
                el_present: flag("el_present_flag"),
                bl_present: flag("bl_present_flag"),
                bl_compat_id: byte("dv_bl_signal_compatibility_id"),
                md_compression: compression,
            });
        }
        MASTERING_DISPLAY => {
            if hdr.mastering_display.is_none() {
                let fields = sd
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(k, _)| *k != "side_data_type")
                    .map(|(k, v)| (k.clone(), scalar(v)))
                    .collect();
                hdr.mastering_display = Some(fields);
            }
        }
        CONTENT_LIGHT => {
            hdr.content_light.get_or_insert(ContentLight {
                max_content: int_field(sd, "max_content").unwrap_or(0) as u32,
                max_average: int_field(sd, "max_average").unwrap_or(0) as u32,
            });
        }
        t if t.contains("Dynamic Metadata") || t.contains("HDR10+") => {
            hdr.dynamic_metadata.insert(kind);
        }
        t if t.starts_with("Dolby Vision") => hdr.dovi_rpu = true,
        _ => {}
    }
}

/// Accumulates `-show_entries packet=stream_index,pts,dts,duration,size,flags`
/// lines in `compact=p=0:nk=0` form, e.g.
/// `stream_index=1|pts=-23|dts=-23|duration=N/A|size=2|flags=KD_`.
pub struct PacketAccumulator {
    time_bases: HashMap<u32, (i64, i64)>,
    stats: BTreeMap<u32, Acc>,
    /// Stream whose packet timestamps are kept for timeline checkpoints.
    timeline: Option<u32>,
    timestamps: Vec<i64>,
}

#[derive(Default)]
struct Acc {
    count: u64,
    empty: u64,
    start: Option<i64>,
    end: Option<i64>,
    /// Hidden packets before the first shown one, and the last one's time.
    lead_in: u64,
    lead_in_last: Option<i64>,
    shown: bool,
}

impl PacketAccumulator {
    pub fn new(streams: &[StreamInfo], timeline: Option<u32>) -> Self {
        Self {
            time_bases: streams
                .iter()
                .filter_map(|s| Some((s.index, parse_rational(&s.time_base)?)))
                .collect(),
            stats: BTreeMap::new(),
            timeline,
            timestamps: Vec::new(),
        }
    }

    pub fn push_line(&mut self, line: &str) {
        let mut index = None;
        let (mut pts, mut dts, mut duration, mut size) = (None, None, None, None);
        let mut discard = false;
        for field in line.split('|') {
            let Some((key, value)) = field.split_once('=') else {
                continue;
            };
            if key == "flags" {
                discard = value.contains('D');
                continue;
            }
            let value = value.parse::<i64>().ok();
            match key {
                "stream_index" => index = value,
                "pts" => pts = value,
                "dts" => dts = value,
                "duration" => duration = value,
                "size" => size = value,
                _ => {}
            }
        }
        let Some(index) = index else { return };
        // An edit list hides the video frames before a stream-copy cut
        // (flag D); the decoder drops them, so they are not part of the
        // video. Other streams keep them: a stream copy carries them over
        // and the comparisons see them on both sides.
        if discard && self.timeline == Some(index as u32) {
            return;
        }
        let acc = self.stats.entry(index as u32).or_default();
        acc.count += 1;
        if !acc.shown {
            if discard {
                acc.lead_in += 1;
                acc.lead_in_last = pts.or(dts);
            } else {
                acc.shown = true;
            }
        }
        if size.is_some_and(|s| s <= 2) {
            acc.empty += 1;
        }
        if let Some(ts) = pts.or(dts) {
            acc.start = Some(acc.start.map_or(ts, |s| s.min(ts)));
            let end = ts + duration.unwrap_or(0);
            acc.end = Some(acc.end.map_or(end, |e| e.max(end)));
            if self.timeline == Some(index as u32) {
                self.timestamps.push(ts);
            }
        }
    }

    pub fn finish(mut self, streams: &mut [StreamInfo]) {
        // Packets arrive in decode order; sorted timestamps are the frames
        // in presentation order.
        self.timestamps.sort_unstable();
        for s in streams {
            let acc = self.stats.get(&s.index);
            let tb = self.time_bases.get(&s.index).copied();
            let secs = |ticks: Option<i64>| {
                let (num, den) = tb?;
                Some(ticks? as f64 * num as f64 / den as f64)
            };
            let checkpoints = if self.timeline == Some(s.index) {
                checkpoint_frames(self.timestamps.len() as u64)
                    .filter_map(|frame| {
                        let pts_s = secs(self.timestamps.get(frame as usize).copied())?;
                        Some(Checkpoint { frame, pts_s })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            s.packets = Some(PacketStats {
                count: acc.map_or(0, |a| a.count),
                empty: acc.map_or(0, |a| a.empty),
                start_s: secs(acc.and_then(|a| a.start)),
                end_s: secs(acc.and_then(|a| a.end)),
                checkpoints,
                lead_in: acc.filter(|a| a.lead_in > 0).map(|a| LeadIn {
                    packets: a.lead_in,
                    last_start_s: secs(a.lead_in_last),
                }),
                first_frame_s: None,
            });
        }
    }
}

/// Sets the primary video's first shown frame from `-show_entries
/// frame=pts,best_effort_timestamp` in `csv=p=0` form. The decoder outputs
/// frames in presentation order, but the earliest is taken anyway.
pub fn apply_first_frame(info: &mut MediaInfo, index: u32, csv: &str) {
    let Some(s) = info.streams.iter_mut().find(|s| s.index == index) else {
        return;
    };
    let Some((num, den)) = parse_rational(&s.time_base) else {
        return;
    };
    let first = csv
        .lines()
        .filter_map(|l| {
            let mut fields = l.trim().trim_end_matches(',').split(',');
            let pts = fields.next()?.parse::<i64>().ok();
            pts.or_else(|| fields.next()?.parse().ok())
        })
        .min();
    if let (Some(p), Some(first)) = (s.packets.as_mut(), first) {
        p.first_frame_s = Some(first as f64 * num as f64 / den as f64);
    }
}

/// Frame numbers of the checkpoints: evenly spaced inside the video, first
/// and last frame excluded (stream start and end are checked separately).
fn checkpoint_frames(frames: u64) -> impl Iterator<Item = u64> {
    let mut last = None;
    (1..=CHECKPOINTS)
        .map(move |i| frames * i / (CHECKPOINTS + 1))
        .filter(move |&k| k > 0 && last.replace(k) != Some(k))
}

fn parse_rational(s: &str) -> Option<(i64, i64)> {
    let (n, d) = s.split_once('/')?;
    let (n, d) = (n.parse().ok()?, d.parse().ok()?);
    (d != 0).then_some((n, d))
}

fn array<'a>(v: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter())
        .into_iter()
        .flatten()
}

fn tags(v: &Value) -> BTreeMap<String, String> {
    v.get("tags")
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), scalar(v))).collect())
        .unwrap_or_default()
}

/// Case-insensitive tag lookup: MKV writes `TITLE`, MP4 writes `title`.
fn lookup<'a>(tags: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    tags.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).map(scalar).filter(|s| !s.is_empty())
}

/// ffprobe prints some numbers as strings (`"size": "73655"`).
fn num_field(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn int_field(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn color_value(v: &Value, key: &str) -> Option<String> {
    str_field(v, key).filter(|s| !matches!(s.as_str(), "unknown" | "unspecified" | "reserved"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Chroma, DynamicRange};

    const ID: FileIdentity = FileIdentity {
        dev: 1,
        ino: 2,
        size: 3,
        mtime_ns: 4,
    };

    /// Trimmed from real ffprobe 8.1 output: HEVC 10-bit PQ with a cover-art
    /// stream ahead of it, AAC, PGS, a font attachment and two chapters.
    const MKV: &str = r#"{
      "streams": [
        {"index": 0, "codec_name": "mjpeg", "codec_type": "video", "width": 600, "height": 900,
         "pix_fmt": "yuvj420p", "time_base": "1/90000",
         "disposition": {"default": 0, "attached_pic": 1}, "tags": {"filename": "cover.jpg"}},
        {"index": 1, "codec_name": "hevc", "profile": "Main 10", "codec_type": "video",
         "codec_tag_string": "[0][0][0][0]", "width": 3840, "height": 2160,
         "pix_fmt": "yuv420p10le", "color_range": "tv", "color_space": "bt2020nc",
         "color_transfer": "smpte2084", "color_primaries": "bt2020", "field_order": "progressive",
         "avg_frame_rate": "24000/1001", "time_base": "1/1000",
         "disposition": {"default": 1, "forced": 0, "attached_pic": 0},
         "tags": {"DURATION": "00:00:02.000000000"},
         "side_data_list": [
           {"side_data_type": "DOVI configuration record", "dv_version_major": 1, "dv_version_minor": 0,
            "dv_profile": 8, "dv_level": 6, "rpu_present_flag": 1, "el_present_flag": 0,
            "bl_present_flag": 1, "dv_bl_signal_compatibility_id": 1}
         ]},
        {"index": 2, "codec_name": "aac", "codec_type": "audio", "sample_rate": "48000",
         "channels": 6, "channel_layout": "5.1", "time_base": "1/1000",
         "disposition": {"default": 1, "forced": 0}, "tags": {"language": "jpn", "title": "Main"}},
        {"index": 3, "codec_name": "hdmv_pgs_subtitle", "codec_type": "subtitle", "time_base": "1/1000",
         "disposition": {"default": 0, "forced": 1}, "tags": {"language": "chi"}},
        {"index": 4, "codec_name": "ttf", "codec_type": "attachment", "time_base": "1/90000",
         "disposition": {"default": 0}, "tags": {"filename": "a.ttf", "mimetype": "font/ttf"}}
      ],
      "chapters": [
        {"id": 1, "time_base": "1/1000000000", "start": 0, "start_time": "0.000000", "end": 1000000000,
         "end_time": "1.000000", "tags": {"title": "Opening"}},
        {"id": 2, "time_base": "1/1000000000", "start": 1000000000, "start_time": "1.000000",
         "end": 2000000000, "end_time": "2.000000"}
      ],
      "format": {"format_name": "matroska,webm", "duration": "2.023000", "size": "73655",
                 "bit_rate": "253654", "tags": {"FNCOMPRESS": "1;v=0.1.0;profile=movies", "ENCODER": "Lavf62"}}
    }"#;

    const FRAMES: &str = r#"{"frames": [
      {"side_data_list": [
        {"side_data_type": "H.26[45] User Data Unregistered SEI message"},
        {"side_data_type": "Mastering display metadata", "red_x": "34000/50000", "max_luminance": "10000000/10000"},
        {"side_data_type": "Content light level metadata", "max_content": 1000, "max_average": 400},
        {"side_data_type": "HDR Dynamic Metadata SMPTE2094-40 (HDR10+)", "application version": 1},
        {"side_data_type": "Dolby Vision RPU Data"}
      ]},
      {"side_data_list": [
        {"side_data_type": "Mastering display metadata", "red_x": "1/1", "max_luminance": "1/1"}
      ]}
    ]}"#;

    #[test]
    fn parses_streams_tags_and_chapters() {
        let info = parse_metadata(MKV, ID).unwrap();
        assert_eq!(info.identity, ID);
        assert_eq!(info.streams.len(), 5);
        assert_eq!(
            info.primary_video,
            Some(1),
            "cover art is not the primary video"
        );
        assert_eq!(info.marker.as_ref().unwrap().profile, "movies");
        assert_eq!(info.format.size, Some(73655));
        assert_eq!(info.chapters.len(), 2);
        assert_eq!(info.chapters[0].title.as_deref(), Some("Opening"));

        let v = info.primary_video_stream().unwrap().video.as_ref().unwrap();
        assert_eq!((v.width, v.height, v.bit_depth), (3840, 2160, Some(10)));
        assert_eq!(v.chroma, Some(Chroma::Yuv420));
        assert_eq!(v.color.transfer.as_deref(), Some("smpte2084"));
        assert!(!v.is_interlaced());
        assert_eq!(info.streams[1].codec_tag, None);

        let a = &info.streams[2];
        assert_eq!(a.kind, StreamKind::Audio);
        assert_eq!(a.language.as_deref(), Some("jpn"));
        assert_eq!(a.title.as_deref(), Some("Main"));
        assert!(a.has_disposition("default"));
        assert_eq!(a.audio.as_ref().unwrap().sample_rate, Some(48000));

        assert!(info.streams[3].has_disposition("forced"));
        assert_eq!(info.streams[4].kind, StreamKind::Attachment);
    }

    #[test]
    fn stream_level_dovi_config_is_read() {
        let info = parse_metadata(MKV, ID).unwrap();
        let dv = info.hdr.dolby_vision.unwrap();
        assert_eq!((dv.profile, dv.level, dv.bl_compat_id), (8, 6, 1));
        assert!(dv.rpu_present && dv.bl_present && !dv.el_present);
        assert_eq!(info.dynamic_range(), DynamicRange::DolbyVision);
    }

    #[test]
    fn frame_side_data_fills_hdr_info() {
        let mut info = parse_metadata(MKV, ID).unwrap();
        apply_frame_side_data(&mut info, FRAMES).unwrap();
        let md = info.hdr.mastering_display.as_ref().unwrap();
        assert_eq!(md["red_x"], "34000/50000", "first frame wins");
        assert_eq!(
            info.hdr.content_light,
            Some(ContentLight {
                max_content: 1000,
                max_average: 400
            })
        );
        assert!(info.hdr.has_hdr10_plus());
        assert!(info.hdr.dovi_rpu);
        assert!(info
            .frame_side_data
            .contains("H.26[45] User Data Unregistered SEI message"));
    }

    #[test]
    fn dynamic_range_from_transfer() {
        let mut info = parse_metadata(MKV, ID).unwrap();
        info.hdr.dolby_vision = None;
        assert_eq!(info.dynamic_range(), DynamicRange::Hdr10);
        let color = &mut info.streams[1].video.as_mut().unwrap().color;
        color.transfer = Some("arib-std-b67".into());
        assert_eq!(info.dynamic_range(), DynamicRange::Hlg);
        info.streams[1].video.as_mut().unwrap().color.transfer = Some("bt709".into());
        assert_eq!(info.dynamic_range(), DynamicRange::Sdr);
        info.hdr
            .dynamic_metadata
            .insert("HDR Dynamic Metadata CUVA".into());
        assert_eq!(info.dynamic_range(), DynamicRange::DynamicHdr);
    }

    #[test]
    fn unspecified_colors_are_none() {
        let json = r#"{"format": {}, "streams": [{"index": 0, "codec_type": "video",
            "color_primaries": "unknown", "color_transfer": "unspecified", "color_range": "tv",
            "field_order": "tt", "pix_fmt": "pal8", "bits_per_raw_sample": "8"}]}"#;
        let info = parse_metadata(json, ID).unwrap();
        let v = info.streams[0].video.as_ref().unwrap();
        assert_eq!(v.color.primaries, None);
        assert_eq!(v.color.transfer, None);
        assert_eq!(v.color.range.as_deref(), Some("tv"));
        assert!(v.is_interlaced());
        assert_eq!((v.bit_depth, v.chroma), (Some(8), None));
    }

    #[test]
    fn packets_give_counts_starts_and_ends() {
        let mut info = parse_metadata(MKV, ID).unwrap();
        let mut acc = PacketAccumulator::new(&info.streams, Some(1));
        for line in [
            "stream_index=1|pts=0|dts=N/A|duration=40",
            "stream_index=1|pts=80|dts=N/A|duration=40",
            "stream_index=2|pts=-23|dts=-23|duration=N/A|size=2",
            "stream_index=1|pts=40|dts=0|duration=40",
            "stream_index=2|pts=N/A|dts=0|duration=23",
            "garbage",
        ] {
            acc.push_line(line);
        }
        acc.finish(&mut info.streams);
        let v = info.streams[1].packets.as_ref().unwrap();
        assert_eq!(v.count, 3);
        assert_eq!(v.start_s, Some(0.0));
        assert_eq!(v.end_s, Some(0.12));
        // Presentation order, not decode order.
        assert_eq!(
            v.checkpoints,
            vec![
                Checkpoint {
                    frame: 1,
                    pts_s: 0.04
                },
                Checkpoint {
                    frame: 2,
                    pts_s: 0.08
                },
            ]
        );
        let a = info.streams[2].packets.as_ref().unwrap();
        assert_eq!((a.count, a.empty), (2, 1));
        assert_eq!(a.start_s, Some(-0.023));
        assert_eq!(a.end_s, Some(0.023));
        assert!(a.checkpoints.is_empty());
        assert_eq!(info.streams[3].packets.as_ref().unwrap().count, 0);
    }

    #[test]
    fn hidden_video_frames_are_not_part_of_the_video() {
        let mut info = parse_metadata(MKV, ID).unwrap();
        let mut acc = PacketAccumulator::new(&info.streams, Some(1));
        for line in [
            "stream_index=1|pts=-80|dts=-120|duration=40|flags=KD_",
            "stream_index=2|pts=-46|dts=-46|duration=23|size=300|flags=KD_",
            "stream_index=1|pts=-40|dts=-80|duration=40|flags=_D_",
            "stream_index=1|pts=0|dts=-40|duration=40|flags=___",
            "stream_index=2|pts=-23|dts=-23|duration=23|size=300|flags=K__",
            "stream_index=1|pts=40|dts=0|duration=40|flags=___",
        ] {
            acc.push_line(line);
        }
        acc.finish(&mut info.streams);
        let v = info.streams[1].packets.as_ref().unwrap();
        assert_eq!((v.count, v.start_s, v.end_s), (2, Some(0.0), Some(0.08)));
        assert_eq!(v.lead_in, None);
        // Other streams keep their hidden lead-in, recorded as such.
        let a = info.streams[2].packets.as_ref().unwrap();
        assert_eq!((a.count, a.start_s), (2, Some(-0.046)));
        assert_eq!(
            a.lead_in,
            Some(LeadIn {
                packets: 1,
                last_start_s: Some(-0.046)
            })
        );
    }

    #[test]
    fn first_shown_frame_comes_from_the_decoder() {
        let mut info = parse_metadata(MKV, ID).unwrap();
        let mut acc = PacketAccumulator::new(&info.streams, Some(1));
        // An open-GOP keyframe followed by two leading B frames.
        for line in [
            "stream_index=1|pts=80|dts=0|duration=40|flags=K__",
            "stream_index=1|pts=0|dts=40|duration=40|flags=___",
            "stream_index=1|pts=40|dts=80|duration=40|flags=___",
        ] {
            acc.push_line(line);
        }
        acc.finish(&mut info.streams);
        // The decoder drops the B frames; a frame without pts falls back to
        // its best-effort timestamp.
        apply_first_frame(
            &mut info,
            1,
            "N/A,160
80,80
120,120,
",
        );
        let v = info.streams[1].packets.as_ref().unwrap();
        assert_eq!(v.start_s, Some(0.0));
        assert_eq!(v.first_frame_s, Some(0.08));
        assert_eq!(v.shown_start_s(), Some(0.08));

        // Nothing decoded: the first packet stands.
        let mut info = parse_metadata(MKV, ID).unwrap();
        let mut acc = PacketAccumulator::new(&info.streams, Some(1));
        acc.push_line("stream_index=1|pts=0|dts=0|duration=40|flags=K__");
        acc.finish(&mut info.streams);
        apply_first_frame(&mut info, 1, "");
        let v = info.streams[1].packets.as_ref().unwrap();
        assert_eq!((v.first_frame_s, v.shown_start_s()), (None, Some(0.0)));
    }

    #[test]
    fn checkpoints_are_evenly_spaced_inside_the_video() {
        let frames: Vec<u64> = checkpoint_frames(1100).collect();
        assert_eq!(frames, (1..=10).map(|i| i * 100).collect::<Vec<_>>());
        assert_eq!(checkpoint_frames(0).count(), 0);
        assert_eq!(checkpoint_frames(1).count(), 0);
    }
}
