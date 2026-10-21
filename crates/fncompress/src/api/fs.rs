use std::path::PathBuf;

use axum::extract::State;
use serde::Deserialize;
use utoipa::IntoParams;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{ApiState, ErrorBody, Json, Query};
use crate::{
    browse::{self, Listing},
    error::{AppError, AppResult},
};

pub fn routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new().routes(routes!(list_dir))
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct FsQuery {
    /// Directory to list; the roots (directories authorized in fnOS, then
    /// the volumes `/vol*` the service can read) when absent. The roots can
    /// be empty: nothing authorized and no readable volume.
    #[param(value_type = Option<String>)]
    path: Option<PathBuf>,
}

/// Subdirectories and video files of a directory, for picking paths in the
/// web UI. Hidden entries, `@` directories and `#recycle` are left out.
#[utoipa::path(get, path = "/api/fs", tag = "system", params(FsQuery),
    responses((status = 200, body = Listing), (status = 400, body = ErrorBody, description = "不是目录、不存在或没有权限")))]
async fn list_dir(
    State(state): State<ApiState>,
    Query(q): Query<FsQuery>,
) -> AppResult<Json<Listing>> {
    let roots_file = state.roots_file.clone();
    let authorized = move || browse::authorized(roots_file.as_deref());
    let Some(path) = q.path.filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(Json(
            tokio::task::spawn_blocking(move || browse::roots(&authorized()))
                .await
                .map_err(|e| AppError::internal(format!("后台任务异常：{e}")))?,
        ));
    };
    let listing = tokio::task::spawn_blocking(move || {
        browse::list(&path, &authorized()).map_err(|e| {
            let why = match e.kind() {
                std::io::ErrorKind::NotFound => "不存在".to_owned(),
                std::io::ErrorKind::PermissionDenied => "没有读取权限".to_owned(),
                std::io::ErrorKind::NotADirectory => "不是目录".to_owned(),
                _ => e.to_string(),
            };
            AppError::bad_request("bad_path", format!("{}：{why}", path.display()))
        })
    })
    .await
    .map_err(|e| AppError::internal(format!("后台任务异常：{e}")))??;
    Ok(Json(listing))
}
