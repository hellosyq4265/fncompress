//! Processed marker stored in the output container's global metadata, e.g.
//! `FNCOMPRESS=1;v=0.1.0;profile=movies-hevc`. Auto-monitoring always skips a
//! file that carries it; manual transcodes may force through with a warning.
//!
//! MKV stores it under its own key. MP4 only keeps arbitrary keys with
//! `-movflags +use_metadata_tags`, which also drops cover art, so there the
//! marker text goes into the standard `keywords` tag instead.

use std::{collections::BTreeMap, fmt};

use serde::{Deserialize, Serialize};

/// Global metadata key written with `-metadata FNCOMPRESS=...`.
pub const METADATA_KEY: &str = "FNCOMPRESS";

/// MP4 tag that carries `FNCOMPRESS=...` as text (see module docs).
pub const MP4_TAG: &str = "keywords";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProcessedMarker {
    pub version: String,
    pub profile: String,
}

impl ProcessedMarker {
    /// Value of the metadata tag (without the key).
    pub fn to_value(&self) -> String {
        format!("1;v={};profile={}", self.version, self.profile)
    }

    /// Parses a tag value. Unknown fields are ignored so later versions can add
    /// fields; anything not starting with `1` is not our marker.
    pub fn parse(value: &str) -> Option<Self> {
        let mut parts = value.trim().split(';');
        if parts.next()? != "1" {
            return None;
        }
        let mut version = None;
        let mut profile = None;
        for part in parts {
            match part.split_once('=') {
                Some(("v", v)) => version = Some(v.to_owned()),
                Some(("profile", p)) => profile = Some(p.to_owned()),
                _ => {}
            }
        }
        Some(Self {
            version: version.unwrap_or_default(),
            profile: profile.unwrap_or_default(),
        })
    }

    /// Finds the marker in container tags: our own key (any case), or
    /// `FNCOMPRESS=...` at the end of the MP4 `keywords` tag.
    pub fn find(tags: &BTreeMap<String, String>) -> Option<Self> {
        let get = |key: &str| {
            tags.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.as_str())
        };
        if let Some(m) = get(METADATA_KEY).and_then(Self::parse) {
            return Some(m);
        }
        let keywords = get(MP4_TAG)?;
        let at = keywords.rfind(&format!("{METADATA_KEY}="))?;
        Self::parse(&keywords[at + METADATA_KEY.len() + 1..])
    }

    /// New `keywords` value for MP4: existing keywords are kept in front.
    pub fn mp4_keywords(&self, existing: Option<&str>) -> String {
        match existing.map(str::trim).filter(|s| !s.is_empty()) {
            Some(prev) => format!("{prev} {self}"),
            None => self.to_string(),
        }
    }
}

impl fmt::Display for ProcessedMarker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{METADATA_KEY}={}", self.to_value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let m = ProcessedMarker {
            version: "0.1.0".into(),
            profile: "movies".into(),
        };
        assert_eq!(m.to_string(), "FNCOMPRESS=1;v=0.1.0;profile=movies");
        assert_eq!(ProcessedMarker::parse(&m.to_value()), Some(m));
    }

    #[test]
    fn rejects_foreign_values() {
        assert_eq!(ProcessedMarker::parse("0;v=1"), None);
        assert_eq!(ProcessedMarker::parse(""), None);
    }

    #[test]
    fn found_under_own_key_or_in_mp4_keywords() {
        let m = ProcessedMarker {
            version: "0.1.0".into(),
            profile: "tv".into(),
        };
        let tags = |k: &str, v: &str| BTreeMap::from([(k.to_owned(), v.to_owned())]);
        assert_eq!(
            ProcessedMarker::find(&tags("FNCOMPRESS", &m.to_value())),
            Some(m.clone())
        );
        assert_eq!(
            ProcessedMarker::find(&tags("fncompress", &m.to_value())),
            Some(m.clone())
        );
        let kw = m.mp4_keywords(Some("anime, 2024"));
        assert_eq!(kw, "anime, 2024 FNCOMPRESS=1;v=0.1.0;profile=tv");
        assert_eq!(
            ProcessedMarker::find(&tags("keywords", &kw)),
            Some(m.clone())
        );
        assert_eq!(m.mp4_keywords(None), m.to_string());
        assert_eq!(ProcessedMarker::find(&tags("keywords", "anime")), None);
        assert_eq!(
            ProcessedMarker::find(&tags("comment", &m.to_string())),
            None
        );
    }

    #[test]
    fn ignores_unknown_fields() {
        let m = ProcessedMarker::parse("1;v=2.0;profile=x;future=y").unwrap();
        assert_eq!(m.profile, "x");
    }
}
