//! Tuning the video the browser encodes: H.264 first, the PC's bitrate and frame
//! rate caps, and keeping the resolution when bandwidth is short.

use js_sys::{Array, Object, Promise};
use protocol::StreamConfig;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::js::{call_method, get_property, log, set_property};
use crate::page::PageRef;

pub fn prefer_h264(transceiver: &JsValue) {
    if set_h264_first(transceiver).is_err() {
        log("could not set codec preferences");
    }
}

/// Apply the PC's limits to the live video sender, if there is one.
pub async fn apply_sender_limits(page: PageRef) {
    let (sender, config) = {
        let state = page.state.borrow();
        (state.rtc.sender.clone(), state.config.clone())
    };
    let Some(sender) = sender else { return };
    let sender: JsValue = sender.into();
    limit_bitrate_and_frame_rate(&sender, &config).await;
    // Separately, so a browser that rejects this field still gets the limits above.
    keep_resolution_when_congested(&sender).await;
}

/// iPhones encode H.264 in hardware: better quality per bit and less heat than VP8.
fn set_h264_first(transceiver: &JsValue) -> Result<(), JsValue> {
    let window: JsValue = web_sys::window().unwrap().into();
    let receiver_class = get_property(&window, "RTCRtpReceiver");
    let capabilities = call_method(&receiver_class, "getCapabilities", &["video".into()])?;
    let codecs: Array = get_property(&capabilities, "codecs").dyn_into()?;
    let h264 = codecs.iter().filter(is_h264);
    let others = codecs.iter().filter(|codec| !is_h264(codec));
    let sorted: Array = h264.chain(others).collect();
    call_method(transceiver, "setCodecPreferences", &[sorted.into()])?;
    Ok(())
}

fn is_h264(codec: &JsValue) -> bool {
    let mime_type = get_property(codec, "mimeType").as_string();
    mime_type.is_some_and(|mime_type| mime_type.eq_ignore_ascii_case("video/h264"))
}

async fn limit_bitrate_and_frame_rate(sender: &JsValue, config: &StreamConfig) {
    let Ok(params) = call_method(sender, "getParameters", &[]) else { return };
    set_encoding_limits(&params, config);
    if set_parameters(sender, params).await.is_err() {
        log("could not set bitrate limits");
    }
}

/// A webcam should keep its resolution while bandwidth estimation ramps up; lower the
/// picture quality instead of shrinking the picture.
async fn keep_resolution_when_congested(sender: &JsValue) {
    let Ok(params) = call_method(sender, "getParameters", &[]) else { return };
    set_property(&params, "degradationPreference", &"maintain-resolution".into());
    if set_parameters(sender, params).await.is_err() {
        log("could not set degradation preference");
    }
}

fn set_encoding_limits(params: &JsValue, config: &StreamConfig) {
    let mut encodings = get_property(params, "encodings").dyn_into::<Array>().unwrap_or_default();
    if encodings.length() == 0 {
        encodings = Array::of1(&Object::new());
        set_property(params, "encodings", &encodings);
    }
    let first = encodings.get(0);
    set_property(&first, "maxBitrate", &config.max_bitrate.into());
    set_property(&first, "maxFramerate", &config.fps.into());
}

async fn set_parameters(sender: &JsValue, params: JsValue) -> Result<(), JsValue> {
    let promise = call_method(sender, "setParameters", &[params])?;
    JsFuture::from(Promise::from(promise)).await?;
    Ok(())
}
