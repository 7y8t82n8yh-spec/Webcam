//! iPhone Webcam: use an iPhone as a webcam on a Windows 11 PC.
//!
//! The phone opens a page served by this app and streams its camera over WebRTC
//! (H.264), or JPEG frames as a fallback; frames feed a virtual camera.

pub mod app;
pub mod camera;
pub mod certs;
pub mod decode;
pub mod hub;
pub mod netutil;
pub mod server;
pub mod web_assets;
pub mod webrtc;

#[cfg(windows)]
pub mod mf_decode;
#[cfg(windows)]
pub mod vcam_client;
