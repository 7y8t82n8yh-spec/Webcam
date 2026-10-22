//! Wires the certificates, server, frame hub and virtual camera output together.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use protocol::StreamConfig;
use tokio::sync::oneshot;

use crate::camera::{self, CameraSink};
use crate::hub::{FitMode, FrameHub};
use crate::server::{self, Server};
use crate::{certs, netutil};

pub struct Settings {
    pub host: IpAddr,
    pub port: u16,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub quality: f64,
    pub bitrate_mbps: f64,
    pub mode: FitMode,
    pub camera: bool,
    pub camera_name: String,
    pub data_dir: PathBuf,
}

pub struct App {
    pub hub: Arc<FrameHub>,
    pub server: Arc<Server>,
    pub urls: Vec<String>,
    pub settings: Settings,
    pub bound: SocketAddr,
    /// What the virtual camera is doing, for display.
    pub camera_status: Arc<Mutex<String>>,
    shutdown: Option<oneshot::Sender<()>>,
    server_thread: Option<JoinHandle<()>>,
    camera_thread: Option<camera::OutputThread>,
}

/// The pairing key embedded in the QR code. Kept across runs so bookmarks keep working.
pub fn load_or_create_key(dir: &Path) -> Result<String> {
    let path = dir.join("pairing-key.txt");
    if let Ok(key) = std::fs::read_to_string(&path) {
        let key = key.trim().to_string();
        if key.len() >= 8 {
            return Ok(key);
        }
    }
    let alphabet = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let key: String = (0..16).map(|_| alphabet[rand::random_range(0..alphabet.len())] as char).collect();
    std::fs::create_dir_all(dir)?;
    std::fs::write(&path, &key)?;
    Ok(key)
}

/// Body of the server thread: serve until `shutdown` fires, reporting the bound address
/// (or why it couldn't listen) on `bound_tx`.
fn run_server(
    server: Arc<Server>,
    addr: SocketAddr,
    cert: Vec<u8>,
    cert_key: Vec<u8>,
    shutdown: oneshot::Receiver<()>,
    bound_tx: std::sync::mpsc::Sender<Result<SocketAddr, String>>,
) {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime");
    let router = server.clone().router();
    let bound_ok = bound_tx.clone();
    let on_shutdown = async move {
        let _ = shutdown.await;
        server.shutdown();
    };
    let on_bound = move |bound| {
        let _ = bound_ok.send(Ok(bound));
    };
    let serving = server::serve(router, addr, cert, cert_key, on_shutdown, on_bound);
    if let Err(err) = runtime.block_on(serving) {
        let _ = bound_tx.send(Err(format!("{err:#}")));
    }
}

/// The page URLs to show, one per LAN address (localhost if there is none).
fn pairing_urls(ips: &[IpAddr], port: u16, key: &str) -> Vec<String> {
    let mut url_ips = ips.to_vec();
    if url_ips.is_empty() {
        url_ips.push(IpAddr::from([127, 0, 0, 1]));
    }
    url_ips.iter().map(|ip| format!("https://{ip}:{port}/?k={key}")).collect()
}

/// Open the virtual camera unless it's disabled, recording the outcome in `status`.
fn open_camera(settings: &Settings, status: &Mutex<String>) -> Option<Box<dyn CameraSink>> {
    if !settings.camera {
        *status.lock().unwrap() = "disabled (--no-camera)".into();
        return None;
    }
    match camera::open(&settings.camera_name, settings.width, settings.height) {
        Ok(sink) => {
            *status.lock().unwrap() = sink.describe();
            Some(sink)
        }
        Err(err) => {
            tracing::error!("virtual camera unavailable: {err:#}");
            *status.lock().unwrap() = format!("unavailable: {err:#}");
            None
        }
    }
}

impl App {
    pub fn start(settings: Settings) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ips = netutil::lan_ips();
        let certs = certs::ensure(&settings.data_dir, &ips)?;
        let key = load_or_create_key(&settings.data_dir)?;

        let hub = Arc::new(FrameHub::new(settings.width, settings.height, settings.mode));
        // This image is what video-call participants see, so it must not show the key.
        hub.set_placeholder_text(&["Waiting for iPhone...", "Scan the QR code in the iPhone Webcam window"]);
        let config = StreamConfig {
            width: settings.width,
            height: settings.height,
            fps: settings.fps,
            quality: settings.quality.clamp(0.1, 1.0),
            max_bitrate: (settings.bitrate_mbps * 1_000_000.0) as u32,
        };
        let server = Arc::new(Server::new(hub.clone(), key.clone(), config, ips.clone(), certs.ca_cert_der.clone()));

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let (bound_tx, bound_rx) = std::sync::mpsc::channel::<Result<SocketAddr, String>>();
        let addr = SocketAddr::new(settings.host, settings.port);
        let (cert, cert_key) = (certs.server_cert_pem, certs.server_key_pem);
        let server_for_thread = server.clone();
        let serve = move || run_server(server_for_thread, addr, cert, cert_key, shutdown_rx, bound_tx);
        let server_thread = std::thread::Builder::new().name("server".into()).spawn(serve)?;
        let bound = bound_rx
            .recv_timeout(Duration::from_secs(10))
            .context("server did not start")?
            .map_err(|e| anyhow::anyhow!("could not listen on {addr}: {e}"))?;

        let urls = pairing_urls(&ips, bound.port(), &key);

        let camera_status = Arc::new(Mutex::new(String::from("not started")));
        let sink = open_camera(&settings, &camera_status);
        let camera_thread = camera::OutputThread::spawn(hub.clone(), sink, settings.fps, camera_status.clone());

        Ok(Self {
            hub,
            server,
            urls,
            settings,
            bound,
            camera_status,
            shutdown: Some(shutdown_tx),
            server_thread: Some(server_thread),
            camera_thread: Some(camera_thread),
        })
    }

    pub fn key(&self) -> &str {
        &self.server.key
    }

    pub fn stop(&mut self) {
        let started = Instant::now();
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(camera) = self.camera_thread.take() {
            camera.stop();
        }
        if let Some(thread) = self.server_thread.take() {
            let _ = thread.join();
        }
        tracing::debug!("stopped in {:?}", started.elapsed());
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}
