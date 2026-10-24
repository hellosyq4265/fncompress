use std::{collections::BTreeMap, path::PathBuf};

use axum::{extract::State, http::StatusCode};
use fncompress_core::{
    caps::EncoderProbe,
    pipeline::Outcome,
    recycle::{self, RecycleEntry},
    verify::{Report, Severity, Verdict},
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{blocking, ApiState, ErrorBody, Json, Path, Query, SystemInfo};
use crate::{
    app::{DoviStatus, ScorerStatus},
    db::TaskFilter,
    error::{AppError, AppResult},
    model::Attempt,
    space::SpaceStatus,
};

pub fn routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(routes!(health))
        .routes(routes!(system))
        .routes(routes!(list_attempts))
        .routes(routes!(delete_attempt))
        .routes(routes!(list_recycle))
        .routes(routes!(empty_recycle))
        .routes(routes!(verify_stats))
}

#[derive(Debug, Serialize, ToSchema)]
struct Health {
    ok: bool,
}

#[utoipa::path(get, path = "/api/health", tag = "system", responses((status = 200, body = Health)))]
async fn health() -> Json<Health> {
    Json(Health { ok: true })
}

#[derive(Debug, Serialize, ToSchema)]
struct System {
    #[serde(flatten)]
    info: SystemInfo,
    /// Startup trial encode of every encoder.
    encoders: Vec<EncoderProbe>,
    /// The scoring FFmpeg for 样片推荐.
    scorer: ScorerStatus,
    /// dovi_tool for Dolby Vision routes.
    dovi: DoviStatus,
    running_tasks: Vec<i64>,
    /// Queued transcodes waiting for free space.
    space: SpaceStatus,
}

#[utoipa::path(get, path = "/api/system", tag = "system",
    responses((status = 200, body = System), (status = 500, body = ErrorBody)))]
async fn system(State(state): State<ApiState>) -> AppResult<Json<System>> {
    let scorer = blocking(&state, |app| app.scorer_status()).await?;
    let dovi = blocking(&state, |app| app.dovi_status()).await?;
    Ok(Json(System {
        info: state.info.as_ref().clone(),
        encoders: state.app.encoders.clone(),
        scorer,
        dovi,
        running_tasks: state.app.scheduler.running_ids(),
        space: state.app.scheduler.space_status(),
    }))
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct AttemptQuery {
    /// Only attempts on files under this directory.
    path_prefix: Option<String>,
    /// Default 100, at most 1000.
    limit: Option<u32>,
}

#[derive(Debug, Serialize, ToSchema)]
struct AttemptList {
    attempts: Vec<Attempt>,
}

/// 尝试记录, newest first. A recorded file is not retried automatically with
/// the same profile.
#[utoipa::path(get, path = "/api/attempts", tag = "system", params(AttemptQuery),
    responses((status = 200, body = AttemptList)))]
async fn list_attempts(
    State(state): State<ApiState>,
    Query(q): Query<AttemptQuery>,
) -> AppResult<Json<AttemptList>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let attempts = blocking(&state, move |app| {
        Ok(app
            .db
            .attempts(q.path_prefix.as_deref().map(std::path::Path::new), limit)?)
    })
    .await?;
    Ok(Json(AttemptList { attempts }))
}

/// Forgets an attempt: the monitor may try the file again.
#[utoipa::path(delete, path = "/api/attempts/{id}", tag = "system",
    params(("id" = i64, Path, description = "尝试记录 id")),
    responses((status = 204), (status = 404, body = ErrorBody)))]
async fn delete_attempt(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let deleted = blocking(&state, move |app| Ok(app.db.delete_attempt(id)?)).await?;
    if !deleted {
        return Err(AppError::not_found(&format!("尝试记录 #{id}")));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize, ToSchema)]
struct RecycleDir {
    #[schema(value_type = String)]
    dir: PathBuf,
    entries: Vec<RecycleEntry>,
}

#[derive(Debug, Serialize, ToSchema)]
struct RecycleList {
    dirs: Vec<RecycleDir>,
}

fn recycle_dirs(app: &crate::app::App) -> AppResult<Vec<RecycleDir>> {
    let mut out = Vec::new();
    for dir in app.db.recycle_dirs()? {
        match recycle::list(&dir) {
            Ok(entries) => out.push(RecycleDir { dir, entries }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(AppError::internal(format!(
                    "读取回收目录 {} 失败：{e}",
                    dir.display()
                )))
            }
        }
    }
    Ok(out)
}

/// Replaced sources in every recycle directory the server has used.
#[utoipa::path(get, path = "/api/recycle", tag = "system", responses((status = 200, body = RecycleList)))]
async fn list_recycle(State(state): State<ApiState>) -> AppResult<Json<RecycleList>> {
    let dirs = blocking(&state, recycle_dirs).await?;
    Ok(Json(RecycleList { dirs }))
}

#[derive(Debug, Serialize, ToSchema)]
struct Emptied {
    removed: Vec<RecycleEntry>,
}

/// 一键清空: deletes every entry of every recycle directory.
#[utoipa::path(post, path = "/api/recycle/empty", tag = "system", responses((status = 200, body = Emptied)))]
async fn empty_recycle(State(state): State<ApiState>) -> AppResult<Json<Emptied>> {
    let removed =
        blocking(&state, |app| {
            let mut removed = Vec::new();
            for d in recycle_dirs(app)? {
                removed.extend(recycle::empty(&d.dir).map_err(|e| {
                    AppError::internal(format!("清空 {} 失败：{e}", d.dir.display()))
                })?);
            }
            Ok(removed)
        })
        .await?;
    Ok(Json(Emptied { removed }))
}

#[derive(Debug, Serialize, ToSchema)]
struct CheckCount {
    check: String,
    severity: Severity,
    count: u32,
}

#[derive(Debug, Default, Serialize, ToSchema)]
struct VerifyStats {
    /// Verification reports counted.
    reports: u32,
    passed: u32,
    passed_with_warnings: u32,
    rejected: u32,
    /// How often each check reported, per severity.
    checks: Vec<CheckCount>,
    /// Observation mode: files held so far and the window size.
    observed: u32,
    observe_first_n: u32,
}

fn report_of(outcome: &Outcome) -> Option<&Report> {
    match outcome {
        Outcome::Skipped { .. } | Outcome::PredictedNoGain { .. } => None,
        Outcome::Rejected { candidate } | Outcome::Verified { candidate } => {
            Some(&candidate.report)
        }
        Outcome::InsufficientSavings { report, .. } | Outcome::Published { report, .. } => {
            Some(report)
        }
    }
}

/// How often each check fired across all verification reports on record;
/// the data for calibrating thresholds after the observation mode.
#[utoipa::path(get, path = "/api/verify-stats", tag = "system", responses((status = 200, body = VerifyStats)))]
async fn verify_stats(State(state): State<ApiState>) -> AppResult<Json<VerifyStats>> {
    let stats = blocking(&state, |app| {
        let tasks = app.db.tasks(&TaskFilter::default())?;
        let mut stats = VerifyStats {
            observed: app.db.observed_count()?,
            observe_first_n: app.db.server_settings()?.observe_first_n,
            ..Default::default()
        };
        let mut counts: BTreeMap<(String, Severity), u32> = BTreeMap::new();
        for report in tasks
            .iter()
            .filter_map(|t| t.outcome.as_ref().and_then(report_of))
        {
            stats.reports += 1;
            match report.verdict {
                Verdict::Passed => stats.passed += 1,
                Verdict::PassedWithWarnings => stats.passed_with_warnings += 1,
                Verdict::Rejected => stats.rejected += 1,
            }
            for f in &report.findings {
                *counts.entry((f.check.clone(), f.severity)).or_default() += 1;
            }
        }
        stats.checks = counts
            .into_iter()
            .map(|((check, severity), count)| CheckCount {
                check,
                severity,
                count,
            })
            .collect();
        Ok(stats)
    })
    .await?;
    Ok(Json(stats))
}
