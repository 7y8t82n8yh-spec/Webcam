//! The real phone page (Rust/WebAssembly) in a headless Chromium with a fake camera:
//! streaming over each transport, the fallbacks between them, and its error messages.
//!
//! Skipped when no browser is found or the page wasn't built (`cargo xtask web`).
//! Set CHROMIUM_PATH to choose the browser.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::handler::Handler;
use chromiumoxide::Page;
use common::test_app::{Status, TestApp};
use futures_util::StreamExt;
use tokio::sync::{Mutex, MutexGuard};
use tokio::task::JoinHandle;

const DEFAULT_BROWSER_PATH: &str = "/opt/pw-browsers/chromium";
/// Generous: Chromium starts slowly on small CI machines.
const BROWSER_LAUNCH_TIMEOUT: Duration = Duration::from_secs(60);
const PAGE_READY_POLL_ATTEMPTS: u32 = 100;
const PAGE_READY_POLL_INTERVAL: Duration = Duration::from_millis(50);
const FRAMES_TO_WAIT_FOR: u64 = 10;
const STREAMING_TIMEOUT: Duration = Duration::from_secs(30);
const ERROR_MESSAGE_DELAY: Duration = Duration::from_millis(500);

// Init scripts that take transports away, to exercise the page's fallbacks.
const NO_WEBRTC: &str = "delete window.RTCPeerConnection;";
/// Like iOS refusing wss:// to a self-signed certificate.
const NO_WEBSOCKET: &str = "window.WebSocket = class extends WebSocket { \
     constructor(url) { super(url.replace('/ws?', '/no-such-endpoint?')); } };";

#[tokio::test(flavor = "multi_thread")]
async fn page_streams_over_webrtc_or_falls_back() {
    let test_app = TestApp::start();
    let Some(outcome) = stream_from_page(&test_app, &[]).await else { return };

    let status = &outcome.status;
    let message = &outcome.message;
    assert!(status["framesIn"].as_u64().unwrap() >= FRAMES_TO_WAIT_FOR, "{status} / {message}");
    // Chromium without H.264 can't use our WebRTC and must fall back on its own;
    // Chrome uses WebRTC. Either way the page must say which one is in use.
    match status["transport"].as_str().unwrap() {
        "WebRTC" => assert_eq!(message, "Streaming (WebRTC)"),
        "JPEG" => assert_eq!(message, "Streaming"),
        other => panic!("unexpected transport {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn page_without_webrtc_uses_websocket() {
    let test_app = TestApp::start();
    let Some(outcome) = stream_from_page(&test_app, &[NO_WEBRTC]).await else { return };

    assert_eq!(outcome.status["transport"], "JPEG", "{} / {}", outcome.status, outcome.message);
    assert_eq!(outcome.message, "Streaming");
}

#[tokio::test(flavor = "multi_thread")]
async fn page_falls_back_to_https_posts() {
    let test_app = TestApp::start();
    let Some(outcome) = stream_from_page(&test_app, &[NO_WEBRTC, NO_WEBSOCKET]).await else { return };

    assert_eq!(outcome.status["transport"], "JPEG", "{} / {}", outcome.status, outcome.message);
    assert_eq!(outcome.message, "Streaming (HTTP)");
}

#[tokio::test(flavor = "multi_thread")]
async fn page_without_key_explains_what_to_do() {
    let test_app = TestApp::start();
    let Some(browser) = launch_if_page_built(&test_app).await else { return };
    let page_url = test_app.client.url("/");
    let page = open_phone_page(&browser, &[], page_url).await;

    click_start(&page).await;
    tokio::time::sleep(ERROR_MESSAGE_DELAY).await;
    let error = element_text(&page, "error").await;
    browser.close().await;

    assert!(error.contains("QR code"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn double_tap_starts_once() {
    const COUNT_PEER_CONNECTIONS: &str = "window.__peers = 0; const P = window.RTCPeerConnection; \
         window.RTCPeerConnection = class extends P { constructor(c) { super(c); window.__peers++; } };";
    const DOUBLE_TAP_START: &str = "const b = document.getElementById('start'); b.click(); b.click();";
    let test_app = TestApp::start();
    let Some(browser) = launch_if_page_built(&test_app).await else { return };
    let page = open_phone_page(&browser, &[COUNT_PEER_CONNECTIONS], paired_page_url(&test_app)).await;

    page.evaluate(DOUBLE_TAP_START).await.unwrap();
    test_app.client.wait_for_frames(FRAMES_TO_WAIT_FOR, STREAMING_TIMEOUT).await;
    let peer_connections: u64 = page.evaluate("window.__peers").await.unwrap().into_value().unwrap();
    browser.close().await;

    assert_eq!(peer_connections, 1);
}

/// What the app and the page reported once the page had streamed for a while.
struct StreamOutcome {
    status: Status,
    /// The page's state line, or its error line when there is no state.
    message: String,
}

/// Open the paired page with `init_scripts` injected, press Start and wait for frames;
/// `None` when the test should be skipped.
async fn stream_from_page(test_app: &TestApp, init_scripts: &[&str]) -> Option<StreamOutcome> {
    let browser = launch_if_page_built(test_app).await?;
    let page = open_phone_page(&browser, init_scripts, paired_page_url(test_app)).await;

    click_start(&page).await;
    let status = test_app.client.wait_for_frames(FRAMES_TO_WAIT_FOR, STREAMING_TIMEOUT).await;
    let state = element_text(&page, "state").await;
    let error = element_text(&page, "error").await;
    browser.close().await;

    let message = if state.is_empty() { error } else { state };
    Some(StreamOutcome { status, message })
}

/// The phone page with the pairing key, as the QR code links to it.
fn paired_page_url(test_app: &TestApp) -> String {
    test_app.client.url(&format!("/?k={}", test_app.client.key))
}

/// A browser, or `None` (skip) when the page wasn't built or there is no browser.
async fn launch_if_page_built(test_app: &TestApp) -> Option<HeadlessBrowser> {
    if !page_built(test_app).await {
        return None;
    }
    HeadlessBrowser::launch(test_app).await
}

async fn page_built(test_app: &TestApp) -> bool {
    let response = test_app.client.get("/pkg/web_bg.wasm").await;
    let wasm = response.bytes().await.unwrap();
    if wasm.is_empty() {
        eprintln!("skipping: phone page not built (run `cargo xtask web`)");
    }
    !wasm.is_empty()
}

/// A headless Chromium with its own profile, and the task running its event loop.
struct HeadlessBrowser {
    browser: Browser,
    event_loop: JoinHandle<()>,
}

impl HeadlessBrowser {
    /// `None` (skip) only when there is no browser.
    async fn launch(test_app: &TestApp) -> Option<HeadlessBrowser> {
        let Some(path) = browser_path() else {
            eprintln!("skipping: no Chromium (set CHROMIUM_PATH)");
            return None;
        };
        let config = BrowserConfig::builder()
            .chrome_executable(path)
            .user_data_dir(test_app.data_dir.join("browser-profile"))
            .no_sandbox()
            .arg("use-fake-device-for-media-stream")
            .arg("use-fake-ui-for-media-stream")
            .arg("ignore-certificate-errors")
            .launch_timeout(BROWSER_LAUNCH_TIMEOUT)
            .build()
            .unwrap();
        // One browser at a time: several starting at once time out on small CI machines.
        static ONE_AT_A_TIME: Mutex<()> = Mutex::const_new(());
        let turn = ONE_AT_A_TIME.lock().await;
        let (browser, handler) = Browser::launch(config).await.expect("launch browser");
        // The turn ends when `close` aborts this task, or the runtime drops it on a panic.
        let event_loop = tokio::spawn(run_event_loop(handler, turn));
        Some(HeadlessBrowser { browser, event_loop })
    }

    async fn close(mut self) {
        self.browser.close().await.ok();
        self.event_loop.abort();
    }
}

fn browser_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHROMIUM_PATH") {
        return Some(path.into());
    }
    let default = PathBuf::from(DEFAULT_BROWSER_PATH);
    default.exists().then_some(default)
}

/// Run the browser's event loop, holding the turn until the task ends.
async fn run_event_loop(mut handler: Handler, turn: MutexGuard<'static, ()>) {
    let _turn = turn;
    while handler.next().await.is_some() {}
}

/// A new tab with `init_scripts` injected, navigated to `url` and ready to use.
async fn open_phone_page(browser: &HeadlessBrowser, init_scripts: &[&str], url: String) -> Page {
    let page = browser.browser.new_page("about:blank").await.unwrap();
    for script in init_scripts {
        page.evaluate_on_new_document(*script).await.unwrap();
    }
    page.goto(url).await.unwrap();
    wait_until_ready(&page).await;
    page
}

/// Wait until the WebAssembly has started and attached its handlers.
async fn wait_until_ready(page: &Page) {
    for _ in 0..PAGE_READY_POLL_ATTEMPTS {
        if page.find_element("html[data-ready]").await.is_ok() {
            return;
        }
        tokio::time::sleep(PAGE_READY_POLL_INTERVAL).await;
    }
    panic!("phone page never started");
}

async fn click_start(page: &Page) {
    let button = page.find_element("#start").await.unwrap();
    button.click().await.unwrap();
}

/// The text of the element with `id`, or empty if it can't be read.
async fn element_text(page: &Page, id: &str) -> String {
    let script = format!("document.getElementById('{id}').textContent");
    let Ok(result) = page.evaluate(script).await else {
        return String::new();
    };
    result.into_value::<String>().unwrap_or_default()
}
