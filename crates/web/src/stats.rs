//! The stats line: resolution, frame rate and bitrate, refreshed every second. In
//! WebRTC mode the browser does the sending, so its counters are read instead.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::HtmlElement;

use crate::js::{get_property, into_js_function, now_ms};
use crate::page::PageRef;
use crate::state::Transport;

const STATS_INTERVAL_MS: i32 = 1000;

/// The counters at the previous stats tick.
#[derive(Clone, Copy)]
struct StatsSample {
    time_ms: f64,
    frames: f64,
    bytes: f64,
}

pub fn start_stats(page: &PageRef) {
    let previous = Rc::new(Cell::new(StatsSample { time_ms: now_ms(), frames: 0.0, bytes: 0.0 }));
    let page_for_tick = page.clone();
    let tick = move || {
        let (page, previous) = (page_for_tick.clone(), previous.clone());
        spawn_local(async move {
            read_rtc_counters(&page).await;
            show_stats(&page, &previous);
        });
    };
    let tick = into_js_function(Closure::<dyn FnMut()>::new(tick));
    let _ = page.window.set_interval_with_callback_and_timeout_and_arguments_0(&tick, STATS_INTERVAL_MS);
}

async fn read_rtc_counters(page: &PageRef) {
    let pc = {
        let state = page.state.borrow();
        if state.transport != Transport::WebRtc {
            return;
        }
        state.rtc.pc.clone()
    };
    let Some(pc) = pc else { return };
    let Ok(report) = JsFuture::from(pc.get_stats()).await else { return };
    let report: js_sys::Map = report.unchecked_into();
    report.for_each(&mut |entry, _| record_outbound_video(page, &entry));
}

fn record_outbound_video(page: &PageRef, entry: &JsValue) {
    if !(has_string_property(entry, "type", "outbound-rtp") && has_string_property(entry, "kind", "video")) {
        return;
    }
    let frames_sent = get_property(entry, "framesSent").as_f64();
    let frames_encoded = get_property(entry, "framesEncoded").as_f64();
    let mut state = page.state.borrow_mut();
    if let Some(frames) = frames_sent.or(frames_encoded) {
        state.frames_sent = frames;
    }
    if let Some(bytes) = get_property(entry, "bytesSent").as_f64() {
        state.bytes_sent = bytes;
    }
}

fn has_string_property(target: &JsValue, name: &str, expected: &str) -> bool {
    get_property(target, name).as_string().as_deref() == Some(expected)
}

fn show_stats(page: &PageRef, previous: &Cell<StatsSample>) {
    let current = {
        let state = page.state.borrow();
        StatsSample { time_ms: now_ms(), frames: state.frames_sent, bytes: state.bytes_sent }
    };
    let last = previous.replace(current);
    let seconds = ((current.time_ms - last.time_ms) / 1000.0).max(0.001);
    // Counters restart with each new peer connection: never show negative rates.
    let fps = (current.frames - last.frames).max(0.0) / seconds;
    let kbps = (current.bytes - last.bytes).max(0.0) * 8.0 / 1000.0 / seconds;
    let text = stats_text(page, fps, kbps);
    page.element::<HtmlElement>("stats").set_text_content(Some(&text));
}

fn stats_text(page: &PageRef, fps: f64, kbps: f64) -> String {
    let (video_width, video_height) = (page.video.video_width(), page.video.video_height());
    let resolution = if video_width > 0 { format!("{video_width}×{video_height} · ") } else { String::new() };
    let rate = if kbps >= 1000.0 { format!("{:.1} Mbps", kbps / 1000.0) } else { format!("{kbps:.0} kbps") };
    format!("{resolution}{fps:.0} fps · {rate}")
}
