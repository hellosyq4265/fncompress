//! Free space on the volume a file lives on. Candidates, intermediate files
//! and the recycle (by default) all go next to the source, so the source's
//! volume is the one that fills up.

use std::{
    io,
    path::{Path, PathBuf},
};

/// A mounted filesystem as seen from one of its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// Device number, shared by every file on the filesystem.
    pub dev: u64,
    /// Topmost directory of the filesystem above the file, e.g. `/vol1`.
    pub mount: PathBuf,
    /// Bytes an unprivileged process may still write.
    pub free: u64,
}

/// The volume `path` is on. `None` where this cannot be told (not Unix).
#[cfg(unix)]
pub fn volume_of(path: &Path) -> io::Result<Option<Volume>> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path)?.dev();
    Ok(Some(Volume {
        dev,
        mount: mount_point(path, dev),
        free: free_bytes(path)?,
    }))
}

#[cfg(not(unix))]
pub fn volume_of(_path: &Path) -> io::Result<Option<Volume>> {
    Ok(None)
}

#[cfg(unix)]
fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c` is a valid C string and `st` is written by statvfs on success.
    let rc = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs returned 0, so it filled `st`.
    let st = unsafe { st.assume_init() };
    #[allow(clippy::unnecessary_cast)]
    Ok(st.f_bavail as u64 * st.f_frsize as u64)
}

/// Walks up from `path` while the parent is still on device `dev`.
#[cfg(unix)]
fn mount_point(path: &Path, dev: u64) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let mut top = path.to_owned();
    for parent in path.ancestors().skip(1) {
        match std::fs::metadata(parent) {
            Ok(m) if m.dev() == dev => top = parent.to_owned(),
            _ => break,
        }
    }
    top
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_on_the_volume_of_its_directory() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("fncompress-space-{}", std::process::id()));
        std::fs::write(&file, b"x").unwrap();
        let v = volume_of(&file).unwrap().unwrap();
        let d = volume_of(&dir).unwrap().unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!((v.dev, &v.mount), (d.dev, &d.mount));
        assert!(file.starts_with(&v.mount));
        assert!(v.free > 0);
    }
}
