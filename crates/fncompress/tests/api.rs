//! HTTP API end to end: requests go through the axum router in-process
//! (tower `oneshot`), tasks run real FFmpeg on lavfi samples.

mod common;

use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use fncompress::server::{ServeOptions, Server};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tower::ServiceExt;

async fn server(tools: Option<&common::Tools>, dir: &std::path::Path) -> Server {
    let s = Server::start(&ServeOptions {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.join("data"),
        ffmpeg: tools.map(|t| t.ffmpeg.clone()),
        ffprobe: tools.map(|t| t.ffprobe.clone()),
        render_device: None,
        scoring_ffmpeg: None,
        dovi_tool: None,
        bundled_scoring_ffmpeg: None,
        socket: None,
        roots_file: None,
        probe_encoders: false,
    })
    .await
    .unwrap();
    let r = s.router();
    // Fast encodes, no observation unless a test asks for it.
    let (status, _) = call(
        &r,
        "PUT",
        "/api/profiles/default",
        Some(json!({
            "name": "软件 HEVC（x265）",
            "profile": { "encoder": "libx265", "quality": 32, "preset": "ultrafast" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let mut settings = call(&r, "GET", "/api/settings", None).await.1;
    settings["server"]["observe_first_n"] = json!(0);
    let (status, body) = call(&r, "PUT", "/api/settings", Some(settings)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    s
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri);
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, value)
}

/// Opens `/api/events` and forwards each (event, data) pair.
async fn subscribe(router: &Router) -> mpsc::UnboundedReceiver<(String, Value)> {
    let req = Request::get("/api/events").body(Body::empty()).unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
    // nginx in front (the fnOS gateway) must not buffer events.
    assert_eq!(res.headers()["x-accel-buffering"], "no");
    let (tx, rx) = mpsc::unbounded_channel();
    let mut body = res.into_body().into_data_stream();
    tokio::spawn(async move {
        let mut buf = String::new();
        while let Some(Ok(chunk)) = body.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buf.find("\n\n") {
                let frame: String = buf.drain(..end + 2).collect();
                let mut event = String::new();
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(v) = line.strip_prefix("event: ") {
                        event = v.into();
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        data.push_str(v);
                    }
                }
                if !event.is_empty() {
                    let _ = tx.send((event, serde_json::from_str(&data).unwrap_or(Value::Null)));
                }
            }
        }
    });
    rx
}

/// Waits for an event matching `want`; panics after a timeout.
async fn expect_event(
    rx: &mut mpsc::UnboundedReceiver<(String, Value)>,
    want: impl Fn(&str, &Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let (event, data) = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("event before timeout")
            .expect("stream open");
        if want(&event, &data) {
            return data;
        }
    }
}

fn final_update(id: i64) -> impl Fn(&str, &Value) -> bool {
    move |event, data| {
        event == "task_updated"
            && data["task"]["id"] == id
            && !matches!(data["task"]["state"].as_str(), Some("queued" | "running"))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_have_one_shape_and_openapi_is_served() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(None, dir.path()).await;
    let r = s.router();

    for (method, uri, body, status, code) in [
        ("GET", "/api/nope", None, 404, "not_found"),
        ("GET", "/api/tasks/999", None, 404, "not_found"),
        ("GET", "/api/tasks/abc", None, 400, "bad_request"),
        ("GET", "/api/tasks?state=bogus", None, 400, "bad_request"),
        (
            "POST",
            "/api/tasks",
            Some(json!({ "path": 3 })),
            400,
            "bad_request",
        ),
        ("POST", "/api/tasks", Some(json!({})), 400, "bad_request"),
        (
            "POST",
            "/api/tasks",
            Some(json!({ "type": "recommend", "source": "folder_batch", "dir": "/x" })),
            400,
            "bad_source",
        ),
        (
            "POST",
            "/api/tasks",
            Some(json!({ "type": "recommend", "path": "/x",
                "recommend": { "target_vmaf": 120 } })),
            400,
            "bad_request",
        ),
        (
            "POST",
            "/api/tasks/1/choose",
            Some(json!({ "quality": 20 })),
            404,
            "not_found",
        ),
        (
            "POST",
            "/api/tasks",
            Some(json!({ "source": "monitor", "path": "/x" })),
            400,
            "bad_source",
        ),
        (
            "POST",
            "/api/tasks",
            Some(json!({ "path": "/definitely/missing.mkv" })),
            400,
            "bad_path",
        ),
        (
            "POST",
            "/api/profiles",
            Some(json!({ "id": "a;b", "name": "x", "profile": {} })),
            400,
            "invalid_settings",
        ),
        (
            "DELETE",
            "/api/profiles/default",
            None,
            409,
            "profile_in_use",
        ),
        (
            "POST",
            "/api/monitors",
            Some(json!({ "path": "/definitely/missing" })),
            400,
            "bad_path",
        ),
        ("POST", "/api/tasks/1/release", None, 404, "not_found"),
    ] {
        let (got, body) = call(&r, method, uri, body).await;
        assert_eq!(got.as_u16(), status, "{method} {uri}: {body}");
        assert_eq!(body["error"]["code"], code, "{method} {uri}: {body}");
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()));
    }

    let (status, doc) = call(&r, "GET", "/api/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    for path in [
        "/api/tasks",
        "/api/tasks/{id}/release",
        "/api/tasks/{id}/choose",
        "/api/events",
        "/api/settings",
        "/api/monitors",
        "/api/verify-stats",
    ] {
        assert!(doc["paths"][path].is_object(), "{path} documented");
    }
    for schema in ["Task", "Outcome", "Event", "ErrorBody", "SettingsSnapshot"] {
        assert!(
            doc["components"]["schemas"][schema].is_object(),
            "{schema} schema"
        );
    }
    let (status, health) = call(&r, "GET", "/api/health", None).await;
    assert_eq!((status, health), (StatusCode::OK, json!({ "ok": true })));
}

#[tokio::test(flavor = "multi_thread")]
async fn risk_notice_is_acknowledged_once_on_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(None, dir.path()).await;
    let r = s.router();
    let (_, settings) = call(&r, "GET", "/api/settings", None).await;
    assert_eq!(settings["risk_acknowledged"], json!(false));

    let (status, settings) = call(&r, "POST", "/api/settings/risk/acknowledge", None).await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert_eq!(settings["risk_acknowledged"], json!(true));
    // Saving the settings form (which echoes the whole body) keeps it.
    let (status, body) = call(&r, "PUT", "/api/settings", Some(settings)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, settings) = call(&r, "GET", "/api/settings", None).await;
    assert_eq!(settings["risk_acknowledged"], json!(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn monitors_over_http() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(None, dir.path()).await;
    let r = s.router();
    let watch = dir.path().join("watch");
    std::fs::create_dir_all(&watch).unwrap();

    let (status, m) = call(
        &r,
        "POST",
        "/api/monitors",
        Some(json!({ "path": watch, "excludes": ["**/Extras/**"] })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{m}");
    assert_eq!(m["profile_id"], "default");
    assert_eq!(m["recursive"], true);
    let (status, dup) = call(&r, "POST", "/api/monitors", Some(json!({ "path": watch }))).await;
    assert_eq!(
        (status, dup["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("monitor_exists"))
    );
    let (status, bad) = call(
        &r,
        "POST",
        "/api/monitors",
        Some(json!({ "path": dir.path(), "excludes": ["["] })),
    )
    .await;
    assert_eq!(
        (status, bad["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_settings"))
    );

    // The monitor picks the directory up and reports its watch.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (_, list) = call(&r, "GET", "/api/monitors", None).await;
        if list["status"]["dirs"][0]["watching"] == true
            && list["status"]["last_scan_at"].is_number()
        {
            assert_eq!(list["enabled"], true);
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{list}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let (status, _) = call(&r, "POST", "/api/monitors/scan", None).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let mut settings = call(&r, "GET", "/api/settings", None).await.1;
    settings["server"]["monitor_enabled"] = json!(false);
    call(&r, "PUT", "/api/settings", Some(settings)).await;
    let (status, off) = call(&r, "POST", "/api/monitors/scan", None).await;
    assert_eq!(
        (status, off["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("monitor_disabled"))
    );

    let id = m["id"].as_i64().unwrap();
    let (status, _) = call(&r, "DELETE", &format!("/api/monitors/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn manual_task_over_http_pushes_progress() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let s = server(Some(&tools), dir.path()).await;
    let r = s.router();
    let src = dir.path().join("media/a.mkv");
    common::sample(&tools, &src, 3);

    let mut events = subscribe(&r).await;
    expect_event(&mut events, |e, _| e == "ready").await;
    let (status, created) = call(&r, "POST", "/api/tasks", Some(json!({ "path": src }))).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["batch_id"], Value::Null);
    let task = &created["tasks"][0];
    let id = task["id"].as_i64().unwrap();
    assert_eq!(task["type"], "transcode");
    assert_eq!(task["source"], "manual");
    assert_eq!(task["state"], "queued");
    assert_eq!(task["snapshot"]["profile"]["preset"], "ultrafast");

    expect_event(&mut events, |e, d| {
        e == "task_created" && d["task"]["id"] == id
    })
    .await;
    let progress = expect_event(&mut events, |e, d| e == "task_progress" && d["id"] == id).await;
    assert!(progress["out_time_s"].is_number());
    let done = expect_event(&mut events, final_update(id)).await;
    assert_eq!(done["task"]["state"], "published", "{done}");
    assert_eq!(done["task"]["outcome"]["outcome"], "published");

    let (_, again) = call(&r, "POST", "/api/tasks", Some(json!({ "path": src }))).await;
    assert_eq!(again["error"]["code"], "already_processed");
    let (_, list) = call(&r, "GET", "/api/tasks?state=published,failed", None).await;
    assert_eq!(list["tasks"].as_array().unwrap().len(), 1);
    let (status, _) = call(&r, "DELETE", &format!("/api/tasks/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    expect_event(&mut events, |e, d| e == "task_deleted" && d["id"] == id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn folder_batch_observation_and_release() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let s = server(Some(&tools), dir.path()).await;
    let r = s.router();
    let media = dir.path().join("media");
    common::sample(&tools, &media.join("a.mkv"), 2);
    common::sample(&tools, &media.join("b.mkv"), 2);

    let mut settings = call(&r, "GET", "/api/settings", None).await.1;
    settings["server"]["observe_first_n"] = json!(1);
    call(&r, "PUT", "/api/settings", Some(settings)).await;

    let mut events = subscribe(&r).await;
    let (status, created) = call(
        &r,
        "POST",
        "/api/tasks",
        Some(json!({ "source": "folder_batch", "dir": media })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let batch = created["batch_id"].as_i64().unwrap();
    let ids: Vec<i64> = created["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    let mut states = Vec::new();
    for id in &ids {
        let d = expect_event(&mut events, final_update(*id)).await;
        states.push(d["task"]["state"].as_str().unwrap().to_owned());
    }
    // One CPU slot: the older task ran first and was held for observation.
    assert_eq!(states, ["pending", "published"]);
    let (_, held) = call(&r, "GET", &format!("/api/tasks/{}", ids[0]), None).await;
    assert_eq!(held["pending_reason"], "observation");
    assert_eq!(held["batch_id"], batch);

    let (status, released) =
        call(&r, "POST", &format!("/api/tasks/{}/release", ids[0]), None).await;
    assert_eq!(status, StatusCode::OK, "{released}");
    assert_eq!(released["state"], "released");

    let (_, stats) = call(&r, "GET", "/api/verify-stats", None).await;
    assert_eq!(stats["reports"], 2, "{stats}");
    assert_eq!(stats["observed"], 1);
    let (_, recycle) = call(&r, "GET", "/api/recycle", None).await;
    assert_eq!(recycle["dirs"][0]["entries"].as_array().unwrap().len(), 2);
    let (_, batch_tasks) = call(&r, "GET", &format!("/api/tasks?batch_id={batch}"), None).await;
    assert_eq!(batch_tasks["tasks"].as_array().unwrap().len(), 2);

    // Both outputs carry the marker now: a second batch queues nothing.
    let (_, second) = call(
        &r,
        "POST",
        "/api/tasks",
        Some(json!({ "source": "folder_batch", "dir": media })),
    )
    .await;
    assert!(second["tasks"].as_array().unwrap().is_empty());
    assert_eq!(second["skipped"][0]["code"], "already_processed");
}

/// The front end tells a quiet stream from one a proxy holds back by these.
#[tokio::test(flavor = "multi_thread")]
async fn idle_event_streams_ping() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(None, dir.path()).await;
    let mut events = subscribe(&s.router()).await;
    expect_event(&mut events, |e, _| e == "ready").await;
    let (event, _) = tokio::time::timeout(Duration::from_secs(8), events.recv())
        .await
        .expect("an event within 8 s")
        .expect("stream open");
    assert_eq!(event, "ping");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_ends_event_streams() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(None, dir.path()).await;
    let mut events = subscribe(&s.router()).await;
    expect_event(&mut events, |e, _| e == "ready").await;
    s.stop().await;
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        while events.recv().await.is_some() {}
    })
    .await;
    assert!(end.is_ok(), "SSE stream closed on shutdown");
}

#[tokio::test(flavor = "multi_thread")]
async fn browsing_and_batch_preview() {
    let tools = require_tools!();
    let dir = tempfile::tempdir().unwrap();
    let s = server(Some(&tools), dir.path()).await;
    let r = s.router();
    let media = dir.path().join("media");
    common::sample(&tools, &media.join("a.mkv"), 2);
    common::sample(&tools, &media.join("sub/b.mkv"), 2);
    std::fs::write(media.join("notes.txt"), "x").unwrap();
    std::fs::create_dir_all(media.join("@eaDir")).unwrap();

    // Browsing: directories and videos only, with a way up.
    let uri = |p: &std::path::Path| {
        let encoded: String = p
            .to_str()
            .unwrap()
            .bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect();
        format!("/api/fs?path={encoded}")
    };
    let (status, l) = call(&r, "GET", &uri(&media), None).await;
    assert_eq!(status, StatusCode::OK, "{l}");
    assert_eq!(l["dirs"].as_array().unwrap().len(), 1);
    assert_eq!(l["dirs"][0]["name"], "sub");
    assert_eq!(l["files"][0]["name"], "a.mkv");
    assert_eq!(l["files"].as_array().unwrap().len(), 1);
    assert!(l["parent"].is_string());
    let (status, roots) = call(&r, "GET", "/api/fs", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!roots["dirs"].as_array().unwrap().is_empty());
    let (status, e) = call(&r, "GET", &uri(&media.join("a.mkv")), None).await;
    assert_eq!(
        (status, e["error"]["code"].clone()),
        (StatusCode::BAD_REQUEST, json!("bad_path"))
    );
    let (status, e) = call(&r, "GET", &uri(&media.join("missing")), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        e["error"]["message"].as_str().unwrap().contains("不存在"),
        "{e}"
    );

    // A file with a task in flight is reported, nothing gets queued.
    let (status, _) = call(
        &r,
        "POST",
        "/api/tasks",
        Some(json!({ "path": media.join("a.mkv") })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, p) = call(
        &r,
        "POST",
        "/api/tasks/preview-batch",
        Some(json!({ "dir": media })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{p}");
    assert_eq!(p["profile_id"], "default");
    let files = p["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].as_str().unwrap().ends_with("b.mkv"));
    assert_eq!(p["skipped"][0]["code"], "active_task");
    let (_, list) = call(&r, "GET", "/api/tasks", None).await;
    assert_eq!(list["tasks"].as_array().unwrap().len(), 1);

    let (_, flat) = call(
        &r,
        "POST",
        "/api/tasks/preview-batch",
        Some(json!({ "dir": media, "recursive": false })),
    )
    .await;
    assert_eq!(flat["files"].as_array().unwrap().len(), 0);
    let (status, e) = call(
        &r,
        "POST",
        "/api/tasks/preview-batch",
        Some(json!({ "dir": media, "profile_id": "nope" })),
    )
    .await;
    assert_eq!(
        (status, e["error"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("not_found"))
    );
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn system_reports_dovi_tool() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(None, dir.path()).await;
    let r = s.router();
    let set = |path: Option<String>| {
        let r = r.clone();
        async move {
            let mut settings = call(&r, "GET", "/api/settings", None).await.1;
            settings["global"]["dovi_tool"] = json!(path);
            let (status, body) = call(&r, "PUT", "/api/settings", Some(settings)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            call(&r, "GET", "/api/system", None).await.1["dovi"].clone()
        }
    };
    let missing = set(Some("no-such-dovi_tool".into())).await;
    assert_eq!(missing["available"], json!(false), "{missing}");
    assert!(missing["error"].as_str().is_some_and(|e| !e.is_empty()));
    let Some(path) = std::env::var_os("FNCOMPRESS_DOVI_TOOL") else {
        eprintln!("skipped the available case: set FNCOMPRESS_DOVI_TOOL");
        return;
    };
    let found = set(Some(path.to_string_lossy().into_owned())).await;
    assert_eq!(found["available"], json!(true), "{found}");
    assert!(found["version"]
        .as_str()
        .is_some_and(|v| v.starts_with('2')));
}
