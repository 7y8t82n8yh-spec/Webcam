//! The real phone page (Rust/WebAssembly) in a headless Chromium with a fake camera.
//!
//! Skipped when no browser is found or the page wasn't built (`cargo xtask web`).
//! Set CHROMIUM_PATH to choose the browser.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::handler::Handler;
use futures_util::StreamExt;

/// Init scripts that take transports away, to exercise the page's fallbacks.
const NO_WEBRTC: &str = "delete window.RTCPeerConnection;";
// Like iOS refusing wss:// to a self-signed certificate.
const NO_WEBSOCKET: &str = "window.WebSocket = class extends WebSocket { \
     constructor(url) { super(url.replace('/ws?', '/no-such-endpoint?')); } };";

fn browser_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHROMIUM_PATH") {
        return Some(path.into());
    }
    let default = PathBuf::from("/opt/pw-browsers/chromium");
    default.exists().then_some(default)
}

/// A headless browser with its own profile; `None` (skip) only when there is no browser.
async fn launch(t: &common::TestApp) -> Option<(Browser, tokio::task::JoinHandle<()>)> {
    let Some(path) = browser_path() else {
        eprintln!("skipping: no Chromium (set CHROMIUM_PATH)");
        return None;
    };
    let config = BrowserConfig::builder()
        .chrome_executable(path)
        .user_data_dir(t.dir.join("browser-profile"))
        .no_sandbox()
        .arg("use-fake-device-for-media-stream")
        .arg("use-fake-ui-for-media-stream")
        .arg("ignore-certificate-errors")
        .launch_timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    // One browser at a time: several starting at once time out on small CI machines.
    static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let turn = ONE_AT_A_TIME.lock().await;
    let (browser, handler) = Browser::launch(config).await.expect("launch browser");
    // The turn ends when the test aborts this task, or its runtime drops it on a panic.
    let task = tokio::spawn(drive(handler, turn));
    Some((browser, task))
}

/// Run the browser's event loop, holding the turn until the task ends.
async fn drive(mut handler: Handler, turn: tokio::sync::MutexGuard<'static, ()>) {
    let _turn = turn;
    while handler.next().await.is_some() {}
}

async fn page_built(t: &common::TestApp) -> bool {
    let response = t.client.get(format!("{}/pkg/web_bg.wasm", t.base)).send().await.unwrap();
    let wasm = response.bytes().await.unwrap();
    if wasm.is_empty() {
        eprintln!("skipping: phone page not built (run `cargo xtask web`)");
    }
    !wasm.is_empty()
}

/// Navigate and wait until the WebAssembly has started and attached its handlers.
async fn open(page: &chromiumoxide::Page, url: String) {
    page.goto(url).await.unwrap();
    for _ in 0..100 {
        if page.find_element("html[data-ready]").await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("phone page never started");
}

async fn text(page: &chromiumoxide::Page, id: &str) -> String {
    let Ok(result) = page.evaluate(format!("document.getElementById('{id}').textContent")).await else {
        return String::new();
    };
    result.into_value::<String>().unwrap_or_default()
}

/// Open the page with `scripts` injected, press Start, wait for `frames` frames.
async fn stream(t: &common::TestApp, scripts: &[&str], url_suffix: &str) -> Option<(serde_json::Value, String)> {
    if !page_built(t).await {
        return None;
    }
    let (mut browser, task) = launch(t).await?;
    let page = browser.new_page("about:blank").await.unwrap();
    for script in scripts {
        page.evaluate_on_new_document(*script).await.unwrap();
    }
    open(&page, format!("{}/{url_suffix}", t.base)).await;
    page.find_element("#start").await.unwrap().click().await.unwrap();
    let status = t.wait_for_frames(10, Duration::from_secs(30)).await;
    let state = text(&page, "state").await;
    let error = text(&page, "error").await;
    browser.close().await.ok();
    task.abort();
    let shown = if state.is_empty() { error } else { state };
    Some((status, shown))
}

#[tokio::test(flavor = "multi_thread")]
async fn page_streams_over_webrtc_or_falls_back() {
    let t = common::start();
    let url = format!("?k={}", t.key);
    let Some((status, state)) = stream(&t, &[], &url).await else { return };
    assert!(status["framesIn"].as_u64().unwrap() >= 10, "{status} / {state}");
    // Chromium without H.264 can't use our WebRTC and must fall back on its own;
    // Chrome uses WebRTC. Either way the status must say which one is in use.
    match status["transport"].as_str().unwrap() {
        "WebRTC" => assert_eq!(state, "Streaming (WebRTC)"),
        "JPEG" => assert_eq!(state, "Streaming"),
        other => panic!("unexpected transport {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn page_without_webrtc_uses_websocket() {
    let t = common::start();
    let url = format!("?k={}", t.key);
    let Some((status, state)) = stream(&t, &[NO_WEBRTC], &url).await else { return };
    assert_eq!(status["transport"], "JPEG", "{status} / {state}");
    assert_eq!(state, "Streaming");
}

#[tokio::test(flavor = "multi_thread")]
async fn page_falls_back_to_https_posts() {
    let t = common::start();
    let url = format!("?k={}", t.key);
    let Some((status, state)) = stream(&t, &[NO_WEBRTC, NO_WEBSOCKET], &url).await else { return };
    assert_eq!(status["transport"], "JPEG", "{status} / {state}");
    assert_eq!(state, "Streaming (HTTP)");
}

#[tokio::test(flavor = "multi_thread")]
async fn page_without_key_explains_what_to_do() {
    let t = common::start();
    if !page_built(&t).await {
        return;
    }
    let Some((mut browser, task)) = launch(&t).await else { return };
    let page = browser.new_page("about:blank").await.unwrap();
    open(&page, format!("{}/", t.base)).await;
    page.find_element("#start").await.unwrap().click().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let error = text(&page, "error").await;
    browser.close().await.ok();
    task.abort();
    assert!(error.contains("QR code"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn double_tap_starts_once() {
    const COUNT_PEERS: &str = "window.__peers = 0; const P = window.RTCPeerConnection; \
         window.RTCPeerConnection = class extends P { constructor(c) { super(c); window.__peers++; } };";
    let t = common::start();
    if !page_built(&t).await {
        return;
    }
    let Some((mut browser, task)) = launch(&t).await else { return };
    let page = browser.new_page("about:blank").await.unwrap();
    page.evaluate_on_new_document(COUNT_PEERS).await.unwrap();
    open(&page, format!("{}/?k={}", t.base, t.key)).await;
    page.evaluate("const b = document.getElementById('start'); b.click(); b.click();").await.unwrap();
    t.wait_for_frames(10, Duration::from_secs(30)).await;
    let peers: u64 = page.evaluate("window.__peers").await.unwrap().into_value().unwrap();
    browser.close().await.ok();
    task.abort();
    assert_eq!(peers, 1);
}
