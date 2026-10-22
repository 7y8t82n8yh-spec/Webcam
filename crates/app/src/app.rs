//! Wires the certificates, server, frame hub and virtual camera output together.

use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use protocol::StreamConfig;

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
    server_handle: axum_server::Handle<SocketAddr>,
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

        // Bound here, so a busy port is reported right away (and port 0 gets its number).
        let addr = SocketAddr::new(settings.host, settings.port);
        let listener = TcpListener::bind(addr).with_context(|| format!("could not listen on {addr}"))?;
        listener.set_nonblocking(true)?;
        let bound = listener.local_addr()?;
        let server_handle = axum_server::Handle::new();
        let serve = {
            let (server, handle) = (server.clone(), server_handle.clone());
            move || server::run(server, listener, certs.server_cert_pem, certs.server_key_pem, handle)
        };
        let server_thread = std::thread::Builder::new().name("server".into()).spawn(serve)?;

        // One URL per LAN address; localhost if there is none.
        let url_ips = if ips.is_empty() { vec![IpAddr::from([127, 0, 0, 1])] } else { ips.clone() };
        let urls = url_ips.iter().map(|ip| format!("https://{ip}:{}/?k={key}", bound.port())).collect();

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
            server_handle,
            server_thread: Some(server_thread),
            camera_thread: Some(camera_thread),
        })
    }

    pub fn key(&self) -> &str {
        &self.server.key
    }

    pub fn stop(&mut self) {
        self.server.shutdown(); // ends the phone's connection
        self.server_handle.graceful_shutdown(Some(Duration::from_secs(1)));
        if let Some(camera) = self.camera_thread.take() {
            camera.stop();
        }
        if let Some(thread) = self.server_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}
