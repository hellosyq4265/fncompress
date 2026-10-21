use std::path::{Path, PathBuf};

use fncompress_core::settings::{GlobalSettings, Profile};
use rusqlite::{params, OptionalExtension, Row};
use serde::{de::DeserializeOwned, Serialize};

use super::{from_json, now, path_str, to_json, Db, Result};
use crate::model::{MonitorDir, SavedProfile, ServerSettings};

const GLOBAL_KEY: &str = "global_settings";
const SERVER_KEY: &str = "server_settings";
const OBSERVED_KEY: &str = "observed_count";
const RISK_ACK_KEY: &str = "risk_acknowledged";

fn row_to_profile(r: &Row) -> Result<SavedProfile> {
    Ok(SavedProfile {
        id: r.get(0)?,
        name: r.get(1)?,
        profile: from_json(2, &r.get::<_, String>(2)?)?,
    })
}

fn row_to_monitor(r: &Row) -> Result<MonitorDir> {
    Ok(MonitorDir {
        id: r.get(0)?,
        path: PathBuf::from(r.get::<_, String>(1)?),
        enabled: r.get(2)?,
        profile_id: r.get(3)?,
        recursive: r.get(4)?,
        excludes: from_json(5, &r.get::<_, String>(5)?)?,
    })
}

/// A monitored file as the last scan saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct SeenFile {
    pub size: u64,
    pub mtime_ns: i64,
    /// What the monitor decided, e.g. `queued`, `marked`, `attempted`.
    pub decision: String,
}

/// Name of the built-in profile (id `default`): what it does, not "默认",
/// since another profile may become the default.
pub(super) const DEFAULT_PROFILE_NAME: &str = "软件 HEVC（x265）";

impl Db {
    pub(super) fn ensure_default_profile(&self) -> Result<()> {
        let p = Profile::default();
        self.conn().execute(
            "INSERT OR IGNORE INTO profiles (id, name, profile) VALUES (?1, ?2, ?3)",
            params![p.id, DEFAULT_PROFILE_NAME, to_json(&p)],
        )?;
        Ok(())
    }

    fn kv_get<T: DeserializeOwned + Default>(&self, key: &str) -> Result<T> {
        let value: Option<String> = self
            .conn()
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?;
        value.map_or_else(|| Ok(T::default()), |v| from_json(0, &v))
    }

    fn kv_set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        self.conn().execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, to_json(value)],
        )?;
        Ok(())
    }

    pub fn global_settings(&self) -> Result<GlobalSettings> {
        self.kv_get(GLOBAL_KEY)
    }

    pub fn set_global_settings(&self, s: &GlobalSettings) -> Result<()> {
        self.kv_set(GLOBAL_KEY, s)
    }

    pub fn server_settings(&self) -> Result<ServerSettings> {
        self.kv_get(SERVER_KEY)
    }

    pub fn set_server_settings(&self, s: &ServerSettings) -> Result<()> {
        self.kv_set(SERVER_KEY, s)
    }

    /// Files the observation mode has taken so far.
    pub fn observed_count(&self) -> Result<u32> {
        self.kv_get(OBSERVED_KEY)
    }

    pub fn set_observed_count(&self, n: u32) -> Result<()> {
        self.kv_set(OBSERVED_KEY, &n)
    }

    /// Counts one more observed file, atomically; returns the new count.
    pub fn bump_observed_count(&self) -> Result<u32> {
        self.conn().query_row(
            "INSERT INTO kv (key, value) VALUES (?1, '1') \
             ON CONFLICT (key) DO UPDATE SET value = CAST(value AS INTEGER) + 1 \
             RETURNING CAST(value AS INTEGER)",
            [OBSERVED_KEY],
            |r| r.get(0),
        )
    }

    /// The app version in which the user acknowledged the risk notice;
    /// `None` until they do.
    pub fn risk_acknowledged(&self) -> Result<Option<String>> {
        self.kv_get(RISK_ACK_KEY)
    }

    pub fn set_risk_acknowledged(&self, version: &str) -> Result<()> {
        self.kv_set(RISK_ACK_KEY, &version)
    }

    pub fn profiles(&self) -> Result<Vec<SavedProfile>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id, name, profile FROM profiles ORDER BY id")?;
        let rows = stmt.query_map([], row_to_profile)?;
        rows.collect()
    }

    pub fn profile(&self, id: &str) -> Result<Option<SavedProfile>> {
        self.conn()
            .query_row(
                "SELECT id, name, profile FROM profiles WHERE id = ?1",
                [id],
                row_to_profile,
            )
            .optional()
    }

    /// Inserts or replaces; the stored profile's id is forced to `id`.
    pub fn put_profile(&self, p: &SavedProfile) -> Result<SavedProfile> {
        let mut p = p.clone();
        p.profile.id = p.id.clone();
        self.conn().execute(
            "INSERT INTO profiles (id, name, profile) VALUES (?1, ?2, ?3) \
             ON CONFLICT (id) DO UPDATE SET name = excluded.name, profile = excluded.profile",
            params![p.id, p.name, to_json(&p.profile)],
        )?;
        Ok(p)
    }

    pub fn delete_profile(&self, id: &str) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM profiles WHERE id = ?1", [id])?
            == 1)
    }

    /// Monitored directories using this profile.
    pub fn monitors_using(&self, profile_id: &str) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM monitors WHERE profile_id = ?1")?;
        let rows = stmt.query_map([profile_id], |r| r.get(0))?;
        rows.collect()
    }

    pub fn monitors(&self) -> Result<Vec<MonitorDir>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, path, enabled, profile_id, recursive, excludes FROM monitors ORDER BY path",
        )?;
        let rows = stmt.query_map([], row_to_monitor)?;
        rows.collect()
    }

    pub fn monitor(&self, id: i64) -> Result<Option<MonitorDir>> {
        self.conn()
            .query_row(
                "SELECT id, path, enabled, profile_id, recursive, excludes FROM monitors WHERE id = ?1",
                [id],
                row_to_monitor,
            )
            .optional()
    }

    /// Inserts when `m.id` is 0, otherwise updates.
    pub fn put_monitor(&self, m: &MonitorDir) -> Result<MonitorDir> {
        let id = {
            let conn = self.conn();
            let args = params![
                path_str(&m.path),
                m.enabled,
                m.profile_id,
                m.recursive,
                to_json(&m.excludes)
            ];
            if m.id == 0 {
                conn.execute(
                    "INSERT INTO monitors (path, enabled, profile_id, recursive, excludes) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    args,
                )?;
                conn.last_insert_rowid()
            } else {
                conn.execute(
                    "UPDATE monitors SET path = ?1, enabled = ?2, profile_id = ?3, recursive = ?4, \
                     excludes = ?5 WHERE id = ?6",
                    params![
                        path_str(&m.path),
                        m.enabled,
                        m.profile_id,
                        m.recursive,
                        to_json(&m.excludes),
                        m.id
                    ],
                )?;
                m.id
            }
        };
        self.monitor(id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)
    }

    pub fn delete_monitor(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM monitors WHERE id = ?1", [id])?
            == 1)
    }

    pub fn seen_file(&self, path: &Path) -> Result<Option<SeenFile>> {
        self.conn()
            .query_row(
                "SELECT size, mtime_ns, decision FROM monitor_files WHERE path = ?1",
                [path_str(path)],
                |r| {
                    Ok(SeenFile {
                        size: r.get::<_, i64>(0)? as u64,
                        mtime_ns: r.get(1)?,
                        decision: r.get(2)?,
                    })
                },
            )
            .optional()
    }

    pub fn put_seen_file(&self, path: &Path, f: &SeenFile) -> Result<()> {
        self.conn().execute(
            "INSERT INTO monitor_files (path, size, mtime_ns, decision, at) VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT (path) DO UPDATE SET size = excluded.size, mtime_ns = excluded.mtime_ns, \
             decision = excluded.decision, at = excluded.at",
            params![path_str(path), f.size as i64, f.mtime_ns, f.decision, now()],
        )?;
        Ok(())
    }

    pub fn forget_seen_file(&self, path: &Path) -> Result<()> {
        self.conn().execute(
            "DELETE FROM monitor_files WHERE path = ?1",
            [path_str(path)],
        )?;
        Ok(())
    }

    pub fn add_recycle_dir(&self, dir: &Path) -> Result<()> {
        self.conn().execute(
            "INSERT OR IGNORE INTO recycle_dirs (path) VALUES (?1)",
            [path_str(dir)],
        )?;
        Ok(())
    }

    pub fn recycle_dirs(&self) -> Result<Vec<PathBuf>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT path FROM recycle_dirs ORDER BY path")?;
        let rows = stmt.query_map([], |r| Ok(PathBuf::from(r.get::<_, String>(0)?)))?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use fncompress_core::encoder::Encoder;

    use super::*;

    #[test]
    fn settings_default_until_written() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.server_settings().unwrap(), ServerSettings::default());
        assert_eq!(db.global_settings().unwrap(), GlobalSettings::default());
        let s = ServerSettings {
            gpu_slots: 2,
            ..Default::default()
        };
        db.set_server_settings(&s).unwrap();
        assert_eq!(db.server_settings().unwrap(), s);
        assert_eq!(db.observed_count().unwrap(), 0);
        assert_eq!(db.bump_observed_count().unwrap(), 1);
        assert_eq!(db.bump_observed_count().unwrap(), 2);
        db.set_observed_count(5).unwrap();
        assert_eq!(db.bump_observed_count().unwrap(), 6);
        assert_eq!(db.observed_count().unwrap(), 6);
        db.set_observed_count(0).unwrap();
        assert_eq!(db.observed_count().unwrap(), 0);
    }

    #[test]
    fn profiles_keep_their_id_in_the_profile() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.profiles().unwrap()[0].id, "default");
        let saved = db
            .put_profile(&SavedProfile {
                id: "movies".into(),
                name: "电影".into(),
                profile: Profile {
                    id: "whatever".into(),
                    encoder: Encoder::HevcQsv,
                    ..Default::default()
                },
            })
            .unwrap();
        assert_eq!(saved.profile.id, "movies");
        assert_eq!(db.profile("movies").unwrap().unwrap(), saved);
        assert_eq!(db.profiles().unwrap().len(), 2);
        assert!(db.delete_profile("movies").unwrap());
        assert!(db.profile("movies").unwrap().is_none());
    }

    #[test]
    fn monitors_and_seen_files() {
        let db = Db::open_in_memory().unwrap();
        let m = db
            .put_monitor(&MonitorDir {
                id: 0,
                path: "/vol1/tv".into(),
                enabled: true,
                profile_id: "default".into(),
                recursive: true,
                excludes: vec!["**/Extras/**".into()],
            })
            .unwrap();
        assert!(m.id > 0);
        let updated = db
            .put_monitor(&MonitorDir {
                enabled: false,
                ..m.clone()
            })
            .unwrap();
        assert!(!updated.enabled);
        assert_eq!(db.monitors().unwrap(), [updated]);
        assert_eq!(db.monitors_using("default").unwrap(), [m.id]);

        let p = Path::new("/vol1/tv/a.mkv");
        assert!(db.seen_file(p).unwrap().is_none());
        let f = SeenFile {
            size: 10,
            mtime_ns: 5,
            decision: "queued".into(),
        };
        db.put_seen_file(p, &f).unwrap();
        assert_eq!(db.seen_file(p).unwrap(), Some(f));
        db.forget_seen_file(p).unwrap();
        assert!(db.seen_file(p).unwrap().is_none());
        assert!(db.delete_monitor(m.id).unwrap());

        db.add_recycle_dir(Path::new("/vol1/tv/.fncompress-recycle"))
            .unwrap();
        db.add_recycle_dir(Path::new("/vol1/tv/.fncompress-recycle"))
            .unwrap();
        assert_eq!(db.recycle_dirs().unwrap().len(), 1);
    }
}
