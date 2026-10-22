//! Types shared by the PC app (server), the phone page (WebAssembly) and the
//! Windows 11 camera DLL.

pub mod frame_section;

use serde::{Deserialize, Serialize};

/// Header carrying the pairing key on HTTP requests (WebSocket uses `?k=`).
pub const KEY_HEADER: &str = "X-Access-Key";
/// Header identifying a phone across reconnects (one browser tab = one id).
pub const CLIENT_ID_HEADER: &str = "X-Client-Id";

/// Settings the PC tells the phone about.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamConfig {
    /// Output size of the virtual camera.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// JPEG quality for the fallback transport, 0.1..=1.0.
    pub quality: f64,
    /// WebRTC bitrate cap in bit/s.
    pub max_bitrate: u32,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self { width: 1280, height: 720, fps: 30, quality: 0.75, max_bitrate: 6_000_000 }
    }
}

/// Body of `POST /offer`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfferRequest {
    pub sdp: String,
    /// The phone's client id.
    pub id: String,
    #[serde(flatten)]
    pub info: ClientInfo,
}

/// Response of `POST /offer`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OfferResponse {
    pub sdp: String,
    pub config: StreamConfig,
}

/// What the phone tells about itself (shown in the PC window).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub ua: String,
    #[serde(default)]
    pub camera: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// Messages from the PC to the phone, on the WebRTC data channel or the WebSocket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ServerMessage {
    /// Sent on WebSocket connect.
    Config(StreamConfig),
    /// Another device took over the camera.
    Replaced,
}

/// Messages from the phone to the PC on the WebSocket (JPEG fallback).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMessage {
    Hello(ClientInfo),
}

/// Text frame the server answers every binary JPEG frame with on the WebSocket.
pub const ACK: &str = "ack";
/// Ditto, for a frame that could not be decoded.
pub const BAD: &str = "bad";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_have_stable_json() {
        let json = serde_json::to_string(&ServerMessage::Replaced).unwrap();
        assert_eq!(json, r#"{"type":"replaced"}"#);
        let cfg = ServerMessage::Config(StreamConfig::default());
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.starts_with(r#"{"type":"config","width":1280"#), "{json}");
        assert!(json.contains(r#""maxBitrate":6000000"#));
        let back: ServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn offer_request_flattens_client_info() {
        let req: OfferRequest =
            serde_json::from_str(r#"{"sdp":"v=0","id":"abc","ua":"Safari","camera":"user","width":1280}"#)
                .unwrap();
        assert_eq!(req.info.ua, "Safari");
        assert_eq!(req.info.height, 0);
    }
}
