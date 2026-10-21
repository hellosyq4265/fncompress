//! SQLite store (rusqlite, bundled): tasks, attempt records, probe cache,
//! saved profiles, monitored directories, server settings. One connection
//! behind a mutex; every statement is short, the long work happens outside.

mod attempts;
mod config;
mod probe_cache;
mod tasks;

use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{types::Type, Connection};
use serde::{de::DeserializeOwned, Serialize};

pub use attempts::NewAttempt;
pub use config::SeenFile;
pub use probe_cache::SqliteProbeCache;
pub use tasks::TaskFilter;

pub type Result<T> = rusqlite::Result<T>;

/// Schema migrations; `PRAGMA user_version` is the number applied.
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE batches (
    id INTEGER PRIMARY KEY,
    dir TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE tasks (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    source TEXT NOT NULL,
    batch_id INTEGER REFERENCES batches(id),
    monitor_id INTEGER,
    path TEXT NOT NULL,
    state TEXT NOT NULL,
    quota TEXT NOT NULL,
    force INTEGER NOT NULL,
    profile_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    snapshot TEXT NOT NULL,
    observe INTEGER NOT NULL DEFAULT 0,
    outcome TEXT,
    candidate TEXT,
    pending_reason TEXT,
    pending_until INTEGER,
    error TEXT,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER
);
CREATE INDEX tasks_state ON tasks(state, id);
CREATE INDEX tasks_batch ON tasks(batch_id);
CREATE INDEX tasks_path ON tasks(path);

CREATE TABLE attempts (
    id INTEGER PRIMARY KEY,
    dev INTEGER NOT NULL,
    ino INTEGER NOT NULL,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    fingerprint TEXT NOT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    code TEXT,
    message TEXT NOT NULL,
    task_id INTEGER,
    at INTEGER NOT NULL,
    UNIQUE (dev, ino, size, mtime_ns, fingerprint)
);
CREATE INDEX attempts_path ON attempts(path);

CREATE TABLE probe_cache (
    identity TEXT NOT NULL,
    depth TEXT NOT NULL,
    schema INTEGER NOT NULL,
    info TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (identity, depth)
);

CREATE TABLE profiles (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    profile TEXT NOT NULL
);
CREATE TABLE monitors (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    enabled INTEGER NOT NULL,
    profile_id TEXT NOT NULL,
    recursive INTEGER NOT NULL,
    excludes TEXT NOT NULL
);
-- Last seen size and mtime of files under monitored directories, so the
-- incremental full scan only looks at what changed.
CREATE TABLE monitor_files (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    decision TEXT NOT NULL,
    at INTEGER NOT NULL
);
-- Recycle directories that received a source, for the retention purge.
CREATE TABLE recycle_dirs (path TEXT PRIMARY KEY);
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#,
    r#"
-- 样片推荐 (M4): the request, the measurements, the chosen candidate.
ALTER TABLE tasks ADD COLUMN recommend TEXT;
ALTER TABLE tasks ADD COLUMN recommendation TEXT;
ALTER TABLE tasks ADD COLUMN choice TEXT;
"#,
];

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&mut conn)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.ensure_default_profile()?;
        Ok(db)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves SQLite itself consistent.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let applied: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A unit enum's serde name, e.g. `TaskState::Pending` → `pending`.
fn name<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        other => panic!("not a unit enum: {other:?}"),
    }
}

fn from_name<T: DeserializeOwned>(col: usize, s: String) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s)).map_err(|e| conversion(col, e))
}

fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("serializable")
}

fn from_json<T: DeserializeOwned>(col: usize, s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| conversion(col, e))
}

fn conversion(col: usize, e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(col, Type::Text, Box::new(e))
}

fn path_str(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_applied_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        drop(Db::open(&path).unwrap());
        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    fn profile_name(db: &Db, id: &str) -> String {
        db.conn()
            .query_row("SELECT name FROM profiles WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    }

    #[test]
    fn built_in_profile_is_named_by_what_it_does() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(profile_name(&db, "default"), config::DEFAULT_PROFILE_NAME);
    }
}
