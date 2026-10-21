//! Bit depth and chroma layout from an FFmpeg pixel format name.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Chroma {
    Yuv420,
    Yuv422,
    Yuv444,
    Yuv440,
    Yuv411,
    Yuv410,
    Rgb,
    Gray,
}

/// `(bit depth, chroma)` for a pixel format, or `None` for names we do not
/// recognise. Unknown formats are left for the route to refuse.
pub fn pix_fmt_layout(name: &str) -> Option<(u8, Chroma)> {
    let name = name
        .strip_suffix("le")
        .or_else(|| name.strip_suffix("be"))
        .unwrap_or(name);

    let yuv = name
        .strip_prefix("yuvj")
        .or_else(|| name.strip_prefix("yuva"))
        .or_else(|| name.strip_prefix("yuv"));
    if let Some(rest) = yuv {
        let (layout, rest) = rest.split_at_checked(3)?;
        let depth = rest.strip_prefix('p')?;
        return Some((depth_or_8(depth)?, yuv_layout(layout)?));
    }

    match name {
        "nv12" | "nv21" => return Some((8, Chroma::Yuv420)),
        "nv16" => return Some((8, Chroma::Yuv422)),
        "nv20" => return Some((10, Chroma::Yuv422)),
        "nv24" | "nv42" => return Some((8, Chroma::Yuv444)),
        "rgb24" | "bgr24" | "rgba" | "bgra" | "argb" | "abgr" | "rgb0" | "bgr0" | "0rgb"
        | "0bgr" => return Some((8, Chroma::Rgb)),
        "x2rgb10" | "x2bgr10" => return Some((10, Chroma::Rgb)),
        "rgb48" | "bgr48" | "rgba64" | "bgra64" => return Some((16, Chroma::Rgb)),
        _ => {}
    }

    // p010, p012, p016, p210, p216, p410, p416: layout digit + depth.
    if let Some(rest) = name.strip_prefix('p') {
        if rest.len() == 3 && rest.bytes().all(|b| b.is_ascii_digit()) {
            let chroma = match &rest[..1] {
                "0" => Chroma::Yuv420,
                "2" => Chroma::Yuv422,
                "4" => Chroma::Yuv444,
                _ => return None,
            };
            return Some((rest[1..].parse().ok()?, chroma));
        }
    }
    for (prefix, chroma) in [
        ("gbrap", Chroma::Rgb),
        ("gbrp", Chroma::Rgb),
        ("gray", Chroma::Gray),
        ("ya", Chroma::Gray),
    ] {
        if let Some(depth) = name.strip_prefix(prefix) {
            return Some((depth_or_8(depth)?, chroma));
        }
    }
    None
}

fn yuv_layout(digits: &str) -> Option<Chroma> {
    Some(match digits {
        "420" => Chroma::Yuv420,
        "422" => Chroma::Yuv422,
        "444" => Chroma::Yuv444,
        "440" => Chroma::Yuv440,
        "411" => Chroma::Yuv411,
        "410" => Chroma::Yuv410,
        _ => return None,
    })
}

fn depth_or_8(s: &str) -> Option<u8> {
    if s.is_empty() {
        Some(8)
    } else {
        s.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_common_formats() {
        let cases = [
            ("yuv420p", Some((8, Chroma::Yuv420))),
            ("yuvj420p", Some((8, Chroma::Yuv420))),
            ("yuv420p10le", Some((10, Chroma::Yuv420))),
            ("yuv422p10le", Some((10, Chroma::Yuv422))),
            ("yuv444p12be", Some((12, Chroma::Yuv444))),
            ("yuva420p", Some((8, Chroma::Yuv420))),
            ("nv12", Some((8, Chroma::Yuv420))),
            ("p010le", Some((10, Chroma::Yuv420))),
            ("p210le", Some((10, Chroma::Yuv422))),
            ("gray10le", Some((10, Chroma::Gray))),
            ("gray", Some((8, Chroma::Gray))),
            ("gbrp10le", Some((10, Chroma::Rgb))),
            ("rgb24", Some((8, Chroma::Rgb))),
            ("pal8", None),
            ("", None),
            ("yuv", None),
        ];
        for (name, want) in cases {
            assert_eq!(pix_fmt_layout(name), want, "{name}");
        }
    }
}
