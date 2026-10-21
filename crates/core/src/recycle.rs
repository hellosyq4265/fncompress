//! Recycle directory (回收目录): replaced sources are kept here for a while
//! (default 14 days) and can be emptied in one go.
//!
//! Layout: `<recycle>/<unix seconds>-<nonce>/<original file name>` plus an
//! `entry.json` describing it. Only directories with an `entry.json` are ever
//! deleted, so stray files in the recycle directory are left alone.

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::{identity::FileIdentity, settings::GlobalSettings};

/// Default recycle directory name, created next to each source.
pub const DEFAULT_DIR: &str = ".fncompress-recycle";
const ENTRY_FILE: &str = "entry.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RecycleEntry {
    /// Directory name inside the recycle directory.
    pub id: String,
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub original_path: PathBuf,
    /// Where the file lives now.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub file: PathBuf,
    pub identity: FileIdentity,
    /// Unix seconds.
    pub recycled_at: u64,
    /// The file that replaced it.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub replaced_by: PathBuf,
}

/// Recycle directory for `source`: the configured one, or `.fncompress-recycle`
/// in the source's directory (same filesystem, so recycling is a rename).
pub fn dir_for(source: &Path, global: &GlobalSettings) -> PathBuf {
    match &global.recycle_dir {
        Some(dir) => dir.clone(),
        None => source.parent().unwrap_or(Path::new(".")).join(DEFAULT_DIR),
    }
}

/// Creates an empty slot for `source`; returns (entry dir, path for the file).
pub(crate) fn new_slot(recycle: &Path, source: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?;
    fs::create_dir_all(recycle)?;
    for attempt in 0u32.. {
        let id = format!("{}-{:08x}", now.as_secs(), now.subsec_nanos() ^ attempt);
        let dir = recycle.join(&id);
        match fs::create_dir(&dir) {
            Ok(()) => {
                // The slot and a freshly created recycle directory must
                // survive a power cut along with what goes into them.
                if let Err(e) = sync_dir(recycle).and_then(|()| sync_dir(parent_dir(recycle))) {
                    let _ = fs::remove_dir(&dir);
                    return Err(e);
                }
                let file = dir.join(name);
                return Ok((dir, file));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

pub(crate) fn write_entry(dir: &Path, entry: &RecycleEntry) -> io::Result<()> {
    let json = serde_json::to_vec_pretty(entry).expect("serializable");
    let mut file = fs::File::create(dir.join(ENTRY_FILE))?;
    io::Write::write_all(&mut file, &json)?;
    file.sync_all()?;
    sync_dir(dir)
}

/// Flushes a directory's entries (new names, renames, removals) to disk.
/// Windows cannot open a directory as a file, so there it does nothing.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Directory holding `path`; `.` for a bare file name.
pub(crate) fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Entries in `recycle`, oldest first. A missing directory is empty.
pub fn list(recycle: &Path) -> io::Result<Vec<RecycleEntry>> {
    let read = match fs::read_dir(recycle) {
        Ok(r) => r,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut entries = Vec::new();
    for item in read {
        let path = item?.path();
        let Ok(bytes) = fs::read(path.join(ENTRY_FILE)) else {
            continue;
        };
        match serde_json::from_slice::<RecycleEntry>(&bytes) {
            Ok(e) => entries.push(e),
            Err(err) => tracing::warn!(path = %path.display(), "unreadable recycle entry: {err}"),
        }
    }
    entries.sort_by_key(|e| (e.recycled_at, e.id.clone()));
    Ok(entries)
}

/// Deletes entries older than `retention_days`; returns what was removed.
pub fn purge_expired(
    recycle: &Path,
    retention_days: u32,
    now: u64,
) -> io::Result<Vec<RecycleEntry>> {
    let keep = u64::from(retention_days) * 86_400;
    remove_where(recycle, |e| e.recycled_at.saturating_add(keep) <= now)
}

/// Deletes every entry (一键清空).
pub fn empty(recycle: &Path) -> io::Result<Vec<RecycleEntry>> {
    remove_where(recycle, |_| true)
}

fn remove_where(
    recycle: &Path,
    pred: impl Fn(&RecycleEntry) -> bool,
) -> io::Result<Vec<RecycleEntry>> {
    let mut removed = Vec::new();
    for entry in list(recycle)? {
        if pred(&entry) {
            fs::remove_dir_all(recycle.join(&entry.id))?;
            removed.push(entry);
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(recycle: &Path, name: &str, at: u64) -> RecycleEntry {
        let src = recycle.parent().unwrap().join(name);
        let (dir, file) = new_slot(recycle, &src).unwrap();
        fs::write(&file, name).unwrap();
        let entry = RecycleEntry {
            id: dir.file_name().unwrap().to_string_lossy().into_owned(),
            original_path: src.clone(),
            file,
            identity: FileIdentity {
                dev: 0,
                ino: 0,
                size: name.len() as u64,
                mtime_ns: 0,
            },
            recycled_at: at,
            replaced_by: src,
        };
        write_entry(&dir, &entry).unwrap();
        entry
    }

    #[test]
    fn default_dir_is_next_to_the_source() {
        let g = GlobalSettings::default();
        assert_eq!(
            dir_for(Path::new("/vol1/Movies/a.mkv"), &g),
            Path::new("/vol1/Movies/.fncompress-recycle")
        );
        let g = GlobalSettings {
            recycle_dir: Some("/vol1/recycle".into()),
            ..Default::default()
        };
        assert_eq!(
            dir_for(Path::new("/vol1/Movies/a.mkv"), &g),
            Path::new("/vol1/recycle")
        );
    }

    #[test]
    fn purge_and_empty_only_touch_entries() {
        let root = tempfile::tempdir().unwrap();
        let recycle = root.path().join(DEFAULT_DIR);
        assert!(list(&recycle).unwrap().is_empty(), "missing dir is empty");

        let day = 86_400;
        let old = put(&recycle, "old.mkv", 100);
        let new = put(&recycle, "new.mkv", 100 + 10 * day);
        fs::create_dir_all(recycle.join("someone-elses")).unwrap();
        assert_eq!(list(&recycle).unwrap(), vec![old.clone(), new.clone()]);

        let now = 100 + 14 * day;
        assert_eq!(purge_expired(&recycle, 14, now).unwrap(), vec![old.clone()]);
        assert!(!old.file.exists());
        assert!(new.file.exists());

        assert_eq!(empty(&recycle).unwrap(), vec![new.clone()]);
        assert!(!new.file.exists());
        assert!(recycle.join("someone-elses").exists());
    }

    #[test]
    fn slots_never_collide() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("a.mkv");
        let a = new_slot(root.path(), &src).unwrap();
        let b = new_slot(root.path(), &src).unwrap();
        assert_ne!(a.0, b.0);
        assert_eq!(a.1.file_name(), src.file_name());
    }
}
