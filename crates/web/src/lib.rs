//! The phone page, compiled to WebAssembly.
//!
//! The camera is sent to the PC over WebRTC: a real video stream (H.264, which the
//! iPhone encodes in hardware) with congestion control. If WebRTC can't connect -
//! typically UDP blocked by a firewall - the page falls back to JPEG frames drawn from
//! a canvas and sent over a WebSocket, or HTTPS POSTs if even that is refused. In the
//! JPEG modes at most `MAX_IN_FLIGHT` frames are unacknowledged at a time, so a slow
//! network drops frames instead of building up latency.

use std::cell::RefCell;
use std::rc::Rc;

use js_sys::{Array, Object, Promise, Reflect};
use protocol::{ClientInfo, ClientMessage, OfferRequest, OfferResponse, ServerMessage, StreamConfig};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    AbortController, Blob, CanvasRenderingContext2d, Document, HtmlCanvasElement, HtmlElement, HtmlSelectElement,
    HtmlVideoElement, MediaStream, MediaStreamTrack, MessageEvent, RequestInit, Response, RtcConfiguration,
    RtcDataChannel, RtcIceGatheringState, RtcPeerConnection, RtcPeerConnectionState, RtcRtpSender,
    RtcRtpTransceiverDirection, RtcRtpTransceiverInit, RtcSdpType, RtcSessionDescriptionInit, WebSocket, Window,
};

mod nosleep;

const MAX_IN_FLIGHT: u32 = 2;
const RTC_CONNECT_TIMEOUT_MS: i32 = 10_000;
/// Safari can sit in "disconnected" for ~30 s before declaring "failed" (e.g. the PC
/// app restarted); reconnect sooner.
const RTC_DISCONNECT_GRACE_MS: i32 = 4_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transport {
    WebRtc,
    WebSocket,
    Http,
}

struct State {
    config: StreamConfig,
    facing: &'static str,
    requested: Option<(u32, u32)>,
    stream: Option<MediaStream>,
    transport: Transport,
    pc: Option<RtcPeerConnection>,
    sender: Option<RtcRtpSender>,
    rtc_ever_connected: bool,
    ws: Option<WebSocket>,
    ws_open: bool,
    ws_ever_opened: bool,
    ws_failures: u32,
    in_flight: u32,
    encoding: bool,
    running: bool,
    starting: bool,
    replaced: bool,
    mirror: bool,
    sent_frames: f64,
    sent_bytes: f64,
}

struct Page {
    window: Window,
    document: Document,
    video: HtmlVideoElement,
    canvas: HtmlCanvasElement,
    client_id: String,
    key: String,
    state: RefCell<State>,
}

type Rcp = Rc<Page>;

// ---------------------------------------------------------------- small helpers

fn el<T: JsCast>(document: &Document, id: &str) -> T {
    document.get_element_by_id(id).unwrap_or_else(|| panic!("#{id} missing")).unchecked_into()
}

fn obj(pairs: &[(&str, JsValue)]) -> Object {
    let o = Object::new();
    for (k, v) in pairs {
        let _ = Reflect::set(&o, &(*k).into(), v);
    }
    o
}

fn get(target: &JsValue, key: &str) -> JsValue {
    Reflect::get(target, &key.into()).unwrap_or(JsValue::UNDEFINED)
}

/// Call `target.method(args...)` dynamically, for APIs web-sys lacks or Safari may not have.
fn call(target: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let f: js_sys::Function = get(target, method).dyn_into()?;
    Reflect::apply(&f, target, &args.iter().collect::<Array>())
}

fn now() -> f64 {
    web_sys::window().and_then(|w| w.performance()).map_or(0.0, |p| p.now())
}

fn log(message: &str) {
    web_sys::console::warn_1(&message.into());
}

fn set_timeout(window: &Window, ms: i32, f: impl FnOnce() + 'static) -> i32 {
    let closure = Closure::once_into_js(f);
    window.set_timeout_with_callback_and_timeout_and_arguments_0(closure.unchecked_ref(), ms).unwrap_or(0)
}

fn random_id() -> String {
    format!("{:x}", (js_sys::Math::random() * 1e16) as u64)
}

impl Page {
    fn id<T: JsCast>(&self, id: &str) -> T {
        el(&self.document, id)
    }

    fn set_status(&self, text: &str, kind: &str) {
        self.id::<HtmlElement>("state").set_text_content(Some(text));
        self.id::<HtmlElement>("dot").set_class_name(&format!("dot {kind}"));
    }

    fn show_error(&self, message: &str) {
        self.id::<HtmlElement>("error").set_text_content(Some(message));
        let _ = self.id::<HtmlElement>("start-screen").class_list().remove_1("hidden");
    }

    fn hidden(&self) -> bool {
        self.document.hidden()
    }
}

// ---------------------------------------------------------------- keep awake

fn request_wake_lock(page: &Rcp) {
    let page = page.clone();
    spawn_local(async move {
        let wake_lock = get(&page.window.navigator(), "wakeLock");
        if !wake_lock.is_undefined() {
            if let Ok(promise) = call(&wake_lock, "request", &["screen".into()]) {
                if let Ok(sentinel) = JsFuture::from(Promise::from(promise)).await {
                    // iOS can drop the lock without hiding the page (e.g. after a system
                    // alert); take it again.
                    let again = page.clone();
                    let on_release = Closure::<dyn FnMut()>::new(move || {
                        let state = again.state.borrow();
                        if state.running && !again.hidden() {
                            let p = again.clone();
                            set_timeout(&again.window, 1000, move || request_wake_lock(&p));
                        }
                    });
                    let _ = call(&sentinel, "addEventListener", &["release".into(), on_release.into_js_value()]);
                    return;
                }
            }
            log("wake lock failed, using video fallback");
        }
        // Older iOS: a silent looping video keeps the screen on, like NoSleep.js.
        let video: HtmlVideoElement = page.id("nosleep");
        if video.src().is_empty() {
            video.set_src(nosleep::MP4_DATA_URI);
        }
        if let Ok(p) = video.play() {
            let _ = JsFuture::from(p).await;
        }
    });
}

// ---------------------------------------------------------------- camera

async fn open_camera(page: &Rcp) -> Result<(), JsValue> {
    let (facing, (w, h), fps) = {
        let mut state = page.state.borrow_mut();
        if let Some(old) = state.stream.take() {
            old.get_tracks().for_each(&mut |t, _, _| t.unchecked_into::<MediaStreamTrack>().stop());
        }
        let size = state.requested.unwrap_or((state.config.width, state.config.height));
        (state.facing, size, state.config.fps)
    };
    let ideal = |v: f64| obj(&[("ideal", v.into())]).into();
    let constraints = obj(&[
        ("audio", false.into()),
        (
            "video",
            obj(&[
                ("facingMode", obj(&[("ideal", facing.into())]).into()),
                ("width", ideal(w as f64)),
                ("height", ideal(h as f64)),
                ("frameRate", obj(&[("ideal", fps.into()), ("max", fps.max(30).into())]).into()),
            ])
            .into(),
        ),
    ]);
    let devices = page.window.navigator().media_devices()?;
    let stream: MediaStream =
        JsFuture::from(devices.get_user_media_with_constraints(constraints.unchecked_ref())?).await?.dyn_into()?;
    page.video.set_src_object(Some(&stream));
    update_mirror(page);
    if let Ok(p) = page.video.play() {
        let _ = JsFuture::from(p).await;
    }
    let track: MediaStreamTrack = stream.get_video_tracks().get(0).dyn_into()?;
    let on_ended = {
        let page = page.clone();
        Closure::<dyn FnMut()>::new(move || {
            if page.state.borrow().running {
                let p = page.clone();
                set_timeout(&page.window, 300, move || recover_camera(&p));
            }
        })
    };
    track.add_event_listener_with_callback("ended", on_ended.into_js_value().unchecked_ref())?;
    let sender = {
        let mut state = page.state.borrow_mut();
        state.stream = Some(stream);
        state.sender.clone()
    };
    // Swap the new camera into the live WebRTC stream without renegotiating.
    if let Some(sender) = sender {
        let _ = JsFuture::from(sender.replace_track(Some(&track))).await;
    }
    send_hello(page);
    Ok(())
}

fn update_mirror(page: &Rcp) {
    let state = page.state.borrow();
    let mirrored = state.facing == "user" && state.mirror;
    let _ = page.video.class_list().toggle_with_force("mirror", mirrored);
}

fn recover_camera(page: &Rcp) {
    let live = {
        let state = page.state.borrow();
        if !state.running || page.hidden() {
            return;
        }
        state
            .stream
            .as_ref()
            .and_then(|s| s.get_video_tracks().get(0).dyn_into::<MediaStreamTrack>().ok())
            .is_some_and(|t| t.ready_state() == web_sys::MediaStreamTrackState::Live && !t.muted())
    };
    if !live {
        let page = page.clone();
        spawn_local(async move {
            if let Err(e) = open_camera(&page).await {
                page.set_status(&format!("Camera error: {}", error_text(&e)), "bad");
            }
        });
    }
}

fn error_text(e: &JsValue) -> String {
    let message = get(e, "message").as_string().unwrap_or_default();
    if message.is_empty() {
        get(e, "name").as_string().unwrap_or_else(|| format!("{e:?}"))
    } else {
        message
    }
}

fn client_info(page: &Page) -> ClientInfo {
    ClientInfo {
        ua: page.window.navigator().user_agent().unwrap_or_default(),
        camera: page.state.borrow().facing.to_string(),
        width: page.video.video_width(),
        height: page.video.video_height(),
    }
}

fn apply_config(page: &Rcp, config: StreamConfig) {
    let changed = {
        let mut state = page.state.borrow_mut();
        let changed = (config.width, config.height) != (state.config.width, state.config.height);
        state.config = config.clone();
        changed && state.requested.is_none() && state.stream.is_some()
    };
    let select: HtmlSelectElement = page.id("quality");
    let value = format!("{}x{}", config.width, config.height);
    let options = select.options();
    let exists = (0..options.length()).any(|i| {
        options
            .item(i)
            .and_then(|o| o.dyn_into::<web_sys::HtmlOptionElement>().ok())
            .is_some_and(|o| o.value() == value)
    });
    if !exists {
        if let Ok(option) =
            web_sys::HtmlOptionElement::new_with_text_and_value(&format!("{}p (PC)", config.height), &value)
        {
            let _ = options.add_with_html_option_element(&option);
        }
    }
    if page.state.borrow().requested.is_none() {
        select.set_value(&value);
    }
    if changed {
        let page = page.clone();
        spawn_local(async move {
            let _ = open_camera(&page).await;
        });
    }
}

// ---------------------------------------------------------------- WebRTC

fn prefer_h264(transceiver: &JsValue) {
    // iPhones encode H.264 in hardware: better quality per bit and less heat than VP8.
    let result = (|| -> Result<(), JsValue> {
        let receiver_class = get(&web_sys::window().unwrap().into(), "RTCRtpReceiver");
        let caps = call(&receiver_class, "getCapabilities", &["video".into()])?;
        let codecs: Array = get(&caps, "codecs").dyn_into()?;
        let is_h264 =
            |c: &JsValue| get(c, "mimeType").as_string().is_some_and(|m| m.eq_ignore_ascii_case("video/h264"));
        let sorted: Array = codecs.iter().filter(is_h264).chain(codecs.iter().filter(|c| !is_h264(c))).collect();
        call(transceiver, "setCodecPreferences", &[sorted.into()])?;
        Ok(())
    })();
    if result.is_err() {
        log("could not set codec preferences");
    }
}

async fn wait_for_ice_gathering(page: &Rcp, pc: &RtcPeerConnection, timeout_ms: i32) {
    // The PC answers in one HTTP round trip, so send the offer with all candidates in it.
    if pc.ice_gathering_state() == RtcIceGatheringState::Complete {
        return;
    }
    let promise = Promise::new(&mut |resolve, _| {
        let resolve2 = resolve.clone();
        set_timeout(&page.window, timeout_ms, move || {
            let _ = resolve2.call0(&JsValue::NULL);
        });
        let pc2 = pc.clone();
        let on_change = Closure::<dyn FnMut()>::new(move || {
            if pc2.ice_gathering_state() == RtcIceGatheringState::Complete {
                let _ = resolve.call0(&JsValue::NULL);
            }
        });
        let _ =
            pc.add_event_listener_with_callback("icegatheringstatechange", on_change.into_js_value().unchecked_ref());
    });
    let _ = JsFuture::from(promise).await;
}

async fn apply_sender_limits(page: &Rcp) {
    let (sender, config) = {
        let state = page.state.borrow();
        (state.sender.clone(), state.config.clone())
    };
    let Some(sender) = sender else { return };
    let sender: JsValue = sender.into();
    let set = |params: JsValue| async {
        match call(&sender, "setParameters", &[params]) {
            Ok(p) => JsFuture::from(Promise::from(p)).await.map(|_| ()),
            Err(e) => Err(e),
        }
    };
    if let Ok(params) = call(&sender, "getParameters", &[]) {
        let mut encodings = get(&params, "encodings").dyn_into::<Array>().unwrap_or_default();
        if encodings.length() == 0 {
            encodings = Array::of1(&Object::new());
            let _ = Reflect::set(&params, &"encodings".into(), &encodings);
        }
        let first = encodings.get(0);
        let _ = Reflect::set(&first, &"maxBitrate".into(), &config.max_bitrate.into());
        let _ = Reflect::set(&first, &"maxFramerate".into(), &config.fps.into());
        if set(params).await.is_err() {
            log("could not set bitrate limits");
        }
    }
    // Separately, so a browser that rejects this field still gets the limits above. A
    // webcam should keep its resolution while bandwidth estimation ramps up; lower the
    // picture quality instead of shrinking the picture.
    if let Ok(params) = call(&sender, "getParameters", &[]) {
        let _ = Reflect::set(&params, &"degradationPreference".into(), &"maintain-resolution".into());
        if set(params).await.is_err() {
            log("could not set degradation preference");
        }
    }
}

fn handle_server_message(page: &Rcp, text: &str) {
    match serde_json::from_str::<ServerMessage>(text) {
        Ok(ServerMessage::Config(config)) => apply_config(page, config),
        Ok(ServerMessage::Replaced) => {
            let pc = {
                let mut state = page.state.borrow_mut();
                state.replaced = true;
                state.sender = None;
                state.pc.take()
            };
            if let Some(pc) = pc {
                pc.close();
            }
            page.set_status("Another device took over — tap here to take it back", "bad");
        }
        Err(_) => {}
    }
}

fn connect_webrtc(page: &Rcp) {
    {
        let state = page.state.borrow();
        if state.replaced || !state.running {
            return;
        }
    }
    let page = page.clone();
    spawn_local(async move {
        if let Err(e) = connect_webrtc_inner(&page).await {
            log(&format!("WebRTC setup failed: {}", error_text(&e)));
            let pc = page.state.borrow().pc.clone();
            if let Some(pc) = pc {
                rtc_failed(&page, &pc);
            }
        }
    });
}

async fn connect_webrtc_inner(page: &Rcp) -> Result<(), JsValue> {
    if get(&page.window.clone().into(), "RTCPeerConnection").is_undefined() {
        fallback_to_jpeg(page);
        return Ok(());
    }
    page.set_status("Connecting…", "");
    // No STUN/TURN servers: on a LAN (or Tailscale) the devices reach each other directly.
    let config = RtcConfiguration::new();
    config.set_ice_servers(&Array::new());
    let pc = RtcPeerConnection::new_with_configuration(&config)?;
    let (stream, track) = {
        let state = page.state.borrow();
        let stream = state.stream.clone().ok_or("no camera")?;
        let track: MediaStreamTrack = stream.get_video_tracks().get(0).dyn_into()?;
        (stream, track)
    };
    let init = RtcRtpTransceiverInit::new();
    init.set_direction(RtcRtpTransceiverDirection::Sendonly);
    init.set_streams(&Array::of1(&stream));
    let transceiver = pc.add_transceiver_with_media_stream_track_and_init(&track, &init);
    prefer_h264(&transceiver);
    {
        let mut state = page.state.borrow_mut();
        state.pc = Some(pc.clone());
        state.sender = Some(transceiver.sender());
    }

    let channel: RtcDataChannel = pc.create_data_channel("control");
    let on_message = {
        let (page, pc) = (page.clone(), pc.clone());
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let current = page.state.borrow().pc.clone();
            if current.as_ref() == Some(&pc) {
                if let Some(text) = event.data().as_string() {
                    handle_server_message(&page, &text);
                }
            }
        })
    };
    channel.set_onmessage(Some(on_message.into_js_value().unchecked_ref()));

    // Aborting the signalling request on timeout tells the PC to drop this attempt, so
    // a late answer can't take over from the fallback connection we switch to.
    let signalling = AbortController::new()?;
    let timer = {
        let (page, pc, signalling) = (page.clone(), pc.clone(), signalling.clone());
        set_timeout(&page.window.clone(), RTC_CONNECT_TIMEOUT_MS, move || {
            let current = page.state.borrow().pc.clone();
            if current.as_ref() == Some(&pc) && pc.connection_state() != RtcPeerConnectionState::Connected {
                signalling.abort();
                rtc_failed(&page, &pc);
            }
        })
    };
    let disconnect_timer = Rc::new(RefCell::new(None::<i32>));
    let on_state = {
        let (page, pc) = (page.clone(), pc.clone());
        Closure::<dyn FnMut()>::new(move || {
            let current = page.state.borrow().pc.clone();
            if current.as_ref() != Some(&pc) {
                return;
            }
            if let Some(t) = disconnect_timer.borrow_mut().take() {
                page.window.clear_timeout_with_handle(t);
            }
            match pc.connection_state() {
                RtcPeerConnectionState::Disconnected => {
                    let (p, c) = (page.clone(), pc.clone());
                    let t = set_timeout(&page.window, RTC_DISCONNECT_GRACE_MS, move || {
                        let current = p.state.borrow().pc.clone();
                        if current.as_ref() == Some(&c) {
                            rtc_failed(&p, &c);
                        }
                    });
                    *disconnect_timer.borrow_mut() = Some(t);
                }
                RtcPeerConnectionState::Connected => {
                    page.window.clear_timeout_with_handle(timer);
                    page.state.borrow_mut().rtc_ever_connected = true;
                    page.set_status("Streaming (WebRTC)", "ok");
                    let p = page.clone();
                    spawn_local(async move { apply_sender_limits(&p).await });
                }
                RtcPeerConnectionState::Failed | RtcPeerConnectionState::Closed => {
                    page.window.clear_timeout_with_handle(timer);
                    rtc_failed(&page, &pc);
                }
                _ => {}
            }
        })
    };
    pc.set_onconnectionstatechange(Some(on_state.into_js_value().unchecked_ref()));

    let offer = JsFuture::from(pc.create_offer()).await?;
    JsFuture::from(pc.set_local_description(offer.unchecked_ref())).await?;
    wait_for_ice_gathering(page, &pc, 3000).await;
    let sdp = pc.local_description().map(|d| d.sdp()).ok_or("no local description")?;
    let request = OfferRequest { sdp, id: page.client_id.clone(), info: client_info(page) };
    let response = post_json(page, "/offer", &serde_json::to_string(&request).unwrap(), Some(&signalling)).await?;
    if response.status() == 403 {
        page.window.clear_timeout_with_handle(timer);
        pc.close();
        let mut state = page.state.borrow_mut();
        state.pc = None;
        state.sender = None;
        drop(state);
        page.set_status("Pairing key rejected — scan the QR code on the PC again", "bad");
        return Ok(());
    }
    let text = JsFuture::from(response.text()?).await?.as_string().unwrap_or_default();
    if !response.ok() {
        return Err(format!("offer rejected: {} {text}", response.status()).into());
    }
    let answer: OfferResponse = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if page.state.borrow().pc.as_ref() != Some(&pc) {
        return Ok(());
    }
    apply_config(page, answer.config);
    let description = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
    description.set_sdp(&answer.sdp);
    JsFuture::from(pc.set_remote_description(&description)).await?;
    Ok(())
}

fn rtc_failed(page: &Rcp, pc: &RtcPeerConnection) {
    pc.close();
    let ever_connected = {
        let mut state = page.state.borrow_mut();
        if state.pc.as_ref() != Some(pc) {
            return;
        }
        state.pc = None;
        state.sender = None;
        if state.replaced || !state.running {
            return;
        }
        state.rtc_ever_connected
    };
    if !ever_connected {
        fallback_to_jpeg(page); // WebRTC never worked here: UDP is probably blocked
        return;
    }
    page.set_status("Reconnecting…", "bad");
    let p = page.clone();
    set_timeout(&page.window, 2000, move || connect_webrtc(&p));
}

fn fallback_to_jpeg(page: &Rcp) {
    log("falling back to JPEG frames");
    page.state.borrow_mut().transport = Transport::WebSocket;
    connect_ws(page);
}

fn reconnect(page: &Rcp) {
    let transport = page.state.borrow().transport;
    match transport {
        Transport::WebRtc => connect_webrtc(page),
        Transport::WebSocket => connect_ws(page),
        Transport::Http => {}
    }
}

// ---------------------------------------------------------------- JPEG fallback transports

fn send_hello(page: &Rcp) {
    let ws = {
        let state = page.state.borrow();
        if !state.ws_open {
            return;
        }
        state.ws.clone()
    };
    if let Some(ws) = ws {
        let hello = serde_json::to_string(&ClientMessage::Hello(client_info(page))).unwrap();
        let _ = ws.send_with_str(&hello);
    }
}

fn connect_ws(page: &Rcp) {
    if page.state.borrow().replaced {
        return;
    }
    let location = page.window.location();
    let proto = if location.protocol().unwrap_or_default() == "https:" { "wss:" } else { "ws:" };
    let url = format!(
        "{proto}//{}/ws?id={}&k={}",
        location.host().unwrap_or_default(),
        page.client_id,
        String::from(js_sys::encode_uri_component(&page.key))
    );
    let Ok(ws) = WebSocket::new(&url) else { return };
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
    page.state.borrow_mut().ws = Some(ws.clone());
    page.set_status("Connecting…", "");

    let is_current = |page: &Rcp, ws: &WebSocket| page.state.borrow().ws.as_ref() == Some(ws);
    let open_timer = {
        let ws = ws.clone();
        set_timeout(&page.window, 5000, move || {
            if ws.ready_state() != WebSocket::OPEN {
                let _ = ws.close();
            }
        })
    };
    // Every handler ignores sockets that have since been superseded by a newer one,
    // otherwise a stale socket's close would tear down the live connection's state.
    let on_open = {
        let (page, ws) = (page.clone(), ws.clone());
        Closure::<dyn FnMut()>::new(move || {
            page.window.clear_timeout_with_handle(open_timer);
            if !is_current(&page, &ws) {
                let _ = ws.close();
                return;
            }
            {
                let mut state = page.state.borrow_mut();
                state.ws_open = true;
                state.ws_ever_opened = true;
                state.ws_failures = 0;
                state.in_flight = 0;
            }
            page.set_status("Streaming", "ok");
            send_hello(&page);
        })
    };
    let on_message = {
        let (page, ws) = (page.clone(), ws.clone());
        Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if !is_current(&page, &ws) {
                return;
            }
            let Some(text) = event.data().as_string() else { return };
            if text == protocol::ACK || text == protocol::BAD {
                let mut state = page.state.borrow_mut();
                state.in_flight = state.in_flight.saturating_sub(1);
            } else {
                handle_server_message(&page, &text);
            }
        })
    };
    let on_close = {
        let (page, ws) = (page.clone(), ws.clone());
        Closure::<dyn FnMut()>::new(move || {
            page.window.clear_timeout_with_handle(open_timer);
            if !is_current(&page, &ws) {
                return;
            }
            let (replaced, give_up) = {
                let mut state = page.state.borrow_mut();
                let was_open = state.ws_open;
                state.ws_open = false;
                state.in_flight = 0;
                if !was_open {
                    state.ws_failures += 1;
                }
                // Some iOS versions refuse wss:// to a self-signed certificate even after
                // the user accepted it for the page. Plain HTTPS POSTs do work.
                (state.replaced, !state.ws_ever_opened && state.ws_failures >= 2)
            };
            if replaced {
                return;
            }
            if give_up {
                start_http(&page);
                return;
            }
            page.set_status("Reconnecting…", "bad");
            let p = page.clone();
            set_timeout(&page.window, 1000, move || connect_ws(&p));
        })
    };
    ws.set_onopen(Some(on_open.into_js_value().unchecked_ref()));
    ws.set_onmessage(Some(on_message.into_js_value().unchecked_ref()));
    ws.set_onclose(Some(on_close.into_js_value().unchecked_ref()));
}

fn start_http(page: &Rcp) {
    page.state.borrow_mut().transport = Transport::Http;
    let page = page.clone();
    spawn_local(async move {
        if let Ok(response) = JsFuture::from(page.window.fetch_with_str("/config")).await {
            let response: Response = response.unchecked_into();
            if let Ok(text) = response.text() {
                if let Ok(text) = JsFuture::from(text).await {
                    if let Ok(config) = serde_json::from_str(&text.as_string().unwrap_or_default()) {
                        apply_config(&page, config);
                    }
                }
            }
        }
        page.set_status("Streaming (HTTP)", "ok");
    });
}

async fn post_json(page: &Rcp, url: &str, body: &str, abort: Option<&AbortController>) -> Result<Response, JsValue> {
    let headers = web_sys::Headers::new()?;
    headers.set("Content-Type", "application/json")?;
    headers.set(protocol::KEY_HEADER, &page.key)?;
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_headers(&headers);
    init.set_body(&body.into());
    if let Some(abort) = abort {
        init.set_signal(Some(&abort.signal()));
    }
    Ok(JsFuture::from(page.window.fetch_with_str_and_init(url, &init)).await?.unchecked_into())
}

fn transport_ready(state: &State) -> bool {
    match state.transport {
        Transport::Http => true,
        Transport::WebSocket => state.ws_open,
        Transport::WebRtc => false,
    }
}

fn send_frame(page: &Rcp, blob: Blob) {
    let (transport, ws) = {
        let mut state = page.state.borrow_mut();
        state.in_flight += 1;
        state.sent_frames += 1.0;
        state.sent_bytes += blob.size();
        (state.transport, state.ws.clone())
    };
    if transport == Transport::WebSocket {
        if let Some(ws) = ws {
            let _ = ws.send_with_blob(&blob);
        }
        return;
    }
    let page = page.clone();
    spawn_local(async move {
        let result = async {
            let headers = web_sys::Headers::new()?;
            headers.set("Content-Type", "image/jpeg")?;
            headers.set(protocol::CLIENT_ID_HEADER, &page.client_id)?;
            headers.set(protocol::KEY_HEADER, &page.key)?;
            let abort = AbortController::new()?;
            let a = abort.clone();
            // Never let a hung request block sending.
            let timer = set_timeout(&page.window, 5000, move || a.abort());
            let init = RequestInit::new();
            init.set_method("POST");
            init.set_headers(&headers);
            init.set_body(&blob);
            init.set_signal(Some(&abort.signal()));
            let response = JsFuture::from(page.window.fetch_with_str_and_init("/frame", &init)).await;
            page.window.clear_timeout_with_handle(timer);
            Ok::<_, JsValue>(response?.unchecked_into::<Response>().status())
        }
        .await;
        match result {
            Ok(403) => page.set_status("Pairing key rejected — scan the QR code on the PC again", "bad"),
            Ok(_) => page.set_status("Streaming (HTTP)", "ok"),
            Err(_) => page.set_status("PC unreachable", "bad"),
        }
        let mut state = page.state.borrow_mut();
        state.in_flight = state.in_flight.saturating_sub(1);
    });
}

fn capture_loop(page: &Rcp) {
    let started = now();
    let (running, fps, quality, max_w, max_h, ready) = {
        let state = page.state.borrow();
        let ready = transport_ready(&state) && state.in_flight < MAX_IN_FLIGHT && !state.encoding;
        (state.running, state.config.fps, state.config.quality, state.config.width, state.config.height, ready)
    };
    if !running {
        return;
    }
    let (vw, vh) = (page.video.video_width(), page.video.video_height());
    if vw > 0 && vh > 0 && ready && !page.hidden() {
        // Never send more pixels than the PC output can use.
        let scale = (max_w as f64 / vw as f64).min(max_h as f64 / vh as f64).min(1.0);
        let (cw, ch) = ((vw as f64 * scale).round() as u32, (vh as f64 * scale).round() as u32);
        if page.canvas.width() != cw || page.canvas.height() != ch {
            page.canvas.set_width(cw);
            page.canvas.set_height(ch);
        }
        if let Ok(Some(ctx)) = page.canvas.get_context("2d") {
            let ctx: CanvasRenderingContext2d = ctx.unchecked_into();
            let _ = ctx.draw_image_with_html_video_element_and_dw_and_dh(&page.video, 0.0, 0.0, cw as f64, ch as f64);
            page.state.borrow_mut().encoding = true;
            let p = page.clone();
            let callback = Closure::once_into_js(move |blob: JsValue| {
                p.state.borrow_mut().encoding = false;
                let ready = transport_ready(&p.state.borrow());
                if let (Ok(blob), true) = (blob.dyn_into::<Blob>(), ready) {
                    send_frame(&p, blob);
                }
            });
            let _ = page.canvas.to_blob_with_type_and_encoder_options(
                callback.unchecked_ref(),
                "image/jpeg",
                &quality.into(),
            );
        }
    }
    let interval = 1000.0 / fps.max(1) as f64;
    let elapsed = now() - started;
    let p = page.clone();
    set_timeout(&page.window, (interval - elapsed).max(0.0) as i32, move || capture_loop(&p));
}

// ---------------------------------------------------------------- stats

fn start_stats(page: &Rcp) {
    let last = Rc::new(RefCell::new((now(), 0.0f64, 0.0f64)));
    let page_for_tick = page.clone();
    let tick = Closure::<dyn FnMut()>::new(move || {
        let (page, last) = (page_for_tick.clone(), last.clone());
        spawn_local(async move {
            // In WebRTC mode the browser does the sending; read its counters instead.
            let pc = {
                let state = page.state.borrow();
                (state.transport == Transport::WebRtc).then(|| state.pc.clone()).flatten()
            };
            if let Some(pc) = pc {
                if let Ok(report) = JsFuture::from(pc.get_stats()).await {
                    let map: js_sys::Map = report.unchecked_into();
                    map.for_each(&mut |value, _| {
                        if get(&value, "type").as_string().as_deref() == Some("outbound-rtp")
                            && get(&value, "kind").as_string().as_deref() == Some("video")
                        {
                            let mut state = page.state.borrow_mut();
                            if let Some(f) =
                                get(&value, "framesSent").as_f64().or(get(&value, "framesEncoded").as_f64())
                            {
                                state.sent_frames = f;
                            }
                            if let Some(b) = get(&value, "bytesSent").as_f64() {
                                state.sent_bytes = b;
                            }
                        }
                    });
                }
            }
            let (frames, bytes) = {
                let state = page.state.borrow();
                (state.sent_frames, state.sent_bytes)
            };
            let t = now();
            let (t0, f0, b0) = *last.borrow();
            *last.borrow_mut() = (t, frames, bytes);
            let dt = ((t - t0) / 1000.0).max(0.001);
            // Counters restart with each new peer connection: never show negative rates.
            let fps = (frames - f0).max(0.0) / dt;
            let kbps = (bytes - b0).max(0.0) * 8.0 / 1000.0 / dt;
            let (vw, vh) = (page.video.video_width(), page.video.video_height());
            let res = if vw > 0 { format!("{vw}×{vh} · ") } else { String::new() };
            let rate = if kbps >= 1000.0 { format!("{:.1} Mbps", kbps / 1000.0) } else { format!("{kbps:.0} kbps") };
            page.id::<HtmlElement>("stats").set_text_content(Some(&format!("{res}{fps:.0} fps · {rate}")));
        });
    });
    let _ =
        page.window.set_interval_with_callback_and_timeout_and_arguments_0(tick.into_js_value().unchecked_ref(), 1000);
}

// ---------------------------------------------------------------- startup

async fn start(page: Rcp) {
    {
        let mut state = page.state.borrow_mut();
        if state.starting || state.running {
            return; // double tap
        }
        state.starting = true;
    }
    start_inner(&page).await;
    page.state.borrow_mut().starting = false;
}

async fn start_inner(page: &Rcp) {
    page.id::<HtmlElement>("error").set_text_content(Some(""));
    let secure = page.window.is_secure_context();
    if !secure || page.window.navigator().media_devices().is_err() {
        page.show_error(
            "Camera access needs a secure (https://) connection. Open the https:// address shown on the PC.",
        );
        return;
    }
    if page.key.is_empty() {
        page.show_error("Open this page by scanning the QR code shown on the PC (the link contains a pairing key).");
        return;
    }
    request_wake_lock(page); // must start inside the tap handler on iOS
    if let Err(e) = open_camera(page).await {
        let name = get(&e, "name").as_string().unwrap_or_default();
        page.show_error(&if name == "NotAllowedError" {
            "Camera permission was denied. Allow camera access for this site in Settings › Safari › Camera, then reload."
                .to_string()
        } else {
            format!("Could not open the camera: {}", error_text(&e))
        });
        return;
    }
    for (id, hide) in [("start-screen", true), ("top", false), ("bottom", false)] {
        let list = page.id::<HtmlElement>(id).class_list();
        let _ = if hide { list.add_1("hidden") } else { list.remove_1("hidden") };
    }
    page.state.borrow_mut().running = true;
    connect_webrtc(page);
    capture_loop(page); // idles unless a JPEG fallback transport is active
}

fn on(page: &Rcp, id: &str, event: &str, handler: impl FnMut() + 'static) {
    let closure = Closure::<dyn FnMut()>::new(handler);
    let _ = page.id::<HtmlElement>(id).add_event_listener_with_callback(event, closure.into_js_value().unchecked_ref());
}

fn storage_id(window: &Window) -> String {
    let storage = window.session_storage().ok().flatten();
    if let Some(id) = storage.as_ref().and_then(|s| s.get_item("clientId").ok().flatten()) {
        return id;
    }
    let id = random_id();
    if let Some(s) = storage {
        let _ = s.set_item("clientId", &id);
    }
    id
}

/// Pairing key from the QR code link. Remembered so a plain reload keeps working.
fn pairing_key(window: &Window) -> String {
    let from_url = window
        .location()
        .search()
        .ok()
        .and_then(|s| web_sys::UrlSearchParams::new_with_str(&s).ok())
        .and_then(|p| p.get("k"));
    let storage = window.local_storage().ok().flatten();
    match from_url {
        Some(key) => {
            if let Some(s) = storage {
                let _ = s.set_item("pairingKey", &key);
            }
            key
        }
        None => storage.and_then(|s| s.get_item("pairingKey").ok().flatten()).unwrap_or_default(),
    }
}

#[wasm_bindgen(start)]
pub fn main() {
    let window = web_sys::window().expect("window");
    let document = window.document().expect("document");
    let page = Rc::new(Page {
        video: el(&document, "preview"),
        canvas: document.create_element("canvas").expect("canvas").unchecked_into(),
        client_id: storage_id(&window),
        key: pairing_key(&window),
        state: RefCell::new(State {
            config: StreamConfig::default(),
            facing: "environment",
            requested: None,
            stream: None,
            transport: Transport::WebRtc,
            pc: None,
            sender: None,
            rtc_ever_connected: false,
            ws: None,
            ws_open: false,
            ws_ever_opened: false,
            ws_failures: 0,
            in_flight: 0,
            encoding: false,
            running: false,
            starting: false,
            replaced: false,
            mirror: true,
            sent_frames: 0.0,
            sent_bytes: 0.0,
        }),
        window,
        document,
    });

    let p = page.clone();
    on(&page, "start", "click", move || spawn_local(start(p.clone())));
    let p = page.clone();
    on(&page, "top", "click", move || {
        let replaced = std::mem::replace(&mut p.state.borrow_mut().replaced, false);
        if replaced {
            reconnect(&p);
        }
    });
    let p = page.clone();
    on(&page, "flip", "click", move || {
        {
            let mut state = p.state.borrow_mut();
            state.facing = if state.facing == "user" { "environment" } else { "user" };
        }
        let p = p.clone();
        spawn_local(async move {
            if let Err(e) = open_camera(&p).await {
                p.set_status(&format!("Camera error: {}", error_text(&e)), "bad");
            }
        });
    });
    let p = page.clone();
    on(&page, "quality", "change", move || {
        let value = p.id::<HtmlSelectElement>("quality").value();
        p.state.borrow_mut().requested =
            value.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
        let p = p.clone();
        spawn_local(async move {
            if let Err(e) = open_camera(&p).await {
                p.set_status(&format!("Camera error: {}", error_text(&e)), "bad");
            }
        });
    });
    let p = page.clone();
    on(&page, "mirror", "click", move || {
        {
            let mut state = p.state.borrow_mut();
            state.mirror = !state.mirror;
        }
        update_mirror(&p);
    });
    let p = page.clone();
    on(&page, "dimbtn", "click", move || {
        let _ = p.id::<HtmlElement>("dim").class_list().add_1("on");
    });
    let p = page.clone();
    on(&page, "dim", "click", move || {
        let _ = p.id::<HtmlElement>("dim").class_list().remove_1("on");
    });

    let p = page.clone();
    let on_visibility = Closure::<dyn FnMut()>::new(move || {
        if p.hidden() || !p.state.borrow().running {
            return;
        }
        // The wake lock is released whenever the page is hidden, and iOS may have
        // stopped the camera meanwhile. iOS un-mutes the track shortly after the page
        // returns; only reopen it if it is still dead after that.
        request_wake_lock(&p);
        let p2 = p.clone();
        set_timeout(&p.window, 1000, move || recover_camera(&p2));
    });
    let _ = page
        .document
        .add_event_listener_with_callback("visibilitychange", on_visibility.into_js_value().unchecked_ref());
    start_stats(&page);
    // Handlers are attached: tell automated tests the page is usable.
    if let Some(root) = page.document.document_element() {
        let _ = root.set_attribute("data-ready", "");
    }
}
