//! Matroska rewriting for Dolby Vision routes. FFmpeg writes the re-encoded
//! video of a DV route to a video-only Matroska file; [`inject`] appends each
//! frame's RPU NAL unit and states the configuration record as a
//! BlockAdditionMapping, which FFmpeg's demuxer turns back into the stream's
//! DOVI configuration when it muxes the result with the other streams.
//!
//! Frames are stored in decode order; the RPU file is in display order. A
//! frame's place in display order is its rank by presentation timestamp,
//! which the blocks carry, so no HEVC slice parsing is needed.
//!
//! Only what that final mux reads is written: the EBML header, Info, Tracks
//! and the Clusters, in a Segment of unknown size. Cues, SeekHead, Tags and
//! CRC-32 elements are dropped.
//!
//! HDR10+ routes use the same rewriting: [`inject_frames`] also puts each
//! frame's HDR10+ message, in an SEI NAL unit, in front of its first slice
//! (see `crate::hdr10plus`), with or without an RPU. [`frames`] reads the
//! NAL units of a small file back in display order.
//!
//! The same rewriting serves samples cut from the middle of a source whose
//! parameter sets are only in-band at its start (an hvcC without arrays):
//! [`prepend`] puts them in front of the first frame (see `crate::hevc`).

use std::{
    fs::File,
    io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use super::{config_fourcc, config_record};
use crate::{hdr10plus, probe::DoviConfig};

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const BLOCK_ADDITION_MAPPING: u32 = 0x41E4;
const BLOCK_ADD_ID_NAME: u32 = 0x41A4;
const BLOCK_ADD_ID_TYPE: u32 = 0x41E7;
const BLOCK_ADD_ID_EXTRA_DATA: u32 = 0x41ED;
const CLUSTER: u32 = 0x1F43_B675;
const TIMESTAMP: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const CRC32: u32 = 0xBF;

/// HEVC NAL unit type of a Dolby Vision RPU (UNSPEC62).
const RPU_NAL_TYPE: u8 = 62;

#[derive(Debug, thiserror::Error)]
pub enum MkvError {
    #[error("读写中间文件失败：{0}")]
    Io(#[from] io::Error),
    #[error("中间文件的 Matroska 结构不符合预期：{0}")]
    Format(String),
    #[error("{what}数量（{found}）与视频帧数（{frames}）不同")]
    Count {
        what: &'static str,
        found: usize,
        frames: usize,
    },
}

fn bad(msg: impl Into<String>) -> MkvError {
    MkvError::Format(msg.into())
}

#[derive(Debug, Clone, Copy)]
struct Header {
    id: u32,
    /// `None`: unknown size (runs to the end of the parent).
    size: Option<u64>,
}

/// Reads an element ID (marker bits kept, as IDs are written), or `None` at
/// a clean end of input.
fn read_id(r: &mut impl Read) -> Result<Option<u32>, MkvError> {
    let mut first = [0u8; 1];
    if r.read(&mut first)? == 0 {
        return Ok(None);
    }
    let width = first[0].leading_zeros() as usize + 1;
    if width > 4 {
        return Err(bad(format!("无效的元素 ID 字节 {:#04x}", first[0])));
    }
    let mut rest = [0u8; 3];
    r.read_exact(&mut rest[..width - 1])?;
    Ok(Some(
        rest[..width - 1]
            .iter()
            .fold(u32::from(first[0]), |id, &b| (id << 8) | u32::from(b)),
    ))
}

/// Reads an element data size; `None` is the reserved "unknown" value.
fn read_size(r: &mut impl Read) -> Result<Option<u64>, MkvError> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first)?;
    let width = first[0].leading_zeros() as usize + 1;
    if width > 8 {
        return Err(bad("无效的元素长度"));
    }
    let mut value = u64::from(first[0]) & (0xff >> width);
    let mut rest = [0u8; 7];
    r.read_exact(&mut rest[..width - 1])?;
    for &b in &rest[..width - 1] {
        value = (value << 8) | u64::from(b);
    }
    let all_ones = (1u64 << (7 * width)) - 1;
    Ok((value != all_ones).then_some(value))
}

fn read_header(r: &mut impl Read) -> Result<Option<Header>, MkvError> {
    let Some(id) = read_id(r)? else {
        return Ok(None);
    };
    Ok(Some(Header {
        id,
        size: read_size(r)?,
    }))
}

fn known(h: Header) -> Result<u64, MkvError> {
    h.size
        .ok_or_else(|| bad(format!("元素 {:#x} 的长度未知", h.id)))
}

/// Matroska block header: track number (a size-style vint), 16-bit relative
/// timestamp, flags.
fn block_header(data: &[u8]) -> Result<(u64, i16, usize), MkvError> {
    let first = *data.first().ok_or_else(|| bad("空的 Block"))?;
    let width = first.leading_zeros() as usize + 1;
    if width > 8 || data.len() < width + 3 {
        return Err(bad("Block 头部不完整"));
    }
    let track = data[1..width]
        .iter()
        .fold(u64::from(first) & (0xff >> width), |v, &b| {
            (v << 8) | u64::from(b)
        });
    let ts = i16::from_be_bytes([data[width], data[width + 1]]);
    let flags = data[width + 2];
    if flags & 0x06 != 0 {
        return Err(bad("视频 Block 使用了 lacing"));
    }
    Ok((track, ts, width + 3))
}

fn write_id(out: &mut Vec<u8>, id: u32) {
    let bytes = id.to_be_bytes();
    let skip = (id.leading_zeros() / 8) as usize;
    out.extend_from_slice(&bytes[skip..]);
}

fn write_size(out: &mut Vec<u8>, size: u64) {
    let width = (1..=8)
        .find(|&w| size < (1u64 << (7 * w)) - 1)
        .expect("element sizes fit in 8 bytes");
    let marked = size | (1u64 << (7 * width));
    out.extend_from_slice(&marked.to_be_bytes()[8 - width..]);
}

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 12);
    write_id(&mut out, id);
    write_size(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    out
}

fn uint(id: u32, v: u64) -> Vec<u8> {
    let bytes = v.to_be_bytes();
    let skip = ((v.leading_zeros() / 8) as usize).min(7);
    element(id, &bytes[skip..])
}

fn be_uint(data: &[u8]) -> u64 {
    data.iter().fold(0, |v, &b| (v << 8) | u64::from(b))
}

/// A child element: (ID, whole element bytes, body bytes).
type Child<'a> = (u32, &'a [u8], &'a [u8]);

/// Children of a master element held in memory.
fn children(body: &[u8]) -> Result<Vec<Child<'_>>, MkvError> {
    let mut out = Vec::new();
    let mut cursor = io::Cursor::new(body);
    while (cursor.position() as usize) < body.len() {
        let start = cursor.position() as usize;
        let h = read_header(&mut cursor)?.ok_or_else(|| bad("元素被截断"))?;
        let size = known(h)? as usize;
        let at = cursor.position() as usize;
        let end = at
            .checked_add(size)
            .filter(|&e| e <= body.len())
            .ok_or_else(|| bad(format!("元素 {:#x} 超出了父元素", h.id)))?;
        out.push((h.id, &body[start..end], &body[at..end]));
        cursor.set_position(end as u64);
    }
    Ok(out)
}

/// The video track of the intermediate file.
struct Track {
    number: u64,
    /// Bytes of each NAL unit length prefix (hvcC lengthSizeMinusOne + 1).
    nal_length: usize,
}

fn video_track(tracks: &[u8]) -> Result<Track, MkvError> {
    let entries: Vec<_> = children(tracks)?
        .into_iter()
        .filter(|(id, ..)| *id == TRACK_ENTRY)
        .collect();
    let [(_, _, entry)] = entries[..] else {
        return Err(bad(format!("应只有一条轨道，实际 {} 条", entries.len())));
    };
    let fields = children(entry)?;
    let field = |id| fields.iter().find(|(i, ..)| *i == id).map(|(_, _, b)| *b);
    if field(TRACK_TYPE).map(be_uint) != Some(1) {
        return Err(bad("唯一的轨道不是视频"));
    }
    if field(CODEC_ID) != Some(b"V_MPEGH/ISO/HEVC") {
        return Err(bad("视频不是 HEVC"));
    }
    let number = field(TRACK_NUMBER)
        .map(be_uint)
        .ok_or_else(|| bad("缺少轨道编号"))?;
    let hvcc = field(CODEC_PRIVATE).ok_or_else(|| bad("缺少 hvcC"))?;
    if hvcc.len() < 23 {
        return Err(bad("hvcC 太短"));
    }
    Ok(Track {
        number,
        nal_length: usize::from(hvcc[21] & 0x03) + 1,
    })
}

/// Tracks with the configuration record added to the (only) entry, any
/// older Dolby Vision mapping and CRC-32 removed.
fn tracks_with_config(tracks: &[u8], config: &DoviConfig) -> Result<Vec<u8>, MkvError> {
    let mut body = Vec::new();
    for (id, raw, entry) in children(tracks)? {
        match id {
            CRC32 => {}
            TRACK_ENTRY => {
                let mut e = Vec::new();
                for (id, raw, _) in children(entry)? {
                    if !matches!(id, CRC32 | BLOCK_ADDITION_MAPPING) {
                        e.extend_from_slice(raw);
                    }
                }
                let mut mapping = element(BLOCK_ADD_ID_NAME, b"Dolby Vision configuration");
                mapping.extend(uint(
                    BLOCK_ADD_ID_TYPE,
                    u64::from(u32::from_be_bytes(config_fourcc(config))),
                ));
                mapping.extend(element(BLOCK_ADD_ID_EXTRA_DATA, &config_record(config)));
                e.extend(element(BLOCK_ADDITION_MAPPING, &mapping));
                body.extend(element(TRACK_ENTRY, &e));
            }
            _ => body.extend_from_slice(raw),
        }
    }
    Ok(body)
}

/// The NAL units of a frame (without their length prefixes), each with the
/// offset of its prefix.
fn nal_units(frame: &[u8], nal_length: usize) -> Result<Vec<(usize, &[u8])>, MkvError> {
    let (mut at, mut units) = (0, Vec::new());
    while at < frame.len() {
        let start = at;
        let len = frame
            .get(at..at + nal_length)
            .map(be_uint)
            .ok_or_else(|| bad("NAL 长度前缀被截断"))? as usize;
        at += nal_length;
        let nal = frame.get(at..at + len).ok_or_else(|| bad("NAL 被截断"))?;
        units.push((start, nal));
        at += len;
    }
    Ok(units)
}

fn nal_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3f)
}

/// The HEVC NAL unit types of a frame, in order.
fn nal_types(frame: &[u8], nal_length: usize) -> Result<Vec<u8>, MkvError> {
    Ok(nal_units(frame, nal_length)?
        .iter()
        .filter_map(|(_, nal)| nal_type(nal))
        .collect())
}

/// Whether a frame already carries an RPU (it would end up with two).
fn has_rpu(frame: &[u8], nal_length: usize) -> Result<bool, MkvError> {
    Ok(nal_types(frame, nal_length)?.contains(&RPU_NAL_TYPE))
}

/// `nal`'s length prefix, checked against what `nal_length` bytes can hold.
fn length_prefix(len: usize, nal_length: usize) -> Result<Vec<u8>, MkvError> {
    if nal_length < 4 && len >= 1 << (8 * nal_length) {
        return Err(bad("NAL 超出了长度前缀能表示的范围"));
    }
    Ok((len as u64).to_be_bytes()[8 - nal_length..].to_vec())
}

/// The block payload with `nals` put in front of the frame's NAL units.
fn with_prefix(
    block: &[u8],
    header_len: usize,
    nals: &[Vec<u8>],
    track: &Track,
) -> Result<Vec<u8>, MkvError> {
    let mut out = Vec::with_capacity(block.len() + nals.iter().map(|n| n.len() + 4).sum::<usize>());
    out.extend_from_slice(&block[..header_len]);
    for nal in nals {
        out.extend(length_prefix(nal.len(), track.nal_length)?);
        out.extend_from_slice(nal);
    }
    out.extend_from_slice(&block[header_len..]);
    Ok(out)
}

/// The block payload with the RPU NAL unit appended as the last NAL unit of
/// the access unit.
fn with_rpu(
    block: &[u8],
    header_len: usize,
    rpu: &[u8],
    track: &Track,
) -> Result<Vec<u8>, MkvError> {
    if has_rpu(&block[header_len..], track.nal_length)? {
        return Err(bad("编码结果已经带 RPU"));
    }
    let len = rpu.len() + 2;
    let mut out = Vec::with_capacity(block.len() + len + track.nal_length);
    out.extend_from_slice(block);
    out.extend(length_prefix(len, track.nal_length)?);
    out.extend_from_slice(&[RPU_NAL_TYPE << 1, 0x01]);
    out.extend_from_slice(rpu);
    Ok(out)
}

/// The block payload with `message` in an HDR10+ SEI NAL unit in front of
/// the frame's first slice, after its parameter sets and other prefix SEI.
fn with_hdr10_plus(
    block: &[u8],
    header_len: usize,
    message: &[u8],
    track: &Track,
) -> Result<Vec<u8>, MkvError> {
    let units = nal_units(&block[header_len..], track.nal_length)?;
    if units
        .iter()
        .any(|(_, nal)| hdr10plus::payload(nal).is_some())
    {
        return Err(bad("编码结果已经带 HDR10+"));
    }
    let &(at, slice) = units
        .iter()
        .find(|(_, nal)| nal_type(nal).is_some_and(|t| t < 32))
        .ok_or_else(|| bad("帧里没有切片"))?;
    // The SEI belongs to the slices' temporal sub-layer.
    let nal = hdr10plus::sei_nal(message, slice.get(1).map_or(1, |b| b & 0x07));
    let split = header_len + at;
    let mut out = Vec::with_capacity(block.len() + nal.len() + track.nal_length);
    out.extend_from_slice(&block[..split]);
    out.extend(length_prefix(nal.len(), track.nal_length)?);
    out.extend(nal);
    out.extend_from_slice(&block[split..]);
    Ok(out)
}

/// Top-level layout of the intermediate file.
struct Layout {
    ebml: Vec<u8>,
    info: Vec<u8>,
    tracks: Vec<u8>,
    /// (offset, size) of each Cluster body.
    clusters: Vec<(u64, u64)>,
}

fn layout(r: &mut BufReader<File>) -> Result<Layout, MkvError> {
    let len = r.get_ref().metadata()?.len();
    let read_body = |r: &mut BufReader<File>, size: u64| -> Result<Vec<u8>, MkvError> {
        let mut body = vec![0; size as usize];
        r.read_exact(&mut body)?;
        Ok(body)
    };
    let h = read_header(r)?.ok_or_else(|| bad("空文件"))?;
    if h.id != EBML {
        return Err(bad("不是 Matroska 文件"));
    }
    let body = read_body(r, known(h)?)?;
    let ebml = element(EBML, &body);
    let h = read_header(r)?.ok_or_else(|| bad("缺少 Segment"))?;
    if h.id != SEGMENT {
        return Err(bad("缺少 Segment"));
    }
    let end = match h.size {
        Some(size) => r.stream_position()? + size,
        None => len,
    };
    let (mut info, mut tracks, mut clusters) = (None, None, Vec::new());
    while r.stream_position()? < end {
        let Some(h) = read_header(r)? else { break };
        let size = known(h)?;
        match h.id {
            INFO => info = Some(element(INFO, &read_body(r, size)?)),
            TRACKS => tracks = Some(read_body(r, size)?),
            CLUSTER => {
                clusters.push((r.stream_position()?, size));
                r.seek_relative(size as i64)?;
            }
            _ => r.seek_relative(size as i64)?,
        }
    }
    Ok(Layout {
        ebml,
        info: info.ok_or_else(|| bad("缺少 Info"))?,
        tracks: tracks.ok_or_else(|| bad("缺少 Tracks"))?,
        clusters,
    })
}

fn read_cluster(r: &mut BufReader<File>, (at, size): (u64, u64)) -> Result<Vec<u8>, MkvError> {
    r.seek(SeekFrom::Start(at))?;
    let mut body = vec![0; size as usize];
    r.read_exact(&mut body)?;
    Ok(body)
}

/// Track number and presentation timestamp of every block in a cluster,
/// reading only element and block headers.
fn scan_cluster(
    r: &mut BufReader<File>,
    (at, size): (u64, u64),
) -> Result<Vec<(u64, i64)>, MkvError> {
    fn block(r: &mut BufReader<File>, size: u64) -> Result<(u64, i16), MkvError> {
        let mut head = [0u8; 11];
        let n = (size as usize).min(head.len());
        r.read_exact(&mut head[..n])?;
        let (track, ts, _) = block_header(&head[..n])?;
        r.seek_relative(size as i64 - n as i64)?;
        Ok((track, ts))
    }
    r.seek(SeekFrom::Start(at))?;
    let end = at + size;
    let (mut base, mut blocks) = (None, Vec::new());
    while r.stream_position()? < end {
        let h = read_header(r)?.ok_or_else(|| bad("Cluster 被截断"))?;
        let size = known(h)?;
        match h.id {
            TIMESTAMP => {
                let mut v = vec![0; size as usize];
                r.read_exact(&mut v)?;
                base = Some(be_uint(&v) as i64);
            }
            SIMPLE_BLOCK => blocks.push(block(r, size)?),
            BLOCK_GROUP => {
                let group_end = r.stream_position()? + size;
                while r.stream_position()? < group_end {
                    let h = read_header(r)?.ok_or_else(|| bad("BlockGroup 被截断"))?;
                    let size = known(h)?;
                    if h.id == BLOCK {
                        blocks.push(block(r, size)?);
                    } else {
                        r.seek_relative(size as i64)?;
                    }
                }
            }
            _ => r.seek_relative(size as i64)?,
        }
    }
    let base = base.ok_or_else(|| bad("Cluster 缺少 Timestamp"))?;
    Ok(blocks
        .into_iter()
        .map(|(track, ts)| (track, base + i64::from(ts)))
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Injected {
    pub frames: usize,
}

/// What [`inject_frames`] puts into each frame; every list is in display
/// order, one entry per frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameMetadata<'a> {
    /// Dolby Vision RPUs (as [`super::read_rpus`] returns them) and the
    /// configuration the track states.
    pub rpus: Option<(&'a [Vec<u8>], &'a DoviConfig)>,
    /// HDR10+ messages (as [`hdr10plus::payload`] returns them).
    pub hdr10_plus: Option<&'a [Vec<u8>]>,
}

/// Writes `output`: `input` (the video-only Matroska file of a DV route)
/// with `rpus[i]` appended to the frame shown `i`-th and `config` as the
/// track's Dolby Vision configuration.
pub fn inject(
    input: &Path,
    rpus: &[Vec<u8>],
    config: &DoviConfig,
    output: &Path,
) -> Result<Injected, MkvError> {
    let metadata = FrameMetadata {
        rpus: Some((rpus, config)),
        hdr10_plus: None,
    };
    rewrite(input, Edit::Frames(metadata), output)
}

/// Writes `output`: `input` (the video-only Matroska file of a DV or HDR10+
/// route) with each frame's metadata: the HDR10+ message in front of its
/// first slice, the RPU after its last NAL unit.
pub fn inject_frames(
    input: &Path,
    metadata: &FrameMetadata,
    output: &Path,
) -> Result<Injected, MkvError> {
    rewrite(input, Edit::Frames(*metadata), output)
}

/// Writes `output`: `input`, whose frames already carry their RPUs, with
/// `config` as the track's Dolby Vision configuration.
pub fn set_config(input: &Path, config: &DoviConfig, output: &Path) -> Result<Injected, MkvError> {
    rewrite(input, Edit::Config(config), output)
}

/// Writes `output`: `input` (a video-only HEVC Matroska file) with `nals`
/// (the source's parameter sets) in front of its first stored frame.
pub fn prepend(input: &Path, nals: &[Vec<u8>], output: &Path) -> Result<Injected, MkvError> {
    rewrite(input, Edit::Prefix(nals), output)
}

enum Edit<'a> {
    /// Add each frame's metadata, by display rank; state the configuration
    /// when there are RPUs.
    Frames(FrameMetadata<'a>),
    /// State the configuration only.
    Config(&'a DoviConfig),
    /// Put these NAL units in front of the first stored frame.
    Prefix(&'a [Vec<u8>]),
}

fn rewrite(input: &Path, edit: Edit<'_>, output: &Path) -> Result<Injected, MkvError> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(input)?);
    let layout = layout(&mut r)?;
    let track = video_track(&layout.tracks)?;
    let lists = match edit {
        Edit::Frames(m) => vec![
            ("RPU ", m.rpus.map(|(rpus, _)| rpus.len())),
            ("HDR10+ 元数据", m.hdr10_plus.map(<[_]>::len)),
        ],
        _ => Vec::new(),
    };

    // Presentation timestamps of the frames, in stored (decode) order.
    let mut pts = Vec::new();
    for &cluster in &layout.clusters {
        for (number, t) in scan_cluster(&mut r, cluster)? {
            if number != track.number {
                return Err(bad(format!("出现了第 {number} 条轨道的 Block")));
            }
            pts.push(t);
        }
    }
    for (what, found) in lists {
        if let Some(found) = found.filter(|&n| n != pts.len()) {
            return Err(MkvError::Count {
                what,
                found,
                frames: pts.len(),
            });
        }
    }
    let mut order: Vec<usize> = (0..pts.len()).collect();
    order.sort_by_key(|&i| (pts[i], i));
    if order.windows(2).any(|w| pts[w[0]] == pts[w[1]]) {
        return Err(bad("有两帧的显示时间相同，无法确定显示顺序"));
    }
    let mut rank = vec![0; pts.len()];
    for (shown, &stored) in order.iter().enumerate() {
        rank[stored] = shown;
    }

    let mut w = BufWriter::with_capacity(1 << 20, File::create(output)?);
    w.write_all(&layout.ebml)?;
    let mut segment = Vec::new();
    write_id(&mut segment, SEGMENT);
    segment.extend_from_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    w.write_all(&segment)?;
    w.write_all(&layout.info)?;
    let tracks = match edit {
        Edit::Frames(FrameMetadata {
            rpus: Some((_, config)),
            ..
        })
        | Edit::Config(config) => tracks_with_config(&layout.tracks, config)?,
        _ => layout.tracks.clone(),
    };
    w.write_all(&element(TRACKS, &tracks))?;
    let add = |block: &[u8], frame: usize| -> Result<Vec<u8>, MkvError> {
        let (_, _, header_len) = block_header(block)?;
        match edit {
            Edit::Frames(m) => {
                let shown = rank[frame];
                let block = match m.hdr10_plus {
                    Some(messages) => with_hdr10_plus(block, header_len, &messages[shown], &track)?,
                    None => block.to_vec(),
                };
                match m.rpus {
                    Some((rpus, _)) => with_rpu(&block, header_len, &rpus[shown], &track),
                    None => Ok(block),
                }
            }
            Edit::Prefix(nals) if frame == 0 => with_prefix(block, header_len, nals, &track),
            _ => Ok(block.to_vec()),
        }
    };
    let mut frame = 0;
    for &cluster in &layout.clusters {
        let body = read_cluster(&mut r, cluster)?;
        let mut out = Vec::with_capacity(body.len() + 1024);
        for (id, raw, child) in children(&body)? {
            match id {
                TIMESTAMP => out.extend_from_slice(raw),
                SIMPLE_BLOCK => {
                    out.extend(element(SIMPLE_BLOCK, &add(child, frame)?));
                    frame += 1;
                }
                BLOCK_GROUP => {
                    let mut group = Vec::new();
                    for (id, raw, part) in children(child)? {
                        match id {
                            BLOCK => {
                                group.extend(element(BLOCK, &add(part, frame)?));
                                frame += 1;
                            }
                            CRC32 => {}
                            _ => group.extend_from_slice(raw),
                        }
                    }
                    out.extend(element(BLOCK_GROUP, &group));
                }
                // CRC-32 would no longer match; positions would be wrong.
                _ => {}
            }
        }
        w.write_all(&element(CLUSTER, &out))?;
    }
    w.flush()?;
    w.get_ref().sync_all()?;
    Ok(Injected { frames: frame })
}

/// The NAL units (headers included) of each frame of a video-only HEVC
/// Matroska file, frames in display order. Reads everything into memory:
/// meant for files of a few NAL units per frame, such as the prefix SEI
/// [`hdr10plus::extract`] keeps.
pub fn frames(input: &Path) -> Result<Vec<Vec<Vec<u8>>>, MkvError> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(input)?);
    let layout = layout(&mut r)?;
    let track = video_track(&layout.tracks)?;
    let mut frames = Vec::new();
    for &cluster in &layout.clusters {
        let body = read_cluster(&mut r, cluster)?;
        let mut base = None;
        let mut blocks = Vec::new();
        for (id, _, child) in children(&body)? {
            match id {
                TIMESTAMP => base = Some(be_uint(child) as i64),
                SIMPLE_BLOCK => blocks.push((base, child)),
                BLOCK_GROUP => {
                    for (id, _, part) in children(child)? {
                        if id == BLOCK {
                            blocks.push((base, part));
                        }
                    }
                }
                _ => {}
            }
        }
        for (base, block) in blocks {
            let base = base.ok_or_else(|| bad("Cluster 缺少 Timestamp"))?;
            let (number, ts, header_len) = block_header(block)?;
            if number != track.number {
                return Err(bad(format!("出现了第 {number} 条轨道的 Block")));
            }
            let nals = nal_units(&block[header_len..], track.nal_length)?
                .into_iter()
                .map(|(_, nal)| nal.to_vec())
                .collect::<Vec<_>>();
            frames.push((base + i64::from(ts), nals));
        }
    }
    frames.sort_by_key(|(pts, _)| *pts);
    Ok(frames.into_iter().map(|(_, nals)| nals).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vints_round_trip() {
        for size in [0u64, 1, 126, 127, 128, 16_382, 16_383, 1 << 40] {
            let mut out = Vec::new();
            write_size(&mut out, size);
            assert_eq!(
                read_size(&mut io::Cursor::new(&out)).unwrap(),
                Some(size),
                "{size}"
            );
        }
        // 0x7f with width 1 is the reserved "unknown" value.
        assert_eq!(read_size(&mut io::Cursor::new([0xffu8])).unwrap(), None);
        for id in [0xA3u32, 0x41E4, 0x1F43_B675] {
            let mut out = Vec::new();
            write_id(&mut out, id);
            assert_eq!(read_id(&mut io::Cursor::new(&out)).unwrap(), Some(id));
        }
        assert_eq!(uint(TRACK_NUMBER, 1), [0xD7, 0x81, 0x01]);
        assert_eq!(uint(TRACK_NUMBER, 0), [0xD7, 0x81, 0x00]);
    }

    #[test]
    fn rpu_goes_last_with_a_length_prefix() {
        let track = Track {
            number: 1,
            nal_length: 4,
        };
        // Track 1, timestamp 0, keyframe; one NAL unit of 2 bytes.
        let block = [0x81, 0, 0, 0x80, 0, 0, 0, 2, 0x26, 0x01];
        let (number, ts, header) = block_header(&block).unwrap();
        assert_eq!((number, ts, header), (1, 0, 4));
        let out = with_rpu(&block, header, &[0x19, 0x80], &track).unwrap();
        assert_eq!(out[block.len()..], [0, 0, 0, 4, 0x7c, 0x01, 0x19, 0x80]);
        assert!(has_rpu(&out[header..], 4).unwrap());
        assert!(with_rpu(&out, header, &[0x19, 0x80], &track).is_err());
        let laced = [0x81, 0, 0, 0x82, 0];
        assert!(block_header(&laced).is_err());
    }

    #[test]
    fn parameter_sets_go_first() {
        let track = Track {
            number: 1,
            nal_length: 2,
        };
        let block = [0x81, 0, 0, 0x80, 0, 2, 0x26, 0x01];
        let (vps, sps) = (vec![0x40, 0x01, 0xaa], vec![0x42, 0x01]);
        let out = with_prefix(&block, 4, &[vps, sps], &track).unwrap();
        assert_eq!(
            out,
            [0x81, 0, 0, 0x80, 0, 3, 0x40, 0x01, 0xaa, 0, 2, 0x42, 0x01, 0, 2, 0x26, 0x01]
        );
        assert_eq!(nal_types(&out[4..], 2).unwrap(), [32, 33, 19]);
        assert!(with_prefix(&block, 4, &[vec![0; 1 << 16]], &track).is_err());
    }

    #[test]
    fn hdr10_plus_goes_in_front_of_the_first_slice() {
        let track = Track {
            number: 1,
            nal_length: 2,
        };
        // AUD, prefix SEI (mastering display), slice in temporal layer 0.
        let block = [
            0x81, 0, 0, 0x80, 0, 3, 0x46, 0x01, 0x50, 0, 4, 0x4e, 0x01, 0x89, 0x80, 0, 2, 0x02,
            0x01,
        ];
        let message = [0xb5, 0x00, 0x3c, 0x00, 0x01, 0x04, 0x01, 0x40];
        let out = with_hdr10_plus(&block, 4, &message, &track).unwrap();
        assert_eq!(nal_types(&out[4..], 2).unwrap(), [35, 39, 39, 1]);
        let units = nal_units(&out[4..], 2).unwrap();
        assert_eq!(hdr10plus::payload(units[2].1), Some(message.to_vec()));
        assert_eq!(units[2].1[1], 0x01, "the slice's temporal ID");
        assert!(
            with_hdr10_plus(&out, 4, &message, &track).is_err(),
            "already there"
        );
        assert!(
            with_hdr10_plus(&block[..15], 4, &message, &track).is_err(),
            "no slice"
        );
    }
}
