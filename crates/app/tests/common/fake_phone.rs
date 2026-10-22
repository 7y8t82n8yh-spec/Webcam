//! A fake phone that streams like the phone page: an H.264 video track plus a "control"
//! data channel, negotiated by POSTing an SDP offer to `/offer`.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use openh264::encoder::Encoder;
use openh264::formats::{RgbSliceU8, YUVBuffer};
use protocol::{OfferRequest, OfferResponse};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;

use super::images::Rgb;
use super::test_app::AppClient;

pub const FRAME_WIDTH: usize = 640;
pub const FRAME_HEIGHT: usize = 360;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const H264_PAYLOAD_TYPE: u8 = 108;
const H264_RETRANSMISSION_PAYLOAD_TYPE: u8 = 109;
/// H.264 Constrained Baseline profile, level 3.1.
const H264_PROFILE_LEVEL_ID: u32 = 0x42e01f;
const RTP_VIDEO_CLOCK_HZ: f64 = 90_000.0;
const UDP_RECEIVE_BUFFER_BYTES: usize = 2000;

/// What a fake phone saw while streaming.
#[derive(Debug, Default)]
pub struct PhoneReport {
    pub connected: bool,
    pub frames_sent: usize,
    pub control_messages: Vec<String>,
}

/// Connect to the app as `client_id` and stream solid `color` frames for `duration`.
pub async fn stream_webrtc(app: &AppClient, client_id: &str, color: Rgb, duration: Duration) -> PhoneReport {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let local_address = socket.local_addr().unwrap();
    let (mut rtc, video) = negotiate(app, client_id, local_address).await;

    let pixels = color.repeat(FRAME_WIDTH * FRAME_HEIGHT);
    let frame = YUVBuffer::from_rgb_source(RgbSliceU8::new(&pixels, (FRAME_WIDTH, FRAME_HEIGHT)));
    let mut encoder = Encoder::new().unwrap();

    let mut report = PhoneReport::default();
    let start = Instant::now();
    let end = start + duration;
    let mut next_frame_at = start;
    let mut receive_buffer = vec![0u8; UDP_RECEIVE_BUFFER_BYTES];
    while Instant::now() < end {
        let rtc_wakes_at = handle_output(&mut rtc, &socket, &mut report, &mut encoder).await;
        if report.connected && Instant::now() >= next_frame_at {
            send_frame(&mut rtc, video, &mut encoder, &frame, start);
            report.frames_sent += 1;
            next_frame_at += FRAME_INTERVAL;
            continue;
        }
        let wait_until = if report.connected { rtc_wakes_at.min(next_frame_at) } else { rtc_wakes_at };
        receive_until(&mut rtc, &socket, local_address, &mut receive_buffer, wait_until).await;
    }
    report
}

/// Offer a send-only H.264 track and a "control" channel, and accept the app's answer.
async fn negotiate(app: &AppClient, client_id: &str, local_address: SocketAddr) -> (Rtc, Mid) {
    let mut config = RtcConfig::new().clear_codecs();
    config.codec_config().add_h264(
        H264_PAYLOAD_TYPE.into(),
        Some(H264_RETRANSMISSION_PAYLOAD_TYPE.into()),
        true,
        H264_PROFILE_LEVEL_ID,
    );
    let mut rtc = config.build(Instant::now());
    rtc.add_local_candidate(Candidate::host(local_address, "udp").unwrap());

    let mut sdp = rtc.sdp_api();
    let video = sdp.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
    sdp.add_channel("control".into());
    let (offer, pending) = sdp.apply().unwrap();

    let answer = post_offer(app, client_id, &offer).await;
    rtc.sdp_api().accept_answer(pending, answer).unwrap();
    (rtc, video)
}

async fn post_offer(app: &AppClient, client_id: &str, offer: &SdpOffer) -> SdpAnswer {
    let request = OfferRequest { sdp: offer.to_sdp_string(), id: client_id.into(), info: Default::default() };
    let sent = app.http.post(app.url("/offer")).header(protocol::KEY_HEADER, &app.key).json(&request).send();
    let response = sent.await.unwrap().error_for_status().unwrap();
    let response: OfferResponse = response.json().await.unwrap();
    SdpAnswer::from_sdp_string(&response.sdp).unwrap()
}

/// Send what `rtc` wants sent and note its events, until it asks to be woken; returns when.
async fn handle_output(rtc: &mut Rtc, socket: &UdpSocket, report: &mut PhoneReport, encoder: &mut Encoder) -> Instant {
    loop {
        match rtc.poll_output().unwrap() {
            Output::Timeout(wake_at) => return wake_at,
            Output::Transmit(packet) => {
                let _ = socket.send_to(&packet.contents, packet.destination).await;
            }
            Output::Event(Event::Connected) => report.connected = true,
            Output::Event(Event::ChannelData(message)) => {
                report.control_messages.push(String::from_utf8_lossy(&message.data).into_owned())
            }
            Output::Event(Event::KeyframeRequest(_)) => encoder.force_intra_frame(),
            Output::Event(_) => {}
        }
    }
}

/// Encode one frame and write it to the video track.
fn send_frame(rtc: &mut Rtc, video: Mid, encoder: &mut Encoder, frame: &YUVBuffer, start: Instant) {
    let encoded = encoder.encode(frame).unwrap().to_vec();
    let writer = rtc.writer(video).unwrap();
    let payload_type = writer.payload_params().next().unwrap().pt();
    let ticks = (start.elapsed().as_secs_f64() * RTP_VIDEO_CLOCK_HZ) as u64;
    let rtp_time = MediaTime::new(ticks, Frequency::NINETY_KHZ);
    writer.write(payload_type, Instant::now(), rtp_time, encoded).unwrap();
}

/// Feed `rtc` the next packet from `socket`, or a timeout if `until` comes first.
async fn receive_until(
    rtc: &mut Rtc,
    socket: &UdpSocket,
    local_address: SocketAddr,
    buffer: &mut [u8],
    until: Instant,
) {
    tokio::select! {
        received = socket.recv_from(buffer) => {
            // Windows reports ICMP "port unreachable" for an earlier send as an error on
            // the next receive, e.g. once the app has dropped this phone's session.
            if matches!(&received, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset) {
                return;
            }
            let (length, source) = received.unwrap();
            let contents = buffer[..length].try_into().unwrap();
            let receive = Receive { proto: Protocol::Udp, source, destination: local_address, contents };
            let input = Input::Receive(Instant::now(), receive);
            if rtc.accepts(&input) {
                rtc.handle_input(input).unwrap();
            }
        }
        _ = tokio::time::sleep_until(until.into()) => rtc.handle_input(Input::Timeout(Instant::now())).unwrap(),
    }
}
