//! The main transport: the camera as a real WebRTC video stream, with the PC's
//! messages on a data channel. Manages the peer connection's lifecycle: connect,
//! reconnect after a drop, and fall back to JPEG frames if WebRTC never works.

mod encoding;
mod signalling;

use std::cell::RefCell;

use js_sys::Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::{
    AbortController, MediaStream, MediaStreamTrack, MessageEvent, RtcConfiguration, RtcDataChannel, RtcPeerConnection,
    RtcPeerConnectionState, RtcRtpTransceiverDirection, RtcRtpTransceiverInit,
};

use crate::camera::video_track;
use crate::jpeg;
use crate::js::{error_message, get_property, into_js_function, log, set_timeout, TimerHandle};
use crate::page::{Page, PageRef, StatusKind};
use crate::server_messages::handle_server_message;

const RTC_CONNECT_TIMEOUT_MS: i32 = 10_000;
/// Safari can sit in "disconnected" for ~30 s before declaring "failed" (e.g. the PC
/// app restarted); reconnect sooner.
const RTC_DISCONNECT_GRACE_MS: i32 = 4_000;
const RTC_RECONNECT_DELAY_MS: i32 = 2_000;

/// Start a new WebRTC connection in the background, unless streaming is stopped.
pub fn connect(page: &PageRef) {
    {
        let state = page.state.borrow();
        if state.is_replaced || !state.is_running {
            return;
        }
    }
    spawn_local(connect_or_fail(page.clone()));
}

/// Whether `pc` is still the page's peer connection; events from older ones are ignored.
pub fn is_current_pc(page: &Page, pc: &RtcPeerConnection) -> bool {
    page.state.borrow().rtc.pc.as_ref() == Some(pc)
}

async fn connect_or_fail(page: PageRef) {
    let Err(error) = try_connect(&page).await else { return };
    log(&format!("WebRTC setup failed: {}", error_message(&error)));
    let pc = page.state.borrow().rtc.pc.clone();
    if let Some(pc) = pc {
        connection_failed(&page, &pc);
    }
}

async fn try_connect(page: &PageRef) -> Result<(), JsValue> {
    if get_property(&page.window, "RTCPeerConnection").is_undefined() {
        jpeg::start_fallback(page);
        return Ok(());
    }
    page.set_status("Connecting…", StatusKind::Neutral);
    let pc = create_peer_connection(page)?;
    open_control_channel(page, &pc);

    // Aborting the signalling request on timeout tells the PC to drop this attempt, so
    // a late answer can't take over from the fallback connection we switch to.
    let signalling = AbortController::new()?;
    let connect_timer = start_connect_timer(page, &pc, &signalling);
    watch_connection_state(page, &pc, connect_timer);
    signalling::negotiate(page, &pc, &signalling, connect_timer).await
}

/// A send-only peer connection carrying the camera track, stored as the current one.
fn create_peer_connection(page: &PageRef) -> Result<RtcPeerConnection, JsValue> {
    // No STUN/TURN servers: on a LAN (or Tailscale) the devices reach each other directly.
    let config = RtcConfiguration::new();
    config.set_ice_servers(&Array::new());
    let pc = RtcPeerConnection::new_with_configuration(&config)?;
    let (stream, track) = current_camera(page)?;
    let init = RtcRtpTransceiverInit::new();
    init.set_direction(RtcRtpTransceiverDirection::Sendonly);
    init.set_streams(&Array::of1(&stream));
    let transceiver = pc.add_transceiver_with_media_stream_track_and_init(&track, &init);
    encoding::prefer_h264(&transceiver);
    let mut state = page.state.borrow_mut();
    state.rtc.pc = Some(pc.clone());
    state.rtc.sender = Some(transceiver.sender());
    Ok(pc)
}

fn current_camera(page: &PageRef) -> Result<(MediaStream, MediaStreamTrack), JsValue> {
    let state = page.state.borrow();
    let stream = state.camera.stream.clone().ok_or("no camera")?;
    let track = video_track(&stream)?;
    Ok((stream, track))
}

fn open_control_channel(page: &PageRef, pc: &RtcPeerConnection) {
    let channel: RtcDataChannel = pc.create_data_channel("control");
    let on_message = {
        let (page, pc) = (page.clone(), pc.clone());
        let handler = move |event: MessageEvent| control_message_received(&page, &pc, &event);
        into_js_function(Closure::<dyn FnMut(MessageEvent)>::new(handler))
    };
    channel.set_onmessage(Some(&on_message));
}

fn control_message_received(page: &PageRef, pc: &RtcPeerConnection, event: &MessageEvent) {
    if !is_current_pc(page, pc) {
        return;
    }
    if let Some(text) = event.data().as_string() {
        handle_server_message(page, &text);
    }
}

/// Give up on `pc` if it hasn't connected in time.
fn start_connect_timer(page: &PageRef, pc: &RtcPeerConnection, signalling: &AbortController) -> TimerHandle {
    let (page_for_timer, pc, signalling) = (page.clone(), pc.clone(), signalling.clone());
    set_timeout(&page.window, RTC_CONNECT_TIMEOUT_MS, move || {
        if is_current_pc(&page_for_timer, &pc) && pc.connection_state() != RtcPeerConnectionState::Connected {
            signalling.abort();
            connection_failed(&page_for_timer, &pc);
        }
    })
}

fn watch_connection_state(page: &PageRef, pc: &RtcPeerConnection, connect_timer: TimerHandle) {
    let (page, watched_pc) = (page.clone(), pc.clone());
    let disconnect_timer = RefCell::new(None);
    let handler = move || connection_state_changed(&page, &watched_pc, connect_timer, &disconnect_timer);
    let on_change = into_js_function(Closure::<dyn FnMut()>::new(handler));
    pc.set_onconnectionstatechange(Some(&on_change));
}

fn connection_state_changed(
    page: &PageRef,
    pc: &RtcPeerConnection,
    connect_timer: TimerHandle,
    disconnect_timer: &RefCell<Option<TimerHandle>>,
) {
    if !is_current_pc(page, pc) {
        return;
    }
    if let Some(timer) = disconnect_timer.borrow_mut().take() {
        page.window.clear_timeout_with_handle(timer);
    }
    match pc.connection_state() {
        RtcPeerConnectionState::Disconnected => {
            let (page_for_timer, pc) = (page.clone(), pc.clone());
            let timer = set_timeout(&page.window, RTC_DISCONNECT_GRACE_MS, move || {
                if is_current_pc(&page_for_timer, &pc) {
                    connection_failed(&page_for_timer, &pc);
                }
            });
            *disconnect_timer.borrow_mut() = Some(timer);
        }
        RtcPeerConnectionState::Connected => connection_established(page, connect_timer),
        RtcPeerConnectionState::Failed | RtcPeerConnectionState::Closed => {
            page.window.clear_timeout_with_handle(connect_timer);
            connection_failed(page, pc);
        }
        _ => {}
    }
}

fn connection_established(page: &PageRef, connect_timer: TimerHandle) {
    page.window.clear_timeout_with_handle(connect_timer);
    page.state.borrow_mut().rtc.has_ever_connected = true;
    page.set_status("Streaming (WebRTC)", StatusKind::Good);
    spawn_local(encoding::apply_sender_limits(page.clone()));
}

/// Drop `pc`; reconnect if WebRTC has worked before, otherwise fall back to JPEG frames.
fn connection_failed(page: &PageRef, pc: &RtcPeerConnection) {
    pc.close();
    let has_ever_connected = {
        let mut state = page.state.borrow_mut();
        if state.rtc.pc.as_ref() != Some(pc) {
            return;
        }
        state.rtc.pc = None;
        state.rtc.sender = None;
        if state.is_replaced || !state.is_running {
            return;
        }
        state.rtc.has_ever_connected
    };
    // WebRTC never worked here: UDP is probably blocked.
    if !has_ever_connected {
        jpeg::start_fallback(page);
        return;
    }
    page.set_status("Reconnecting…", StatusKind::Bad);
    let page_for_retry = page.clone();
    set_timeout(&page.window, RTC_RECONNECT_DELAY_MS, move || connect(&page_for_retry));
}
