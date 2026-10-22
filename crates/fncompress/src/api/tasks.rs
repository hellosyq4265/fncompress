use std::path::PathBuf;

use axum::{extract::State, http::StatusCode};
use fncompress_core::recommend::RecommendRequest;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{blocking, ApiState, ErrorBody, Json, Path, Query};
use crate::{
    app::{
        BatchPreview, BatchRequest, ChooseRequest, ChooseResult, ManualRequest, Preview,
        RecommendSubmit,
    },
    db::TaskFilter,
    error::{AppError, AppResult},
    model::{Task, TaskKind, TaskSource, TaskState},
    scan::SkippedFile,
};

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 500;

pub fn routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(routes!(list_tasks, create_task))
        .routes(routes!(preview_task))
        .routes(routes!(preview_batch))
        .routes(routes!(get_task, delete_task))
        .routes(routes!(cancel_task))
        .routes(routes!(retry_task))
        .routes(routes!(release_task))
        .routes(routes!(discard_task))
        .routes(routes!(choose_candidate))
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct ListQuery {
    /// Comma-separated states, e.g. `queued,running`.
    state: Option<String>,
    source: Option<TaskSource>,
    batch_id: Option<i64>,
    /// Only tasks with a smaller id (paging, newest first).
    before: Option<i64>,
    /// Default 100, at most 500.
    limit: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
struct TaskList {
    tasks: Vec<Task>,
}

fn parse_states(s: &str) -> AppResult<Vec<TaskState>> {
    s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            serde_json::from_value(serde_json::Value::String(s.into()))
                .map_err(|_| AppError::bad_request("bad_request", format!("未知的任务状态：{s}")))
        })
        .collect()
}

fn with_progress(state: &ApiState, mut task: Task) -> Task {
    if task.state == TaskState::Running {
        task.progress = state.app.scheduler.progress(task.id);
    }
    task
}

/// List tasks, newest first.
#[utoipa::path(get, path = "/api/tasks", tag = "tasks", params(ListQuery),
    responses((status = 200, body = TaskList), (status = 400, body = ErrorBody)))]
async fn list_tasks(
    State(state): State<ApiState>,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<TaskList>> {
    let filter = TaskFilter {
        states: q
            .state
            .as_deref()
            .map(parse_states)
            .transpose()?
            .unwrap_or_default(),
        source: q.source,
        batch_id: q.batch_id,
        before: q.before,
        limit: Some(q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)),
    };
    let tasks = blocking(&state, move |app| Ok(app.db.tasks(&filter)?)).await?;
    let tasks = tasks
        .into_iter()
        .map(|t| with_progress(&state, t))
        .collect();
    Ok(Json(TaskList { tasks }))
}

/// A new task. `source: manual` takes `path`; `source: folder_batch` takes
/// `dir` and queues every video under it that passes screening.
/// `type: recommend` (样片推荐) takes `path`, the profile to start from and
/// `recommend`; it is always manual.
#[derive(Debug, Deserialize, ToSchema)]
struct CreateTask {
    #[serde(rename = "type", default = "transcode")]
    kind: TaskKind,
    #[serde(default = "manual")]
    source: TaskSource,
    #[schema(value_type = Option<String>)]
    path: Option<PathBuf>,
    #[schema(value_type = Option<String>)]
    dir: Option<PathBuf>,
    /// Saved profile; the server's default profile when absent.
    profile_id: Option<String>,
    /// Process files that already carry the processed marker.
    #[serde(default)]
    force: bool,
    /// Folder batch: include subdirectories (default true).
    #[serde(default = "yes")]
    recursive: bool,
    /// Folder batch: also queue files with an attempt record for this profile.
    #[serde(default)]
    include_attempted: bool,
    /// Folder batch: screen only these files (the preview's `files`)
    /// instead of walking `dir` again.
    #[schema(value_type = Option<Vec<String>>)]
    files: Option<Vec<PathBuf>>,
    /// Folder batch: sent back in `scan_progress` events.
    scan_id: Option<String>,
    /// 样片推荐: target and samples (defaults: VMAF 95, 4 auto samples of 10 s
    /// picked by bitrate).
    recommend: Option<RecommendRequest>,
}

fn transcode() -> TaskKind {
    TaskKind::Transcode
}
fn manual() -> TaskSource {
    TaskSource::Manual
}
fn yes() -> bool {
    true
}

#[derive(Debug, Serialize, ToSchema)]
struct Created {
    /// Folder batches only.
    batch_id: Option<i64>,
    tasks: Vec<Task>,
    /// Files of a folder batch that were not queued.
    skipped: Vec<SkippedFile>,
}

#[utoipa::path(post, path = "/api/tasks", tag = "tasks", request_body = CreateTask,
    responses(
        (status = 201, body = Created),
        (status = 400, body = ErrorBody, description = "路径无效、来源不支持"),
        (status = 404, body = ErrorBody, description = "配置方案不存在"),
        (status = 409, body = ErrorBody, description = "已有任务（task_exists）、已处理过（already_processed，需要 force）或评分工具不可用（scoring_unavailable）"),
    ))]
async fn create_task(
    State(state): State<ApiState>,
    Json(req): Json<CreateTask>,
) -> AppResult<(StatusCode, Json<Created>)> {
    if req.kind == TaskKind::Recommend {
        if req.source != TaskSource::Manual {
            return Err(AppError::bad_request(
                "bad_source",
                "样片推荐只能针对单个文件手动提交",
            ));
        }
        let r = RecommendSubmit {
            path: req
                .path
                .ok_or_else(|| AppError::bad_request("bad_request", "样片推荐需要 path"))?,
            profile_id: req.profile_id,
            request: req.recommend.unwrap_or_default(),
        };
        let task = blocking(&state, move |app| app.submit_recommend(&r)).await?;
        return Ok((
            StatusCode::CREATED,
            Json(Created {
                batch_id: None,
                tasks: vec![task],
                skipped: vec![],
            }),
        ));
    }
    let created = match req.source {
        TaskSource::Manual => {
            let path = req
                .path
                .ok_or_else(|| AppError::bad_request("bad_request", "手动任务需要 path"))?;
            let r = ManualRequest {
                path,
                profile_id: req.profile_id,
                force: req.force,
            };
            let task = blocking(&state, move |app| app.submit_manual(&r)).await?;
            Created {
                batch_id: None,
                tasks: vec![task],
                skipped: vec![],
            }
        }
        TaskSource::FolderBatch => {
            let dir = req
                .dir
                .ok_or_else(|| AppError::bad_request("bad_request", "文件夹批次需要 dir"))?;
            let r = BatchRequest {
                dir,
                profile_id: req.profile_id,
                recursive: req.recursive,
                force: req.force,
                include_attempted: req.include_attempted,
                files: req.files,
                scan_id: req.scan_id,
            };
            let b = blocking(&state, move |app| app.submit_batch(&r)).await?;
            Created {
                batch_id: Some(b.batch_id),
                tasks: b.tasks,
                skipped: b.skipped,
            }
        }
        TaskSource::Monitor => {
            return Err(AppError::bad_request(
                "bad_source",
                "自动监控的任务由监控目录产生，不能手动提交",
            ))
        }
    };
    Ok((StatusCode::CREATED, Json(created)))
}

#[derive(Debug, Deserialize, ToSchema)]
struct PreviewRequest {
    #[schema(value_type = String)]
    path: PathBuf,
    profile_id: Option<String>,
    #[serde(default)]
    force: bool,
}

/// What a transcode of this file would do: route, expected losses,
/// protective skip, earlier attempts. Nothing is queued.
#[utoipa::path(post, path = "/api/tasks/preview", tag = "tasks", request_body = PreviewRequest,
    responses((status = 200, body = Preview), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn preview_task(
    State(state): State<ApiState>,
    Json(req): Json<PreviewRequest>,
) -> AppResult<Json<Preview>> {
    let p = blocking(&state, move |app| {
        app.preview(&req.path, req.profile_id.as_deref(), req.force)
    })
    .await?;
    Ok(Json(p))
}

/// What a folder batch would queue and which files it would skip, and why.
/// Nothing is queued.
#[utoipa::path(post, path = "/api/tasks/preview-batch", tag = "tasks", request_body = BatchRequest,
    responses((status = 200, body = BatchPreview), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn preview_batch(
    State(state): State<ApiState>,
    Json(req): Json<BatchRequest>,
) -> AppResult<Json<BatchPreview>> {
    Ok(Json(
        blocking(&state, move |app| app.preview_batch(&req)).await?,
    ))
}

#[utoipa::path(get, path = "/api/tasks/{id}", tag = "tasks",
    params(("id" = i64, Path, description = "任务 id")),
    responses((status = 200, body = Task), (status = 404, body = ErrorBody)))]
async fn get_task(State(state): State<ApiState>, Path(id): Path<i64>) -> AppResult<Json<Task>> {
    let task = blocking(&state, move |app| app.task(id)).await?;
    Ok(Json(with_progress(&state, task)))
}

/// Removes a finished task from the list (not queued, running or pending).
#[utoipa::path(delete, path = "/api/tasks/{id}", tag = "tasks",
    params(("id" = i64, Path, description = "任务 id")),
    responses((status = 204), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody)))]
async fn delete_task(State(state): State<ApiState>, Path(id): Path<i64>) -> AppResult<StatusCode> {
    blocking(&state, move |app| app.delete(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Cancels a queued task at once, or stops a running one (its state turns
/// `cancelled` when FFmpeg has exited; watch `task_updated`).
#[utoipa::path(post, path = "/api/tasks/{id}/cancel", tag = "tasks",
    params(("id" = i64, Path, description = "任务 id")),
    responses((status = 200, body = Task), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody)))]
async fn cancel_task(State(state): State<ApiState>, Path(id): Path<i64>) -> AppResult<Json<Task>> {
    Ok(Json(blocking(&state, move |app| app.cancel(id)).await?))
}

/// Queues the same file again with the profile as it is now. Attempt
/// records do not stop a manual retry.
#[utoipa::path(post, path = "/api/tasks/{id}/retry", tag = "tasks",
    params(("id" = i64, Path, description = "任务 id")),
    responses((status = 201, body = Task), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody)))]
async fn retry_task(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> AppResult<(StatusCode, Json<Task>)> {
    let task = blocking(&state, move |app| app.retry(id)).await?;
    Ok((StatusCode::CREATED, Json(task)))
}

/// 人工放行: publishes the pending output; the source goes to the recycle.
#[utoipa::path(post, path = "/api/tasks/{id}/release", tag = "tasks",
    params(("id" = i64, Path, description = "任务 id")),
    responses((status = 200, body = Task), (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody, description = "没有待确认成品、收益不足或发布失败")))]
async fn release_task(State(state): State<ApiState>, Path(id): Path<i64>) -> AppResult<Json<Task>> {
    Ok(Json(blocking(&state, move |app| app.release(id)).await?))
}

/// Deletes the pending output; the source stays. For a recommendation
/// awaiting a choice: ends it without choosing.
#[utoipa::path(post, path = "/api/tasks/{id}/discard", tag = "tasks",
    params(("id" = i64, Path, description = "任务 id")),
    responses((status = 200, body = Task), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody)))]
async fn discard_task(State(state): State<ApiState>, Path(id): Path<i64>) -> AppResult<Json<Task>> {
    Ok(Json(
        blocking(&state, move |app| app.discard(id, TaskState::Discarded)).await?,
    ))
}

/// Saves a candidate of a recommendation awaiting a choice as a new profile,
/// and optionally queues a transcode of the source with it. A transcode that
/// cannot be queued is reported in `transcode_error`; the profile stays.
#[utoipa::path(post, path = "/api/tasks/{id}/choose", tag = "tasks", request_body = ChooseRequest,
    params(("id" = i64, Path, description = "任务 id")),
    responses((status = 200, body = ChooseResult), (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody, description = "不在待选择状态，或方案 id 已存在")))]
async fn choose_candidate(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(req): Json<ChooseRequest>,
) -> AppResult<Json<ChooseResult>> {
    Ok(Json(
        blocking(&state, move |app| app.choose(id, &req)).await?,
    ))
}
