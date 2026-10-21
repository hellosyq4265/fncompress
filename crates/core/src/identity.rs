//! File identity: the key for the probe cache and attempt records (ADR 0005).
//! A path can be renamed or moved; (device, inode, size, mtime) cannot change
//! without the file itself changing.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
}

impl FileIdentity {
    pub fn of(path: &Path) -> io::Result<Self> {
        Ok(Self::from_metadata(&fs::metadata(path)?))
    }

    #[cfg(unix)]
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        }
    }

    /// Windows is only a development host: the stable std API exposes no
    /// volume serial or file index, so dev/ino are 0 there.
    #[cfg(not(unix))]
    pub fn from_metadata(meta: &fs::Metadata) -> Self {
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i64);
        Self {
            dev: 0,
            ino: 0,
            size: meta.len(),
            mtime_ns,
        }
    }

    /// Stable string form, used as a cache file name.
    pub fn key(&self) -> String {
        format!("{}-{}-{}-{}", self.dev, self.ino, self.size, self.mtime_ns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn changes_when_content_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        fs::write(&path, b"one").unwrap();
        let before = FileIdentity::of(&path).unwrap();
        assert_eq!(before, FileIdentity::of(&path).unwrap());

        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"more")
            .unwrap();
        let after = FileIdentity::of(&path).unwrap();
        assert_ne!(before, after);
        assert_eq!(after.size, 7);
    }
}
