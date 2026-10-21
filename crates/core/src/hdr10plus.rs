//! HDR10+ (SMPTE ST 2094-40) dynamic metadata (ADR 0011). FFmpeg's libx265,
//! SVT-AV1, QSV and VAAPI wrappers drop it when they encode, and the NAS's
//! libx265 is built without HDR10_PLUS, so the source's messages are put
//! back the way a Dolby Vision RPU is:
//!
//! 1. [`extract`] stream-copies the source's video keeping only its prefix
//!    SEI NAL units (a few dozen bytes per frame) and takes each frame's
//!    HDR10+ message, in display order;
//! 2. the re-encoded video goes to an intermediate Matroska file;
//! 3. [`mkv::inject_frames`] puts each message, in an SEI NAL unit of its
//!    own, in front of the frame's first slice.
//!
//! Verification extracts the candidate's messages the same way and compares
//! them byte for byte.

use std::{path::Path, process::Command, time::Duration};

use crate::{
    dovi::mkv,
    probe::ffmpeg_input,
    tool::{self, CancelToken, ToolError},
};

/// Start of an HDR10+ user_data_registered_itu_t_t35 payload: country code
/// (United States), terminal provider code (Samsung), provider oriented
/// code, application identifier 4.
const T35_PREFIX: [u8; 6] = [0xB5, 0x00, 0x3C, 0x00, 0x01, 0x04];
/// HEVC NAL unit type of a prefix SEI.
pub const PREFIX_SEI: u8 = 39;
const USER_DATA_REGISTERED_ITU_T_T35: usize = 4;
/// RBSP trailing bits of a byte-aligned SEI.
const TRAILING: u8 = 0x80;

/// The RBSP of a NAL unit: emulation prevention bytes removed.
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Emulation prevention bytes added to an RBSP.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// The HDR10+ message of a NAL unit (header included) when it is a prefix
/// SEI carrying one: the T.35 payload, without emulation prevention.
pub fn payload(nal: &[u8]) -> Option<Vec<u8>> {
    if nal.len() < 3 || (nal[0] >> 1) & 0x3f != PREFIX_SEI {
        return None;
    }
    let rbsp = unescape(&nal[2..]);
    let mut at = 0;
    let read_value = |at: &mut usize| {
        let mut value = 0;
        loop {
            let b = *rbsp.get(*at)?;
            *at += 1;
            value += usize::from(b);
            if b != 0xff {
                return Some(value);
            }
        }
    };
    // A message needs a type and a size byte; the trailing bits are one.
    while at + 1 < rbsp.len() {
        let kind = read_value(&mut at)?;
        let size = read_value(&mut at)?;
        let body = rbsp.get(at..at + size)?;
        if kind == USER_DATA_REGISTERED_ITU_T_T35 && body.starts_with(&T35_PREFIX) {
            return Some(body.to_vec());
        }
        at += size;
    }
    None
}

/// A prefix SEI NAL unit (header included) carrying `payload` alone, in
/// temporal sub-layer `temporal_id_plus1` (that of the frame's slices).
pub fn sei_nal(payload: &[u8], temporal_id_plus1: u8) -> Vec<u8> {
    let mut rbsp = vec![USER_DATA_REGISTERED_ITU_T_T35 as u8];
    let mut size = payload.len();
    while size >= 0xff {
        rbsp.push(0xff);
        size -= 0xff;
    }
    rbsp.push(size as u8);
    rbsp.extend_from_slice(payload);
    rbsp.push(TRAILING);
    let mut nal = vec![PREFIX_SEI << 1, temporal_id_plus1];
    nal.extend(escape(&rbsp));
    nal
}

/// Processing windows of a message. Windows beyond the first carry picture
/// coordinates, which a downscaled encode would no longer match.
pub fn windows(payload: &[u8]) -> Option<u8> {
    payload.get(T35_PREFIX.len() + 1).map(|b| b >> 6)
}

/// Each frame's HDR10+ message, in display order; `None` for a frame with
/// prefix SEI but no HDR10+. A frame without any prefix SEI is missing from
/// the list (the stream copy drops packets left empty), so callers compare
/// the length with the frame count.
pub fn extract(
    ffmpeg: &Path,
    input: &Path,
    video: u32,
    scratch: &Path,
    timeout: Duration,
    cancel: Option<&CancelToken>,
) -> Result<Vec<Option<Vec<u8>>>, ToolError> {
    let _ = std::fs::remove_file(scratch);
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "error", "-i"])
        .arg(ffmpeg_input(input))
        .args(["-map", &format!("0:{video}"), "-c:v", "copy"])
        .args(["-bsf:v", &format!("filter_units=pass_types={PREFIX_SEI}")])
        .args(["-f", "matroska", "-n"])
        .arg(ffmpeg_input(scratch));
    let ran = tool::run_capture(&mut cmd, timeout, cancel);
    let frames = ran.and_then(|_| {
        mkv::frames(scratch).map_err(|e| ToolError::Unexpected {
            program: ffmpeg.display().to_string(),
            message: format!("读取 SEI 失败：{e}"),
        })
    });
    let _ = std::fs::remove_file(scratch);
    Ok(frames?
        .iter()
        .map(|nals| nals.iter().find_map(|n| payload(n)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An HDR10+ payload: the T.35 prefix, application version 1, one
    /// window, then bytes that need emulation prevention.
    fn message() -> Vec<u8> {
        let mut p = T35_PREFIX.to_vec();
        p.extend([0x01, 0x40, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02]);
        p
    }

    #[test]
    fn sei_round_trips_with_emulation_prevention() {
        let p = message();
        let nal = sei_nal(&p, 1);
        assert_eq!(nal[..2], [0x4e, 0x01]);
        assert!(nal.windows(3).all(|w| w[..2] != [0, 0] || w[2] >= 3));
        assert_eq!(payload(&nal), Some(p.clone()));
        assert_eq!(windows(&p), Some(1));
        // Long payloads use 0xff size bytes.
        let long = [T35_PREFIX.to_vec(), vec![7; 300]].concat();
        assert_eq!(payload(&sei_nal(&long, 1)), Some(long));
    }

    #[test]
    fn other_messages_are_skipped() {
        let p = message();
        // Mastering display (137, 24 bytes), then HDR10+, in one NAL unit.
        let mut rbsp = vec![137, 24];
        rbsp.extend([0x11; 24]);
        rbsp.extend([4, p.len() as u8]);
        rbsp.extend(&p);
        rbsp.push(TRAILING);
        let nal = [vec![0x4e, 0x01], escape(&rbsp)].concat();
        assert_eq!(payload(&nal), Some(p));
        // Other T.35 payloads, suffix SEI, slices: none.
        let other = [vec![0x4e, 0x01, 4, 3, 0xb5, 0x00, 0x31], vec![TRAILING]].concat();
        assert_eq!(payload(&other), None);
        let suffix = sei_nal(&message(), 1);
        let suffix = [vec![40 << 1], suffix[1..].to_vec()].concat();
        assert_eq!(payload(&suffix), None);
        assert_eq!(payload(&[0x26, 0x01, 0xaf]), None);
        assert_eq!(payload(&[0x4e, 0x01, 4, 40, 0xb5]), None, "truncated");
    }
}
