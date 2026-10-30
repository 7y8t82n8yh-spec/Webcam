//! The fallback for networks where WebRTC can't connect: JPEG frames drawn from a
//! canvas, sent over a WebSocket, or as HTTPS POSTs if even that is refused. At most
//! `MAX_FRAMES_IN_FLIGHT` frames are unacknowledged at a time, so a slow network
//! drops frames instead of building up latency.

pub mod capture;
mod http;
pub mod websocket;

use web_sys::Blob;

use crate::js::log;
use crate::page::PageRef;
use crate::state::{State, Transport};

pub const MAX_FRAMES_IN_FLIGHT: u32 = 2;

/// Switch from WebRTC to JPEG frames, starting with the WebSocket.
pub fn start_fallback(page: &PageRef) {
    log("falling back to JPEG frames");
    page.state.borrow_mut().transport = Transport::WebSocket;
    websocket::connect(page);
}

/// Whether a JPEG transport is active and able to take a frame.
pub fn is_transport_ready(state: &State) -> bool {
    match state.transport {
        Transport::Http => true,
        Transport::WebSocket => state.socket.is_open,
        Transport::WebRtc => false,
    }
}

pub fn send_frame(page: &PageRef, blob: Blob) {
    let transport = {
        let mut state = page.state.borrow_mut();
        state.frames_in_flight += 1;
        state.frames_sent += 1.0;
        state.bytes_sent += blob.size();
        state.transport
    };
    if transport == Transport::WebSocket {
        websocket::send_frame(page, &blob);
        return;
    }
    http::send_frame(page, blob);
}

/// The PC has received (or rejected) one frame, making room for the next.
pub fn frame_acknowledged(page: &PageRef) {
    let mut state = page.state.borrow_mut();
    state.frames_in_flight = state.frames_in_flight.saturating_sub(1);
}
