//! Wires the certificates, server, frame hub and virtual camera output together.

use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use protocol::StreamConfig;

use crate::admin::Admin;
use crate::camera::{self, CameraConfig};
use crate::hub::{FitMode, FrameHub};
use crate::server::{self, Server};
use crate::{certs, netutil};

pub struct Settings {
    pub host: IpAddr,
    pub port: u16,
    /// Port of the admin page on 127.0.0.1; another is picked if it's taken.
    pub admin_port: u16,
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
    /// The admin page, for this PC's browser.
    pub admin_url: String,
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

impl App {
    pub fn start(settings: Settings) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ips = netutil::lan_ips();
        let certs = certs::ensure(&settings.data_dir, &ips)?;
        let key = load_or_create_key(&settings.data_dir)?;

        // This image is what video-call participants see, so it must not show the key.
        let waiting = ["Waiting for iPhone...", "Scan the QR code on the iPhone Webcam page"];
        let hub = Arc::new(FrameHub::new(settings.width, settings.height, settings.mode, &waiting));
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
        let admin_listener = bind_admin(settings.admin_port)?;
        let admin_port = admin_listener.local_addr()?.port();

        // One URL per LAN address; localhost if there is none.
        let url_ips = if ips.is_empty() { vec![IpAddr::from([127, 0, 0, 1])] } else { ips.clone() };
        let urls: Vec<String> = url_ips.iter().map(|ip| format!("https://{ip}:{}/?k={key}", bound.port())).collect();

        let camera_status = Arc::new(Mutex::new(String::from("not started")));
        let admin = Arc::new(Admin {
            hub: hub.clone(),
            urls: urls.clone(),
            camera_status: camera_status.clone(),
            fps: settings.fps,
            port: admin_port,
        });
        let server_handle = axum_server::Handle::new();
        let serve = {
            let (server, handle) = (server.clone(), server_handle.clone());
            let (cert, cert_key) = (certs.server_cert_pem, certs.server_key_pem);
            move || server::run(server, listener, cert, cert_key, admin, admin_listener, handle)
        };
        let server_thread = std::thread::Builder::new().name("server".into()).spawn(serve)?;

        let camera = settings.camera.then(|| CameraConfig {
            name: settings.camera_name.clone(),
            width: settings.width,
            height: settings.height,
        });
        let camera_thread = camera::OutputThread::spawn(hub.clone(), camera, settings.fps, camera_status.clone());

        Ok(Self {
            hub,
            server,
            urls,
            admin_url: format!("http://127.0.0.1:{admin_port}/"),
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

/// The admin page's listener on 127.0.0.1: `port`, or any free one if that's taken
/// (by a second copy of the app, say).
fn bind_admin(port: u16) -> Result<TcpListener> {
    let listener = TcpListener::bind(("127.0.0.1", port)).or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}
