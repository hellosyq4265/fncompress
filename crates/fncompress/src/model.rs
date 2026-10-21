//! Records the server keeps: tasks (ADR 0003), attempt records (ADR 0005),
//! saved profiles, monitored directories and server settings.

use std::path::PathBuf;

use fncompress_core::{
    encoder::Quota,
    identity::FileIdentity,
    pipeline::{Candidate, Outcome},
    recommend::{RecommendProgress, RecommendRequest, Recommendation},
    settings::{Profile, SettingsSnapshot},
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Transcode,
    /// 样片推荐 (M4).
    Recommend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskSource {
    Manual,
    FolderBatch,
    Monitor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    /// The output replaced the source; the source is in the recycle.
    Published,
    /// Protective skip (保护性跳过).
    Skipped,
    /// 收益不足: the source stays, the candidate is gone.
    InsufficientSavings,
    /// 待确认成品: rejected by a hard check, or held by the observation mode.
    Pending,
    /// A pending output released by hand and published.
    Released,
    /// A pending output thrown away by hand.
    Discarded,
    /// A pending output past its retention, removed by the janitor.
    Expired,
    Failed,
    Cancelled,
    /// 待选择: a recommendation has candidates and waits for the user.
    AwaitingChoice,
    /// A recommended candidate was saved as a profile.
    Chosen,
}

impl TaskState {
    pub const ALL: [TaskState; 13] = [
        TaskState::Queued,
        TaskState::Running,
        TaskState::Published,
        TaskState::Skipped,
        TaskState::InsufficientSavings,
        TaskState::Pending,
        TaskState::Released,
        TaskState::Discarded,
        TaskState::Expired,
        TaskState::Failed,
        TaskState::Cancelled,
        TaskState::AwaitingChoice,
        TaskState::Chosen,
    ];

    /// Nothing more happens to the task on its own.
    pub fn is_final(self) -> bool {
        !matches!(
            self,
            TaskState::Queued | TaskState::Running | TaskState::Pending | TaskState::AwaitingChoice
        )
    }
}

/// Why a pending output is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PendingReason {
    /// A hard check failed.
    Rejected,
    /// Passed, held by the observation mode (ADR 0004).
    Observation,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Task {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: TaskKind,
    pub source: TaskSource,
    /// 文件夹批次 group.
    pub batch_id: Option<i64>,
    pub monitor_id: Option<i64>,
    #[schema(value_type = String)]
    pub path: PathBuf,
    pub state: TaskState,
    pub quota: Quota,
    /// Process a file that already carries the processed marker.
    pub force: bool,
    pub profile_id: String,
    /// 设置指纹 of the snapshot's profile.
    pub fingerprint: String,
    /// 设置快照, frozen at enqueue.
    pub snapshot: SettingsSnapshot,
    /// Held for review regardless of the verdict (observation mode).
    pub observe: bool,
    pub outcome: Option<Outcome>,
    /// The candidate awaiting release while [`TaskState::Pending`].
    pub candidate: Option<Candidate>,
    pub pending_reason: Option<PendingReason>,
    /// Unix seconds after which a pending output is removed, or a
    /// recommendation stops waiting for a choice.
    pub pending_until: Option<i64>,
    /// 样片推荐: what was asked.
    pub recommend: Option<RecommendRequest>,
    /// 样片推荐: samples, trials and candidates, filled in as they are measured.
    pub recommendation: Option<Recommendation>,
    /// 样片推荐: the candidate saved as a profile.
    pub choice: Option<RecommendChoice>,
    pub error: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// Latest encode progress while running (not stored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TaskProgress>,
}

/// The candidate a recommendation ended with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RecommendChoice {
    #[serde(default)]
    pub preset: Option<String>,
    pub quality: u8,
    /// The saved profile.
    pub profile_id: String,
    /// Transcode of the source queued with it, if asked for.
    pub transcode_task: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TaskProgress {
    pub id: i64,
    /// Seconds of output written.
    pub out_time_s: f64,
    /// Source duration, when known.
    pub duration_s: Option<f64>,
    pub percent: Option<f64>,
    /// Encode speed relative to real time.
    pub speed: Option<f64>,
    /// 样片推荐: stage, trial and sample.
    pub recommend: Option<RecommendProgress>,
}

/// What enqueueing needs; the rest is filled in by the database.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub kind: TaskKind,
    pub source: TaskSource,
    pub batch_id: Option<i64>,
    pub monitor_id: Option<i64>,
    pub path: PathBuf,
    pub force: bool,
    pub snapshot: SettingsSnapshot,
    /// 样片推荐 only.
    pub recommend: Option<RecommendRequest>,
}

impl NewTask {
    pub fn quota(&self) -> Quota {
        match self.kind {
            TaskKind::Transcode => self.snapshot.profile.encoder.quota(),
            // ADR 0003: recommendations take a CPU slot.
            TaskKind::Recommend => Quota::Cpu,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttemptKind {
    Skipped,
    Rejected,
    InsufficientSavings,
}

/// 尝试记录: a failed attempt on one file with one profile (ADR 0005). The
/// same file with the same fingerprint is not retried automatically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Attempt {
    pub id: i64,
    pub identity: FileIdentity,
    pub fingerprint: String,
    /// Path at the time; informational, the identity is the key.
    #[schema(value_type = String)]
    pub path: PathBuf,
    pub kind: AttemptKind,
    /// Skip code, for protective skips.
    pub code: Option<String>,
    pub message: String,
    pub task_id: Option<i64>,
    pub at: i64,
}

/// 配置方案: a saved, named encoding profile. `profile.id` equals `id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SavedProfile {
    pub id: String,
    pub name: String,
    pub profile: Profile,
}

/// 监控目录.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MonitorDir {
    pub id: i64,
    #[schema(value_type = String)]
    pub path: PathBuf,
    pub enabled: bool,
    pub profile_id: String,
    /// Include subdirectories.
    pub recursive: bool,
    /// Glob patterns relative to the monitored directory, e.g. `**/Extras/**`.
    pub excludes: Vec<String>,
}

/// Server behaviour outside the encoding settings. Changes apply at once,
/// except slot counts, which never interrupt running tasks (ADR 0003).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServerSettings {
    pub gpu_slots: u32,
    pub cpu_slots: u32,
    /// Master switch for automatic monitoring.
    pub monitor_enabled: bool,
    /// Interval of the incremental full scan, minutes.
    pub scan_interval_mins: u32,
    /// A new file must keep its size and mtime this long before it is queued.
    pub settle_secs: u32,
    /// Observation mode: the first N verified files go to review whatever
    /// the verdict (ADR 0004). 0 turns it off.
    pub observe_first_n: u32,
    /// Profile for manual tasks that name none.
    pub default_profile: String,
    /// Free space each volume keeps: a transcode waits, and the monitor
    /// pauses on that volume, rather than go below it (counting every
    /// file at its full source size). GiB.
    pub min_free_gib: u32,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            gpu_slots: 1,
            cpu_slots: 1,
            monitor_enabled: true,
            scan_interval_mins: 360,
            settle_secs: 60,
            observe_first_n: 20,
            default_profile: "default".into(),
            min_free_gib: 10,
        }
    }
}

impl ServerSettings {
    pub fn reserve_bytes(&self) -> u64 {
        u64::from(self.min_free_gib) * crate::space::GIB
    }

    pub fn slots(&self, quota: Quota) -> u32 {
        match quota {
            Quota::Gpu => self.gpu_slots,
            Quota::Cpu => self.cpu_slots,
        }
    }
}
