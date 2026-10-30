//! The first JPEG transport: frames as binary WebSocket messages, acknowledged one by
//! one, with the PC's messages on the same socket. Gives way to HTTP POSTs if the
//! socket never opens.

use protocol::ClientMessage;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, Blob, MessageEvent, WebSocket};

use super::{frame_acknowledged, http};
use crate::camera::client_info;
use crate::js::{into_js_function, set_timeout, TimerHandle};
use crate::page::{Page, PageRef, StatusKind};
use crate::server_messages::handle_server_message;

const SOCKET_OPEN_TIMEOUT_MS: i32 = 5000;
const SOCKET_RECONNECT_DELAY_MS: i32 = 1000;
/// Switch to HTTP POSTs after this many failed attempts, if the socket never opened.
const FAILED_ATTEMPTS_BEFORE_HTTP: u32 = 2;

pub fn connect(page: &PageRef) {
    if page.state.borrow().is_replaced {
        return;
    }
    let url = socket_url(page);
    let Ok(socket) = WebSocket::new(&url) else { return };
    socket.set_binary_type(BinaryType::Arraybuffer);
    page.state.borrow_mut().socket.current = Some(socket.clone());
    page.set_status("Connecting…", StatusKind::Neutral);
    let open_timer = close_unless_open_in_time(page, &socket);
    attach_socket_handlers(page, &socket, open_timer);
}

/// Tell the PC about this phone and camera, if the socket is open.
pub fn send_hello(page: &PageRef) {
    let socket = {
        let state = page.state.borrow();
        if !state.socket.is_open {
            return;
        }
        state.socket.current.clone()
    };
    if let Some(socket) = socket {
        let hello = serde_json::to_string(&ClientMessage::Hello(client_info(page))).unwrap();
        let _ = socket.send_with_str(&hello);
    }
}

pub fn send_frame(page: &PageRef, blob: &Blob) {
    let socket = page.state.borrow().socket.current.clone();
    if let Some(socket) = socket {
        let _ = socket.send_with_blob(blob);
    }
}

fn socket_url(page: &Page) -> String {
    let location = page.window.location();
    let is_https = location.protocol().unwrap_or_default() == "https:";
    let scheme = if is_https { "wss:" } else { "ws:" };
    let host = location.host().unwrap_or_default();
    let key = String::from(js_sys::encode_uri_component(&page.pairing_key));
    format!("{scheme}//{host}/ws?id={}&k={key}", page.client_id)
}

fn close_unless_open_in_time(page: &PageRef, socket: &WebSocket) -> TimerHandle {
    let socket = socket.clone();
    set_timeout(&page.window, SOCKET_OPEN_TIMEOUT_MS, move || {
        if socket.ready_state() != WebSocket::OPEN {
            let _ = socket.close();
        }
    })
}

/// Every handler ignores sockets that have since been superseded by a newer one,
/// otherwise a stale socket's close would tear down the live connection's state.
fn attach_socket_handlers(page: &PageRef, socket: &WebSocket, open_timer: TimerHandle) {
    let on_open = {
        let (page, socket) = (page.clone(), socket.clone());
        into_js_function(Closure::<dyn FnMut()>::new(move || socket_opened(&page, &socket, open_timer)))
    };
    let on_message = {
        let (page, socket) = (page.clone(), socket.clone());
        let handler = move |event: MessageEvent| socket_message_received(&page, &socket, &event);
        into_js_function(Closure::<dyn FnMut(MessageEvent)>::new(handler))
    };
    let on_close = {
        let (page, socket) = (page.clone(), socket.clone());
        into_js_function(Closure::<dyn FnMut()>::new(move || socket_closed(&page, &socket, open_timer)))
    };
    socket.set_onopen(Some(&on_open));
    socket.set_onmessage(Some(&on_message));
    socket.set_onclose(Some(&on_close));
}

fn is_current_socket(page: &Page, socket: &WebSocket) -> bool {
    page.state.borrow().socket.current.as_ref() == Some(socket)
}

fn socket_opened(page: &PageRef, socket: &WebSocket, open_timer: TimerHandle) {
    page.window.clear_timeout_with_handle(open_timer);
    if !is_current_socket(page, socket) {
        let _ = socket.close();
        return;
    }
    {
        let mut state = page.state.borrow_mut();
        state.socket.is_open = true;
        state.socket.has_ever_opened = true;
        state.socket.failed_attempts = 0;
        state.frames_in_flight = 0;
    }
    page.set_status("Streaming", StatusKind::Good);
    send_hello(page);
}

fn socket_message_received(page: &PageRef, socket: &WebSocket, event: &MessageEvent) {
    if !is_current_socket(page, socket) {
        return;
    }
    let Some(text) = event.data().as_string() else { return };
    if text == protocol::ACK || text == protocol::BAD {
        frame_acknowledged(page);
    } else {
        handle_server_message(page, &text);
    }
}

fn socket_closed(page: &PageRef, socket: &WebSocket, open_timer: TimerHandle) {
    page.window.clear_timeout_with_handle(open_timer);
    if !is_current_socket(page, socket) {
        return;
    }
    let should_switch_to_http = {
        let mut state = page.state.borrow_mut();
        if !state.socket.is_open {
            state.socket.failed_attempts += 1;
        }
        state.socket.is_open = false;
        state.frames_in_flight = 0;
        if state.is_replaced {
            return;
        }
        // Some iOS versions refuse wss:// to a self-signed certificate even after
        // the user accepted it for the page. Plain HTTPS POSTs do work.
        !state.socket.has_ever_opened && state.socket.failed_attempts >= FAILED_ATTEMPTS_BEFORE_HTTP
    };
    if should_switch_to_http {
        http::start(page);
        return;
    }
    page.set_status("Reconnecting…", StatusKind::Bad);
    let page_for_retry = page.clone();
    set_timeout(&page.window, SOCKET_RECONNECT_DELAY_MS, move || connect(&page_for_retry));
}
