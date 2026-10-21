//! HTTP API (ADR 0002): one task resource `/api/tasks`, SSE at
//! `/api/events`, one error shape, OpenAPI generated from the handlers.

mod config;
mod fs;
mod sse;
mod system;
mod tasks;

use std::{path::PathBuf, sync::Arc};

use axum::{
    extract::{FromRequest, FromRequestParts, Request, State},
    http::{request::Parts, Method, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Router,
};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::watch;
use utoipa::{OpenApi, ToSchema};
use utoipa_axum::router::OpenApiRouter;

use crate::{
    app::App,
    error::{AppError, AppResult},
    web::Assets,
};

/// Shared by every handler.
#[derive(Clone)]
pub struct ApiState {
    pub app: Arc<App>,
    pub info: Arc<SystemInfo>,
    /// Turns true when the server shuts down; SSE streams end then, so the
    /// graceful shutdown does not wait for them forever.
    pub shutdown: watch::Receiver<bool>,
    /// The front end served outside `/api`.
    pub assets: Arc<dyn Assets>,
    /// Where the fnOS lifecycle scripts keep the authorized directories
    /// (browse roots, ADR 0012).
    pub roots_file: Option<PathBuf>,
}

/// Static facts about this server, for `/api/system`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SystemInfo {
    pub version: String,
    #[schema(value_type = String)]
    pub data_dir: PathBuf,
    /// Unix seconds.
    pub started_at: i64,
}

/// `{ "error": { ... } }`
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: AppError,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, axum::Json(ErrorBody { error: self })).into_response()
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "fncompress",
        description = "飞牛 NAS 视频压缩服务。错误统一为 `{ error: { code, message, details } }`；进度通过 SSE `/api/events` 推送。"
    ),
    components(schemas(ErrorBody, crate::events::Event)),
    tags(
        (name = "tasks", description = "统一任务资源"),
        (name = "config", description = "设置、配置方案、监控目录"),
        (name = "system", description = "系统信息、尝试记录、回收目录、校验统计"),
    )
)]
struct ApiDoc;

fn api() -> OpenApiRouter<ApiState> {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(tasks::routes())
        .merge(config::routes())
        .merge(system::routes())
        .merge(fs::routes())
        .merge(sse::routes())
}

/// The OpenAPI document (also served at `/api/openapi.json`).
pub fn openapi() -> utoipa::openapi::OpenApi {
    api().into_openapi()
}

/// fnOS's unified gateway forwards `/app/fncompress/...` to the app's Unix
/// socket (ADR 0012); the app itself lives at `/`.
pub const GATEWAY_PREFIX: &str = "/app/fncompress";

pub fn router(state: ApiState) -> Router {
    let (router, doc) = api().split_for_parts();
    let doc = Arc::new(doc);
    let app = router
        .route(
            "/api/openapi.json",
            axum::routing::get(move || {
                let doc = doc.clone();
                async move { axum::Json(doc.as_ref().clone()) }
            }),
        )
        .fallback(fallback)
        .with_state(state);
    // Routing happens inside `app`, after the prefix is gone.
    Router::new()
        .fallback_service(app)
        .layer(axum::middleware::from_fn(strip_gateway_prefix))
}

/// Serves `/app/fncompress/x` as `/x`. The bare prefix redirects to the
/// prefix with a slash: the front end's relative URLs need it.
async fn strip_gateway_prefix(mut req: Request, next: Next) -> Response {
    let uri = req.uri();
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    if uri.path() == GATEWAY_PREFIX {
        return Redirect::permanent(&format!("{GATEWAY_PREFIX}/{query}")).into_response();
    }
    if let Some(rest) = uri
        .path()
        .strip_prefix(GATEWAY_PREFIX)
        .filter(|r| r.starts_with('/'))
    {
        let mut parts = uri.clone().into_parts();
        parts.path_and_query = format!("{rest}{query}").parse().ok();
        if let Ok(stripped) = Uri::from_parts(parts) {
            *req.uri_mut() = stripped;
        }
    }
    next.run(req).await
}

/// The router for the gateway socket. The gateway only checks the NAS login
/// and passes the user's role on in `X-Trim-Isadmin` (ADR 0012); the app
/// replaces source videos, so only administrators get in.
pub fn admin_only(router: Router) -> Router {
    router.layer(axum::middleware::from_fn(require_admin))
}

async fn require_admin(req: Request, next: Next) -> Response {
    let admin = req
        .headers()
        .get("x-trim-isadmin")
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"true"));
    if admin {
        return next.run(req).await;
    }
    const MESSAGE: &str = "只有 NAS 管理员可以使用视频压缩。";
    if req.uri().path().contains("/api") {
        AppError::new(403, "forbidden", MESSAGE).into_response()
    } else {
        (
            StatusCode::FORBIDDEN,
            [("content-type", "text/plain; charset=utf-8")],
            MESSAGE,
        )
            .into_response()
    }
}

/// Unknown `/api` paths get the JSON error; everything else is the front end.
async fn fallback(State(state): State<ApiState>, method: Method, uri: Uri) -> Response {
    let path = uri.path();
    if path == "/api" || path.starts_with("/api/") {
        return AppError::new(404, "not_found", "没有这个接口").into_response();
    }
    crate::web::respond(state.assets.as_ref(), &method, &uri)
}

/// Runs blocking work (SQLite, ffprobe, file moves) off the async threads.
async fn blocking<T, F>(state: &ApiState, f: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce(&App) -> AppResult<T> + Send + 'static,
{
    let app = state.app.clone();
    tokio::task::spawn_blocking(move || f(&app))
        .await
        .map_err(|e| AppError::internal(format!("后台任务异常：{e}")))?
}

/// `axum::Json` with rejections in the unified error shape.
pub struct Json<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Json<T> {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, AppError> {
        axum::Json::<T>::from_request(req, state)
            .await
            .map(|axum::Json(v)| Json(v))
            .map_err(|e| {
                AppError::bad_request("bad_request", format!("请求体无效：{}", e.body_text()))
            })
    }
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// `axum::extract::Query` with rejections in the unified error shape.
pub struct Query<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Query<T> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(v)| Query(v))
            .map_err(|e| {
                AppError::bad_request("bad_request", format!("查询参数无效：{}", e.body_text()))
            })
    }
}

/// `axum::extract::Path` with rejections in the unified error shape.
pub struct Path<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for Path<T> {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(v)| Path(v))
            .map_err(|e| {
                AppError::bad_request("bad_request", format!("路径参数无效：{}", e.body_text()))
            })
    }
}
