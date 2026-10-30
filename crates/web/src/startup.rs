//! What the Start button does: check the page can stream, keep the screen awake,
//! open the camera, then connect to the PC.

use wasm_bindgen::prelude::*;
use web_sys::HtmlElement;

use crate::camera::open_camera;
use crate::jpeg::capture::capture_loop;
use crate::js::{error_message, get_property};
use crate::page::PageRef;
use crate::wake_lock::request_wake_lock;
use crate::webrtc;

const NEEDS_HTTPS_MESSAGE: &str =
    "Camera access needs a secure (https://) connection. Open the https:// address shown on the PC.";
const NEEDS_PAIRING_KEY_MESSAGE: &str =
    "Open this page by scanning the QR code shown on the PC (the link contains a pairing key).";
const CAMERA_DENIED_MESSAGE: &str =
    "Camera permission was denied. Allow camera access for this site in Settings › Safari › Camera, then reload.";

pub async fn start(page: PageRef) {
    {
        let mut state = page.state.borrow_mut();
        // A double tap must not start twice.
        if state.is_starting || state.is_running {
            return;
        }
        state.is_starting = true;
    }
    try_start(&page).await;
    page.state.borrow_mut().is_starting = false;
}

async fn try_start(page: &PageRef) {
    page.element::<HtmlElement>("error").set_text_content(Some(""));
    if let Some(problem) = missing_requirement(page) {
        page.show_error(problem);
        return;
    }
    // Must start inside the tap handler on iOS.
    request_wake_lock(page);
    if let Err(error) = open_camera(page).await {
        page.show_error(&camera_error_message(&error));
        return;
    }
    show_streaming_controls(page);
    page.state.borrow_mut().is_running = true;
    webrtc::connect(page);
    // Idles unless a JPEG fallback transport is active.
    capture_loop(page);
}

/// Why this page can't stream at all, if it can't.
fn missing_requirement(page: &PageRef) -> Option<&'static str> {
    if !page.window.is_secure_context() || page.window.navigator().media_devices().is_err() {
        return Some(NEEDS_HTTPS_MESSAGE);
    }
    if page.pairing_key.is_empty() {
        return Some(NEEDS_PAIRING_KEY_MESSAGE);
    }
    None
}

fn camera_error_message(error: &JsValue) -> String {
    let name = get_property(error, "name").as_string().unwrap_or_default();
    if name == "NotAllowedError" {
        return CAMERA_DENIED_MESSAGE.to_string();
    }
    format!("Could not open the camera: {}", error_message(error))
}

fn show_streaming_controls(page: &PageRef) {
    let _ = page.element::<HtmlElement>("start-screen").class_list().add_1("hidden");
    for id in ["top", "bottom"] {
        let _ = page.element::<HtmlElement>(id).class_list().remove_1("hidden");
    }
}
