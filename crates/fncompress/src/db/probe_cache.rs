use std::sync::Arc;

use fncompress_core::{
    identity::FileIdentity,
    probe::{MediaInfo, ProbeCache, ProbeDepth, PROBE_SCHEMA},
};
use rusqlite::{params, OptionalExtension};

use super::{name, now, to_json, Db, Result};

/// Probe results keyed by file identity, in SQLite. Entries of another
/// [`PROBE_SCHEMA`] are ignored and overwritten.
pub struct SqliteProbeCache {
    db: Arc<Db>,
}

impl SqliteProbeCache {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }
}

impl Db {
    fn probe_get(&self, identity: &FileIdentity, depth: ProbeDepth) -> Result<Option<MediaInfo>> {
        let text: Option<String> = self
            .conn()
            .query_row(
                "SELECT info FROM probe_cache WHERE identity = ?1 AND depth = ?2 AND schema = ?3",
                params![identity.key(), name(&depth), PROBE_SCHEMA],
                |r| r.get(0),
            )
            .optional()?;
        // A row that no longer parses is just a miss.
        Ok(text
            .and_then(|t| serde_json::from_str::<MediaInfo>(&t).ok())
            .filter(|info| info.identity == *identity))
    }

    fn probe_put(&self, info: &MediaInfo) -> Result<()> {
        self.conn().execute(
            "INSERT INTO probe_cache (identity, depth, schema, info, at) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (identity, depth) DO UPDATE SET schema = excluded.schema, \
             info = excluded.info, at = excluded.at",
            params![
                info.identity.key(),
                name(&info.depth),
                PROBE_SCHEMA,
                to_json(info),
                now()
            ],
        )?;
        Ok(())
    }

    /// Drops entries not written since `before` (unix seconds) and entries
    /// of an older schema.
    pub fn prune_probe_cache(&self, before: i64) -> Result<usize> {
        self.conn().execute(
            "DELETE FROM probe_cache WHERE at < ?1 OR schema <> ?2",
            params![before, PROBE_SCHEMA],
        )
    }
}

impl ProbeCache for SqliteProbeCache {
    fn get(&self, identity: &FileIdentity, depth: ProbeDepth) -> Option<MediaInfo> {
        let depths: &[ProbeDepth] = match depth {
            ProbeDepth::Quick => &[ProbeDepth::Quick, ProbeDepth::Full],
            ProbeDepth::Full => &[ProbeDepth::Full],
        };
        depths
            .iter()
            .find_map(|d| match self.db.probe_get(identity, *d) {
                Ok(hit) => hit,
                Err(e) => {
                    tracing::warn!("probe cache read failed: {e}");
                    None
                }
            })
    }

    /// Best effort: a failed write only costs a re-probe.
    fn put(&self, info: &MediaInfo) {
        if let Err(e) = self.db.probe_put(info) {
            tracing::warn!("probe cache write failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use fncompress_core::probe::{FormatInfo, HdrInfo};

    use super::*;

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
        let db = Arc::new(Db::open_in_memory().unwrap());
        let cache = SqliteProbeCache::new(db.clone());
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
        assert_eq!(
            cache.get(&info(ProbeDepth::Full, 30).identity, ProbeDepth::Quick),
            None
        );
    }

    #[test]
    fn other_schemas_miss_and_are_pruned() {
        let db = Arc::new(Db::open_in_memory().unwrap());
        let cache = SqliteProbeCache::new(db.clone());
        let full = info(ProbeDepth::Full, 10);
        cache.put(&full);
        db.conn()
            .execute("UPDATE probe_cache SET schema = schema - 1", [])
            .unwrap();
        assert_eq!(cache.get(&full.identity, ProbeDepth::Full), None);
        assert_eq!(db.prune_probe_cache(0).unwrap(), 1);
    }
}
