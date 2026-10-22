//! Opening the camera, and reopening it when iOS stops it or the user flips or
//! resizes it. A reopened camera is swapped into the live stream.

use js_sys::Object;
use protocol::ClientInfo;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{MediaStream, MediaStreamTrack, MediaStreamTrackState};

use crate::jpeg::websocket::send_hello;
use crate::js::{add_listener, error_message, js_object, set_timeout};
use crate::page::{Page, PageRef, StatusKind};
use crate::state::{Facing, VideoSize};

/// Wait this long after the camera track ends before checking whether to reopen it.
const TRACK_ENDED_RECOVERY_DELAY_MS: i32 = 300;
/// The `frameRate.max` constraint is never lower than this.
const MIN_MAX_FRAME_RATE: u32 = 30;

/// Open the camera with the current settings, replacing any camera already open.
pub async fn open_camera(page: &PageRef) -> Result<(), JsValue> {
    let constraints = release_camera_and_build_constraints(page);
    let devices = page.window.navigator().media_devices()?;
    let request = devices.get_user_media_with_constraints(constraints.unchecked_ref())?;
    let stream: MediaStream = JsFuture::from(request).await?.dyn_into()?;
    show_preview(page, &stream).await;
    let track = video_track(&stream)?;
    recover_when_track_ends(page, &track)?;
    let sender = {
        let mut state = page.state.borrow_mut();
        state.camera.stream = Some(stream);
        state.rtc.sender.clone()
    };
    // Swap the new camera into the live WebRTC stream without renegotiating.
    if let Some(sender) = sender {
        let _ = JsFuture::from(sender.replace_track(Some(&track))).await;
    }
    send_hello(page);
    Ok(())
}

/// Reopen the camera in the background, showing any error in the status line.
pub fn reopen_camera(page: &PageRef) {
    spawn_local(reopen_camera_reporting_errors(page.clone()));
}

/// Reopen the camera if it is no longer delivering frames.
pub fn recover_camera(page: &PageRef) {
    let is_live = {
        let state = page.state.borrow();
        if !state.is_running || page.is_hidden() {
            return;
        }
        let track = state.camera.stream.as_ref().and_then(|stream| video_track(stream).ok());
        track.as_ref().is_some_and(is_track_live)
    };
    if !is_live {
        reopen_camera(page);
    }
}

pub fn update_mirror(page: &PageRef) {
    let state = page.state.borrow();
    let is_mirrored = state.camera.facing == Facing::User && state.camera.is_mirror_on;
    let _ = page.video.class_list().toggle_with_force("mirror", is_mirrored);
}

pub fn video_track(stream: &MediaStream) -> Result<MediaStreamTrack, JsValue> {
    stream.get_video_tracks().get(0).dyn_into()
}

/// What the PC shows about this phone and its camera.
pub fn client_info(page: &Page) -> ClientInfo {
    ClientInfo {
        ua: page.window.navigator().user_agent().unwrap_or_default(),
        camera: page.state.borrow().camera.facing.as_str().to_string(),
        width: page.video.video_width(),
        height: page.video.video_height(),
    }
}

/// Stop the open camera (iOS allows only one) and describe the camera to open next.
fn release_camera_and_build_constraints(page: &PageRef) -> Object {
    let mut state = page.state.borrow_mut();
    if let Some(old_stream) = state.camera.stream.take() {
        stop_all_tracks(&old_stream);
    }
    let size = state.camera.requested_size.unwrap_or(VideoSize::of_config(&state.config));
    camera_constraints(state.camera.facing, size, state.config.fps)
}

fn stop_all_tracks(stream: &MediaStream) {
    let mut stop_track = |track: JsValue, _: u32, _: js_sys::Array| track.unchecked_into::<MediaStreamTrack>().stop();
    stream.get_tracks().for_each(&mut stop_track);
}

fn camera_constraints(facing: Facing, size: VideoSize, fps: u32) -> Object {
    let ideal = |value: JsValue| js_object(&[("ideal", value)]);
    let frame_rate = js_object(&[("ideal", fps.into()), ("max", fps.max(MIN_MAX_FRAME_RATE).into())]);
    let video = js_object(&[
        ("facingMode", ideal(facing.as_str().into()).into()),
        ("width", ideal((size.width as f64).into()).into()),
        ("height", ideal((size.height as f64).into()).into()),
        ("frameRate", frame_rate.into()),
    ]);
    js_object(&[("audio", false.into()), ("video", video.into())])
}

async fn show_preview(page: &PageRef, stream: &MediaStream) {
    page.video.set_src_object(Some(stream));
    update_mirror(page);
    if let Ok(playing) = page.video.play() {
        let _ = JsFuture::from(playing).await;
    }
}

fn recover_when_track_ends(page: &PageRef, track: &MediaStreamTrack) -> Result<(), JsValue> {
    let page = page.clone();
    add_listener(track, "ended", move || camera_track_ended(&page))
}

fn camera_track_ended(page: &PageRef) {
    if !page.state.borrow().is_running {
        return;
    }
    let page_for_timer = page.clone();
    set_timeout(&page.window, TRACK_ENDED_RECOVERY_DELAY_MS, move || recover_camera(&page_for_timer));
}

async fn reopen_camera_reporting_errors(page: PageRef) {
    if let Err(error) = open_camera(&page).await {
        let status = format!("Camera error: {}", error_message(&error));
        page.set_status(&status, StatusKind::Bad);
    }
}

fn is_track_live(track: &MediaStreamTrack) -> bool {
    track.ready_state() == MediaStreamTrackState::Live && !track.muted()
}
