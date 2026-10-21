//! The embedded front end next to the API (ADR 0009): `index.html` and
//! hashed assets outside `/api`, the JSON error inside it, and a hint page
//! when the front end was not built. Also checks that `web/openapi.json`,
//! which the front-end types are generated from, matches the server.

use std::{borrow::Cow, collections::HashMap, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use fncompress::{
    server::{ServeOptions, Server},
    web::Assets,
};
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Fake(HashMap<&'static str, &'static str>);

impl Assets for Fake {
    fn get(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        self.0.get(path).map(|s| Cow::Borrowed(s.as_bytes()))
    }
}

async fn server(dir: &std::path::Path) -> Server {
    Server::start(&ServeOptions {
        listen: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.join("data"),
        ffmpeg: None,
        ffprobe: None,
        render_device: None,
        scoring_ffmpeg: None,
        dovi_tool: None,
        bundled_scoring_ffmpeg: None,
        socket: None,
        roots_file: None,
        probe_encoders: false,
    })
    .await
    .unwrap()
}

struct Reply {
    status: StatusCode,
    content_type: String,
    cache: String,
    location: String,
    body: String,
}

async fn get(router: &Router, method: &str, uri: &str) -> Reply {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let res = router.clone().oneshot(req).await.unwrap();
    let header = |name: &str| {
        res.headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    };
    let (status, content_type, cache, location) = (
        res.status(),
        header("content-type"),
        header("cache-control"),
        header("location"),
    );
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        content_type,
        cache,
        location,
        body: String::from_utf8_lossy(&bytes).into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_index_and_assets_beside_the_api() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path()).await;
    let r = s.router_with_assets(Arc::new(Fake(HashMap::from([
        ("index.html", "<!doctype html><title>fncompress</title>"),
        ("assets/index-Ab12.js", "console.log(1)"),
        ("assets/index-Cd34.css", "body{}"),
        ("favicon.svg", "<svg/>"),
    ]))));

    for uri in ["/", "/index.html"] {
        let res = get(&r, "GET", uri).await;
        assert_eq!(res.status, StatusCode::OK, "{uri}");
        assert_eq!(res.content_type, "text/html; charset=utf-8");
        assert_eq!(res.cache, "no-cache");
        assert!(res.body.contains("<title>fncompress</title>"));
    }

    let js = get(&r, "GET", "/assets/index-Ab12.js").await;
    assert_eq!(js.status, StatusCode::OK);
    assert!(
        js.content_type.contains("javascript"),
        "{}",
        js.content_type
    );
    assert!(js.cache.contains("immutable"));
    let css = get(&r, "GET", "/assets/index-Cd34.css").await;
    assert_eq!(css.content_type, "text/css; charset=utf-8");
    let svg = get(&r, "GET", "/favicon.svg").await;
    assert_eq!(svg.content_type, "image/svg+xml");
    assert_eq!(svg.cache, "no-cache");

    // Hash routing: nothing else exists outside /api.
    assert_eq!(
        get(&r, "GET", "/assets/gone.js").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&r, "GET", "/tasks").await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        get(&r, "POST", "/").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(get(&r, "HEAD", "/").await.status, StatusCode::OK);

    // The API is unchanged, unknown API paths keep the JSON error.
    let health = get(&r, "GET", "/api/health").await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.body, r#"{"ok":true}"#);
    for uri in ["/api", "/api/nope", "/api/tasks/1/nope"] {
        let res = get(&r, "GET", uri).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(res.content_type, "application/json");
        let v: serde_json::Value = serde_json::from_str(&res.body).unwrap();
        assert_eq!(v["error"]["code"], "not_found");
    }
    s.stop().await;
}

/// fnOS's unified gateway forwards `/app/fncompress/...` (ADR 0012).
#[tokio::test(flavor = "multi_thread")]
async fn the_gateway_prefix_serves_the_same_app() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path()).await;
    let r = s.router_with_assets(Arc::new(Fake(HashMap::from([
        ("index.html", "<!doctype html><title>fncompress</title>"),
        ("assets/index-Ab12.js", "console.log(1)"),
    ]))));

    // Relative URLs need the trailing slash.
    for (uri, to) in [
        ("/app/fncompress", "/app/fncompress/"),
        ("/app/fncompress?a=1", "/app/fncompress/?a=1"),
    ] {
        let res = get(&r, "GET", uri).await;
        assert_eq!(res.status, StatusCode::PERMANENT_REDIRECT, "{uri}");
        assert_eq!(res.location, to);
    }
    let index = get(&r, "GET", "/app/fncompress/").await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("<title>fncompress</title>"));
    let js = get(&r, "GET", "/app/fncompress/assets/index-Ab12.js").await;
    assert_eq!(js.body, "console.log(1)");
    let health = get(&r, "GET", "/app/fncompress/api/health").await;
    assert_eq!(health.body, r#"{"ok":true}"#);
    let tasks = get(&r, "GET", "/app/fncompress/api/tasks?limit=1").await;
    assert_eq!(tasks.status, StatusCode::OK);
    let missing = get(&r, "GET", "/app/fncompress/api/nope").await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.content_type, "application/json");
    // Only the exact prefix is taken off.
    assert_eq!(
        get(&r, "GET", "/app/fncompressx/").await.status,
        StatusCode::NOT_FOUND
    );
    s.stop().await;
}

/// The gateway checks the NAS login only; the socket admits administrators
/// by the `X-Trim-Isadmin` header it forwards (ADR 0012).
#[tokio::test(flavor = "multi_thread")]
async fn the_gateway_socket_admits_only_administrators() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path()).await;
    let r = fncompress::api::admin_only(s.router_with_assets(Arc::new(Fake(HashMap::from([(
        "index.html",
        "<!doctype html><title>fncompress</title>",
    )])))));
    let send = |uri: &str, admin: Option<&str>| {
        let mut req = Request::builder().uri(uri);
        if let Some(v) = admin {
            req = req.header("X-Trim-Isadmin", v);
        }
        r.clone().oneshot(req.body(Body::empty()).unwrap())
    };

    for uri in [
        "/app/fncompress",
        "/app/fncompress/",
        "/app/fncompress/api/tasks",
    ] {
        for admin in [None, Some("false"), Some("")] {
            let res = send(uri, admin).await.unwrap();
            assert_eq!(res.status(), StatusCode::FORBIDDEN, "{uri} {admin:?}");
        }
    }
    let api = send("/app/fncompress/api/tasks", None).await.unwrap();
    let body = api.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "forbidden");
    let page = send("/app/fncompress/", Some("false")).await.unwrap();
    let body = page.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("管理员"));

    for admin in ["true", "True"] {
        let index = send("/app/fncompress/", Some(admin)).await.unwrap();
        assert_eq!(index.status(), StatusCode::OK);
        let tasks = send("/app/fncompress/api/tasks?limit=1", Some(admin))
            .await
            .unwrap();
        assert_eq!(tasks.status(), StatusCode::OK);
    }
    s.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_build_the_root_explains_and_the_api_works() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path()).await;
    let r = s.router_with_assets(Arc::new(Fake(HashMap::new())));

    let root = get(&r, "GET", "/").await;
    assert_eq!(root.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(root.content_type, "text/html; charset=utf-8");
    assert!(root.body.contains("前端没有构建"));
    assert!(root.body.contains("npm run build"));
    assert_eq!(
        get(&r, "GET", "/assets/x.js").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&r, "GET", "/api/health").await.status, StatusCode::OK);
    s.stop().await;
}

/// The real router uses `web/dist` (read from disk in debug builds).
#[tokio::test(flavor = "multi_thread")]
async fn the_server_serves_web_dist() {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path()).await;
    let root = get(&s.router(), "GET", "/").await;
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/dist/index.html");
    match std::fs::read_to_string(&dist) {
        Ok(index) => {
            assert_eq!(root.status, StatusCode::OK);
            assert_eq!(root.body, index);
        }
        Err(_) => assert_eq!(root.status, StatusCode::SERVICE_UNAVAILABLE),
    }
    s.stop().await;
}

/// `web/openapi.json` is what the front-end types come from; it must match
/// the server. `UPDATE_OPENAPI=1 cargo test -p fncompress --test web`
/// rewrites it.
#[test]
fn web_openapi_json_is_current() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/openapi.json");
    let current = fncompress::api::openapi().to_pretty_json().unwrap() + "\n";
    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(&path, &current).unwrap();
        return;
    }
    // Compiled in, so the check also runs from the NAS test binaries.
    let committed = include_str!("../../../web/openapi.json");
    let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).ok();
    assert!(
        parse(committed).is_some() && parse(committed) == parse(&current),
        "web/openapi.json is out of date; run `UPDATE_OPENAPI=1 cargo test -p fncompress --test web` \
         (PowerShell: $env:UPDATE_OPENAPI=1), then `cd web; npm run build`"
    );
}
