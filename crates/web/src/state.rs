//! Everything about the page that changes while it runs, kept in one `State` behind
//! the page's `RefCell` so every callback sees the same values.

use protocol::StreamConfig;
use web_sys::{MediaStream, RtcPeerConnection, RtcRtpSender, WebSocket};

/// How frames currently reach the PC.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    WebRtc,
    WebSocket,
    Http,
}

/// Which camera to use, as `getUserMedia`'s `facingMode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Facing {
    User,
    Environment,
}

impl Facing {
    pub fn as_str(self) -> &'static str {
        match self {
            Facing::User => "user",
            Facing::Environment => "environment",
        }
    }

    pub fn flipped(self) -> Facing {
        match self {
            Facing::User => Facing::Environment,
            Facing::Environment => Facing::User,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VideoSize {
    pub width: u32,
    pub height: u32,
}

impl VideoSize {
    pub fn of_config(config: &StreamConfig) -> VideoSize {
        VideoSize { width: config.width, height: config.height }
    }

    /// Parse a "WIDTHxHEIGHT" value from the quality picker; `None` for any other value.
    pub fn from_picker_value(value: &str) -> Option<VideoSize> {
        let (width, height) = value.split_once('x')?;
        Some(VideoSize { width: width.parse().ok()?, height: height.parse().ok()? })
    }

    pub fn picker_value(self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

pub struct CameraState {
    pub facing: Facing,
    /// The size picked in the quality menu; `None` follows the PC's configured size.
    pub requested_size: Option<VideoSize>,
    pub stream: Option<MediaStream>,
    /// The user's mirror toggle; only the front camera is ever mirrored.
    pub is_mirror_on: bool,
}

pub struct RtcState {
    pub pc: Option<RtcPeerConnection>,
    pub sender: Option<RtcRtpSender>,
    /// Once WebRTC has worked, a failure means "reconnect", not "fall back to JPEG".
    pub has_ever_connected: bool,
}

pub struct SocketState {
    pub current: Option<WebSocket>,
    pub is_open: bool,
    pub has_ever_opened: bool,
    /// Connection attempts that closed before opening.
    pub failed_attempts: u32,
}

pub struct State {
    pub config: StreamConfig,
    pub camera: CameraState,
    pub transport: Transport,
    pub rtc: RtcState,
    pub socket: SocketState,
    /// JPEG frames sent but not yet acknowledged by the PC.
    pub frames_in_flight: u32,
    pub is_encoding: bool,
    pub is_running: bool,
    pub is_starting: bool,
    /// Another device took over streaming; stay idle until the user takes it back.
    pub is_replaced: bool,
    pub frames_sent: f64,
    pub bytes_sent: f64,
}

impl State {
    pub fn initial() -> State {
        State {
            config: StreamConfig::default(),
            camera: CameraState { facing: Facing::Environment, requested_size: None, stream: None, is_mirror_on: true },
            transport: Transport::WebRtc,
            rtc: RtcState { pc: None, sender: None, has_ever_connected: false },
            socket: SocketState { current: None, is_open: false, has_ever_opened: false, failed_attempts: 0 },
            frames_in_flight: 0,
            is_encoding: false,
            is_running: false,
            is_starting: false,
            is_replaced: false,
            frames_sent: 0.0,
            bytes_sent: 0.0,
        }
    }
}
