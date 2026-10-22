//! The capture loop: at the configured frame rate, draw the preview onto the canvas
//! and encode it as a JPEG, whenever a JPEG transport can take another frame.

use protocol::StreamConfig;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Blob, CanvasRenderingContext2d};

use super::{is_transport_ready, send_frame, MAX_FRAMES_IN_FLIGHT};
use crate::js::{now_ms, set_timeout};
use crate::page::PageRef;
use crate::state::{State, VideoSize};

const MS_PER_SECOND: f64 = 1000.0;

/// Capture one frame if possible, then schedule the next. Runs until streaming stops;
/// while WebRTC is the transport it only keeps ticking.
pub fn capture_loop(page: &PageRef) {
    let started_ms = now_ms();
    let (config, can_send) = {
        let state = page.state.borrow();
        if !state.is_running {
            return;
        }
        (state.config.clone(), can_send_frame(&state))
    };
    let (video_width, video_height) = (page.video.video_width(), page.video.video_height());
    if video_width > 0 && video_height > 0 && can_send && !page.is_hidden() {
        let size = frame_size(video_width, video_height, &config);
        encode_frame(page, size, config.quality);
    }
    schedule_next_capture(page, started_ms, config.fps);
}

fn can_send_frame(state: &State) -> bool {
    is_transport_ready(state) && state.frames_in_flight < MAX_FRAMES_IN_FLIGHT && !state.is_encoding
}

/// The video size scaled down to fit the PC's output: never send more pixels than it can use.
fn frame_size(video_width: u32, video_height: u32, config: &StreamConfig) -> VideoSize {
    let width_scale = config.width as f64 / video_width as f64;
    let height_scale = config.height as f64 / video_height as f64;
    let scale = width_scale.min(height_scale).min(1.0);
    let width = (video_width as f64 * scale).round() as u32;
    let height = (video_height as f64 * scale).round() as u32;
    VideoSize { width, height }
}

fn schedule_next_capture(page: &PageRef, started_ms: f64, fps: u32) {
    let interval_ms = MS_PER_SECOND / fps.max(1) as f64;
    let elapsed_ms = now_ms() - started_ms;
    let delay_ms = (interval_ms - elapsed_ms).max(0.0) as i32;
    let page_for_timer = page.clone();
    set_timeout(&page.window, delay_ms, move || capture_loop(&page_for_timer));
}

/// Draw the current video frame at `size` and start encoding it as a JPEG.
fn encode_frame(page: &PageRef, size: VideoSize, quality: f64) {
    let canvas = &page.canvas;
    if canvas.width() != size.width || canvas.height() != size.height {
        canvas.set_width(size.width);
        canvas.set_height(size.height);
    }
    let Ok(Some(context)) = canvas.get_context("2d") else { return };
    let context: CanvasRenderingContext2d = context.unchecked_into();
    let (width, height) = (size.width as f64, size.height as f64);
    let _ = context.draw_image_with_html_video_element_and_dw_and_dh(&page.video, 0.0, 0.0, width, height);
    page.state.borrow_mut().is_encoding = true;
    let page_for_callback = page.clone();
    let on_encoded = Closure::once_into_js(move |blob: JsValue| frame_encoded(&page_for_callback, blob));
    let _ = canvas.to_blob_with_type_and_encoder_options(on_encoded.unchecked_ref(), "image/jpeg", &quality.into());
}

fn frame_encoded(page: &PageRef, blob: JsValue) {
    page.state.borrow_mut().is_encoding = false;
    let is_ready = is_transport_ready(&page.state.borrow());
    if let (Ok(blob), true) = (blob.dyn_into::<Blob>(), is_ready) {
        send_frame(page, blob);
    }
}
