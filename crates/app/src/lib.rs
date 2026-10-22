//! iPhone Webcam: use an iPhone as a webcam on a Windows 11 or Linux PC.
//!
//! The phone opens a page served by this app and streams its camera over WebRTC
//! (H.264), or JPEG frames as a fallback; frames feed a virtual camera.

pub mod app;
pub mod camera;
pub mod certs;
pub mod decode;
pub mod gui;
pub mod hub;
pub mod netutil;
pub mod server;
pub mod web_assets;
pub mod webrtc;

#[cfg(windows)]
pub mod install;
#[cfg(target_os = "linux")]
#[path = "install_linux.rs"]
pub mod install;
#[cfg(windows)]
pub mod mf_decode;
#[cfg(target_os = "linux")]
pub mod v4l2;
#[cfg(windows)]
pub mod vcam_client;
