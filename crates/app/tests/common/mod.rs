//! Helpers shared by the integration tests: a running app and a fake phone.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use iphone_webcam::app::{App, Settings};
use iphone_webcam::hub::FitMode;
use openh264::encoder::Encoder;
use openh264::formats::{RgbSliceU8, YUVBuffer};
use protocol::{OfferRequest, OfferResponse};
use str0m::change::SdpAnswer;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;

pub struct TestApp {
    pub app: App,
    pub base: String,
    pub key: String,
    pub client: reqwest::Client,
    pub dir: PathBuf,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.app.stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub fn start() -> TestApp {
    start_with(false)
}

/// Start the app, optionally with the real Windows 11 virtual camera.
pub fn start_with(camera: bool) -> TestApp {
    let run = rand::random::<u64>();
    let dir = std::env::temp_dir().join(format!("iphone-webcam-it-{run}"));
    let app = App::start(Settings {
        host: "127.0.0.1".parse().unwrap(),
        port: 0,
        width: 1280,
        height: 720,
        fps: 30,
        quality: 0.75,
        bitrate_mbps: 6.0,
        mode: FitMode::Fit,
        camera,
        camera_name: "iPhone Webcam".into(),
        data_dir: dir.clone(),
    })
    .expect("app starts");
    let base = format!("https://127.0.0.1:{}", app.bound.port());
    let key = app.key().to_string();
    let client = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
    TestApp { app, base, key, client, dir }
}

/// Enough to talk to a running app; cheap to clone into other tasks.
#[derive(Clone)]
pub struct Target {
    pub base: String,
    pub key: String,
    pub client: reqwest::Client,
}

impl TestApp {
    pub fn target(&self) -> Target {
        Target { base: self.base.clone(), key: self.key.clone(), client: self.client.clone() }
    }

    pub async fn status(&self) -> serde_json::Value {
        let response = self.client.get(format!("{}/status?k={}", self.base, self.key)).send().await.unwrap();
        response.json().await.unwrap()
    }

    /// Wait until `frames_in` reaches `count`; returns the last status.
    pub async fn wait_for_frames(&self, count: u64, timeout: Duration) -> serde_json::Value {
        let deadline = Instant::now() + timeout;
        loop {
            let status = self.status().await;
            if status["framesIn"].as_u64().unwrap() >= count || Instant::now() > deadline {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Average colour of the hub's current output frame.
    pub fn output_color(&self) -> [f64; 3] {
        let frame = self.app.hub.latest();
        let mut sum = [0f64; 3];
        for px in frame.rgb.chunks_exact(3) {
            for (s, v) in sum.iter_mut().zip(px) {
                *s += *v as f64;
            }
        }
        let n = (frame.rgb.len() / 3) as f64;
        sum.map(|s| s / n)
    }
}

pub fn solid_jpeg(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
    let pixels = rgb.repeat((w * h) as usize);
    let mut out = Vec::new();
    jpeg_encoder::Encoder::new(&mut out, 95).encode(&pixels, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).unwrap();
    out
}

/// What a fake phone saw while streaming.
#[derive(Debug, Default)]
pub struct PhoneReport {
    pub connected: bool,
    pub frames_sent: usize,
    pub control_messages: Vec<String>,
}

/// Act like the phone page: offer an H.264 video track plus a "control" data channel,
/// POST it to /offer, then stream `seconds` of solid `rgb` frames.
pub async fn webrtc_phone(test: &Target, client_id: &str, rgb: [u8; 3], seconds: f64) -> PhoneReport {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local: SocketAddr = socket.local_addr().unwrap();
    let mut config = RtcConfig::new().clear_codecs();
    config.codec_config().add_h264(108.into(), Some(109.into()), true, 0x42e01f);
    let mut rtc = config.build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
    let mut api = rtc.sdp_api();
    let mid = api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
    api.add_channel("control".into());
    let (offer, pending) = api.apply().unwrap();

    let request = OfferRequest { sdp: offer.to_sdp_string(), id: client_id.into(), info: Default::default() };
    let sent = test
        .client
        .post(format!("{}/offer", test.base))
        .header(protocol::KEY_HEADER, &test.key)
        .json(&request)
        .send()
        .await
        .unwrap();
    let response: OfferResponse = sent.error_for_status().unwrap().json().await.unwrap();
    let answer = SdpAnswer::from_sdp_string(&response.sdp).unwrap();
    rtc.sdp_api().accept_answer(pending, answer).unwrap();

    let (w, h) = (640usize, 360usize);
    let pixels = rgb.repeat(w * h);
    let source = RgbSliceU8::new(&pixels, (w, h));
    let yuv = YUVBuffer::from_rgb_source(source);
    let mut encoder = Encoder::new().unwrap();

    let mut report = PhoneReport::default();
    let start = Instant::now();
    let end = start + Duration::from_secs_f64(seconds);
    let mut next_frame = start;
    let mut buf = vec![0u8; 2000];
    while Instant::now() < end {
        let deadline = handle_output(&mut rtc, &socket, &mut report, &mut encoder).await;
        if report.connected && Instant::now() >= next_frame {
            send_frame(&mut rtc, mid, &mut encoder, &yuv, start);
            report.frames_sent += 1;
            next_frame += Duration::from_millis(33);
            continue;
        }
        let wait_until = if report.connected { deadline.min(next_frame) } else { deadline };
        receive_until(&mut rtc, &socket, local, &mut buf, wait_until).await;
    }
    report
}

/// Send what `rtc` wants sent and note its events, until it asks to be woken; returns when.
async fn handle_output(rtc: &mut Rtc, socket: &UdpSocket, report: &mut PhoneReport, encoder: &mut Encoder) -> Instant {
    loop {
        match rtc.poll_output().unwrap() {
            Output::Timeout(at) => return at,
            Output::Transmit(t) => {
                let _ = socket.send_to(&t.contents, t.destination).await;
            }
            Output::Event(Event::Connected) => report.connected = true,
            Output::Event(Event::ChannelData(data)) => {
                report.control_messages.push(String::from_utf8_lossy(&data.data).into_owned())
            }
            Output::Event(Event::KeyframeRequest(_)) => encoder.force_intra_frame(),
            Output::Event(_) => {}
        }
    }
}

/// Encode one frame and write it to the video track.
fn send_frame(rtc: &mut Rtc, mid: Mid, encoder: &mut Encoder, yuv: &YUVBuffer, start: Instant) {
    let data = encoder.encode(yuv).unwrap().to_vec();
    let writer = rtc.writer(mid).unwrap();
    let pt = writer.payload_params().next().unwrap().pt();
    let ticks = (start.elapsed().as_secs_f64() * 90_000.0) as u64;
    let rtp_time = MediaTime::new(ticks, Frequency::NINETY_KHZ);
    writer.write(pt, Instant::now(), rtp_time, data).unwrap();
}

/// Feed `rtc` the next packet from `socket`, or a timeout if `until` comes first.
async fn receive_until(rtc: &mut Rtc, socket: &UdpSocket, local: SocketAddr, buf: &mut [u8], until: Instant) {
    tokio::select! {
        received = socket.recv_from(buf) => {
            // Windows reports ICMP "port unreachable" for an earlier send as an error on
            // the next receive, e.g. once the app has dropped this phone's session.
            if matches!(&received, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset) {
                return;
            }
            let (n, source) = received.unwrap();
            let contents = buf[..n].try_into().unwrap();
            let receive = Receive { proto: Protocol::Udp, source, destination: local, contents };
            let input = Input::Receive(Instant::now(), receive);
            if rtc.accepts(&input) {
                rtc.handle_input(input).unwrap();
            }
        }
        _ = tokio::time::sleep_until(until.into()) => rtc.handle_input(Input::Timeout(Instant::now())).unwrap(),
    }
}
