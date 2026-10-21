//! Probe cache keyed by file identity. M1 stores one JSON file per entry; the
//! M2 server swaps in SQLite behind the same trait.

use std::{fs, path::PathBuf};

use super::{MediaInfo, ProbeDepth, PROBE_SCHEMA};
use crate::identity::FileIdentity;

pub trait ProbeCache: Send + Sync {
    /// A cached result at least as deep as `depth`.
    fn get(&self, identity: &FileIdentity, depth: ProbeDepth) -> Option<MediaInfo>;
    fn put(&self, info: &MediaInfo);
}

pub struct JsonDirCache {
    dir: PathBuf,
}

impl JsonDirCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, identity: &FileIdentity, depth: ProbeDepth) -> PathBuf {
        let depth = match depth {
            ProbeDepth::Quick => "quick",
            ProbeDepth::Full => "full",
        };
        self.dir
            .join(format!("{}.v{PROBE_SCHEMA}.{depth}.json", identity.key()))
    }
}

impl ProbeCache for JsonDirCache {
    fn get(&self, identity: &FileIdentity, depth: ProbeDepth) -> Option<MediaInfo> {
        let depths: &[ProbeDepth] = match depth {
            ProbeDepth::Quick => &[ProbeDepth::Quick, ProbeDepth::Full],
            ProbeDepth::Full => &[ProbeDepth::Full],
        };
        depths.iter().find_map(|d| {
            let bytes = fs::read(self.path(identity, *d)).ok()?;
            let info: MediaInfo = serde_json::from_slice(&bytes).ok()?;
            (info.identity == *identity).then_some(info)
        })
    }

    /// Best effort: a cache that cannot be written only costs a re-probe.
    fn put(&self, info: &MediaInfo) {
        let path = self.path(&info.identity, info.depth);
        let tmp = path.with_extension("tmp");
        let result = fs::create_dir_all(&self.dir)
            .and_then(|()| fs::write(&tmp, serde_json::to_vec(info).expect("serializable")))
            .and_then(|()| fs::rename(&tmp, &path));
        if let Err(e) = result {
            tracing::warn!(path = %path.display(), "probe cache write failed: {e}");
            let _ = fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{FormatInfo, HdrInfo};

    fn info(depth: ProbeDepth, size: u64) -> MediaInfo {
        MediaInfo {
            identity: FileIdentity {
                dev: 1,
                ino: 2,
                size,
                mtime_ns: 3,
            },
            depth,
            format: FormatInfo {
                format_name: "matroska,webm".into(),
                duration_s: Some(1.0),
                size: Some(size),
                bit_rate: None,
                tags: Default::default(),
            },
            streams: vec![],
            chapters: vec![],
            marker: None,
            primary_video: None,
            hdr: HdrInfo::default(),
            frame_side_data: Default::default(),
        }
    }

    #[test]
    fn full_entries_serve_quick_requests_but_not_the_reverse() {
        let dir = tempfile::tempdir().unwrap();
        let cache = JsonDirCache::new(dir.path().join("probe"));
        let quick = info(ProbeDepth::Quick, 10);
        cache.put(&quick);
        assert_eq!(
            cache.get(&quick.identity, ProbeDepth::Quick),
            Some(quick.clone())
        );
        assert_eq!(cache.get(&quick.identity, ProbeDepth::Full), None);

        let full = info(ProbeDepth::Full, 20);
        cache.put(&full);
        assert_eq!(
            cache.get(&full.identity, ProbeDepth::Quick),
            Some(full.clone())
        );
        assert_eq!(cache.get(&full.identity, ProbeDepth::Full), Some(full));
    }

    #[test]
    fn other_identities_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = JsonDirCache::new(dir.path());
        cache.put(&info(ProbeDepth::Full, 10));
        assert_eq!(
            cache.get(&info(ProbeDepth::Full, 11).identity, ProbeDepth::Quick),
            None
        );
    }
}
