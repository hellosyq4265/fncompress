//! Finding and screening files, shared by folder batches and the monitor:
//! which files count as videos, which directories are never entered, and
//! whether a file should be queued at all (ADR 0005).

use std::{
    fs,
    path::{Path, PathBuf},
};

use fncompress_core::{
    identity::FileIdentity,
    probe::{ProbeDepth, Prober},
};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;
use utoipa::ToSchema;

use crate::db::{self, Db};

/// Extensions treated as video. Containers the route cannot keep are still
/// listed: a profile that converts to MKV or MP4 can handle them.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "m4v", "ts", "m2ts", "mts", "mov", "avi", "wmv", "flv", "webm", "mpg", "mpeg",
    "vob", "3gp", "rmvb",
];

/// Absolute path with symlinks resolved, so one file has one path in the
/// task table. On Windows (development only) the `\\?\` prefix is dropped,
/// which FFmpeg would not understand.
pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let p = fs::canonicalize(path)?;
    #[cfg(windows)]
    if let Some(s) = p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        if !s.starts_with("UNC\\") {
            return Ok(PathBuf::from(s));
        }
    }
    Ok(p)
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.iter().any(|v| e.eq_ignore_ascii_case(v)))
}

/// Names never entered or returned: hidden entries (our candidates and the
/// recycle directory start with a dot) and NAS metadata directories.
pub fn is_ignored_name(name: &str) -> bool {
    name.starts_with('.') || name.starts_with('@') || name == "#recycle" || name == "$RECYCLE.BIN"
}

/// Whether any component of `path` below `root` is ignored.
pub fn is_ignored_path(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components()
            .any(|c| c.as_os_str().to_str().is_some_and(is_ignored_name))
    })
}

/// Exclude rules of a monitored directory: globs matched against the path
/// relative to it, with `/` separators (`**/Extras/**`, `*.sample.mkv`).
#[derive(Debug, Clone, Default)]
pub struct Excludes(Option<GlobSet>);

impl Excludes {
    pub fn new(patterns: &[String]) -> Result<Self, globset::Error> {
        if patterns.is_empty() {
            return Ok(Self(None));
        }
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            b.add(Glob::new(p)?);
        }
        Ok(Self(Some(b.build()?)))
    }

    pub fn matches(&self, root: &Path, path: &Path) -> bool {
        let Some(set) = &self.0 else { return false };
        let Ok(rel) = path.strip_prefix(root) else {
            return false;
        };
        let rel: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        set.is_match(rel.join("/"))
    }
}

/// Video files under `dir`, sorted. Unreadable directories are logged and
/// skipped.
pub fn walk(dir: &Path, recursive: bool, excludes: &Excludes) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let entries = match fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(dir = %d.display(), "cannot read directory: {e}");
                continue;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_str().is_none_or(is_ignored_name) {
                continue;
            }
            let path = entry.path();
            if excludes.matches(dir, &path) {
                continue;
            }
            // Symlinks are not followed: they could loop or leave the tree.
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if recursive {
                    stack.push(path);
                }
            } else if kind.is_file() && is_video(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// A file not queued, and why.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct SkippedFile {
    #[schema(value_type = String)]
    pub path: PathBuf,
    /// `active_task`, `recommend_pending`, `attempted`, `already_processed`,
    /// `probe_failed`, `unreadable`.
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScreenOptions {
    /// Queue files that carry the processed marker.
    pub force: bool,
    /// Queue files with an attempt record for this fingerprint.
    pub include_attempted: bool,
    /// Queue files a recommendation is studying (folder batches do; the
    /// monitor holds them back until a candidate is chosen).
    pub ignore_recommend: bool,
}

/// Decides whether `path` should be queued with a profile of `fingerprint`.
/// The marker needs a quick probe (cached by identity).
pub fn screen(
    db: &Db,
    prober: &Prober,
    path: &Path,
    fingerprint: &str,
    opts: ScreenOptions,
) -> db::Result<Result<FileIdentity, SkippedFile>> {
    let skip = |code, message: String| {
        Ok(Err(SkippedFile {
            path: path.to_owned(),
            code,
            message,
        }))
    };
    let identity = match FileIdentity::of(path) {
        Ok(id) => id,
        Err(e) => return skip("unreadable", format!("无法读取：{e}")),
    };
    if let Some(id) = db.active_task_for(path)? {
        return skip("active_task", format!("已有进行中或待确认的任务 #{id}"));
    }
    if !opts.ignore_recommend {
        if let Some(id) = db.recommend_hold_for(path)? {
            return skip(
                "recommend_pending",
                format!("样片推荐 #{id} 进行中或待选择，暂缓自动处理"),
            );
        }
    }
    if !opts.include_attempted {
        if let Some(a) = db.attempt_for(&identity, fingerprint)? {
            return skip(
                "attempted",
                format!("同一文件、同一设置已尝试过：{}", a.message),
            );
        }
    }
    if !opts.force {
        match prober.probe(path, ProbeDepth::Quick, None) {
            Ok(info) => {
                if let Some(m) = info.marker {
                    return skip(
                        "already_processed",
                        format!("已被 fncompress 处理过（方案 {}）", m.profile),
                    );
                }
            }
            Err(e) => return skip("probe_failed", format!("探测失败：{e}")),
        }
    }
    Ok(Ok(identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_extensions_and_ignored_names() {
        assert!(is_video(Path::new("/a/B.MKV")));
        assert!(is_video(Path::new("/a/b.m2ts")));
        assert!(!is_video(Path::new("/a/b.srt")));
        assert!(!is_video(Path::new("/a/mkv")));
        assert!(is_ignored_name(".fncompress-recycle"));
        assert!(is_ignored_name(".Movie.mkv.fncompress-1-2.mkv"));
        assert!(is_ignored_name("@eaDir"));
        assert!(!is_ignored_name("Movie.mkv"));
        assert!(is_ignored_path(
            Path::new("/m"),
            Path::new("/m/a/.fncompress-recycle/x/b.mkv")
        ));
        assert!(!is_ignored_path(Path::new("/m"), Path::new("/m/a/b.mkv")));
    }

    #[test]
    fn walk_honours_recursion_excludes_and_hidden_entries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for p in [
            "a.mkv",
            "notes.txt",
            ".a.mkv.fncompress-1-2.mkv",
            "Show/S01/e1.mp4",
            "Show/Extras/bonus.mkv",
            ".fncompress-recycle/1/a.mkv",
            "@eaDir/a.mkv",
        ] {
            let path = root.join(p);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }
        let rel = |v: Vec<PathBuf>| -> Vec<String> {
            v.iter()
                .map(|p| {
                    p.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect()
        };
        let none = Excludes::default();
        assert_eq!(rel(walk(root, false, &none)), ["a.mkv"]);
        assert_eq!(
            rel(walk(root, true, &none)),
            ["Show/Extras/bonus.mkv", "Show/S01/e1.mp4", "a.mkv"]
        );
        let ex = Excludes::new(&["**/Extras/**".into()]).unwrap();
        assert_eq!(rel(walk(root, true, &ex)), ["Show/S01/e1.mp4", "a.mkv"]);
        assert!(Excludes::new(&["[".into()]).is_err());
    }
}
