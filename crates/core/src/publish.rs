//! Publication (发布): put a verified candidate in place of its source and
//! keep the source in the recycle directory.
//!
//! - Both files must still have the identities they had at verification.
//! - Same path: the source is hard-linked into the recycle directory, then the
//!   candidate is renamed over it, so the source path never goes missing.
//! - Container change: the candidate goes to the new extension with a
//!   no-clobber rename (an existing target is refused), then the source moves
//!   into the recycle directory.
//! - A recycle directory on another filesystem falls back to copy + delete.
//! - Everything is flushed before the next step relies on it: the candidate
//!   before it is renamed, the recycle slot after the source enters it, the
//!   directories after each rename, and the entry file once written.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    identity::FileIdentity,
    recycle::{self, RecycleEntry},
};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Savings {
    pub source_size: u64,
    pub candidate_size: u64,
    /// Space saved relative to the source, in percent (negative if larger).
    pub saved_pct: f64,
}

impl Savings {
    pub fn new(source_size: u64, candidate_size: u64) -> Self {
        let saved_pct = if source_size == 0 {
            0.0
        } else {
            (source_size as f64 - candidate_size as f64) / source_size as f64 * 100.0
        };
        Self {
            source_size,
            candidate_size,
            saved_pct,
        }
    }

    /// Below `min_pct` the result is "insufficient gain" (收益不足).
    pub fn sufficient(&self, min_pct: f64) -> bool {
        self.saved_pct >= min_pct
    }
}

pub struct PublishRequest<'a> {
    pub source: &'a Path,
    pub candidate: &'a Path,
    /// Equal to `source`, or the source with the new container's extension.
    pub target: &'a Path,
    pub source_identity: FileIdentity,
    pub candidate_identity: FileIdentity,
    pub recycle_dir: &'a Path,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Published {
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub target: PathBuf,
    pub recycled: RecycleEntry,
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("校验后源片被改动：{0}")]
    SourceChanged(PathBuf),
    #[error("校验后成品被改动：{0}")]
    CandidateChanged(PathBuf),
    #[error("目标文件已存在：{0}")]
    TargetExists(PathBuf),
    #[error("发布失败（{path}）：{source}")]
    Io { path: PathBuf, source: io::Error },
}

fn io_err(path: &Path) -> impl FnOnce(io::Error) -> PublishError + '_ {
    move |source| PublishError::Io {
        path: path.to_owned(),
        source,
    }
}

pub fn publish(req: &PublishRequest) -> Result<Published, PublishError> {
    let unchanged =
        |path: &Path, want: FileIdentity| FileIdentity::of(path).is_ok_and(|id| id == want);
    if !unchanged(req.source, req.source_identity) {
        return Err(PublishError::SourceChanged(req.source.to_owned()));
    }
    if !unchanged(req.candidate, req.candidate_identity) {
        return Err(PublishError::CandidateChanged(req.candidate.to_owned()));
    }
    // Otherwise a power cut after the rename can leave an empty or partial
    // file at the source path.
    sync_file(req.candidate).map_err(io_err(req.candidate))?;

    let (slot, recycled_file) =
        recycle::new_slot(req.recycle_dir, req.source).map_err(io_err(req.recycle_dir))?;
    let entry = RecycleEntry {
        id: slot
            .file_name()
            .expect("slot has a name")
            .to_string_lossy()
            .into_owned(),
        original_path: req.source.to_owned(),
        file: recycled_file.clone(),
        identity: req.source_identity,
        recycled_at: recycle::now_secs(),
        replaced_by: req.target.to_owned(),
    };
    let result = if req.target == req.source {
        replace_in_place(req, &recycled_file)
    } else {
        publish_alongside(req, &recycled_file)
    };
    if let Err(e) = result {
        let _ = fs::remove_dir_all(&slot);
        return Err(e);
    }
    // The entry is written last: a slot without one is never purged, so a
    // crash here leaves the old file in place rather than deleting it.
    recycle::write_entry(&slot, &entry).map_err(io_err(&slot))?;
    Ok(Published {
        target: req.target.to_owned(),
        recycled: entry,
    })
}

fn replace_in_place(req: &PublishRequest, recycled: &Path) -> Result<(), PublishError> {
    keep_copy(req.source, recycled).map_err(io_err(recycled))?;
    // A hard link shares the inode; a copy must at least match in size.
    let kept = FileIdentity::of(recycled).map_err(io_err(recycled))?;
    if kept.size != req.source_identity.size {
        let _ = fs::remove_file(recycled);
        return Err(PublishError::SourceChanged(req.source.to_owned()));
    }
    recycle::sync_dir(recycle::parent_dir(recycled)).map_err(io_err(recycled))?;
    fs::rename(req.candidate, req.source).map_err(io_err(req.source))?;
    sync_dir_after_rename(req.source);
    Ok(())
}

fn publish_alongside(req: &PublishRequest, recycled: &Path) -> Result<(), PublishError> {
    match rename_noreplace(req.candidate, req.target) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Err(PublishError::TargetExists(req.target.to_owned()))
        }
        Err(e) => return Err(io_err(req.target)(e)),
    }
    sync_dir_after_rename(req.target);
    move_file(req.source, recycled).map_err(io_err(recycled))?;
    sync_dir_after_rename(recycled);
    if recycle::parent_dir(req.source) != recycle::parent_dir(req.target) {
        sync_dir_after_rename(req.source);
    }
    Ok(())
}

fn sync_file(path: &Path) -> io::Result<()> {
    // Windows only flushes through a handle opened for writing.
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// Flushes the directory holding `path` after a rename. The rename has
/// already happened, so a failure is only logged: failing the publish here
/// would delete the recycle slot, which by now holds the only original.
fn sync_dir_after_rename(path: &Path) {
    let dir = recycle::parent_dir(path);
    if let Err(e) = recycle::sync_dir(dir) {
        tracing::warn!(dir = %dir.display(), "fsync after publish failed: {e}");
    }
}

/// Hard-links `from` to `to`, or copies across filesystems.
fn keep_copy(from: &Path, to: &Path) -> io::Result<()> {
    match fs::hard_link(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) || e.kind() == io::ErrorKind::Unsupported => {
            copy_synced(from, to)
        }
        Err(e) => Err(e),
    }
}

/// Renames, or copies and deletes across filesystems.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            copy_synced(from, to)?;
            fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

/// Copy that keeps the modification time, so a recycled copy looks like the
/// original when restored.
fn copy_synced(from: &Path, to: &Path) -> io::Result<()> {
    fs::copy(from, to)?;
    let file = fs::OpenOptions::new().write(true).open(to)?;
    file.set_modified(fs::metadata(from)?.modified()?)?;
    file.sync_all()
}

fn is_cross_device(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::CrossesDevices
}

/// Rename that fails with `AlreadyExists` instead of replacing `to`.
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};
        let c = |p: &Path| {
            CString::new(p.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
        };
        let (f, t) = (c(from)?, c(to)?);
        // SAFETY: both pointers are valid NUL-terminated strings for the call.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                f.as_ptr(),
                libc::AT_FDCWD,
                t.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        // Filesystems without RENAME_NOREPLACE fall through to link + unlink.
        if !matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS)) {
            return Err(e);
        }
    }
    // link() refuses an existing target, which makes this no-clobber too.
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Setup {
        _dir: tempfile::TempDir,
        source: PathBuf,
        candidate: PathBuf,
        recycle: PathBuf,
    }

    fn setup(source_name: &str) -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join(source_name);
        let candidate = dir.path().join(format!(".{source_name}.fncompress-1.tmp"));
        fs::write(&source, b"original source bytes").unwrap();
        fs::write(&candidate, b"smaller").unwrap();
        let recycle = dir.path().join(recycle::DEFAULT_DIR);
        Setup {
            _dir: dir,
            source,
            candidate,
            recycle,
        }
    }

    fn request<'a>(s: &'a Setup, target: &'a Path) -> PublishRequest<'a> {
        PublishRequest {
            source: &s.source,
            candidate: &s.candidate,
            target,
            source_identity: FileIdentity::of(&s.source).unwrap(),
            candidate_identity: FileIdentity::of(&s.candidate).unwrap(),
            recycle_dir: &s.recycle,
        }
    }

    #[test]
    fn same_path_replaces_and_recycles_the_source() {
        let s = setup("a.mkv");
        let p = publish(&request(&s, &s.source)).unwrap();
        assert_eq!(fs::read(&s.source).unwrap(), b"smaller");
        assert!(!s.candidate.exists());
        assert_eq!(
            fs::read(&p.recycled.file).unwrap(),
            b"original source bytes"
        );
        assert_eq!(p.recycled.original_path, s.source);
        assert_eq!(recycle::list(&s.recycle).unwrap(), vec![p.recycled]);
    }

    #[test]
    fn container_change_writes_new_extension_and_recycles_the_source() {
        let s = setup("a.mp4");
        let target = s.source.with_extension("mkv");
        let p = publish(&request(&s, &target)).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"smaller");
        assert!(!s.source.exists());
        assert!(!s.candidate.exists());
        assert_eq!(
            fs::read(&p.recycled.file).unwrap(),
            b"original source bytes"
        );
        assert_eq!(p.recycled.replaced_by, target);
    }

    #[test]
    fn existing_target_is_refused_and_nothing_moves() {
        let s = setup("a.mp4");
        let target = s.source.with_extension("mkv");
        fs::write(&target, b"someone else's").unwrap();
        let err = publish(&request(&s, &target)).unwrap_err();
        assert!(matches!(err, PublishError::TargetExists(_)), "{err}");
        assert_eq!(fs::read(&target).unwrap(), b"someone else's");
        assert!(s.source.exists() && s.candidate.exists());
        assert!(recycle::list(&s.recycle).unwrap().is_empty());
    }

    #[test]
    fn changed_files_are_refused() {
        let s = setup("a.mkv");
        let req = request(&s, &s.source);
        fs::write(&s.source, b"edited after verification").unwrap();
        assert!(matches!(publish(&req), Err(PublishError::SourceChanged(_))));

        let s = setup("b.mkv");
        let req = request(&s, &s.source);
        fs::write(&s.candidate, b"tampered").unwrap();
        assert!(matches!(
            publish(&req),
            Err(PublishError::CandidateChanged(_))
        ));
        assert_eq!(fs::read(&s.source).unwrap(), b"original source bytes");
    }

    #[test]
    fn noreplace_rename_refuses_existing_targets() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        let err = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&b).unwrap(), b"b");
        fs::remove_file(&b).unwrap();
        rename_noreplace(&a, &b).unwrap();
        assert!(!a.exists());
        assert_eq!(fs::read(&b).unwrap(), b"a");
    }

    #[test]
    fn cross_filesystem_copy_keeps_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, b"data").unwrap();
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(old)
            .unwrap();
        copy_synced(&a, &b).unwrap();
        assert_eq!(fs::read(&b).unwrap(), b"data");
        assert_eq!(fs::metadata(&b).unwrap().modified().unwrap(), old);
    }

    #[test]
    fn savings_threshold() {
        let s = Savings::new(1000, 960);
        assert!((s.saved_pct - 4.0).abs() < 1e-9);
        assert!(!s.sufficient(5.0));
        assert!(Savings::new(1000, 950).sufficient(5.0));
        assert!(!Savings::new(1000, 1200).sufficient(5.0));
        assert_eq!(Savings::new(0, 10).saved_pct, 0.0);
    }
}
