//! The page's buttons and menus, and what happens when the page becomes visible again.

use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlElement, HtmlSelectElement};

use crate::camera::{recover_camera, reopen_camera, update_mirror};
use crate::jpeg::websocket;
use crate::js::{add_listener, set_timeout};
use crate::page::PageRef;
use crate::startup;
use crate::state::{Transport, VideoSize};
use crate::wake_lock::request_wake_lock;
use crate::webrtc;

/// iOS un-mutes the camera track shortly after the page returns; check it after this.
const CAMERA_RECHECK_AFTER_SHOWN_MS: i32 = 1000;

pub fn attach_controls(page: &PageRef) {
    on(page, "start", "click", start_clicked);
    on(page, "top", "click", take_back_stream);
    on(page, "flip", "click", flip_camera);
    on(page, "quality", "change", quality_changed);
    on(page, "mirror", "click", toggle_mirror);
    on(page, "dimbtn", "click", dim_screen);
    on(page, "dim", "click", undim_screen);
}

pub fn watch_visibility(page: &PageRef) {
    let page_for_handler = page.clone();
    let _ = add_listener(&page.document, "visibilitychange", move || page_shown(&page_for_handler));
}

/// Call `handler` whenever the element `element_id` fires `event`.
fn on(page: &PageRef, element_id: &str, event: &str, handler: fn(&PageRef)) {
    let element: HtmlElement = page.element(element_id);
    let page = page.clone();
    let _ = add_listener(&element, event, move || handler(&page));
}

fn start_clicked(page: &PageRef) {
    spawn_local(startup::start(page.clone()));
}

/// Tapping the status bar after another device took over takes the stream back.
fn take_back_stream(page: &PageRef) {
    let was_replaced = std::mem::replace(&mut page.state.borrow_mut().is_replaced, false);
    if was_replaced {
        reconnect(page);
    }
}

fn reconnect(page: &PageRef) {
    let transport = page.state.borrow().transport;
    match transport {
        Transport::WebRtc => webrtc::connect(page),
        Transport::WebSocket => websocket::connect(page),
        Transport::Http => {}
    }
}

fn flip_camera(page: &PageRef) {
    {
        let mut state = page.state.borrow_mut();
        state.camera.facing = state.camera.facing.flipped();
    }
    reopen_camera(page);
}

fn quality_changed(page: &PageRef) {
    let value = page.element::<HtmlSelectElement>("quality").value();
    page.state.borrow_mut().camera.requested_size = VideoSize::from_picker_value(&value);
    reopen_camera(page);
}

fn toggle_mirror(page: &PageRef) {
    {
        let mut state = page.state.borrow_mut();
        state.camera.is_mirror_on = !state.camera.is_mirror_on;
    }
    update_mirror(page);
}

fn dim_screen(page: &PageRef) {
    let _ = page.element::<HtmlElement>("dim").class_list().add_1("on");
}

fn undim_screen(page: &PageRef) {
    let _ = page.element::<HtmlElement>("dim").class_list().remove_1("on");
}

/// The wake lock is released whenever the page is hidden, and iOS may have stopped the
/// camera meanwhile; only reopen the camera if it is still dead after a moment.
fn page_shown(page: &PageRef) {
    if page.is_hidden() || !page.state.borrow().is_running {
        return;
    }
    request_wake_lock(page);
    let page_for_timer = page.clone();
    set_timeout(&page.window, CAMERA_RECHECK_AFTER_SHOWN_MS, move || recover_camera(&page_for_timer));
}
