//! The admin page (`admin.html`), opened in the PC's browser: the address and QR code
//! for the phone, the virtual camera's picture and status, and rotation.

use std::cell::Cell;
use std::rc::Rc;

use protocol::{AdminState, RotateRequest};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{Document, Headers, HtmlElement, HtmlImageElement, HtmlOptionElement, HtmlSelectElement, RequestInit};
use web_sys::{Response, Window};

use crate::js::{self, element_by_id};

const POLL_MS: i32 = 1000;
/// Pause between preview pictures.
const PREVIEW_PAUSE_MS: i32 = 100;

struct Admin {
    window: Window,
    urls: HtmlSelectElement,
    qr: HtmlImageElement,
    preview: HtmlImageElement,
    status: HtmlElement,
    camera: HtmlElement,
    rotation: HtmlElement,
    copied: HtmlElement,
    /// The phone's frame count at the last poll and when (ms), for the frame rate.
    last_count: Cell<Option<(u64, f64)>>,
}

type AdminRef = Rc<Admin>;

pub fn start(window: Window) {
    let document = window.document().expect("document");
    let admin = Rc::new(Admin {
        urls: element_by_id(&document, "urls"),
        qr: element_by_id(&document, "qr"),
        preview: element_by_id(&document, "preview"),
        status: element_by_id(&document, "status"),
        camera: element_by_id(&document, "camera"),
        rotation: element_by_id(&document, "rotation"),
        copied: element_by_id(&document, "copied"),
        last_count: Cell::new(None),
        window,
    });
    on(&admin, &document, "urls", "change", |admin| show_qr_code(admin));
    on(&admin, &document, "copy", "click", copy_url);
    on(&admin, &document, "rotate-left", "click", |admin| rotate(admin, -1));
    on(&admin, &document, "rotate-right", "click", |admin| rotate(admin, 1));
    // One preview picture at a time: the next is asked for once this one is shown.
    let next = |delay| move |admin: &AdminRef| next_preview(admin, delay);
    let _ = js::add_listener(&admin.preview, "load", with(&admin, next(PREVIEW_PAUSE_MS)));
    let _ = js::add_listener(&admin.preview, "error", with(&admin, next(POLL_MS)));
    next_preview(&admin, 0);
    poll(admin);
    if let Some(root) = document.document_element() {
        let _ = root.set_attribute("data-ready", "");
    }
}

/// `handler`, called with the page.
fn with(admin: &AdminRef, handler: impl Fn(&AdminRef) + 'static) -> impl FnMut() + 'static {
    let admin = admin.clone();
    move || handler(&admin)
}

fn on(admin: &AdminRef, document: &Document, id: &str, event: &str, handler: impl Fn(&AdminRef) + 'static) {
    let element: HtmlElement = element_by_id(document, id);
    let _ = js::add_listener(&element, event, with(admin, handler));
}

/// Ask the app for its state every second.
fn poll(admin: AdminRef) {
    spawn_local(async move {
        match request_state(&admin.window, "/api/state", None).await {
            Ok(state) => show(&admin, &state),
            Err(_) => admin.status.set_text_content(Some("○ The app is not running")),
        }
        let window = admin.window.clone();
        js::set_timeout(&window, POLL_MS, move || poll(admin));
    });
}

fn show(admin: &Admin, state: &AdminState) {
    if admin.urls.length() as usize != state.urls.len() {
        list_urls(admin, &state.urls);
    }
    let status = if state.streaming {
        let fps = frame_rate(admin, state.frames_in);
        let (w, h) = (state.source_width, state.source_height);
        format!("● Streaming from iPhone ({}) — {w}×{h}, {fps:.0} fps", state.transport)
    } else {
        admin.last_count.set(None);
        "○ Waiting for iPhone…".to_string()
    };
    admin.status.set_text_content(Some(&status));
    admin.status.set_class_name(if state.streaming { "ok" } else { "" });
    let camera =
        format!("Virtual camera: {}, {}×{} @ {} fps", state.camera, state.output_width, state.output_height, state.fps);
    admin.camera.set_text_content(Some(&camera));
    admin.rotation.set_text_content(Some(&format!("{}°", state.rotation)));
}

fn list_urls(admin: &Admin, urls: &[String]) {
    admin.urls.set_inner_html("");
    for url in urls {
        if let Ok(option) = HtmlOptionElement::new_with_text_and_value(url, url) {
            let _ = admin.urls.add_with_html_option_element(&option);
        }
    }
    show_qr_code(admin);
}

/// Frames per second since the last poll.
fn frame_rate(admin: &Admin, frames_in: u64) -> f64 {
    let now = js::now_ms();
    let fps = match admin.last_count.get() {
        Some((count, at)) if now > at => frames_in.saturating_sub(count) as f64 * 1000.0 / (now - at),
        _ => 0.0,
    };
    admin.last_count.set(Some((frames_in, now)));
    fps
}

fn show_qr_code(admin: &Admin) {
    let index = admin.urls.selected_index().max(0);
    admin.qr.set_src(&format!("/qr.svg?i={index}"));
}

fn next_preview(admin: &AdminRef, delay_ms: i32) {
    let admin = admin.clone();
    let window = admin.window.clone();
    js::set_timeout(&window, delay_ms, move || {
        admin.preview.set_src(&format!("/preview.jpg?t={}", js::now_ms() as u64));
    });
}

fn copy_url(admin: &AdminRef) {
    let admin = admin.clone();
    spawn_local(async move {
        let url = admin.urls.value();
        let clipboard = js::get_property(&admin.window.navigator(), "clipboard");
        let copied = match js::call_method(&clipboard, "writeText", &[url.into()]) {
            Ok(promise) => JsFuture::from(js_sys::Promise::from(promise)).await.is_ok(),
            Err(_) => false,
        };
        let message = if copied { "Copied" } else { "Couldn't copy: select the address and press Ctrl+C" };
        admin.copied.set_text_content(Some(message));
        let copied = admin.copied.clone();
        js::set_timeout(&admin.window, 3000, move || copied.set_text_content(None));
    });
}

fn rotate(admin: &AdminRef, quarters: i8) {
    let admin = admin.clone();
    spawn_local(async move {
        let body = serde_json::to_string(&RotateRequest { quarters }).unwrap_or_default();
        if let Ok(state) = request_state(&admin.window, "/api/rotate", Some(body)).await {
            show(&admin, &state);
        }
    });
}

/// GET `path`, or POST `body` to it; both answer with the app's state.
async fn request_state(window: &Window, path: &str, body: Option<String>) -> Result<AdminState, JsValue> {
    let init = RequestInit::new();
    if let Some(body) = body {
        let headers = Headers::new()?;
        headers.set("Content-Type", "application/json")?;
        headers.set(protocol::ADMIN_HEADER, "1")?;
        init.set_method("POST");
        init.set_headers(&headers);
        init.set_body(&body.into());
    }
    let response: Response = JsFuture::from(window.fetch_with_str_and_init(path, &init)).await?.unchecked_into();
    if !response.ok() {
        return Err(format!("HTTP {}", response.status()).into());
    }
    let text = JsFuture::from(response.text()?).await?.as_string().unwrap_or_default();
    serde_json::from_str(&text).map_err(|e| e.to_string().into())
}
