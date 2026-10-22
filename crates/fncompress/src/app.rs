//! What the API can do, independent of HTTP: submit tasks (manual, folder
//! batch, 样片推荐), preview a transcode, cancel, retry, release and discard
//! pending outputs, choose a recommended candidate. Methods block (SQLite,
//! ffprobe, file moves); async callers run them on the blocking pool.

use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use fncompress_core::{
    caps::EncoderProbe,
    dovi::DoviTool,
    encoder::Encoder,
    identity::FileIdentity,
    marker::ProcessedMarker,
    pipeline::{self, Outcome},
    probe::{DynamicRange, ProbeDepth, Prober},
    recommend::{self, RecommendRequest},
    route::{self, RouteOptions, RoutePlan, SkipReason},
    settings::{GlobalSettings, SettingsSnapshot},
    space::Volume,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;

use crate::{
    db::{Db, SqliteProbeCache},
    error::{AppError, AppResult},
    events::{Event, Events, ScanPhase, ScanProgress},
    model::{
        Attempt, NewTask, RecommendChoice, SavedProfile, Task, TaskKind, TaskSource, TaskState,
    },
    scan::{self, Excludes, ScreenOptions, SkippedFile},
    scheduler::Scheduler,
    space::{self, Gate, SpaceGate, VolumeEstimate},
};

pub struct App {
    pub db: Arc<Db>,
    pub events: Events,
    pub scheduler: Scheduler,
    /// Result of the startup encoder trial.
    pub encoders: Vec<EncoderProbe>,
    /// Status and on-demand scans of the directory monitor.
    pub monitor: crate::monitor::MonitorHandle,
    /// Bumped on every configuration change; the monitor watches it.
    pub config_version: tokio::sync::watch::Sender<u64>,
    /// Tasks whose pending output is being released or discarded right now.
    closing: Mutex<HashSet<i64>>,
    /// Last successful check of the scoring FFmpeg.
    scorer: Mutex<Option<ScorerStatus>>,
    dovi: Mutex<Option<DoviStatus>>,
}

/// Whether 样片推荐 can score: the scoring FFmpeg runs and has libvmaf.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct ScorerStatus {
    #[schema(value_type = String)]
    pub ffmpeg: PathBuf,
    pub available: bool,
    pub version: Option<String>,
    /// Why it is not available.
    pub error: Option<String>,
    /// HDR and Dolby Vision samples are scored through libplacebo (display
    /// proxy, ADR 0011), which needs a Vulkan device.
    pub hdr_proxy: bool,
    pub hdr_proxy_error: Option<String>,
}

/// Whether Dolby Vision routes can run: dovi_tool (ADR 0011) runs.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct DoviStatus {
    #[schema(value_type = String)]
    pub path: PathBuf,
    pub available: bool,
    pub version: Option<String>,
    /// Why it is not available.
    pub error: Option<String>,
}

/// A 样片推荐 (`type: recommend`): the profile is the starting point; only
/// its quality value is searched.
#[derive(Debug, Clone)]
pub struct RecommendSubmit {
    pub path: PathBuf,
    pub profile_id: Option<String>,
    pub request: RecommendRequest,
}

/// Saves a recommended candidate as a profile.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ChooseRequest {
    /// Preset of the candidate; `None` for an encoder without presets.
    #[serde(default)]
    pub preset: Option<String>,
    /// Quality value of the candidate.
    pub quality: u8,
    /// New profile id; `rec-<task>-<preset>-q<quality>` by default
    /// (`rec-<task>-q<quality>` without a preset).
    pub profile_id: Option<String>,
    /// New profile name; "推荐：<file> 预设 <p> 质量 <q>" by default.
    pub name: Option<String>,
    /// Also queue a transcode of the source with the new profile.
    #[serde(default)]
    pub transcode: bool,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ChooseResult {
    pub task: Task,
    pub profile: SavedProfile,
    /// The transcode queued with the new profile.
    pub transcode: Option<Task>,
    /// Why the transcode was not queued (the profile is saved regardless).
    pub transcode_error: Option<String>,
}

/// A manual transcode (`source: manual`).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ManualRequest {
    #[schema(value_type = String)]
    pub path: PathBuf,
    /// Saved profile; the default profile when absent.
    pub profile_id: Option<String>,
    /// Process a file that already carries the processed marker.
    #[serde(default)]
    pub force: bool,
}

/// A folder batch (`source: folder_batch`).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BatchRequest {
    #[schema(value_type = String)]
    pub dir: PathBuf,
    pub profile_id: Option<String>,
    #[serde(default = "yes")]
    pub recursive: bool,
    #[serde(default)]
    pub force: bool,
    /// Also queue files with an attempt record for this profile.
    #[serde(default)]
    pub include_attempted: bool,
    /// Screen only these files under `dir` (the preview's `files`) instead
    /// of walking it again.
    #[serde(default)]
    #[schema(value_type = Option<Vec<String>>)]
    pub files: Option<Vec<PathBuf>>,
    /// Sent back in `scan_progress` events while the request runs.
    #[serde(default)]
    pub scan_id: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchResult {
    pub batch_id: i64,
    pub tasks: Vec<Task>,
    pub skipped: Vec<SkippedFile>,
}

/// What a folder batch would queue, for the "new transcode" form.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchPreview {
    #[schema(value_type = String)]
    pub dir: PathBuf,
    pub profile_id: String,
    /// Files that would be queued, sorted.
    #[schema(value_type = Vec<String>)]
    pub files: Vec<PathBuf>,
    /// Files that would not, and why.
    pub skipped: Vec<SkippedFile>,
    /// Free space each volume of `files` would need, worst case, with the
    /// transcodes already queued or running there. A short volume does not
    /// stop the batch: its tasks wait in the queue until space is freed.
    pub space: Vec<VolumeEstimate>,
}

/// What a transcode of this file would do, for the "new transcode" form.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Preview {
    #[schema(value_type = String)]
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub profile_id: String,
    pub fingerprint: String,
    pub marker: Option<ProcessedMarker>,
    /// The route, including expected losses and warnings.
    pub plan: Option<RoutePlan>,
    /// Protective skip, when the route refuses.
    pub skip: Option<SkipReason>,
    /// Earlier failed attempts on this file, any profile.
    pub attempts: Vec<Attempt>,
    /// A queued, running or pending task for this path.
    pub active_task: Option<i64>,
    /// Free space the file's volume would need, worst case, with the
    /// transcodes already queued or running there; absent when the volume
    /// cannot be told. When short, the task would wait in the queue.
    pub space: Option<VolumeEstimate>,
}

impl App {
    pub fn new(
        db: Arc<Db>,
        events: Events,
        scheduler: Scheduler,
        encoders: Vec<EncoderProbe>,
    ) -> Self {
        Self {
            db,
            events,
            scheduler,
            encoders,
            monitor: Default::default(),
            config_version: tokio::sync::watch::Sender::new(0),
            closing: Mutex::new(HashSet::new()),
            scorer: Mutex::new(None),
            dovi: Mutex::new(None),
        }
    }

    /// Checks the scoring FFmpeg of the current settings. A success is
    /// remembered until the path changes; a failure is checked again next
    /// time (about a second).
    pub fn scorer_status(&self) -> AppResult<ScorerStatus> {
        let g = self.db.global_settings()?;
        let ffmpeg = g.scoring_ffmpeg.unwrap_or(g.ffmpeg);
        let mut cached = self.scorer.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = cached.as_ref().filter(|s| s.ffmpeg == ffmpeg) {
            return Ok(s.clone());
        }
        let status = match recommend::probe_scorer(&ffmpeg) {
            Ok(info) => {
                let proxy = recommend::probe_display_proxy(&ffmpeg);
                ScorerStatus {
                    available: true,
                    version: Some(info.version),
                    error: None,
                    hdr_proxy: proxy.is_ok(),
                    hdr_proxy_error: proxy.err(),
                    ffmpeg,
                }
            }
            Err(e) => ScorerStatus {
                available: false,
                version: None,
                error: Some(e),
                hdr_proxy: false,
                hdr_proxy_error: None,
                ffmpeg,
            },
        };
        *cached = status.available.then(|| status.clone());
        Ok(status)
    }

    /// Checks the dovi_tool of the current settings, cached like
    /// [`App::scorer_status`].
    pub fn dovi_status(&self) -> AppResult<DoviStatus> {
        let path = self.db.global_settings()?.dovi_tool_path();
        let mut cached = self.dovi.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = cached.as_ref().filter(|s| s.path == path) {
            return Ok(s.clone());
        }
        let (version, error) = match DoviTool::new(&path).check() {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let status = DoviStatus {
            path,
            available: version.is_some(),
            version,
            error,
        };
        *cached = status.available.then(|| status.clone());
        Ok(status)
    }

    pub fn monitor_handle(&self) -> &crate::monitor::MonitorHandle {
        &self.monitor
    }

    pub fn available_encoders(&self) -> Vec<Encoder> {
        self.encoders
            .iter()
            .filter(|e| e.available)
            .map(|e| e.encoder)
            .collect()
    }

    pub fn prober(&self, global: &GlobalSettings) -> Prober {
        Prober::new(&global.ffprobe).with_cache(SqliteProbeCache::new(self.db.clone()))
    }

    /// 设置快照: the saved profile (or the default one) plus global settings
    /// as they are now.
    pub fn snapshot(&self, profile_id: Option<&str>) -> AppResult<SettingsSnapshot> {
        let id = match profile_id {
            Some(id) => id.to_owned(),
            None => self.db.server_settings()?.default_profile,
        };
        let saved = self
            .db
            .profile(&id)?
            .ok_or_else(|| AppError::not_found(&format!("配置方案 {id}")))?;
        Ok(SettingsSnapshot {
            profile: saved.profile,
            global: self.db.global_settings()?,
        })
    }

    /// A space gate with every queued and running transcode counted.
    pub fn space_gate(&self) -> AppResult<SpaceGate> {
        let reserve = self.db.server_settings()?.reserve_bytes();
        let mut gate = Gate::new(space::lookup as fn(&Path) -> Option<Volume>, reserve);
        for (path, size) in self.scheduler.running_sources() {
            gate.commit(&path, size);
        }
        for t in self.db.queued()? {
            if t.kind != TaskKind::Transcode {
                continue;
            }
            if let Some(size) = space::size_of(&t.path) {
                gate.commit(&t.path, size);
            }
        }
        Ok(gate)
    }

    pub fn task(&self, id: i64) -> AppResult<Task> {
        self.db
            .task(id)?
            .ok_or_else(|| AppError::not_found(&format!("任务 #{id}")))
    }

    fn enqueue(&self, t: &NewTask) -> AppResult<Task> {
        let task = self.db.insert_task(t)?;
        self.events.task_created(&task);
        self.scheduler.wake();
        Ok(task)
    }

    pub fn submit_manual(&self, req: &ManualRequest) -> AppResult<Task> {
        let path = existing_file(&req.path)?;
        let snapshot = self.snapshot(req.profile_id.as_deref())?;
        if let Some(id) = self.db.active_task_for(&path)? {
            return Err(AppError::conflict(
                "task_exists",
                format!("这个文件已有进行中或待确认的任务 #{id}"),
            )
            .with_details(json!({ "task_id": id })));
        }
        if !req.force {
            let info = self
                .prober(&snapshot.global)
                .probe(&path, ProbeDepth::Quick, None)
                .map_err(|e| AppError::bad_request("probe_failed", format!("探测失败：{e}")))?;
            if let Some(m) = info.marker {
                return Err(AppError::conflict(
                    "already_processed",
                    format!(
                        "这个文件已被 fncompress 处理过（方案 {}），确认要再处理请加 force",
                        m.profile
                    ),
                )
                .with_details(json!({ "marker": m })));
            }
        }
        self.enqueue(&NewTask {
            kind: TaskKind::Transcode,
            source: TaskSource::Manual,
            batch_id: None,
            monitor_id: None,
            path,
            force: req.force,
            snapshot,
            recommend: None,
        })
    }

    /// Queues a 样片推荐 of one file. Refused when the scorer is not
    /// usable or another recommendation of the file is still open.
    pub fn submit_recommend(&self, req: &RecommendSubmit) -> AppResult<Task> {
        req.request
            .validate()
            .map_err(|m| AppError::bad_request("bad_request", m))?;
        let path = existing_file(&req.path)?;
        if !scan::is_video(&path) {
            return Err(AppError::bad_request(
                "bad_path",
                format!("{} 不是视频文件", path.display()),
            ));
        }
        let snapshot = self.snapshot(req.profile_id.as_deref())?;
        req.request
            .presets_for(&snapshot.profile)
            .map_err(|m| AppError::bad_request("bad_request", m))?;
        if let Some(id) = self.db.recommend_hold_for(&path)? {
            return Err(AppError::conflict(
                "task_exists",
                format!("这个文件已有进行中或待选择的样片推荐 #{id}"),
            )
            .with_details(json!({ "task_id": id })));
        }
        let scorer = self.scorer_status()?;
        if !scorer.available {
            return Err(AppError::conflict(
                "scoring_unavailable",
                format!(
                    "评分工具不可用（{}）：{}",
                    scorer.ffmpeg.display(),
                    scorer.error.unwrap_or_default()
                ),
            ));
        }
        self.enqueue(&NewTask {
            kind: TaskKind::Recommend,
            source: TaskSource::Manual,
            batch_id: None,
            monitor_id: None,
            path,
            force: false,
            snapshot,
            recommend: Some(req.request.clone()),
        })
    }

    /// Saves a candidate of a recommendation awaiting a choice as a new
    /// profile and, if asked, queues a transcode of the source with it.
    pub fn choose(&self, id: i64, req: &ChooseRequest) -> AppResult<ChooseResult> {
        let _guard = self.closing(id)?;
        let task = self.task(id)?;
        if task.kind != TaskKind::Recommend || task.state != TaskState::AwaitingChoice {
            return Err(AppError::conflict(
                "not_awaiting_choice",
                format!("任务 #{id} 不是待选择的样片推荐"),
            ));
        }
        let rec = task
            .recommendation
            .as_ref()
            .expect("awaiting choice has a recommendation");
        if !rec
            .candidates
            .iter()
            .any(|c| c.quality == req.quality && c.preset == req.preset)
        {
            let what = match &req.preset {
                Some(p) => format!("预设 {p} 质量 {}", req.quality),
                None => format!("质量 {}", req.quality),
            };
            return Err(AppError::bad_request(
                "bad_request",
                format!("{what} 不是这次推荐的候选"),
            ));
        }
        let file = task
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let q = req.quality;
        let profile_id = req
            .profile_id
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| match &req.preset {
                Some(p) => format!("rec-{id}-{p}-q{q}"),
                None => format!("rec-{id}-q{q}"),
            });
        let name = req
            .name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| match &req.preset {
                Some(p) => format!("推荐：{file} 预设 {p} 质量 {q}"),
                None => format!("推荐：{file} 质量 {q}"),
            });
        let mut profile = task.snapshot.profile.clone();
        profile.id = profile_id.clone();
        profile.quality = q;
        // An encoder without presets keeps the profile's (unused) value.
        if req.preset.is_some() {
            profile.preset = req.preset.clone();
        }
        let saved = self.create_profile(&SavedProfile {
            id: profile_id.clone(),
            name: name.trim().to_owned(),
            profile,
        })?;
        let (transcode, transcode_error) = if req.transcode {
            let r = ManualRequest {
                path: task.path.clone(),
                profile_id: Some(profile_id.clone()),
                force: false,
            };
            match self.submit_manual(&r) {
                Ok(t) => (Some(t), None),
                Err(e) => (None, Some(e.message)),
            }
        } else {
            (None, None)
        };
        let choice = RecommendChoice {
            preset: req.preset.clone(),
            quality: req.quality,
            profile_id,
            transcode_task: transcode.as_ref().map(|t| t.id),
        };
        if !self.db.choose(id, &choice)? {
            return Err(AppError::conflict(
                "not_awaiting_choice",
                format!("任务 #{id} 已不在待选择状态"),
            ));
        }
        let task = self.task(id)?;
        self.events.task_updated(&task);
        Ok(ChooseResult {
            task,
            profile: saved,
            transcode,
            transcode_error,
        })
    }

    /// Screens a folder batch: which files would be queued, which skipped and why.
    fn screen_batch(&self, req: &BatchRequest) -> AppResult<(BatchPreview, SettingsSnapshot)> {
        let dir = scan::canonical(&req.dir).map_err(|e| {
            AppError::bad_request(
                "bad_path",
                format!("目录 {} 无法访问：{e}", req.dir.display()),
            )
        })?;
        if !dir.is_dir() {
            return Err(AppError::bad_request(
                "bad_path",
                format!("{} 不是目录", dir.display()),
            ));
        }
        let snapshot = self.snapshot(req.profile_id.as_deref())?;
        let fingerprint = snapshot.profile.fingerprint();
        let prober = self.prober(&snapshot.global);
        let opts = ScreenOptions {
            force: req.force,
            include_attempted: req.include_attempted,
            ignore_recommend: true,
        };
        let report = ScanReport::new(&self.events, req.scan_id.as_deref());
        let paths = match &req.files {
            Some(files) => listed_files(&dir, req.recursive, files)?,
            None => {
                report.send(ScanPhase::Walking, 0, 0);
                scan::walk(&dir, req.recursive, &Excludes::default())
            }
        };
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        for screened in screen_all(&self.db, &prober, &paths, &fingerprint, opts, &report)? {
            match screened {
                Ok(path) => files.push(path),
                Err(s) => skipped.push(s),
            }
        }
        let sized: Vec<_> = files
            .iter()
            .filter_map(|p| space::size_of(p).map(|s| (p.clone(), s)))
            .collect();
        let space = self.space_gate()?.estimate(&sized);
        let preview = BatchPreview {
            dir,
            profile_id: snapshot.profile.id.clone(),
            files,
            skipped,
            space,
        };
        Ok((preview, snapshot))
    }

    /// What a folder batch would queue, without queueing anything.
    pub fn preview_batch(&self, req: &BatchRequest) -> AppResult<BatchPreview> {
        Ok(self.screen_batch(req)?.0)
    }

    /// Queues a folder batch: the batch and its tasks in one transaction,
    /// announced with one `batch_created` event.
    pub fn submit_batch(&self, req: &BatchRequest) -> AppResult<BatchResult> {
        let (screened, snapshot) = self.screen_batch(req)?;
        let n = screened.files.len();
        ScanReport::new(&self.events, req.scan_id.as_deref()).send(ScanPhase::Queueing, n, n);
        let new: Vec<_> = screened
            .files
            .into_iter()
            .map(|path| NewTask {
                kind: TaskKind::Transcode,
                source: TaskSource::FolderBatch,
                batch_id: None,
                monitor_id: None,
                path,
                force: req.force,
                snapshot: snapshot.clone(),
                recommend: None,
            })
            .collect();
        let (batch_id, tasks) = self.db.insert_batch(&screened.dir, &new)?;
        if !tasks.is_empty() {
            self.events.send(Event::BatchCreated {
                batch_id,
                tasks: tasks.clone(),
            });
            self.scheduler.wake();
        }
        Ok(BatchResult {
            batch_id,
            tasks,
            skipped: screened.skipped,
        })
    }

    pub fn preview(
        &self,
        path: &Path,
        profile_id: Option<&str>,
        force: bool,
    ) -> AppResult<Preview> {
        let path = existing_file(path)?;
        let snapshot = self.snapshot(profile_id)?;
        let info = self
            .prober(&snapshot.global)
            .probe(&path, ProbeDepth::Quick, None)
            .map_err(|e| AppError::bad_request("probe_failed", format!("探测失败：{e}")))?;
        // Without the startup trial (--no-encoder-probe) every encoder
        // counts as available, as in the scheduler.
        let available = (!self.encoders.is_empty()).then(|| self.available_encoders());
        let dovi_unavailable = match info.dynamic_range() {
            DynamicRange::DolbyVision => self.dovi_status()?.error,
            _ => None,
        };
        let opts = RouteOptions {
            force,
            available: available.as_deref(),
            dovi_unavailable: dovi_unavailable.as_deref(),
        };
        let (plan, skip) = match route::plan(&info, &path, &snapshot.profile, &opts) {
            Ok(p) => (Some(p), None),
            Err(s) => (None, Some(s)),
        };
        Ok(Preview {
            identity: info.identity,
            profile_id: snapshot.profile.id.clone(),
            fingerprint: snapshot.profile.fingerprint(),
            marker: info.marker,
            plan,
            skip,
            attempts: self.db.attempts_for_identity(&info.identity)?,
            active_task: self.db.active_task_for(&path)?,
            space: self
                .space_gate()?
                .estimate(&[(path.clone(), info.identity.size)])
                .pop(),
            path,
        })
    }

    /// Cancels a queued or running task. A running task reaches
    /// `cancelled` once FFmpeg has been stopped.
    pub fn cancel(&self, id: i64) -> AppResult<Task> {
        let task = self.task(id)?;
        if self
            .db
            .transition(id, TaskState::Queued, TaskState::Cancelled)?
        {
            let task = self.task(id)?;
            self.events.task_updated(&task);
            return Ok(task);
        }
        if self.scheduler.cancel_running(id) {
            return Ok(task);
        }
        Err(AppError::conflict(
            "not_cancellable",
            format!("任务 #{id} 不在排队或运行中"),
        ))
    }

    /// A new task for the same file with the profile as it is now; attempt
    /// records do not stop a manual retry.
    pub fn retry(&self, id: i64) -> AppResult<Task> {
        let old = self.task(id)?;
        if !old.state.is_final() {
            return Err(AppError::conflict(
                "task_active",
                format!("任务 #{id} 还没有结束"),
            ));
        }
        let path = existing_file(&old.path)?;
        let active = match old.kind {
            TaskKind::Transcode => self.db.active_task_for(&path)?,
            TaskKind::Recommend => self.db.recommend_hold_for(&path)?,
        };
        if let Some(active) = active {
            return Err(AppError::conflict(
                "task_exists",
                format!("这个文件已有进行中或待确认的任务 #{active}"),
            ));
        }
        let snapshot = match self.snapshot(Some(&old.profile_id)) {
            Ok(s) => s,
            // The profile was deleted since: retry with the frozen one.
            Err(e) if e.code == "not_found" => SettingsSnapshot {
                global: self.db.global_settings()?,
                ..old.snapshot
            },
            Err(e) => return Err(e),
        };
        self.enqueue(&NewTask {
            kind: old.kind,
            source: TaskSource::Manual,
            batch_id: None,
            monitor_id: None,
            path,
            force: old.force,
            snapshot,
            recommend: old.recommend,
        })
    }

    /// 人工放行: publishes a pending output; the source goes to the recycle.
    pub fn release(&self, id: i64) -> AppResult<Task> {
        let _guard = self.closing(id)?;
        let task = self.pending(id)?;
        let candidate = task
            .candidate
            .clone()
            .expect("pending task has a candidate");
        let outcome = pipeline::publish_candidate(candidate, &task.snapshot)
            .map_err(|e| AppError::conflict("publish_failed", format!("放行失败：{e}")))?;
        match &outcome {
            Outcome::Published { .. } => {
                self.db.add_recycle_dir(&fncompress_core::recycle::dir_for(
                    &task.path,
                    &task.snapshot.global,
                ))?;
                self.db
                    .close_pending(id, TaskState::Released, Some(&outcome), None)?;
            }
            Outcome::InsufficientSavings {
                savings,
                min_savings_pct,
                ..
            } => {
                return Err(AppError::conflict(
                    "insufficient_savings",
                    format!(
                        "成品只节省 {:.1}%（需要 {min_savings_pct}%），不能放行",
                        savings.saved_pct
                    ),
                ))
            }
            other => return Err(AppError::internal(format!("放行得到意外结果：{other:?}"))),
        }
        let task = self.task(id)?;
        self.events.task_updated(&task);
        Ok(task)
    }

    /// Throws away a pending output, or ends a recommendation awaiting a
    /// choice without choosing (also used by the janitor with `Expired`).
    pub fn discard(&self, id: i64, state: TaskState) -> AppResult<Task> {
        let _guard = self.closing(id)?;
        if self.task(id)?.state == TaskState::AwaitingChoice {
            if !self.db.close_choice(id, state)? {
                return Err(AppError::conflict(
                    "not_awaiting_choice",
                    format!("任务 #{id} 已不在待选择状态"),
                ));
            }
            let task = self.task(id)?;
            self.events.task_updated(&task);
            return Ok(task);
        }
        let task = self.pending(id)?;
        let candidate = task
            .candidate
            .as_ref()
            .expect("pending task has a candidate");
        match std::fs::remove_file(&candidate.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(AppError::internal(format!(
                    "删除成品 {} 失败：{e}",
                    candidate.path.display()
                )))
            }
        }
        self.db.close_pending(id, state, None, None)?;
        let task = self.task(id)?;
        self.events.task_updated(&task);
        Ok(task)
    }

    pub fn delete(&self, id: i64) -> AppResult<()> {
        let task = self.task(id)?;
        if !self.db.delete_task(id)? {
            return Err(AppError::conflict(
                "task_active",
                format!("任务 #{id} 处于 {:?}，不能删除", task.state),
            ));
        }
        self.events.send(Event::TaskDeleted { id });
        Ok(())
    }

    fn pending(&self, id: i64) -> AppResult<Task> {
        let task = self.task(id)?;
        if task.state != TaskState::Pending || task.candidate.is_none() {
            return Err(AppError::conflict(
                "not_pending",
                format!("任务 #{id} 没有待确认成品"),
            ));
        }
        Ok(task)
    }

    fn closing(&self, id: i64) -> AppResult<ClosingGuard<'_>> {
        let mut set = self.closing.lock().unwrap_or_else(|e| e.into_inner());
        if !set.insert(id) {
            return Err(AppError::conflict(
                "busy",
                format!("任务 #{id} 正在放行或丢弃"),
            ));
        }
        Ok(ClosingGuard { app: self, id })
    }
}

struct ClosingGuard<'a> {
    app: &'a App,
    id: i64,
}

impl Drop for ClosingGuard<'_> {
    fn drop(&mut self) {
        self.app
            .closing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id);
    }
}

/// Files screened at once: each may need an ffprobe run, which mostly
/// waits on the disk.
const SCREEN_WORKERS: usize = 4;

/// `scan::screen` for each path, `SCREEN_WORKERS` at a time; results in
/// the order of `paths`.
fn screen_all(
    db: &Db,
    prober: &Prober,
    paths: &[PathBuf],
    fingerprint: &str,
    opts: ScreenOptions,
    report: &ScanReport,
) -> AppResult<Vec<Result<PathBuf, SkippedFile>>> {
    let total = paths.len();
    report.send(ScanPhase::Screening, 0, total);
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let mut results: Vec<_> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..SCREEN_WORKERS.min(total))
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    while !failed.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = paths.get(i) else { break };
                        let r = scan::screen(db, prober, path, fingerprint, opts);
                        failed.fetch_or(r.is_err(), Ordering::Relaxed);
                        out.push((i, r));
                        report.screened(done.fetch_add(1, Ordering::Relaxed) + 1, total);
                    }
                    out
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("screen worker panicked"))
            .collect()
    });
    results.sort_by_key(|(i, _)| *i);
    results
        .into_iter()
        .map(|(i, r)| Ok(r?.map(|_| paths[i].clone())))
        .collect()
}

/// The files of a submitted preview: each must be a video directly under
/// `dir` (or below it when `recursive`), outside ignored directories. A file
/// gone since the preview is left to screening, which skips it.
fn listed_files(dir: &Path, recursive: bool, files: &[PathBuf]) -> AppResult<Vec<PathBuf>> {
    let bad =
        |p: &Path, why: &str| AppError::bad_request("bad_path", format!("{} {why}", p.display()));
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let Ok(rel) = f.strip_prefix(dir) else {
            return Err(bad(f, "不在所选目录下"));
        };
        let depth = rel.components().count();
        if depth == 0 || rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(bad(f, "不在所选目录下"));
        }
        if !recursive && depth > 1 {
            return Err(bad(f, "在子目录里，而本次不含子目录"));
        }
        if !scan::is_video(f) || scan::is_ignored_path(dir, f) {
            return Err(bad(f, "不是可处理的视频文件"));
        }
        if std::fs::symlink_metadata(f).is_ok_and(|m| !m.is_file()) {
            return Err(bad(f, "不是普通文件"));
        }
        out.push(f.clone());
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// `scan_progress` events for one request that carried a `scan_id`;
/// per-file updates at most every `INTERVAL`.
struct ScanReport<'a> {
    events: &'a Events,
    scan_id: Option<&'a str>,
    last: Mutex<Option<Instant>>,
}

impl<'a> ScanReport<'a> {
    const INTERVAL: Duration = Duration::from_millis(200);

    fn new(events: &'a Events, scan_id: Option<&'a str>) -> Self {
        Self {
            events,
            scan_id,
            last: Mutex::new(None),
        }
    }

    fn send(&self, phase: ScanPhase, done: usize, total: usize) {
        let Some(scan_id) = self.scan_id else { return };
        self.events.send(Event::ScanProgress(ScanProgress {
            scan_id: scan_id.to_owned(),
            phase,
            done,
            total,
        }));
    }

    fn screened(&self, done: usize, total: usize) {
        if self.scan_id.is_none() {
            return;
        }
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if done < total && last.is_some_and(|t| now - t < Self::INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        self.send(ScanPhase::Screening, done, total);
    }
}

fn existing_file(path: &Path) -> AppResult<PathBuf> {
    let path = scan::canonical(path).map_err(|e| {
        AppError::bad_request("bad_path", format!("文件 {} 无法访问：{e}", path.display()))
    })?;
    if !path.is_file() {
        return Err(AppError::bad_request(
            "bad_path",
            format!("{} 不是文件", path.display()),
        ));
    }
    Ok(path)
}
