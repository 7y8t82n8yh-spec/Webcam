//! Messages the PC sends over whichever control channel is live (the WebRTC data
//! channel or the WebSocket): a new stream config, or "another device took over".

use protocol::{ServerMessage, StreamConfig};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::{HtmlOptionElement, HtmlOptionsCollection, HtmlSelectElement};

use crate::camera::open_camera;
use crate::page::{PageRef, StatusKind};
use crate::state::VideoSize;

pub fn handle_server_message(page: &PageRef, text: &str) {
    match serde_json::from_str::<ServerMessage>(text) {
        Ok(ServerMessage::Config(config)) => apply_config(page, config),
        Ok(ServerMessage::Replaced) => handle_replaced(page),
        Err(_) => {}
    }
}

/// Use the PC's stream config, reopening the camera if its size changed.
pub fn apply_config(page: &PageRef, config: StreamConfig) {
    let needs_reopen = store_config(page, &config);
    show_config_in_quality_picker(page, &config);
    if needs_reopen {
        spawn_local(reopen_camera_quietly(page.clone()));
    }
}

/// Store `config`; returns whether the open camera must be reopened at the new size.
fn store_config(page: &PageRef, config: &StreamConfig) -> bool {
    let mut state = page.state.borrow_mut();
    let has_size_changed = VideoSize::of_config(config) != VideoSize::of_config(&state.config);
    state.config = config.clone();
    has_size_changed && state.camera.requested_size.is_none() && state.camera.stream.is_some()
}

/// Make sure the PC's size is in the quality picker, and select it unless the user picked another.
fn show_config_in_quality_picker(page: &PageRef, config: &StreamConfig) {
    let select: HtmlSelectElement = page.element("quality");
    let value = VideoSize::of_config(config).picker_value();
    let options = select.options();
    if !has_option(&options, &value) {
        let label = format!("{}p (PC)", config.height);
        if let Ok(option) = HtmlOptionElement::new_with_text_and_value(&label, &value) {
            let _ = options.add_with_html_option_element(&option);
        }
    }
    if page.state.borrow().camera.requested_size.is_none() {
        select.set_value(&value);
    }
}

fn has_option(options: &HtmlOptionsCollection, value: &str) -> bool {
    (0..options.length()).any(|index| option_value(options, index).as_deref() == Some(value))
}

fn option_value(options: &HtmlOptionsCollection, index: u32) -> Option<String> {
    let option: HtmlOptionElement = options.item(index)?.dyn_into().ok()?;
    Some(option.value())
}

async fn reopen_camera_quietly(page: PageRef) {
    let _ = open_camera(&page).await;
}

fn handle_replaced(page: &PageRef) {
    let pc = {
        let mut state = page.state.borrow_mut();
        state.is_replaced = true;
        state.rtc.sender = None;
        state.rtc.pc.take()
    };
    if let Some(pc) = pc {
        pc.close();
    }
    page.set_status("Another device took over — tap here to take it back", StatusKind::Bad);
}
