use std::path::{Path, PathBuf};

use fncompress_core::identity::FileIdentity;
use rusqlite::{params, OptionalExtension, Row};

use super::{from_name, name, now, path_str, Db, Result};
use crate::model::{Attempt, AttemptKind};

const COLUMNS: &str =
    "id, dev, ino, size, mtime_ns, fingerprint, path, kind, code, message, task_id, at";

fn row_to_attempt(r: &Row) -> Result<Attempt> {
    Ok(Attempt {
        id: r.get(0)?,
        identity: FileIdentity {
            dev: r.get::<_, i64>(1)? as u64,
            ino: r.get::<_, i64>(2)? as u64,
            size: r.get::<_, i64>(3)? as u64,
            mtime_ns: r.get(4)?,
        },
        fingerprint: r.get(5)?,
        path: PathBuf::from(r.get::<_, String>(6)?),
        kind: from_name(7, r.get(7)?)?,
        code: r.get(8)?,
        message: r.get(9)?,
        task_id: r.get(10)?,
        at: r.get(11)?,
    })
}

/// A failed attempt to record.
pub struct NewAttempt<'a> {
    pub identity: FileIdentity,
    pub fingerprint: &'a str,
    pub path: &'a Path,
    pub kind: AttemptKind,
    pub code: Option<&'a str>,
    pub message: &'a str,
    pub task_id: Option<i64>,
}

impl Db {
    /// Records (or replaces) the attempt for this file identity and fingerprint.
    pub fn record_attempt(&self, a: &NewAttempt) -> Result<()> {
        let id = a.identity;
        self.conn().execute(
            "INSERT INTO attempts (dev, ino, size, mtime_ns, fingerprint, path, kind, code, message, task_id, at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
             ON CONFLICT (dev, ino, size, mtime_ns, fingerprint) DO UPDATE SET \
             path = excluded.path, kind = excluded.kind, code = excluded.code, \
             message = excluded.message, task_id = excluded.task_id, at = excluded.at",
            params![
                id.dev as i64,
                id.ino as i64,
                id.size as i64,
                id.mtime_ns,
                a.fingerprint,
                path_str(a.path),
                name(&a.kind),
                a.code,
                a.message,
                a.task_id,
                now(),
            ],
        )?;
        Ok(())
    }

    /// The attempt that blocks an automatic retry of this file with this profile.
    pub fn attempt_for(
        &self,
        identity: &FileIdentity,
        fingerprint: &str,
    ) -> Result<Option<Attempt>> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM attempts \
                     WHERE dev = ?1 AND ino = ?2 AND size = ?3 AND mtime_ns = ?4 AND fingerprint = ?5"
                ),
                params![
                    identity.dev as i64,
                    identity.ino as i64,
                    identity.size as i64,
                    identity.mtime_ns,
                    fingerprint
                ],
                row_to_attempt,
            )
            .optional()
    }

    /// Every attempt on this file identity, any profile; newest first.
    pub fn attempts_for_identity(&self, identity: &FileIdentity) -> Result<Vec<Attempt>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM attempts \
             WHERE dev = ?1 AND ino = ?2 AND size = ?3 AND mtime_ns = ?4 ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map(
            params![
                identity.dev as i64,
                identity.ino as i64,
                identity.size as i64,
                identity.mtime_ns
            ],
            row_to_attempt,
        )?;
        rows.collect()
    }

    /// Newest first; `path_prefix` narrows to a directory.
    pub fn attempts(&self, path_prefix: Option<&Path>, limit: u32) -> Result<Vec<Attempt>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM attempts WHERE ?1 IS NULL OR substr(path, 1, length(?1)) = ?1 \
             ORDER BY id DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![path_prefix.map(path_str), limit], row_to_attempt)?;
        rows.collect()
    }

    /// Forgets an attempt so the monitor tries the file again.
    pub fn delete_attempt(&self, id: i64) -> Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM attempts WHERE id = ?1", [id])?
            == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: FileIdentity = FileIdentity {
        dev: 2049,
        ino: u64::MAX - 1,
        size: 1 << 40,
        mtime_ns: 1_700_000_000_123_456_789,
    };

    fn attempt<'a>(fingerprint: &'a str, kind: AttemptKind) -> NewAttempt<'a> {
        NewAttempt {
            identity: ID,
            fingerprint,
            path: Path::new("/vol1/m/a.mkv"),
            kind,
            code: None,
            message: "收益不足",
            task_id: Some(7),
        }
    }

    #[test]
    fn keyed_by_identity_and_fingerprint() {
        let db = Db::open_in_memory().unwrap();
        db.record_attempt(&attempt("aaaa", AttemptKind::InsufficientSavings))
            .unwrap();
        let got = db.attempt_for(&ID, "aaaa").unwrap().unwrap();
        // Large u64 values survive the round trip through SQLite's i64.
        assert_eq!(got.identity, ID);
        assert_eq!(got.kind, AttemptKind::InsufficientSavings);
        assert_eq!(got.task_id, Some(7));
        assert!(db.attempt_for(&ID, "bbbb").unwrap().is_none());
        let touched = FileIdentity {
            mtime_ns: ID.mtime_ns + 1,
            ..ID
        };
        assert!(db.attempt_for(&touched, "aaaa").unwrap().is_none());

        // Same key again replaces the record.
        db.record_attempt(&NewAttempt {
            code: Some("hdr"),
            ..attempt("aaaa", AttemptKind::Skipped)
        })
        .unwrap();
        db.record_attempt(&attempt("bbbb", AttemptKind::Rejected))
            .unwrap();
        let all = db.attempts_for_identity(&ID).unwrap();
        assert_eq!(all.len(), 2);
        let a = db.attempt_for(&ID, "aaaa").unwrap().unwrap();
        assert_eq!(
            (a.kind, a.code.as_deref()),
            (AttemptKind::Skipped, Some("hdr"))
        );

        assert_eq!(
            db.attempts(Some(Path::new("/vol1/m")), 10).unwrap().len(),
            2
        );
        assert_eq!(db.attempts(Some(Path::new("/vol2")), 10).unwrap().len(), 0);
        assert!(db.delete_attempt(a.id).unwrap());
        assert!(db.attempt_for(&ID, "aaaa").unwrap().is_none());
    }
}
