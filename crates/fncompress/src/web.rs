//! The Vue front end (ADR 0002, 0009): `web/dist` embedded with rust-embed
//! and served next to `/api` on the same port. Routing is by hash, so only
//! `index.html` and the files it references are ever requested.
//!
//! Release builds carry the files in the binary; debug builds read
//! `web/dist` from disk at run time. When the front end was not built, `/`
//! answers with a short page saying so and the API keeps working.

use std::borrow::Cow;

use axum::{
    http::{header, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};

/// Where the front-end files come from; tests substitute their own.
pub trait Assets: Send + Sync + 'static {
    /// `path` is relative to the build root, e.g. `assets/index-abc.js`.
    fn get(&self, path: &str) -> Option<Cow<'static, [u8]>>;
}

#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist"]
#[allow_missing = true]
struct Dist;

/// The embedded `web/dist`.
pub struct Embedded;

impl Assets for Embedded {
    fn get(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        Dist::get(path).map(|f| f.data)
    }
}

/// Vite puts content-hashed files here; they never change under one name.
const HASHED_DIR: &str = "assets/";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

const NOT_BUILT: &str = r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>fncompress</title></head>
<body style="font-family: system-ui, sans-serif; max-width: 40em; margin: 3em auto; padding: 0 1em; line-height: 1.6">
<h2>fncompress：前端没有构建</h2>
<p>这个二进制编译时 <code>web/dist</code> 不存在，所以只有 API 可用（<a href="api/openapi.json">/api/openapi.json</a>）。</p>
<p>先在仓库里运行 <code>cd web; npm run build</code>，再重新编译 fncompress。</p>
</body></html>
"#;

/// Answers every request outside `/api`.
pub fn respond(assets: &dyn Assets, method: &Method, uri: &Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match assets.get(path) {
        Some(body) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let cache = if path.starts_with(HASHED_DIR) {
                IMMUTABLE
            } else {
                "no-cache"
            };
            (
                [
                    (
                        header::CONTENT_TYPE,
                        mime.essence_str().to_owned() + charset(&mime),
                    ),
                    (header::CACHE_CONTROL, cache.to_owned()),
                ],
                body,
            )
                .into_response()
        }
        None if path == "index.html" => (
            StatusCode::SERVICE_UNAVAILABLE,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            NOT_BUILT,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn charset(mime: &mime_guess::Mime) -> &'static str {
    let text = mime.type_() == mime_guess::mime::TEXT
        || mime.subtype() == mime_guess::mime::JAVASCRIPT
        || mime.subtype() == mime_guess::mime::JSON;
    if text {
        "; charset=utf-8"
    } else {
        ""
    }
}
