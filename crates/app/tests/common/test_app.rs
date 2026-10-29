//! The app running on localhost, and a client that talks to it over HTTPS.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use iphone_webcam::app::{App, Settings};
use iphone_webcam::hub::FitMode;

use super::images::average_rgb;

pub const OUTPUT_WIDTH: u32 = 1280;
pub const OUTPUT_HEIGHT: u32 = 720;
pub const CAMERA_NAME: &str = "iPhone Webcam";
const OUTPUT_FPS: u32 = 30;
const JPEG_QUALITY: f64 = 0.75;
const BITRATE_MBPS: f64 = 6.0;
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The app's `/status` JSON.
pub type Status = serde_json::Value;

/// A running app with its own data directory, stopped and removed on drop.
pub struct TestApp {
    pub app: App,
    pub client: AppClient,
    pub data_dir: PathBuf,
}

impl TestApp {
    /// Start the app on a free localhost port, without a virtual camera.
    pub fn start() -> TestApp {
        Self::start_with(false)
    }

    /// Start the app with the real virtual camera (needs `iphone-webcam install`).
    pub fn start_with_virtual_camera() -> TestApp {
        Self::start_with(true)
    }

    fn start_with(camera: bool) -> TestApp {
        let run_id = rand::random::<u64>();
        let data_dir = std::env::temp_dir().join(format!("iphone-webcam-it-{run_id}"));
        let settings = Settings {
            host: "127.0.0.1".parse().unwrap(),
            port: 0,
            admin_port: 0,
            width: OUTPUT_WIDTH,
            height: OUTPUT_HEIGHT,
            fps: OUTPUT_FPS,
            quality: JPEG_QUALITY,
            bitrate_mbps: BITRATE_MBPS,
            mode: FitMode::Fit,
            camera,
            camera_name: CAMERA_NAME.into(),
            data_dir: data_dir.clone(),
        };
        let app = App::start(settings).expect("app starts");
        let client = AppClient::new(&app);
        TestApp { app, client, data_dir }
    }

    /// Average colour of the hub's current output frame.
    pub fn output_color(&self) -> [f64; 3] {
        let frame = self.app.hub.latest();
        average_rgb(&frame.rgb)
    }
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.app.stop();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

/// Enough to talk to a running app; cheap to clone into other tasks.
#[derive(Clone)]
pub struct AppClient {
    pub base_url: String,
    pub key: String,
    pub http: reqwest::Client,
}

impl AppClient {
    fn new(app: &App) -> AppClient {
        let base_url = format!("https://127.0.0.1:{}", app.bound.port());
        let key = app.key().to_string();
        // The app serves a self-signed certificate.
        let http = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
        AppClient { base_url, key, http }
    }

    /// The full URL of `path` (which may include a query).
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    pub async fn get(&self, path: &str) -> reqwest::Response {
        self.http.get(self.url(path)).send().await.unwrap()
    }

    pub async fn status(&self) -> Status {
        let response = self.get(&format!("/status?k={}", self.key)).await;
        response.json().await.unwrap()
    }

    /// Poll until `framesIn` reaches `count` or `timeout` passes; returns the last status.
    pub async fn wait_for_frames(&self, count: u64, timeout: Duration) -> Status {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.status().await;
            let frames_in = status["framesIn"].as_u64().unwrap();
            if frames_in >= count || Instant::now() > deadline {
                return status;
            }
            tokio::time::sleep(STATUS_POLL_INTERVAL).await;
        }
    }
}
