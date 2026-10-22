//! The offer/answer exchange with the PC: one HTTPS POST to `/offer` carrying the
//! complete offer, answered with the PC's SDP and stream config.

use js_sys::{Function, Promise};
use protocol::{OfferRequest, OfferResponse};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AbortController, RtcIceGatheringState, RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit};

use super::is_current_pc;
use crate::camera::client_info;
use crate::fetch::{post_json, response_text, HTTP_FORBIDDEN};
use crate::js::{add_listener, set_timeout, TimerHandle};
use crate::page::{Page, PageRef, StatusKind, PAIRING_KEY_REJECTED};
use crate::server_messages::apply_config;

/// Send the offer anyway if gathering candidates takes longer than this.
const ICE_GATHERING_TIMEOUT_MS: i32 = 3000;

/// Send the offer to the PC and apply its answer.
pub async fn negotiate(
    page: &PageRef,
    pc: &RtcPeerConnection,
    signalling: &AbortController,
    connect_timer: TimerHandle,
) -> Result<(), JsValue> {
    let sdp = create_offer_with_candidates(page, pc).await?;
    let request = OfferRequest { sdp, id: page.client_id.clone(), info: client_info(page) };
    let body = serde_json::to_string(&request).unwrap();
    let response = post_json(page, "/offer", &body, Some(signalling)).await?;
    if response.status() == HTTP_FORBIDDEN {
        stop_after_key_rejected(page, pc, connect_timer);
        return Ok(());
    }
    let text = response_text(&response).await?;
    if !response.ok() {
        return Err(format!("offer rejected: {} {text}", response.status()).into());
    }
    let answer: OfferResponse = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    if !is_current_pc(page, pc) {
        return Ok(());
    }
    apply_config(page, answer.config);
    set_remote_answer(pc, &answer.sdp).await
}

async fn create_offer_with_candidates(page: &Page, pc: &RtcPeerConnection) -> Result<String, JsValue> {
    let offer = JsFuture::from(pc.create_offer()).await?;
    JsFuture::from(pc.set_local_description(offer.unchecked_ref())).await?;
    wait_for_ice_gathering(page, pc).await;
    let description = pc.local_description().ok_or("no local description")?;
    Ok(description.sdp())
}

/// The PC answers in one HTTP round trip, so the offer must already hold all candidates.
async fn wait_for_ice_gathering(page: &Page, pc: &RtcPeerConnection) {
    if pc.ice_gathering_state() == RtcIceGatheringState::Complete {
        return;
    }
    // A promise that resolves when gathering completes, or on the timeout.
    let mut wait = |resolve: Function, _reject: Function| {
        let resolve_on_timeout = resolve.clone();
        set_timeout(&page.window, ICE_GATHERING_TIMEOUT_MS, move || {
            let _ = resolve_on_timeout.call0(&JsValue::NULL);
        });
        let watched_pc = pc.clone();
        let _ = add_listener(pc, "icegatheringstatechange", move || {
            if watched_pc.ice_gathering_state() == RtcIceGatheringState::Complete {
                let _ = resolve.call0(&JsValue::NULL);
            }
        });
    };
    let _ = JsFuture::from(Promise::new(&mut wait)).await;
}

fn stop_after_key_rejected(page: &PageRef, pc: &RtcPeerConnection, connect_timer: TimerHandle) {
    page.window.clear_timeout_with_handle(connect_timer);
    pc.close();
    {
        let mut state = page.state.borrow_mut();
        state.rtc.pc = None;
        state.rtc.sender = None;
    }
    page.set_status(PAIRING_KEY_REJECTED, StatusKind::Bad);
}

async fn set_remote_answer(pc: &RtcPeerConnection, sdp: &str) -> Result<(), JsValue> {
    let description = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    description.set_sdp(sdp);
    JsFuture::from(pc.set_remote_description(&description)).await?;
    Ok(())
}
