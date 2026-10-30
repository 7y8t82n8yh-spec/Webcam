//! iPhone Webcam: use an iPhone as a webcam on a Windows 11 or Linux PC.
//!
//! The phone opens a page served by this app and streams its camera over WebRTC
//! (H.264), or JPEG frames as a fallback; frames feed a virtual camera.

pub mod admin;
pub mod app;
pub mod camera;
pub mod certs;
pub mod decode;
pub mod hub;
pub mod netutil;
pub mod plain_http;
pub mod server;
pub mod web_assets;
pub mod webrtc;

#[cfg(windows)]
pub mod install;
#[cfg(target_os = "linux")]
#[path = "install_linux.rs"]
pub mod install;
#[cfg(any(windows, target_os = "linux"))]
pub mod install_record;
#[cfg(target_os = "linux")]
pub mod v4l2;
#[cfg(windows)]
pub mod vcam_client;
