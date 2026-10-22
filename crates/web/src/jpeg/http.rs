//! The last-resort transport: each JPEG frame as its own HTTPS POST to `/frame`, for
//! iOS versions that refuse a WebSocket to the PC's self-signed certificate.

use protocol::StreamConfig;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{AbortController, Blob, Headers, RequestInit, Response};

use super::frame_acknowledged;
use crate::fetch::{response_text, HTTP_FORBIDDEN};
use crate::js::set_timeout;
use crate::page::{PageRef, StatusKind, PAIRING_KEY_REJECTED};
use crate::server_messages::apply_config;
use crate::state::Transport;

/// Never let a hung request block sending.
const FRAME_POST_TIMEOUT_MS: i32 = 5000;
const STREAMING_OVER_HTTP: &str = "Streaming (HTTP)";

/// Switch to HTTP POSTs. Without a socket, the config has to be fetched once instead.
pub fn start(page: &PageRef) {
    page.state.borrow_mut().transport = Transport::Http;
    spawn_local(fetch_and_apply_config(page.clone()));
}

pub fn send_frame(page: &PageRef, blob: Blob) {
    spawn_local(post_frame_and_report(page.clone(), blob));
}

async fn fetch_and_apply_config(page: PageRef) {
    if let Ok(config) = fetch_config(&page).await {
        apply_config(&page, config);
    }
    page.set_status(STREAMING_OVER_HTTP, StatusKind::Good);
}

async fn fetch_config(page: &PageRef) -> Result<StreamConfig, JsValue> {
    let request = page.window.fetch_with_str("/config");
    let response: Response = JsFuture::from(request).await?.unchecked_into();
    let text = response_text(&response).await?;
    serde_json::from_str(&text).map_err(|error| error.to_string().into())
}

async fn post_frame_and_report(page: PageRef, blob: Blob) {
    match post_frame(&page, &blob).await {
        Ok(HTTP_FORBIDDEN) => page.set_status(PAIRING_KEY_REJECTED, StatusKind::Bad),
        Ok(_) => page.set_status(STREAMING_OVER_HTTP, StatusKind::Good),
        Err(_) => page.set_status("PC unreachable", StatusKind::Bad),
    }
    frame_acknowledged(&page);
}

/// POST one JPEG frame; returns the HTTP status.
async fn post_frame(page: &PageRef, blob: &Blob) -> Result<u16, JsValue> {
    let headers = Headers::new()?;
    headers.set("Content-Type", "image/jpeg")?;
    headers.set(protocol::CLIENT_ID_HEADER, &page.client_id)?;
    headers.set(protocol::KEY_HEADER, &page.pairing_key)?;
    let abort = AbortController::new()?;
    let abort_on_timeout = abort.clone();
    let timer = set_timeout(&page.window, FRAME_POST_TIMEOUT_MS, move || abort_on_timeout.abort());
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_headers(&headers);
    init.set_body(blob);
    init.set_signal(Some(&abort.signal()));
    let request = page.window.fetch_with_str_and_init("/frame", &init);
    let response = JsFuture::from(request).await;
    page.window.clear_timeout_with_handle(timer);
    Ok(response?.unchecked_into::<Response>().status())
}
