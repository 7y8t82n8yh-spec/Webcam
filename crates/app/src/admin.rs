//! The admin page: the PC's own view of the app (QR code, status, preview, rotation),
//! opened in the PC's browser. Served over plain HTTP on 127.0.0.1 only, so only this
//! PC can reach it, and browsers treat it as a secure page without a certificate.

use std::sync::{Arc, Mutex};

use axum::extract::{Query, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use protocol::{AdminState, RotateRequest};
use serde::Deserialize;

use crate::hub::{Frame, FrameHub};
use crate::web_assets;

const PREVIEW_WIDTH: u32 = 480;

pub struct Admin {
    pub hub: Arc<FrameHub>,
    pub urls: Vec<String>,
    /// What the virtual camera is doing.
    pub camera_status: Arc<Mutex<String>>,
    pub fps: u32,
    /// The port the page is served on.
    pub port: u16,
}

impl Admin {
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/", get(|| async { web_assets::serve("admin.html") }))
            .route("/pkg/{file}", get(pkg_file))
            .route("/api/state", get(state))
            .route("/api/rotate", post(rotate))
            .route("/qr.svg", get(qr_code))
            .route("/preview.jpg", get(preview))
            .layer(middleware::from_fn_with_state(self.clone(), only_this_page))
            .with_state(self)
    }

    pub fn state(&self) -> AdminState {
        let stats = self.hub.stats();
        let (output_width, output_height) = self.hub.output_size();
        AdminState {
            urls: self.urls.clone(),
            streaming: self.hub.streaming(),
            transport: stats.transport.to_string(),
            source_width: stats.source_size.0,
            source_height: stats.source_size.1,
            frames_in: stats.frames_in,
            camera: self.camera_status.lock().unwrap().clone(),
            output_width,
            output_height,
            fps: self.fps,
            rotation: self.hub.rotation().degrees(),
        }
    }
}

/// Refuse requests that may come from somewhere other than the admin page itself:
/// * a Host other than our own address: a website that pointed its name at 127.0.0.1
///   (DNS rebinding) to read the page, and with it the pairing key;
/// * a change without the admin header: a form another website submits to us.
async fn only_this_page(State(admin): State<Arc<Admin>>, request: Request, next: Next) -> Response {
    let host = request.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let ours = [format!("127.0.0.1:{}", admin.port), format!("localhost:{}", admin.port)];
    if !ours.iter().any(|ours| ours == host) {
        return (StatusCode::FORBIDDEN, "open the admin page at http://127.0.0.1").into_response();
    }
    if request.method() != Method::GET && !request.headers().contains_key(protocol::ADMIN_HEADER) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn pkg_file(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    web_assets::serve(&format!("pkg/{file}"))
}

async fn state(State(admin): State<Arc<Admin>>) -> Json<AdminState> {
    Json(admin.state())
}

async fn rotate(State(admin): State<Arc<Admin>>, Json(request): Json<RotateRequest>) -> Json<AdminState> {
    let hub = &admin.hub;
    hub.set_rotation(hub.rotation().turned(request.quarters));
    Json(admin.state())
}

#[derive(Deserialize)]
struct QrQuery {
    /// Which of the URLs.
    #[serde(default)]
    i: usize,
}

async fn qr_code(State(admin): State<Arc<Admin>>, Query(query): Query<QrQuery>) -> Response {
    let Some(url) = admin.urls.get(query.i) else { return StatusCode::NOT_FOUND.into_response() };
    let Ok(code) = qrcode::QrCode::new(url.as_bytes()) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let svg = code.render::<qrcode::render::svg::Color>().min_dimensions(240, 240).build();
    ([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "no-store")], svg).into_response()
}

/// The virtual camera's current picture, small.
async fn preview(State(admin): State<Arc<Admin>>) -> Response {
    let frame = admin.hub.latest();
    let jpeg = tokio::task::spawn_blocking(move || encode_preview(&frame)).await;
    match jpeg {
        Ok(Ok(jpeg)) => {
            ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "no-store")], jpeg).into_response()
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn encode_preview(frame: &Frame) -> Result<Vec<u8>, jpeg_encoder::EncodingError> {
    let small = shrink(frame, PREVIEW_WIDTH);
    let mut jpeg = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut jpeg, 75);
    encoder.encode(&small.rgb, small.width as u16, small.height as u16, jpeg_encoder::ColorType::Rgb)?;
    Ok(jpeg)
}

/// `frame` scaled down to `width` (nearest neighbour; it's small).
fn shrink(frame: &Frame, width: u32) -> Frame {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let pw = (width as usize).min(w);
    let ph = (pw * h / w.max(1)).max(1);
    let mut rgb = Vec::with_capacity(pw * ph * 3);
    for y in 0..ph {
        let row = (y * h / ph) * w;
        for x in 0..pw {
            let i = (row + x * w / pw) * 3;
            rgb.extend_from_slice(&frame.rgb[i..i + 3]);
        }
    }
    Frame { width: pw as u32, height: ph as u32, rgb }
}
