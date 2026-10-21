//! Storage-space gate. A transcode writes its candidate next to the source
//! (Dolby Vision and HDR10+ routes also an intermediate copy of the video),
//! and afterwards about one output's worth stays on the volume for the
//! retention period: the replaced source in the recycle, or the pending
//! candidate. So a file is counted at its full source size, the worst case.
//!
//! The scheduler starts a task only when its volume keeps the reserve after
//! the sources of the tasks already running there and this one; the monitor
//! queues a file only when the same holds for everything already queued.
//! Short volumes wait (FIFO per volume) and go on by themselves once space
//! is freed.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use fncompress_core::space::{self, Volume};
use serde::Serialize;
use utoipa::ToSchema;

pub const GIB: u64 = 1 << 30;

/// A volume without room for the next file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SpaceShortage {
    /// Mount point, e.g. `/vol1`.
    #[schema(value_type = String)]
    pub volume: PathBuf,
    pub free_bytes: u64,
    /// What the next file needs free: its size, the files already counted
    /// on the volume (running, or for the monitor also queued) and the
    /// reserve.
    pub needed_bytes: u64,
    /// Tasks (scheduler) or files (monitor) waiting for space here.
    pub waiting: u32,
}

/// Queued transcodes the scheduler holds back for space.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct SpaceStatus {
    pub shortages: Vec<SpaceShortage>,
    /// Their ids, oldest first.
    pub waiting_tasks: Vec<i64>,
}

/// Free space a folder batch would need on one volume, worst case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct VolumeEstimate {
    #[schema(value_type = String)]
    pub volume: PathBuf,
    pub free_bytes: u64,
    /// Sizes of the batch's files, the transcodes already queued or running
    /// on the volume, and the reserve.
    pub needed_bytes: u64,
    /// Files of the batch on this volume.
    pub files: u32,
}

impl VolumeEstimate {
    pub fn short(&self) -> bool {
        self.free_bytes < self.needed_bytes
    }
}

/// Size of the file at `path`, if it is there.
pub fn size_of(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// The real lookup: `statvfs` on the file. Errors count as unknown.
pub fn lookup(path: &Path) -> Option<Volume> {
    space::volume_of(path).ok().flatten()
}

/// The gate with the real lookup.
pub type SpaceGate = Gate<fn(&Path) -> Option<Volume>>;

/// Counts files against the free space of their volumes. Each directory's
/// volume is looked up once; a file whose volume is unknown always passes.
pub struct Gate<F> {
    lookup: F,
    reserve: u64,
    dirs: HashMap<PathBuf, u64>,
    volumes: HashMap<u64, Volume>,
    committed: HashMap<u64, u64>,
    short: BTreeMap<u64, SpaceShortage>,
}

impl<F: FnMut(&Path) -> Option<Volume>> Gate<F> {
    pub fn new(lookup: F, reserve: u64) -> Self {
        Self {
            lookup,
            reserve,
            dirs: HashMap::new(),
            volumes: HashMap::new(),
            committed: HashMap::new(),
            short: BTreeMap::new(),
        }
    }

    fn volume(&mut self, path: &Path) -> Option<Volume> {
        let dir = path.parent().unwrap_or(path);
        if let Some(dev) = self.dirs.get(dir) {
            return self.volumes.get(dev).cloned();
        }
        let v = (self.lookup)(path)?;
        self.dirs.insert(dir.to_owned(), v.dev);
        Some(self.volumes.entry(v.dev).or_insert(v).clone())
    }

    /// Counts a file already in progress (or, for the monitor, queued).
    pub fn commit(&mut self, path: &Path, size: u64) {
        if let Some(v) = self.volume(path) {
            *self.committed.entry(v.dev).or_default() += size;
        }
    }

    /// Whether `size` more bytes fit on the volume of `path`; when they do,
    /// they are counted. Once a volume is short everything after it there
    /// waits too, so a big file is not overtaken forever by small ones.
    pub fn take(&mut self, path: &Path, size: u64) -> bool {
        self.fits(path, size, true)
    }

    /// Like [`Gate::take`], but a file that fits is not counted.
    pub fn check(&mut self, path: &Path, size: u64) -> bool {
        self.fits(path, size, false)
    }

    fn fits(&mut self, path: &Path, size: u64, count: bool) -> bool {
        let Some(v) = self.volume(path) else {
            return true;
        };
        if let Some(s) = self.short.get_mut(&v.dev) {
            s.waiting += 1;
            return false;
        }
        let committed = self.committed.entry(v.dev).or_default();
        let needed = committed.saturating_add(size).saturating_add(self.reserve);
        if v.free >= needed {
            if count {
                *committed += size;
            }
            return true;
        }
        self.short.insert(
            v.dev,
            SpaceShortage {
                volume: v.mount,
                free_bytes: v.free,
                needed_bytes: needed,
                waiting: 1,
            },
        );
        false
    }

    /// Volumes found short so far, by mount point.
    pub fn shortages(&self) -> Vec<SpaceShortage> {
        let mut s: Vec<_> = self.short.values().cloned().collect();
        s.sort_by(|a, b| a.volume.cmp(&b.volume));
        s
    }

    /// Worst-case estimate for `files` on top of what is committed, per
    /// volume; counts nothing.
    pub fn estimate(&mut self, files: &[(PathBuf, u64)]) -> Vec<VolumeEstimate> {
        let mut by_dev: BTreeMap<u64, VolumeEstimate> = BTreeMap::new();
        for (path, size) in files {
            let Some(v) = self.volume(path) else { continue };
            let committed = self.committed.get(&v.dev).copied().unwrap_or(0);
            let reserve = self.reserve;
            let e = by_dev.entry(v.dev).or_insert_with(|| VolumeEstimate {
                volume: v.mount.clone(),
                free_bytes: v.free,
                needed_bytes: committed.saturating_add(reserve),
                files: 0,
            });
            e.needed_bytes = e.needed_bytes.saturating_add(*size);
            e.files += 1;
        }
        let mut out: Vec<_> = by_dev.into_values().collect();
        out.sort_by(|a, b| a.volume.cmp(&b.volume));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two volumes: /vol1 with `free1` bytes free, /vol2 with `free2`;
    /// paths elsewhere are unknown.
    fn volumes(free1: u64, free2: u64) -> impl FnMut(&Path) -> Option<Volume> {
        move |p: &Path| {
            let (dev, mount, free) = if p.starts_with("/vol1") {
                (1, "/vol1", free1)
            } else if p.starts_with("/vol2") {
                (2, "/vol2", free2)
            } else {
                return None;
            };
            Some(Volume {
                dev,
                mount: mount.into(),
                free,
            })
        }
    }

    #[test]
    fn running_files_and_the_reserve_count_against_free_space() {
        let mut g = Gate::new(volumes(100, 100), 10);
        g.commit(Path::new("/vol1/running.mkv"), 50);
        assert!(g.take(Path::new("/vol1/a.mkv"), 30), "50 + 30 + 10 ≤ 100");
        assert!(!g.take(Path::new("/vol1/b.mkv"), 20), "80 + 20 + 10 > 100");
        assert_eq!(
            g.shortages(),
            [SpaceShortage {
                volume: "/vol1".into(),
                free_bytes: 100,
                needed_bytes: 110,
                waiting: 1,
            }]
        );
    }

    #[test]
    fn a_short_volume_holds_later_files_but_not_other_volumes() {
        let mut g = Gate::new(volumes(100, 100), 0);
        assert!(!g.take(Path::new("/vol1/big.mkv"), 200));
        assert!(!g.take(Path::new("/vol1/small.mkv"), 1), "FIFO per volume");
        assert!(g.take(Path::new("/vol2/a.mkv"), 60));
        assert!(
            g.take(Path::new("/elsewhere/a.mkv"), u64::MAX),
            "unknown volume"
        );
        let s = g.shortages();
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].waiting, s[0].needed_bytes), (2, 200));
    }

    #[test]
    fn estimate_adds_the_batch_to_what_is_committed() {
        let mut g = Gate::new(volumes(100, 1000), 10);
        g.commit(Path::new("/vol1/queued.mkv"), 40);
        let files = [
            ("/vol1/a.mkv".into(), 30),
            ("/vol1/b.mkv".into(), 30),
            ("/vol2/c.mkv".into(), 500),
        ];
        let e = g.estimate(&files);
        assert_eq!(e.len(), 2);
        assert_eq!(
            (e[0].needed_bytes, e[0].files, e[0].short()),
            (110, 2, true)
        );
        assert_eq!(
            (e[1].needed_bytes, e[1].files, e[1].short()),
            (510, 1, false)
        );
        // Nothing was counted.
        assert!(g.take(Path::new("/vol1/d.mkv"), 50));
    }

    #[test]
    fn check_counts_nothing_but_marks_a_short_volume() {
        let mut g = Gate::new(volumes(100, 100), 0);
        assert!(g.check(Path::new("/vol1/a.mkv"), 80));
        assert!(g.take(Path::new("/vol1/b.mkv"), 80), "a was not counted");
        assert!(!g.check(Path::new("/vol1/c.mkv"), 30));
        assert!(!g.take(Path::new("/vol1/d.mkv"), 1), "c holds the volume");
        assert_eq!(g.shortages()[0].waiting, 2);
    }

    #[test]
    fn a_directory_is_looked_up_once() {
        let mut calls = 0;
        let mut inner = volumes(100, 100);
        let mut g = Gate::new(
            |p: &Path| {
                calls += 1;
                inner(p)
            },
            0,
        );
        g.commit(Path::new("/vol1/x/a.mkv"), 1);
        g.commit(Path::new("/vol1/x/b.mkv"), 1);
        g.commit(Path::new("/vol1/y/c.mkv"), 1);
        drop(g);
        assert_eq!(calls, 2);
    }
}
