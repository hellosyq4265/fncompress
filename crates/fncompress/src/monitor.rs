//! Automatic monitoring: inotify (via notify) finds new files, a low-frequency
//! incremental full scan catches what it missed (only path, size and mtime
//! are compared). A file is queued once its size and mtime have been stable
//! for `settle_secs`, by the deepest monitored directory that covers it, and
//! only if screening passes: no processed marker, no attempt record for the
//! profile's fingerprint, no active task (ADR 0005), no recommendation
//! studying it (ADR 0010). A file whose volume has no room for it on top of
//! the transcodes already queued and running there is not queued: the
//! monitor holds it (and every later file on that volume), says why in its
//! status, and tries again every minute until space is freed.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fncompress_core::identity::FileIdentity;
use notify::{event::ModifyKind, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, watch, Notify};
use utoipa::ToSchema;

use crate::{
    app::App,
    db::{now, SeenFile},
    events::Event,
    model::{MonitorDir, NewTask, TaskKind, TaskSource},
    scan::{self, Excludes, ScreenOptions},
    space::{SpaceGate, SpaceShortage},
};

const TICK: Duration = Duration::from_secs(2);
/// How often files held for space are tried again.
const SPACE_RETRY: Duration = Duration::from_secs(60);
/// Screening decision for a file a recommendation holds back.
const HELD: &str = "recommend_pending";

/// The monitored directory that decides about `path`: the deepest one that
/// covers it (recursive ones cover every descendant, others only direct
/// children). A disabled deepest directory switches monitoring off below it.
pub fn owner<'a>(monitors: &'a [MonitorDir], path: &Path) -> Option<&'a MonitorDir> {
    monitors
        .iter()
        .filter(|m| {
            path.strip_prefix(&m.path).is_ok_and(|rel| {
                let depth = rel.components().count();
                depth >= 1 && (m.recursive || depth == 1)
            })
        })
        .max_by_key(|m| m.path.components().count())
}

/// Files waiting for their size and mtime to stop changing.
#[derive(Debug, Default)]
pub struct Settler {
    files: HashMap<PathBuf, (u64, i64, Instant)>,
}

impl Settler {
    /// Notes the current size and mtime; a change restarts the wait.
    pub fn observe(&mut self, path: PathBuf, size: u64, mtime_ns: i64, at: Instant) {
        match self.files.get_mut(&path) {
            Some(entry) if (entry.0, entry.1) == (size, mtime_ns) => {}
            Some(entry) => *entry = (size, mtime_ns, at),
            None => {
                self.files.insert(path, (size, mtime_ns, at));
            }
        }
    }

    /// Treats a file as already stable (an old file found by a scan).
    pub fn ready_now(
        &mut self,
        path: PathBuf,
        size: u64,
        mtime_ns: i64,
        settle: Duration,
        at: Instant,
    ) {
        let since = at.checked_sub(settle).unwrap_or(at);
        self.files.insert(path, (size, mtime_ns, since));
    }

    pub fn forget(&mut self, path: &Path) {
        self.files.remove(path);
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Re-stats every waiting file (via `stat`) and returns those unchanged
    /// for `settle`. Vanished files are dropped.
    pub fn take_settled(
        &mut self,
        settle: Duration,
        at: Instant,
        mut stat: impl FnMut(&Path) -> Option<(u64, i64)>,
    ) -> Vec<PathBuf> {
        let mut ready = Vec::new();
        self.files.retain(|path, entry| match stat(path) {
            None => false,
            Some((size, mtime)) if (size, mtime) != (entry.0, entry.1) => {
                *entry = (size, mtime, at);
                true
            }
            Some(_) if at.duration_since(entry.2) >= settle => {
                ready.push(path.clone());
                false
            }
            Some(_) => true,
        });
        ready.sort();
        ready
    }
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct WatchState {
    pub id: i64,
    #[schema(value_type = String)]
    pub path: PathBuf,
    /// inotify watch in place; without it only the full scan finds files.
    pub watching: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct MonitorStatus {
    /// Master switch.
    pub enabled: bool,
    pub dirs: Vec<WatchState>,
    pub scanning: bool,
    /// Unix seconds of the last finished full scan.
    pub last_scan_at: Option<i64>,
    /// Files waiting to settle.
    pub settling: usize,
    /// Files queued by the monitor since the server started.
    pub queued: u64,
    /// Volumes where monitoring is paused because the files found there
    /// would not fit; it resumes by itself once space is freed.
    pub space_paused: Vec<SpaceShortage>,
}

/// Handle for the API: status and on-demand scans.
#[derive(Clone, Default)]
pub struct MonitorHandle {
    status: Arc<Mutex<MonitorStatus>>,
    scan: Arc<Notify>,
}

impl MonitorHandle {
    pub fn status(&self) -> MonitorStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Asks for an incremental full scan now.
    pub fn scan(&self) {
        self.scan.notify_one();
    }

    fn update(&self, f: impl FnOnce(&mut MonitorStatus)) {
        f(&mut self.status.lock().unwrap_or_else(|e| e.into_inner()));
    }
}

enum FsEvent {
    Changed(PathBuf),
    Removed(PathBuf),
}

/// Runs until `stop` turns true.
pub async fn run(app: Arc<App>, handle: MonitorHandle, mut stop: watch::Receiver<bool>) {
    let mut config = app.config_version.subscribe();
    let mut events = app.events.subscribe();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut settler = Settler::default();
    let mut watcher: Option<RecommendedWatcher> = None;
    let mut monitors: Vec<MonitorDir> = Vec::new();
    let mut tick = tokio::time::interval(TICK);
    let mut next_scan = Instant::now();
    let mut scan_task: Option<tokio::task::JoinHandle<Vec<(PathBuf, u64, i64)>>> = None;
    let mut eval_task: Option<tokio::task::JoinHandle<Evaluated>> = None;
    // Files held for space, oldest first, and when to try them again.
    let mut held: Vec<PathBuf> = Vec::new();
    let mut retry_at = Instant::now();
    let mut reload = true;

    loop {
        if reload {
            reload = false;
            let (enabled, dirs) = load(&app);
            monitors = if enabled { dirs } else { Vec::new() };
            watcher = None;
            let mut states = Vec::new();
            if enabled {
                let w = new_watcher(tx.clone());
                for m in monitors.iter().filter(|m| m.enabled) {
                    let mut s = WatchState {
                        id: m.id,
                        path: m.path.clone(),
                        ..Default::default()
                    };
                    match &w {
                        Ok(_) => {}
                        Err(e) => s.error = Some(e.clone()),
                    }
                    states.push(s);
                }
                if let Ok(mut w) = w {
                    for (s, m) in states.iter_mut().zip(monitors.iter().filter(|m| m.enabled)) {
                        let mode = if m.recursive {
                            RecursiveMode::Recursive
                        } else {
                            RecursiveMode::NonRecursive
                        };
                        match w.watch(&m.path, mode) {
                            Ok(()) => s.watching = true,
                            Err(e) => {
                                tracing::warn!(path = %m.path.display(), "watch failed: {e}");
                                s.error = Some(format!("无法监听（只靠全量扫描）：{e}"));
                            }
                        }
                    }
                    watcher = Some(w);
                }
                // New or changed directories get a scan right away.
                next_scan = Instant::now();
            }
            settler = Settler::default();
            // Their seen-file records were dropped, so the scan finds them.
            held.clear();
            handle.update(|st| {
                st.enabled = enabled;
                st.dirs = states;
                st.space_paused.clear();
            });
            publish_status(&app, &handle);
        }
        let _ = &watcher;

        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            changed = config.changed() => {
                if changed.is_ok() {
                    reload = true;
                }
            }
            Some(ev) = rx.recv() => match ev {
                FsEvent::Changed(path) => observe_changed(&monitors, &mut settler, path),
                FsEvent::Removed(path) => {
                    settler.forget(&path);
                    let db = app.db.clone();
                    let _ = tokio::task::spawn_blocking(move || db.forget_seen_file(&path)).await;
                }
            },
            () = handle.scan.notified() => next_scan = Instant::now(),
            ev = events.recv() => match ev {
                // A recommendation that ended releases its source at once.
                Ok(Event::TaskUpdated { task })
                    if task.kind == TaskKind::Recommend && task.state.is_final() =>
                {
                    let server = app.db.server_settings().unwrap_or_default();
                    let settle = Duration::from_secs(server.settle_secs.into());
                    recheck(&monitors, &mut settler, task.path.clone(), settle);
                }
                Err(broadcast::error::RecvError::Closed) => break,
                _ => {}
            },
            _ = tick.tick() => {
                let server = app.db.server_settings().unwrap_or_default();
                let settle = Duration::from_secs(server.settle_secs.into());
                // Finished full scan: queue what changed.
                if scan_task.as_ref().is_some_and(|t| t.is_finished()) {
                    let found = scan_task.take().expect("checked").await.unwrap_or_default();
                    let at = Instant::now();
                    let wall = now();
                    for (path, size, mtime_ns) in found {
                        let age_s = wall - mtime_ns.div_euclid(1_000_000_000);
                        if age_s >= i64::from(server.settle_secs) {
                            settler.ready_now(path, size, mtime_ns, settle, at);
                        } else {
                            settler.observe(path, size, mtime_ns, at);
                        }
                    }
                    handle.update(|s| {
                        s.scanning = false;
                        s.last_scan_at = Some(now());
                    });
                    publish_status(&app, &handle);
                }
                if eval_task.as_ref().is_some_and(|t| t.is_finished()) {
                    let done = eval_task.take().expect("checked").await.unwrap_or_default();
                    held = done.held;
                    retry_at = Instant::now() + SPACE_RETRY;
                    let mut changed = done.queued > 0;
                    handle.update(|s| {
                        s.queued += done.queued;
                        changed |= s.space_paused != done.shortages;
                        s.space_paused = done.shortages;
                    });
                    if changed {
                        publish_status(&app, &handle);
                    }
                }
                if !monitors.is_empty() && scan_task.is_none() && Instant::now() >= next_scan {
                    next_scan = Instant::now() + Duration::from_secs(u64::from(server.scan_interval_mins) * 60);
                    handle.update(|s| s.scanning = true);
                    publish_status(&app, &handle);
                    let (app2, dirs) = (app.clone(), monitors.clone());
                    scan_task = Some(tokio::task::spawn_blocking(move || full_scan(&app2, &dirs)));
                }
                if eval_task.is_none() {
                    let ready = if settler.is_empty() {
                        Vec::new()
                    } else {
                        settler.take_settled(settle, Instant::now(), stat)
                    };
                    // Held files go first, in their order; each evaluation
                    // decides about all of them again.
                    if !ready.is_empty() || (!held.is_empty() && Instant::now() >= retry_at) {
                        let mut paths = std::mem::take(&mut held);
                        for p in ready {
                            if !paths.contains(&p) {
                                paths.push(p);
                            }
                        }
                        let (app2, dirs) = (app.clone(), monitors.clone());
                        eval_task = Some(tokio::task::spawn_blocking(move || evaluate(&app2, &dirs, &paths)));
                    }
                }
                let settling = settler.len();
                handle.update(|s| s.settling = settling);
            }
        }
    }
    tracing::info!("monitor stopped");
}

fn load(app: &App) -> (bool, Vec<MonitorDir>) {
    let enabled = app
        .db
        .server_settings()
        .map(|s| s.monitor_enabled)
        .unwrap_or(false);
    let dirs = app.db.monitors().unwrap_or_else(|e| {
        tracing::error!("loading monitored directories failed: {e}");
        Vec::new()
    });
    (enabled, dirs)
}

fn publish_status(app: &App, handle: &MonitorHandle) {
    app.events.send(crate::events::Event::MonitorStatus {
        status: handle.status(),
    });
}

fn new_watcher(tx: mpsc::UnboundedSender<FsEvent>) -> Result<RecommendedWatcher, String> {
    notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        let removed = matches!(ev.kind, EventKind::Remove(_))
            || matches!(
                ev.kind,
                EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::From))
            );
        let interesting = removed
            || matches!(
                ev.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
            );
        if !interesting {
            return;
        }
        for path in ev.paths {
            let _ = tx.send(if removed {
                FsEvent::Removed(path)
            } else {
                FsEvent::Changed(path)
            });
        }
    })
    .map_err(|e| format!("无法创建文件监听：{e}"))
}

fn stat(path: &Path) -> Option<(u64, i64)> {
    let id = FileIdentity::of(path).ok()?;
    Some((id.size, id.mtime_ns))
}

/// A changed path from the watcher: a video file starts settling; a new
/// directory (moved in whole) is walked.
fn observe_changed(monitors: &[MonitorDir], settler: &mut Settler, path: PathBuf) {
    let Some(m) = owner(monitors, &path) else {
        return;
    };
    if !m.enabled || scan::is_ignored_path(&m.path, &path) {
        return;
    }
    let at = Instant::now();
    if path.is_dir() {
        let excludes = Excludes::new(&m.excludes).unwrap_or_default();
        for file in scan::walk(&path, m.recursive, &excludes) {
            if let Some((size, mtime)) = stat(&file) {
                settler.observe(file, size, mtime, at);
            }
        }
    } else if scan::is_video(&path) {
        if let Some((size, mtime)) = stat(&path) {
            settler.observe(path, size, mtime, at);
        }
    }
}

/// Screens a file again on the next tick, if a monitored directory covers it.
fn recheck(monitors: &[MonitorDir], settler: &mut Settler, path: PathBuf, settle: Duration) {
    let Some(m) = owner(monitors, &path) else {
        return;
    };
    let excludes = Excludes::new(&m.excludes).unwrap_or_default();
    if !m.enabled
        || !scan::is_video(&path)
        || scan::is_ignored_path(&m.path, &path)
        || excludes.matches(&m.path, &path)
    {
        return;
    }
    if let Some((size, mtime)) = stat(&path) {
        settler.ready_now(path, size, mtime, settle, Instant::now());
    }
}

/// Incremental full scan: files whose size or mtime differ from what the
/// last scan recorded.
fn full_scan(app: &App, monitors: &[MonitorDir]) -> Vec<(PathBuf, u64, i64)> {
    let mut found = Vec::new();
    let started = Instant::now();
    let mut seen = 0usize;
    for m in monitors.iter().filter(|m| m.enabled) {
        let excludes = Excludes::new(&m.excludes).unwrap_or_default();
        for path in scan::walk(&m.path, m.recursive, &excludes) {
            // Belongs to a deeper monitored directory, which scans it itself.
            if owner(monitors, &path).map(|o| o.id) != Some(m.id) {
                continue;
            }
            seen += 1;
            let Some((size, mtime_ns)) = stat(&path) else {
                continue;
            };
            match app.db.seen_file(&path) {
                Ok(Some(s)) if s.size == size && s.mtime_ns == mtime_ns => {}
                Ok(_) => found.push((path, size, mtime_ns)),
                Err(e) => tracing::warn!("seen-file lookup failed: {e}"),
            }
        }
    }
    tracing::info!(
        files = seen,
        changed = found.len(),
        secs = started.elapsed().as_secs_f64(),
        "full scan done"
    );
    found
}

#[derive(Debug, Default)]
struct Evaluated {
    queued: u64,
    /// Passed screening but did not fit, in order.
    held: Vec<PathBuf>,
    shortages: Vec<SpaceShortage>,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Queued,
    Held,
    Other,
}

/// Screens settled files and queues those that pass and fit.
fn evaluate(app: &App, monitors: &[MonitorDir], paths: &[PathBuf]) -> Evaluated {
    let mut gate = app
        .space_gate()
        .inspect_err(|e| tracing::warn!("free-space check unavailable: {e}"))
        .ok();
    let mut out = Evaluated::default();
    for path in paths {
        match evaluate_one(app, monitors, path, gate.as_mut()) {
            Ok(Decision::Queued) => out.queued += 1,
            Ok(Decision::Held) => out.held.push(path.clone()),
            Ok(Decision::Other) => {}
            Err(e) => tracing::warn!(path = %path.display(), "monitor could not queue: {e}"),
        }
    }
    out.shortages = gate.map(|g| g.shortages()).unwrap_or_default();
    if !out.held.is_empty() {
        tracing::info!(files = out.held.len(), "monitor holds files for free space");
    }
    out
}

fn evaluate_one(
    app: &App,
    monitors: &[MonitorDir],
    path: &Path,
    gate: Option<&mut SpaceGate>,
) -> Result<Decision, crate::error::AppError> {
    let Some(m) = owner(monitors, path) else {
        return Ok(Decision::Other);
    };
    let excludes = Excludes::new(&m.excludes).unwrap_or_default();
    if !m.enabled
        || !scan::is_video(path)
        || scan::is_ignored_path(&m.path, path)
        || excludes.matches(&m.path, path)
    {
        return Ok(Decision::Other);
    }
    let Some((size, mtime_ns)) = stat(path) else {
        return Ok(Decision::Other);
    };
    let snapshot = app.snapshot(Some(&m.profile_id))?;
    let fingerprint = snapshot.profile.fingerprint();
    let prober = app.prober(&snapshot.global);
    let decision = match scan::screen(
        &app.db,
        &prober,
        path,
        &fingerprint,
        ScreenOptions::default(),
    )? {
        Ok(_) if gate.is_some_and(|g| !g.take(path, size)) => {
            // Not remembered: tried again while held, or by the next full
            // scan after a restart.
            app.db.forget_seen_file(path)?;
            return Ok(Decision::Held);
        }
        Ok(_) => {
            let task = app.db.insert_task(&NewTask {
                kind: TaskKind::Transcode,
                source: TaskSource::Monitor,
                batch_id: None,
                monitor_id: Some(m.id),
                path: path.to_owned(),
                force: false,
                snapshot,
                recommend: None,
            })?;
            tracing::info!(id = task.id, path = %path.display(), "monitor queued");
            app.events.task_created(&task);
            app.scheduler.wake();
            "queued"
        }
        Err(skip) => {
            tracing::debug!(path = %path.display(), code = skip.code, "monitor skipped");
            skip.code
        }
    };
    if decision == HELD {
        // Not remembered: the file is looked at again once the
        // recommendation ends (see `recheck`), or by the next full scan.
        app.db.forget_seen_file(path)?;
    } else {
        app.db.put_seen_file(
            path,
            &SeenFile {
                size,
                mtime_ns,
                decision: decision.into(),
            },
        )?;
    }
    Ok(if decision == "queued" {
        Decision::Queued
    } else {
        Decision::Other
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(id: i64, path: &str, recursive: bool, enabled: bool) -> MonitorDir {
        MonitorDir {
            id,
            path: path.into(),
            enabled,
            profile_id: "default".into(),
            recursive,
            excludes: vec![],
        }
    }

    #[test]
    fn deepest_covering_directory_wins() {
        let ms = [
            dir(1, "/vol1/media", true, true),
            dir(2, "/vol1/media/tv", true, true),
            dir(3, "/vol1/media/tv/old", false, false),
            dir(4, "/vol1/media/flat", false, true),
        ];
        let o = |p: &str| owner(&ms, Path::new(p)).map(|m| m.id);
        assert_eq!(o("/vol1/media/a.mkv"), Some(1));
        assert_eq!(o("/vol1/media/tv/s1/e1.mkv"), Some(2));
        // The deepest directory decides even when it is disabled.
        assert_eq!(o("/vol1/media/tv/old/x.mkv"), Some(3));
        // A non-recursive directory only covers its own files; deeper ones
        // fall back to the recursive parent.
        assert_eq!(o("/vol1/media/tv/old/deeper/x.mkv"), Some(2));
        assert_eq!(o("/vol1/media/flat/x.mkv"), Some(4));
        assert_eq!(o("/vol1/media/flat/sub/x.mkv"), Some(1));
        assert_eq!(o("/vol1/other/x.mkv"), None);
        assert_eq!(o("/vol1/media"), None);
        // Prefixes are compared by component, not by string.
        assert_eq!(o("/vol1/media2/x.mkv"), None);
    }

    #[test]
    fn files_settle_only_when_unchanged() {
        let settle = Duration::from_secs(60);
        let t0 = Instant::now();
        let mut s = Settler::default();
        let mut size = 10;
        s.observe("/a.mkv".into(), 10, 1, t0);
        s.ready_now("/old.mkv".into(), 5, 1, settle, t0);
        let stat_a = |size: u64| {
            move |p: &Path| {
                Some(if p == Path::new("/a.mkv") {
                    (size, 1)
                } else {
                    (5, 1)
                })
            }
        };

        assert_eq!(
            s.take_settled(settle, t0, stat_a(size)),
            [PathBuf::from("/old.mkv")]
        );
        // Still growing at 50 s: the wait restarts.
        size = 20;
        assert!(s
            .take_settled(settle, t0 + Duration::from_secs(50), stat_a(size))
            .is_empty());
        assert!(s
            .take_settled(settle, t0 + Duration::from_secs(100), stat_a(size))
            .is_empty());
        assert_eq!(
            s.take_settled(settle, t0 + Duration::from_secs(111), stat_a(size)),
            [PathBuf::from("/a.mkv")]
        );
        assert!(s.is_empty());

        s.observe("/gone.mkv".into(), 1, 1, t0);
        assert!(s.take_settled(settle, t0, |_| None).is_empty());
        assert!(s.is_empty());
    }
}
