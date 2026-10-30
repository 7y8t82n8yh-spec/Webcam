//! Keeps the screen on while streaming, since an auto-locking phone stops the camera.
//! Uses the Wake Lock API where it exists, else a silent looping video.

use js_sys::Promise;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::HtmlVideoElement;

use crate::js::{call_method, get_property, into_js_function, log, set_timeout};
use crate::nosleep;
use crate::page::PageRef;

const WAKE_LOCK_RETRY_DELAY_MS: i32 = 1000;

pub fn request_wake_lock(page: &PageRef) {
    spawn_local(keep_screen_awake(page.clone()));
}

async fn keep_screen_awake(page: PageRef) {
    let wake_lock = get_property(&page.window.navigator(), "wakeLock");
    if !wake_lock.is_undefined() {
        if let Ok(sentinel) = take_wake_lock(&wake_lock).await {
            retake_wake_lock_on_release(&page, &sentinel);
            return;
        }
        log("wake lock failed, using video fallback");
    }
    play_silent_video(&page).await;
}

async fn take_wake_lock(wake_lock: &JsValue) -> Result<JsValue, JsValue> {
    let promise = call_method(wake_lock, "request", &["screen".into()])?;
    JsFuture::from(Promise::from(promise)).await
}

/// Older iOS: a silent looping video keeps the screen on, like NoSleep.js.
async fn play_silent_video(page: &PageRef) {
    let video: HtmlVideoElement = page.element("nosleep");
    if video.src().is_empty() {
        video.set_src(nosleep::MP4_DATA_URI);
    }
    if let Ok(playing) = video.play() {
        let _ = JsFuture::from(playing).await;
    }
}

/// iOS can drop the lock without hiding the page (e.g. after a system alert); take it again.
fn retake_wake_lock_on_release(page: &PageRef, sentinel: &JsValue) {
    let page = page.clone();
    let on_release = move || {
        if page.state.borrow().is_running && !page.is_hidden() {
            let page_for_retry = page.clone();
            set_timeout(&page.window, WAKE_LOCK_RETRY_DELAY_MS, move || request_wake_lock(&page_for_retry));
        }
    };
    let on_release = into_js_function(Closure::<dyn FnMut()>::new(on_release));
    let _ = call_method(sentinel, "addEventListener", &["release".into(), on_release.into()]);
}
