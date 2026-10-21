use axum::{extract::State, http::StatusCode};
use fncompress_core::settings::Profile;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{blocking, ApiState, ErrorBody, Json, Path};
use crate::{
    config::{MonitorInput, Settings, SettingsUpdate},
    error::{AppError, AppResult},
    model::{MonitorDir, SavedProfile},
    monitor::MonitorStatus,
};

pub fn routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(routes!(get_settings, put_settings))
        .routes(routes!(reset_observation))
        .routes(routes!(acknowledge_risk))
        .routes(routes!(list_profiles, create_profile))
        .routes(routes!(get_profile, put_profile, delete_profile))
        .routes(routes!(list_monitors, create_monitor))
        .routes(routes!(put_monitor, delete_monitor))
        .routes(routes!(scan_monitors))
}

#[utoipa::path(get, path = "/api/settings", tag = "config", responses((status = 200, body = Settings)))]
async fn get_settings(State(state): State<ApiState>) -> AppResult<Json<Settings>> {
    Ok(Json(blocking(&state, |app| app.settings()).await?))
}

/// Replaces `global` and/or `server`. New tasks use the new values at once;
/// slot changes apply as running tasks finish.
#[utoipa::path(put, path = "/api/settings", tag = "config", request_body = SettingsUpdate,
    responses((status = 200, body = Settings), (status = 400, body = ErrorBody)))]
async fn put_settings(
    State(state): State<ApiState>,
    Json(u): Json<SettingsUpdate>,
) -> AppResult<Json<Settings>> {
    Ok(Json(
        blocking(&state, move |app| app.update_settings(&u)).await?,
    ))
}

/// Starts the observation mode over: the next `observe_first_n` files go to review.
#[utoipa::path(post, path = "/api/settings/observation/reset", tag = "config",
    responses((status = 200, body = Settings)))]
async fn reset_observation(State(state): State<ApiState>) -> AppResult<Json<Settings>> {
    Ok(Json(blocking(&state, |app| app.reset_observation()).await?))
}

/// The user confirmed the risk notice; the UI stops showing it.
#[utoipa::path(post, path = "/api/settings/risk/acknowledge", tag = "config",
    responses((status = 200, body = Settings)))]
async fn acknowledge_risk(State(state): State<ApiState>) -> AppResult<Json<Settings>> {
    Ok(Json(blocking(&state, |app| app.acknowledge_risk()).await?))
}

#[derive(Debug, Serialize, ToSchema)]
struct ProfileList {
    profiles: Vec<SavedProfile>,
}

#[utoipa::path(get, path = "/api/profiles", tag = "config", responses((status = 200, body = ProfileList)))]
async fn list_profiles(State(state): State<ApiState>) -> AppResult<Json<ProfileList>> {
    let profiles = blocking(&state, |app| Ok(app.db.profiles()?)).await?;
    Ok(Json(ProfileList { profiles }))
}

#[utoipa::path(post, path = "/api/profiles", tag = "config", request_body = SavedProfile,
    responses((status = 201, body = SavedProfile), (status = 400, body = ErrorBody), (status = 409, body = ErrorBody)))]
async fn create_profile(
    State(state): State<ApiState>,
    Json(p): Json<SavedProfile>,
) -> AppResult<(StatusCode, Json<SavedProfile>)> {
    let saved = blocking(&state, move |app| app.create_profile(&p)).await?;
    Ok((StatusCode::CREATED, Json(saved)))
}

#[utoipa::path(get, path = "/api/profiles/{id}", tag = "config",
    params(("id" = String, Path, description = "方案 id")),
    responses((status = 200, body = SavedProfile), (status = 404, body = ErrorBody)))]
async fn get_profile(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> AppResult<Json<SavedProfile>> {
    Ok(Json(blocking(&state, move |app| app.profile(&id)).await?))
}

#[derive(Debug, Deserialize, ToSchema)]
struct ProfileUpdate {
    name: String,
    profile: Profile,
}

/// Queued tasks keep the snapshot they were queued with.
#[utoipa::path(put, path = "/api/profiles/{id}", tag = "config", request_body = ProfileUpdate,
    params(("id" = String, Path, description = "方案 id")),
    responses((status = 200, body = SavedProfile), (status = 400, body = ErrorBody), (status = 404, body = ErrorBody)))]
async fn put_profile(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(u): Json<ProfileUpdate>,
) -> AppResult<Json<SavedProfile>> {
    Ok(Json(
        blocking(&state, move |app| {
            app.update_profile(&id, &u.name, &u.profile)
        })
        .await?,
    ))
}

#[utoipa::path(delete, path = "/api/profiles/{id}", tag = "config",
    params(("id" = String, Path, description = "方案 id")),
    responses((status = 204), (status = 404, body = ErrorBody),
        (status = 409, body = ErrorBody, description = "默认方案或有监控目录在用")))]
async fn delete_profile(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    blocking(&state, move |app| app.delete_profile(&id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize, ToSchema)]
struct MonitorList {
    /// Master switch (`server.monitor_enabled`).
    enabled: bool,
    monitors: Vec<MonitorDir>,
    /// Watches, scans and files waiting to settle.
    status: MonitorStatus,
}

#[utoipa::path(get, path = "/api/monitors", tag = "config", responses((status = 200, body = MonitorList)))]
async fn list_monitors(State(state): State<ApiState>) -> AppResult<Json<MonitorList>> {
    let (enabled, monitors) = blocking(&state, |app| {
        Ok((
            app.db.server_settings()?.monitor_enabled,
            app.db.monitors()?,
        ))
    })
    .await?;
    Ok(Json(MonitorList {
        enabled,
        monitors,
        status: state.app.monitor.status(),
    }))
}

#[utoipa::path(post, path = "/api/monitors", tag = "config", request_body = MonitorInput,
    responses((status = 201, body = MonitorDir), (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody), (status = 409, body = ErrorBody)))]
async fn create_monitor(
    State(state): State<ApiState>,
    Json(m): Json<MonitorInput>,
) -> AppResult<(StatusCode, Json<MonitorDir>)> {
    let saved = blocking(&state, move |app| app.create_monitor(&m)).await?;
    Ok((StatusCode::CREATED, Json(saved)))
}

#[utoipa::path(put, path = "/api/monitors/{id}", tag = "config", request_body = MonitorInput,
    params(("id" = i64, Path, description = "监控目录 id")),
    responses((status = 200, body = MonitorDir), (status = 400, body = ErrorBody),
        (status = 404, body = ErrorBody), (status = 409, body = ErrorBody)))]
async fn put_monitor(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
    Json(m): Json<MonitorInput>,
) -> AppResult<Json<MonitorDir>> {
    Ok(Json(
        blocking(&state, move |app| app.update_monitor(id, &m)).await?,
    ))
}

#[utoipa::path(delete, path = "/api/monitors/{id}", tag = "config",
    params(("id" = i64, Path, description = "监控目录 id")),
    responses((status = 204), (status = 404, body = ErrorBody)))]
async fn delete_monitor(
    State(state): State<ApiState>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    blocking(&state, move |app| app.delete_monitor(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Starts an incremental full scan of every enabled monitored directory now.
#[utoipa::path(post, path = "/api/monitors/scan", tag = "config",
    responses((status = 202, body = MonitorStatus), (status = 409, body = ErrorBody, description = "自动监控已关闭")))]
async fn scan_monitors(
    State(state): State<ApiState>,
) -> AppResult<(StatusCode, Json<MonitorStatus>)> {
    let enabled = blocking(&state, |app| Ok(app.db.server_settings()?.monitor_enabled)).await?;
    if !enabled {
        return Err(AppError::conflict(
            "monitor_disabled",
            "自动监控总开关已关闭",
        ));
    }
    state.app.monitor.scan();
    Ok((StatusCode::ACCEPTED, Json(state.app.monitor.status())))
}
