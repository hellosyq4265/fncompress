//! Dispatches queued tasks within the GPU and CPU quotas (ADR 0003) and runs
//! each on its own thread: transcodes through `pipeline::transcode_file`,
//! recommendations through `recommend::run` (one at a time, ADR 0010). Slot
//! counts are read at every dispatch, so a change applies as soon as a slot
//! frees up and never interrupts a running task. A transcode starts only
//! when its volume has room for it (see `space`); otherwise it stays queued
//! and is looked at again at the next dispatch.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

use fncompress_core::{
    command::Progress,
    encoder::{Encoder, Quota},
    identity::FileIdentity,
    pipeline::{self, Job, JobProgress, Outcome, PipelineError},
    predict::PredictProgress,
    probe::{ProbeDepth, Prober},
    publish::Savings,
    recommend::{self, RecommendError, RecommendJob, RecommendOutcome, RecommendProgress},
    recycle,
    route::SkipCode,
    tool::CancelToken,
    verify::{Report, Severity, Verdict},
};
use tokio::sync::Notify;

use crate::{
    db::{now, Db, NewAttempt, SqliteProbeCache},
    events::{Event, Events},
    model::{AttemptKind, PendingReason, Task, TaskKind, TaskProgress, TaskState},
    space::{self, Gate, SpaceStatus},
};

/// Safety net for missed wake-ups.
const IDLE_POLL: Duration = Duration::from_secs(30);
const PROGRESS_EVERY: Duration = Duration::from_secs(1);
const DAY_S: i64 = 86_400;

/// A queued task as dispatch sees it.
#[derive(Debug, Clone, Copy)]
pub struct Waiting {
    pub id: i64,
    pub quota: Quota,
    pub recommend: bool,
}

/// Picks the queued tasks to start, oldest first, given the free slots per
/// quota. A full quota does not hold back tasks of the other one; only one
/// recommendation runs at a time. `room` is asked about every task in
/// order, with whether a slot is free for it (then it is about to start):
/// a task without room waits and leaves its slot to the next one.
pub fn pick(
    queued: &[Waiting],
    mut free_gpu: u32,
    mut free_cpu: u32,
    mut recommend_running: bool,
    mut room: impl FnMut(&Waiting, bool) -> bool,
) -> Vec<i64> {
    let mut picked = Vec::new();
    for t in queued {
        if t.recommend && recommend_running {
            continue;
        }
        let free = match t.quota {
            Quota::Gpu => &mut free_gpu,
            Quota::Cpu => &mut free_cpu,
        };
        if room(t, *free > 0) && *free > 0 {
            *free -= 1;
            recommend_running |= t.recommend;
            picked.push(t.id);
        }
    }
    picked
}

/// Whether a protective skip goes into the attempt records. Skips that
/// depend on the environment rather than the file are not recorded: the
/// marker already stops the monitor, an encoder or dovi_tool may become
/// available, and a blocking target may be moved away.
pub fn records_attempt(code: SkipCode) -> bool {
    !matches!(
        code,
        SkipCode::AlreadyProcessed
            | SkipCode::EncoderUnavailable
            | SkipCode::DoviToolUnavailable
            | SkipCode::TargetExists
    )
}

struct Running {
    quota: Quota,
    /// Transcode source and its size, counted against free space.
    source: Option<(PathBuf, u64)>,
    observe: bool,
    recommend: bool,
    cancel: CancelToken,
    progress: Option<TaskProgress>,
}

struct Inner {
    db: Arc<Db>,
    events: Events,
    /// Encoders the startup trial found working; `None` skips the check.
    available: Option<Vec<Encoder>>,
    /// Scratch space of recommendations, one directory per task.
    work_dir: PathBuf,
    running: Mutex<HashMap<i64, Running>>,
    /// Queued transcodes waiting for space, as of the last dispatch.
    space: Mutex<SpaceStatus>,
    wake: Notify,
    /// Signalled whenever a task finishes.
    finished: Notify,
    stopping: AtomicBool,
}

#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

impl Scheduler {
    pub fn new(
        db: Arc<Db>,
        events: Events,
        available: Option<Vec<Encoder>>,
        work_dir: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                events,
                available,
                work_dir,
                running: Mutex::new(HashMap::new()),
                space: Mutex::new(SpaceStatus::default()),
                wake: Notify::new(),
                finished: Notify::new(),
                stopping: AtomicBool::new(false),
            }),
        }
    }

    /// The dispatch loop; returns after [`Scheduler::shutdown`].
    pub async fn run(&self) {
        while !self.inner.stopping.load(Ordering::SeqCst) {
            if let Err(e) = self.dispatch() {
                tracing::error!("dispatch failed: {e}");
            }
            let _ = tokio::time::timeout(IDLE_POLL, self.inner.wake.notified()).await;
        }
    }

    /// Asks the loop to look at the queue again.
    pub fn wake(&self) {
        self.inner.wake.notify_one();
    }

    pub fn is_running(&self, id: i64) -> bool {
        self.inner.lock().contains_key(&id)
    }

    pub fn progress(&self, id: i64) -> Option<TaskProgress> {
        self.inner.lock().get(&id).and_then(|r| r.progress)
    }

    pub fn running_ids(&self) -> Vec<i64> {
        self.inner.lock().keys().copied().collect()
    }

    /// Sources of the running transcodes and their sizes.
    pub fn running_sources(&self) -> Vec<(PathBuf, u64)> {
        self.inner
            .lock()
            .values()
            .filter_map(|r| r.source.clone())
            .collect()
    }

    pub fn space_status(&self) -> SpaceStatus {
        self.inner
            .space
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Cancels a running task; its state changes when the worker stops.
    /// False if it is not running.
    pub fn cancel_running(&self, id: i64) -> bool {
        match self.inner.lock().get(&id) {
            Some(r) => {
                r.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Stops dispatching, cancels every running task (they go back to the
    /// queue) and waits up to `grace` for the workers to clean up.
    pub async fn shutdown(&self, grace: Duration) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        self.wake();
        for r in self.inner.lock().values() {
            r.cancel.cancel();
        }
        let deadline = Instant::now() + grace;
        loop {
            let finished = self.inner.finished.notified();
            if self.inner.lock().is_empty() {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || tokio::time::timeout(left, finished).await.is_err() {
                tracing::warn!("shutdown: tasks still running after {grace:?}");
                return;
            }
        }
    }

    fn dispatch(&self) -> crate::db::Result<()> {
        let inner = &self.inner;
        if inner.stopping.load(Ordering::SeqCst) {
            return Ok(());
        }
        let settings = inner.db.server_settings()?;
        let queued = inner.db.queued()?;
        let mut running = inner.lock();
        let used = |q: Quota| running.values().filter(|r| r.quota == q).count() as u32;
        let free_gpu = settings.gpu_slots.saturating_sub(used(Quota::Gpu));
        let free_cpu = settings.cpu_slots.saturating_sub(used(Quota::Cpu));
        let order: Vec<_> = queued
            .iter()
            .map(|t| Waiting {
                id: t.id,
                quota: t.quota,
                recommend: t.kind == TaskKind::Recommend,
            })
            .collect();
        let recommending = running.values().any(|r| r.recommend);
        let by_id: HashMap<i64, &Task> = queued.iter().map(|t| (t.id, t)).collect();
        let mut gate = Gate::new(space::lookup, settings.reserve_bytes());
        for (path, size) in running.values().filter_map(|r| r.source.as_ref()) {
            gate.commit(path, *size);
        }
        let mut sizes = HashMap::new();
        let mut waiting_tasks = Vec::new();
        let picked = pick(&order, free_gpu, free_cpu, recommending, |w, start| {
            let task = by_id[&w.id];
            if task.kind != TaskKind::Transcode {
                return true;
            }
            // A missing source fails at once, which is fine.
            let Some(size) = space::size_of(&task.path) else {
                return true;
            };
            // Without a slot it is only checked against what runs or is
            // starting, so a shortage shows whether or not slots are busy.
            let fits = if start {
                gate.take(&task.path, size)
            } else {
                gate.check(&task.path, size)
            };
            if fits {
                sizes.insert(w.id, size);
            } else {
                waiting_tasks.push(w.id);
            }
            fits
        });
        inner.set_space(SpaceStatus {
            shortages: gate.shortages(),
            waiting_tasks,
        });

        let mut observing = running.values().filter(|r| r.observe).count() as u32;
        let observed = inner.db.observed_count()?;
        for id in picked {
            let task = queued
                .iter()
                .find(|t| t.id == id)
                .expect("picked from queue");
            let observe =
                task.kind == TaskKind::Transcode && observed + observing < settings.observe_first_n;
            if !inner.db.mark_running(id, observe)? {
                continue;
            }
            observing += u32::from(observe);
            let cancel = CancelToken::default();
            running.insert(
                id,
                Running {
                    quota: task.quota,
                    source: sizes.get(&id).map(|s| (task.path.clone(), *s)),
                    observe,
                    recommend: task.kind == TaskKind::Recommend,
                    cancel: cancel.clone(),
                    progress: None,
                },
            );
            let task = inner.db.task(id)?.expect("task exists");
            inner.events.task_updated(&task);
            let worker = self.inner.clone();
            thread::Builder::new()
                .name(format!("task-{id}"))
                .spawn(move || worker.run_task(task, cancel))
                .expect("spawn worker thread");
        }
        Ok(())
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, HashMap<i64, Running>> {
        self.running.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_space(&self, status: SpaceStatus) {
        let mut current = self.space.lock().unwrap_or_else(|e| e.into_inner());
        if *current == status {
            return;
        }
        for s in &status.shortages {
            if !current.shortages.iter().any(|c| c.volume == s.volume) {
                tracing::warn!(
                    volume = %s.volume.display(),
                    free = s.free_bytes,
                    needed = s.needed_bytes,
                    "transcodes wait for free space"
                );
            }
        }
        *current = status.clone();
        drop(current);
        self.events.send(Event::SpaceStatus { status });
    }

    fn run_task(self: Arc<Self>, task: Task, cancel: CancelToken) {
        let id = task.id;
        tracing::info!(id, path = %task.path.display(), observe = task.observe, "task started");
        let settled = match task.kind {
            TaskKind::Transcode => {
                let result = self.transcode(&task, &cancel);
                self.settle(&task, &cancel, result)
            }
            TaskKind::Recommend => {
                let result = self.recommend(&task, &cancel);
                self.settle_recommend(&task, &cancel, result)
            }
        };
        if let Err(e) = settled {
            tracing::error!(id, "recording the task result failed: {e}");
        }
        self.lock().remove(&id);
        if let Ok(Some(t)) = self.db.task(id) {
            tracing::info!(id, state = ?t.state, "task finished");
            self.events.task_updated(&t);
        }
        self.finished.notify_waiters();
        self.wake.notify_one();
    }

    fn transcode(
        &self,
        task: &Task,
        cancel: &CancelToken,
    ) -> Result<(FileIdentity, Outcome), TaskError> {
        let identity = FileIdentity::of(&task.path)
            .map_err(|e| TaskError::Other(format!("无法读取源片：{e}")))?;
        let global = &task.snapshot.global;
        let prober =
            Prober::new(&global.ffprobe).with_cache(SqliteProbeCache::new(self.db.clone()));
        let job = Job {
            source: &task.path,
            snapshot: &task.snapshot,
            force: task.force,
            available: self.available.as_deref(),
            publish: !task.observe,
            cancel: Some(cancel),
        };
        let mut duration = None;
        // When the last report went out, and for which prediction sample
        // (`None`: the encode). A new step is reported at once.
        let mut last: Option<(Instant, Option<u32>)> = None;
        let outcome = pipeline::transcode_file(&prober, &job, |p: JobProgress| {
            let step = match p {
                JobProgress::Predicting(q) => Some(q.sample),
                JobProgress::Encoding(_) => None,
            };
            if last.is_some_and(|(t, s)| s == step && t.elapsed() < PROGRESS_EVERY) {
                return;
            }
            last = Some((Instant::now(), step));
            match p {
                JobProgress::Predicting(q) => self.report_prediction(task.id, q),
                JobProgress::Encoding(p) => {
                    // The full probe has just been cached; this is a lookup.
                    if duration.is_none() {
                        duration = prober
                            .cache
                            .as_ref()
                            .and_then(|c| c.get(&identity, ProbeDepth::Quick))
                            .and_then(|i| i.format.duration_s);
                    }
                    self.report_progress(task.id, p, duration);
                }
            }
        })
        .map_err(TaskError::Pipeline)?;
        Ok((identity, outcome))
    }

    fn report_progress(&self, id: i64, p: Progress, duration_s: Option<f64>) {
        let progress = TaskProgress {
            id,
            out_time_s: p.out_time_s,
            duration_s,
            percent: duration_s
                .filter(|d| *d > 0.0)
                .map(|d| (p.out_time_s / d * 100.0).clamp(0.0, 100.0)),
            speed: p.speed,
            recommend: None,
            predict: None,
        };
        self.send_progress(progress);
    }

    fn report_prediction(&self, id: i64, p: PredictProgress) {
        self.send_progress(TaskProgress {
            id,
            out_time_s: 0.0,
            duration_s: None,
            percent: Some(p.percent),
            speed: None,
            recommend: None,
            predict: Some(p),
        });
    }

    fn send_progress(&self, progress: TaskProgress) {
        if let Some(r) = self.lock().get_mut(&progress.id) {
            r.progress = Some(progress);
        }
        self.events.send(Event::TaskProgress(progress));
    }

    /// Writes the result: final state, attempt record, pending output.
    fn settle(
        &self,
        task: &Task,
        cancel: &CancelToken,
        result: Result<(FileIdentity, Outcome), TaskError>,
    ) -> crate::db::Result<()> {
        let db = &self.db;
        let id = task.id;
        let (identity, outcome) = match result {
            Ok(r) => r,
            Err(_) if cancel.is_cancelled() && self.stopping.load(Ordering::SeqCst) => {
                // Shutdown: back to the queue, restarted on the next start.
                db.transition(id, TaskState::Running, TaskState::Queued)?;
                return Ok(());
            }
            Err(_) if cancel.is_cancelled() => {
                return db.finish_task(id, TaskState::Cancelled, None, None);
            }
            Err(e) => {
                tracing::warn!(id, "task failed: {e}");
                return db.finish_task(id, TaskState::Failed, None, Some(&e.to_string()));
            }
        };
        let attempt = |kind, code: Option<String>, message: &str| {
            db.record_attempt(&NewAttempt {
                identity,
                fingerprint: &task.fingerprint,
                path: &task.path,
                kind,
                code: code.as_deref(),
                message,
                task_id: Some(id),
            })
        };
        let pending_until = now() + i64::from(task.snapshot.global.pending_retention_days) * DAY_S;
        match &outcome {
            Outcome::Skipped { reason } => {
                if records_attempt(reason.code) {
                    let code = serde_json::to_value(reason.code)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned));
                    attempt(AttemptKind::Skipped, code, &reason.message)?;
                }
                db.finish_task(id, TaskState::Skipped, Some(&outcome), None)
            }
            Outcome::Rejected { candidate } => {
                attempt(
                    AttemptKind::Rejected,
                    None,
                    &rejected_message(&candidate.report),
                )?;
                if task.observe {
                    db.bump_observed_count()?;
                }
                db.mark_pending(
                    id,
                    &outcome,
                    candidate,
                    PendingReason::Rejected,
                    pending_until,
                )
            }
            Outcome::InsufficientSavings {
                savings,
                min_savings_pct,
                report,
            } => {
                attempt(
                    AttemptKind::InsufficientSavings,
                    None,
                    &insufficient_message(savings, *min_savings_pct, report),
                )?;
                db.finish_task(id, TaskState::InsufficientSavings, Some(&outcome), None)
            }
            Outcome::PredictedNoGain {
                estimate,
                min_savings_pct,
            } => {
                attempt(
                    AttemptKind::PredictedNoGain,
                    None,
                    &format!(
                        "预判收益不足：估算节省 {:.1}%（需要 {min_savings_pct}%）",
                        estimate.saved_pct
                    ),
                )?;
                db.finish_task(id, TaskState::InsufficientSavings, Some(&outcome), None)
            }
            Outcome::Verified { candidate } => {
                db.bump_observed_count()?;
                db.mark_pending(
                    id,
                    &outcome,
                    candidate,
                    PendingReason::Observation,
                    pending_until,
                )
            }
            Outcome::Published { .. } => {
                db.add_recycle_dir(&recycle::dir_for(&task.path, &task.snapshot.global))?;
                db.finish_task(id, TaskState::Published, Some(&outcome), None)
            }
        }
    }
}

/// The attempt message of a rejected candidate: its hard findings.
pub fn rejected_message(report: &Report) -> String {
    let hard: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Hard)
        .map(|f| f.message.as_str())
        .collect();
    format!("校验未通过：{}", hard.join("；"))
}

/// The attempt message of 收益不足, with the hard findings when the
/// candidate failed the checks as well.
pub fn insufficient_message(savings: &Savings, min_savings_pct: f64, report: &Report) -> String {
    let mut message = format!(
        "收益不足：节省 {:.1}%（需要 {min_savings_pct}%）",
        savings.saved_pct
    );
    if report.verdict == Verdict::Rejected {
        message.push('；');
        message.push_str(&rejected_message(report));
    }
    message
}

#[derive(Debug, thiserror::Error)]
enum TaskError {
    #[error(transparent)]
    Pipeline(PipelineError),
    #[error(transparent)]
    Recommend(RecommendError),
    #[error("{0}")]
    Other(String),
}

impl Inner {
    /// 样片推荐: samples, trials, scores. Measurements are stored and sent
    /// as they arrive, so the page fills in sample by sample.
    fn recommend(&self, task: &Task, cancel: &CancelToken) -> Result<RecommendOutcome, TaskError> {
        let id = task.id;
        let request = task.recommend.clone().unwrap_or_default();
        let prober = Prober::new(&task.snapshot.global.ffprobe)
            .with_cache(SqliteProbeCache::new(self.db.clone()));
        let workdir = self.work_dir.join(id.to_string());
        let job = RecommendJob {
            source: &task.path,
            snapshot: &task.snapshot,
            request: &request,
            workdir: &workdir,
            available: self.available.as_deref(),
            cancel: Some(cancel),
        };
        let mut last: Option<(Instant, RecommendProgress)> = None;
        recommend::run(
            &prober,
            &job,
            |p| {
                // Every change of step goes out; within a step, once a second.
                let same_step = last.is_some_and(|(at, q)| {
                    (q.stage, q.trial, q.sample) == (p.stage, p.trial, p.sample)
                        && at.elapsed() < PROGRESS_EVERY
                });
                if same_step {
                    return;
                }
                last = Some((Instant::now(), p));
                let progress = TaskProgress {
                    id,
                    out_time_s: 0.0,
                    duration_s: None,
                    percent: Some(p.percent),
                    speed: None,
                    recommend: Some(p),
                    predict: None,
                };
                self.send_progress(progress);
            },
            |rec| match self.db.set_recommendation(id, Some(rec)) {
                Ok(()) => {
                    if let Ok(Some(t)) = self.db.task(id) {
                        self.events.task_updated(&t);
                    }
                }
                Err(e) => tracing::warn!(id, "storing recommendation progress failed: {e}"),
            },
        )
        .map_err(TaskError::Recommend)
    }

    /// Writes a recommendation's result: candidates wait for a choice
    /// (待选择); no candidate is a failure, with the measurements kept.
    fn settle_recommend(
        &self,
        task: &Task,
        cancel: &CancelToken,
        result: Result<RecommendOutcome, TaskError>,
    ) -> crate::db::Result<()> {
        let db = &self.db;
        let id = task.id;
        match result {
            Err(_) if cancel.is_cancelled() && self.stopping.load(Ordering::SeqCst) => {
                db.set_recommendation(id, None)?;
                db.transition(id, TaskState::Running, TaskState::Queued)?;
                Ok(())
            }
            Err(_) if cancel.is_cancelled() => db.finish_task(id, TaskState::Cancelled, None, None),
            Err(e) => {
                tracing::warn!(id, "recommendation failed: {e}");
                db.finish_task(id, TaskState::Failed, None, Some(&e.to_string()))
            }
            Ok(RecommendOutcome::Skipped { reason }) => db.finish_task(
                id,
                TaskState::Skipped,
                Some(&Outcome::Skipped { reason }),
                None,
            ),
            Ok(RecommendOutcome::Finished { recommendation }) => {
                if recommendation.candidates.is_empty() {
                    return db.finish_task(
                        id,
                        TaskState::Failed,
                        None,
                        Some(&no_candidate_message(&recommendation)),
                    );
                }
                let until = now() + i64::from(task.snapshot.global.pending_retention_days) * DAY_S;
                db.await_choice(id, &recommendation, until)
            }
        }
    }
}

/// Why a recommendation ended without candidates.
fn no_candidate_message(rec: &recommend::Recommendation) -> String {
    let best = rec
        .trials
        .iter()
        .filter_map(|t| t.min_score.map(|s| (s, t.quality)))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    match best {
        Some((score, q)) => format!(
            "没有质量值能让每个样片都达到 VMAF {}：最好的是质量 {q}，最低样片分 {score:.2}",
            rec.target_vmaf
        ),
        None => format!("没有质量值能让每个样片都达到 VMAF {}", rec.target_vmaf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(list: &[(i64, Quota, bool)]) -> Vec<Waiting> {
        list.iter()
            .map(|&(id, quota, recommend)| Waiting {
                id,
                quota,
                recommend,
            })
            .collect()
    }

    #[test]
    fn pick_fills_each_quota_in_queue_order() {
        use Quota::{Cpu, Gpu};
        let q = waiting(&[
            (1, Gpu, false),
            (2, Gpu, false),
            (3, Cpu, false),
            (4, Gpu, false),
            (5, Cpu, false),
        ]);
        let all = |_: &Waiting, _| true;
        assert_eq!(pick(&q, 1, 1, false, all), [1, 3]);
        assert_eq!(pick(&q, 2, 0, false, all), [1, 2]);
        // A full GPU quota does not block CPU tasks behind GPU ones.
        assert_eq!(pick(&q, 0, 2, false, all), [3, 5]);
        assert_eq!(pick(&q, 0, 0, false, all), Vec::<i64>::new());
        assert_eq!(pick(&[], 3, 3, false, all), Vec::<i64>::new());
    }

    #[test]
    fn a_task_without_room_leaves_its_slot_to_the_next() {
        use Quota::Gpu;
        let q = waiting(&[(1, Gpu, false), (2, Gpu, false), (3, Gpu, false)]);
        let mut asked = Vec::new();
        let picked = pick(&q, 1, 0, false, |w, start| {
            asked.push((w.id, start));
            w.id != 1
        });
        assert_eq!(picked, [2]);
        // Task 3 is still asked, without a slot, so its shortage shows.
        assert_eq!(asked, [(1, true), (2, true), (3, false)]);
    }

    #[test]
    fn one_recommendation_at_a_time() {
        use Quota::Cpu;
        let q = waiting(&[(1, Cpu, true), (2, Cpu, true), (3, Cpu, false)]);
        let all = |_: &Waiting, _| true;
        // A second recommendation waits; the transcode behind it does not.
        assert_eq!(pick(&q, 0, 3, false, all), [1, 3]);
        assert_eq!(pick(&q, 0, 3, true, all), [3]);
        assert_eq!(pick(&q, 0, 1, false, all), [1]);
    }

    #[test]
    fn environment_dependent_skips_are_not_recorded() {
        assert!(!records_attempt(SkipCode::AlreadyProcessed));
        assert!(!records_attempt(SkipCode::EncoderUnavailable));
        assert!(!records_attempt(SkipCode::TargetExists));
        assert!(!records_attempt(SkipCode::DoviToolUnavailable));
        assert!(records_attempt(SkipCode::Hdr));
        assert!(records_attempt(SkipCode::StreamNotSupportedInContainer));
    }
}
