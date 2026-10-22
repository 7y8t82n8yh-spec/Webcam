//! The phone page, compiled into the executable. `cargo xtask web` builds it into
//! `crates/app/web-dist/` (the Rust/WebAssembly code in `crates/web` plus its HTML).

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

macro_rules! asset {
    ($path:literal, $mime:literal) => {
        ($path, $mime, include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/web-dist/", $path)) as &[u8])
    };
}

static ASSETS: &[(&str, &str, &[u8])] = &[
    asset!("index.html", "text/html; charset=utf-8"),
    asset!("pkg/web.js", "text/javascript; charset=utf-8"),
    asset!("pkg/web_bg.wasm", "application/wasm"),
];

pub fn serve(path: &str) -> Response {
    match ASSETS.iter().find(|(p, _, _)| *p == path) {
        Some((_, mime, body)) => {
            ([(header::CONTENT_TYPE, *mime), (header::CACHE_CONTROL, "no-store")], *body).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
