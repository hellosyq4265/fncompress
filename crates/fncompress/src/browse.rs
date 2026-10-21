//! Directory listing for picking paths in the web UI (`GET /api/fs`, ADR
//! 0009): subdirectories and video files, leaving out what scanning ignores
//! (hidden entries, NAS metadata directories, the recycle).

use std::{
    fs, io,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::Serialize;
use utoipa::ToSchema;

use crate::scan;

/// Entries returned per directory; the rest are left out.
pub const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Listing {
    /// The listed directory; empty for the list of roots.
    pub path: String,
    /// Where "up" goes; none at a root.
    pub parent: Option<String>,
    pub dirs: Vec<DirEntry>,
    /// Video files only.
    pub files: Vec<FileEntry>,
    /// The directory has more than [`MAX_ENTRIES`] entries.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DirEntry {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub size: u64,
    /// Unix seconds.
    pub mtime: i64,
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The directories the user authorized for the app in fnOS (ADR 0012):
/// `file` holds `TRIM_DATA_ACCESSIBLE_PATHS` as the lifecycle scripts last
/// saw it, `:`-separated (`;` on Windows; newlines also accepted). Missing
/// file, no paths.
pub fn authorized(file: Option<&Path>) -> Vec<PathBuf> {
    let Some(text) = file.and_then(|f| fs::read_to_string(f).ok()) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for p in text
        .lines()
        .flat_map(|l| std::env::split_paths(l.trim()).collect::<Vec<_>>())
    {
        if p.is_absolute() && !paths.contains(&p) {
            paths.push(p);
        }
    }
    paths
}

/// Where browsing starts: the authorized directories (an app user can open
/// them but usually not their parents), then the NAS volumes (`/vol1`,
/// `/vol2`, ...) the service can read, or `/` when the machine has no
/// volumes at all (not a NAS). An fnOS app user cannot read the volume roots,
/// so with nothing authorized the list is empty and the web UI points at
/// fnOS's authorized directories setting. On Windows (development) the
/// drive letters.
pub fn roots(authorized: &[PathBuf]) -> Listing {
    let mut auth: Vec<DirEntry> = authorized
        .iter()
        .filter(|p| p.is_dir())
        .map(|p| DirEntry {
            name: path_string(p),
            path: path_string(p),
        })
        .collect();
    auth.sort_by(|a, b| a.name.cmp(&b.name));
    #[cfg(unix)]
    let mut dirs = match volumes(Path::new("/"), can_read) {
        Some(v) => v,
        None => vec![DirEntry {
            name: "/".into(),
            path: "/".into(),
        }],
    };
    #[cfg(windows)]
    let mut dirs: Vec<DirEntry> = (b'A'..=b'Z')
        .map(|letter| format!("{}:\\", letter as char))
        .filter(|root| can_read(Path::new(root)))
        .map(|root| DirEntry {
            name: root.clone(),
            path: root,
        })
        .collect();
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.retain(|d| !auth.iter().any(|a| a.path == d.path));
    auth.append(&mut dirs);
    Listing {
        path: String::new(),
        parent: None,
        dirs: auth,
        files: Vec::new(),
        truncated: false,
    }
}

/// The directory can be listed by this process.
fn can_read(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok()
}

/// The `vol*` directories in `base` that `readable` accepts; `None` when
/// `base` has no `vol*` directory at all (or cannot be read).
#[cfg_attr(windows, allow(dead_code))]
fn volumes(base: &Path, readable: impl Fn(&Path) -> bool) -> Option<Vec<DirEntry>> {
    let mut any = false;
    let mut vols = Vec::new();
    for e in fs::read_dir(base).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        if !name.starts_with("vol") || !path.is_dir() {
            continue;
        }
        any = true;
        if readable(&path) {
            vols.push(DirEntry {
                path: path_string(&path),
                name,
            });
        }
    }
    any.then_some(vols)
}

/// Lists `dir` (made absolute, symlinks resolved). Symlinked entries are
/// listed as what they point to. An authorized directory has no parent:
/// "up" goes back to the roots.
pub fn list(dir: &Path, authorized: &[PathBuf]) -> io::Result<Listing> {
    let dir: PathBuf = scan::canonical(dir)?;
    if !dir.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotADirectory, "不是目录"));
    }
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut truncated = false;
    for entry in fs::read_dir(&dir)?.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if scan::is_ignored_name(&name) {
            continue;
        }
        if dirs.len() + files.len() >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            dirs.push(DirEntry {
                name,
                path: path_string(&path),
            });
        } else if meta.is_file() && scan::is_video(&path) {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            files.push(FileEntry {
                name,
                path: path_string(&path),
                size: meta.len(),
                mtime,
            });
        }
    }
    let key = |s: &str| s.to_lowercase();
    dirs.sort_by_key(|d| key(&d.name));
    files.sort_by_key(|f| key(&f.name));
    let is_root = authorized
        .iter()
        .any(|a| scan::canonical(a).is_ok_and(|a| a == dir));
    Ok(Listing {
        parent: dir.parent().filter(|_| !is_root).map(path_string),
        path: path_string(&dir),
        dirs,
        files,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_directories_and_videos_without_ignored_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        for sub in [
            "Season 2",
            "season 1",
            ".fncompress-recycle",
            "@eaDir",
            "#recycle",
        ] {
            fs::create_dir(d.join(sub)).unwrap();
        }
        for f in ["b.MKV", "a.mp4", "notes.txt", ".a.fncompress-1-x.mkv"] {
            fs::write(d.join(f), b"12345").unwrap();
        }
        let l = list(d, &[]).unwrap();
        let dirs: Vec<_> = l.dirs.iter().map(|e| e.name.as_str()).collect();
        let files: Vec<_> = l.files.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(dirs, ["season 1", "Season 2"]);
        assert_eq!(files, ["a.mp4", "b.MKV"]);
        assert_eq!(l.files[0].size, 5);
        assert!(l.files[0].mtime > 0);
        assert!(!l.truncated);
        let canonical = scan::canonical(d).unwrap();
        assert_eq!(l.path, path_string(&canonical));
        assert_eq!(l.parent, canonical.parent().map(path_string));
        assert_eq!(l.dirs[0].path, path_string(&canonical.join("season 1")));
    }

    #[test]
    fn refuses_files_and_missing_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.mkv");
        fs::write(&f, b"x").unwrap();
        assert!(list(&f, &[]).is_err());
        assert_eq!(
            list(&tmp.path().join("nope"), &[]).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn roots_have_no_path_or_parent() {
        // Without authorized directories the list may be empty (an fnOS app
        // user reads no volume), so only its shape is checked.
        let r = roots(&[]);
        assert!(r.path.is_empty() && r.parent.is_none());
    }

    #[test]
    fn volumes_the_service_cannot_read_are_left_out() {
        let tmp = tempfile::tempdir().unwrap();
        for d in ["vol1", "vol2", "home"] {
            fs::create_dir(tmp.path().join(d)).unwrap();
        }
        fs::write(tmp.path().join("vol3"), b"").unwrap();
        let names = |v: Vec<DirEntry>| v.into_iter().map(|d| d.name).collect::<Vec<_>>();
        let mut all = names(volumes(tmp.path(), |_| true).unwrap());
        all.sort();
        assert_eq!(all, ["vol1", "vol2"]);
        let some = volumes(tmp.path(), |p| p.ends_with("vol2")).unwrap();
        assert_eq!(names(some), ["vol2"]);
        // Volumes exist but none is readable: an empty list, not `/`.
        assert!(volumes(tmp.path(), |_| false).unwrap().is_empty());
        assert!(volumes(&tmp.path().join("home"), |_| true).is_none());
    }

    /// A real unreadable directory (mode 000); skipped when the tests run as
    /// root, who can read it anyway.
    #[cfg(unix)]
    #[test]
    fn unreadable_volume_is_left_out() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (v1, v2) = (tmp.path().join("vol1"), tmp.path().join("vol2"));
        fs::create_dir(&v1).unwrap();
        fs::create_dir(&v2).unwrap();
        fs::set_permissions(&v2, fs::Permissions::from_mode(0o000)).unwrap();
        let readable = can_read(&v2);
        let vols = volumes(tmp.path(), can_read).unwrap();
        fs::set_permissions(&v2, fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            eprintln!("running as root: mode 000 is still readable, skipped");
            return;
        }
        let names: Vec<_> = vols.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["vol1"]);
    }

    #[test]
    fn authorized_directories_come_first_and_have_no_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (tmp.path().join("影音"), tmp.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let file = tmp.path().join("authorized");
        let missing = tmp.path().join("gone");
        let line = std::env::join_paths([&a, &missing, &a, Path::new("relative")]).unwrap();
        fs::write(
            &file,
            format!("{}\n{}\n", line.to_string_lossy(), a.display()),
        )
        .unwrap();
        let auth = authorized(Some(&file));
        assert_eq!(auth, [a.clone(), missing]);
        assert!(authorized(Some(&tmp.path().join("none"))).is_empty());
        assert!(authorized(None).is_empty());

        let r = roots(&auth);
        assert_eq!(r.dirs[0].path, path_string(&a));
        assert_eq!(
            r.dirs.iter().filter(|d| d.path == path_string(&a)).count(),
            1
        );

        assert_eq!(list(&a, &auth).unwrap().parent, None);
        assert!(list(&b, &auth).unwrap().parent.is_some());
    }
}
