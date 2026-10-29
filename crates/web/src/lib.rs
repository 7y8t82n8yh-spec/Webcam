//! The phone page, and the PC's admin page (`admin`), compiled to WebAssembly.
//!
//! The camera is sent to the PC over WebRTC: a real video stream (H.264, which the
//! iPhone encodes in hardware) with congestion control. If WebRTC can't connect -
//! typically UDP blocked by a firewall - the page falls back to JPEG frames drawn from
//! a canvas and sent over a WebSocket, or HTTPS POSTs if even that is refused (see
//! `jpeg`).

mod admin;
mod camera;
mod controls;
mod fetch;
mod identity;
mod jpeg;
mod js;
mod nosleep;
mod page;
mod server_messages;
mod startup;
mod state;
mod stats;
mod wake_lock;
mod webrtc;

use wasm_bindgen::prelude::*;

use crate::page::Page;

/// Run the admin page (`admin.html`).
#[wasm_bindgen]
pub fn admin_page() {
    admin::start(web_sys::window().expect("window"));
}

/// Run the phone page (`index.html`).
#[wasm_bindgen]
pub fn phone_page() {
    let window = web_sys::window().expect("window");
    let page = Page::new(window);
    controls::attach_controls(&page);
    controls::watch_visibility(&page);
    stats::start_stats(&page);
    // Handlers are attached: tell automated tests the page is usable.
    if let Some(root) = page.document.document_element() {
        let _ = root.set_attribute("data-ready", "");
    }
}
