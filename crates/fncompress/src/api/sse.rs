use std::{convert::Infallible, time::Duration};

use axum::{
    extract::State,
    http::{header::HeaderName, HeaderValue},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse,
    },
};
use futures_util::{stream, StreamExt};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::ApiState;

pub fn routes() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new().routes(routes!(events))
}

/// Server-sent events. Each SSE `event:` name equals the payload's `event`
/// field (see the `Event` schema): `task_created`, `task_updated`,
/// `task_deleted`, `task_progress`, `config_changed`, `monitor_status`,
/// `space_status`.
/// `ready` is sent first; `lagged` means events were dropped and the client
/// should reload what it shows.
///
/// `X-Accel-Buffering: no` stops nginx in front (the fnOS gateway, ADR
/// 0012, buffers proxied responses by default) from holding events back.
#[utoipa::path(get, path = "/api/events", tag = "tasks",
    responses((status = 200, content_type = "text/event-stream", description = "事件流")))]
async fn events(State(state): State<ApiState>) -> impl IntoResponse {
    let rx = state.app.events.subscribe();
    let ready =
        stream::once(async { Ok::<_, Infallible>(SseEvent::default().event("ready").data("{}")) });
    let events = BroadcastStream::new(rx).map(|item| {
        Ok(match item {
            Ok(e) => SseEvent::default()
                .event(e.name())
                .data(serde_json::to_string(&e).expect("serializable")),
            Err(BroadcastStreamRecvError::Lagged(n)) => SseEvent::default()
                .event("lagged")
                .data(format!("{{\"missed\":{n}}}")),
        })
    });
    let mut shutdown = state.shutdown.clone();
    let stop = async move {
        let _ = shutdown.wait_for(|stopping| *stopping).await;
    };
    let sse = Sse::new(ready.chain(events).take_until(stop))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)));
    (
        [(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        )],
        sse,
    )
}
