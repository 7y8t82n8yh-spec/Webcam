//! WebRTC receiving side, built on str0m.
//!
//! Signalling is one HTTPS round trip: the phone POSTs its offer (with all ICE
//! candidates already gathered) and gets the answer back. Media then flows directly
//! between the two devices over UDP; no STUN/TURN server is involved on a LAN.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::Instant;

use anyhow::{Context, Result};
use protocol::ServerMessage;
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::media::{KeyframeRequestKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::decode;
use crate::hub::FrameHub;

/// Decoder backlog (in frames) after which we skip ahead to the next keyframe.
const MAX_BACKLOG: usize = 6;

enum Command {
    Send(ServerMessage),
    Close,
}

/// Handle to a running session; dropping it does not stop the session.
pub struct RtcSession {
    pub id: u64,
    pub client_id: String,
    commands: mpsc::UnboundedSender<Command>,
}

impl RtcSession {
    pub fn send(&self, message: ServerMessage) {
        let _ = self.commands.send(Command::Send(message));
    }

    pub fn close(&self) {
        let _ = self.commands.send(Command::Close);
    }
}

/// Add libwebrtc's start-bitrate hint to every video codec in an SDP answer.
///
/// Without it the phone starts around 300 kbit/s and needs ~30 s of bandwidth probing
/// to reach full quality. Senders still back off quickly if the network can't carry it.
pub fn with_start_bitrate(sdp: &str, kbps: u32) -> String {
    let hint = format!("x-google-start-bitrate={kbps}");
    let mut out: Vec<String> = Vec::new();
    let mut in_video = false;
    // Payload type -> index of its rtpmap line in `out`, and those that have an fmtp.
    let mut codecs: Vec<(String, usize)> = Vec::new();
    let mut with_fmtp: Vec<String> = Vec::new();
    let finish = |out: &mut Vec<String>, codecs: &mut Vec<(String, usize)>, with_fmtp: &mut Vec<String>| {
        // Codecs without an fmtp line get one right after their rtpmap (back to front so
        // the recorded indices stay valid).
        for (pt, idx) in codecs.iter().rev() {
            if !with_fmtp.contains(pt) {
                out.insert(idx + 1, format!("a=fmtp:{pt} {hint}"));
            }
        }
        codecs.clear();
        with_fmtp.clear();
    };
    for line in sdp.lines() {
        let mut line = line.to_string();
        if line.starts_with("m=") {
            finish(&mut out, &mut codecs, &mut with_fmtp);
            in_video = line.starts_with("m=video");
        } else if in_video {
            if let Some(rest) = line.strip_prefix("a=rtpmap:") {
                let (pt, codec) = rest.split_once(' ').unwrap_or((rest, ""));
                let name = codec.split('/').next().unwrap_or("").to_ascii_lowercase();
                if !["rtx", "red", "ulpfec", "flexfec-03"].contains(&name.as_str()) {
                    codecs.push((pt.to_string(), out.len()));
                }
            } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
                let pt = rest.split(' ').next().unwrap_or("").to_string();
                if codecs.iter().any(|(p, _)| *p == pt) {
                    line.push(';');
                    line.push_str(&hint);
                    with_fmtp.push(pt);
                }
            }
        }
        out.push(line);
    }
    finish(&mut out, &mut codecs, &mut with_fmtp);
    let mut sdp = out.join("\r\n");
    sdp.push_str("\r\n");
    sdp
}

fn rtc_config() -> RtcConfig {
    let mut config = RtcConfig::new().clear_codecs();
    // Constrained baseline / baseline H.264 only: what every iPhone encodes in
    // hardware, and what every decoder handles.
    let codecs = config.codec_config();
    codecs.add_h264(108.into(), Some(109.into()), true, 0x42e01f);
    codecs.add_h264(127.into(), Some(121.into()), true, 0x42001f);
    config
}

/// Accept `offer_sdp` and start receiving. Returns the SDP answer.
pub async fn start(
    offer_sdp: &str,
    client_id: String,
    ips: &[IpAddr],
    hub: Arc<FrameHub>,
    start_bitrate: u32,
    on_end: impl FnOnce(u64) + Send + 'static,
) -> Result<(String, RtcSession)> {
    let offer = SdpOffer::from_sdp_string(offer_sdp).context("invalid SDP offer")?;
    let mut rtc = rtc_config().build(Instant::now());

    // One UDP socket per local address, so we know which address a packet arrived on.
    let mut sockets = Vec::new();
    for ip in ips.iter().chain(std::iter::once(&IpAddr::from([127, 0, 0, 1]))) {
        let Ok(socket) = UdpSocket::bind(SocketAddr::new(*ip, 0)).await else { continue };
        let addr = socket.local_addr()?;
        if let Ok(candidate) = Candidate::host(addr, "udp") {
            rtc.add_local_candidate(candidate);
            sockets.push(Arc::new(socket));
        }
    }
    anyhow::ensure!(!sockets.is_empty(), "no usable network address for WebRTC");

    let answer = rtc.sdp_api().accept_offer(offer).context("offer not acceptable (no H.264?)")?;
    let answer = with_start_bitrate(&answer.to_sdp_string(), start_bitrate / 1000);

    static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64;
    let (commands, command_rx) = mpsc::unbounded_channel();
    let session = RtcSession { id, client_id: client_id.clone(), commands };
    tokio::spawn(async move {
        if let Err(err) = run(rtc, sockets, command_rx, hub).await {
            tracing::warn!("WebRTC session {client_id} ended: {err:#}");
        } else {
            tracing::info!("WebRTC session {client_id} ended");
        }
        on_end(id);
    });
    Ok((answer, session))
}

async fn run(
    mut rtc: Rtc,
    sockets: Vec<Arc<UdpSocket>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    hub: Arc<FrameHub>,
) -> Result<()> {
    // Datagrams from all sockets are funnelled into one channel.
    let (packet_tx, mut packets) = mpsc::channel::<(Vec<u8>, SocketAddr, SocketAddr)>(256);
    let mut readers = Vec::new();
    for socket in &sockets {
        let socket = socket.clone();
        let tx = packet_tx.clone();
        readers.push(tokio::spawn(async move {
            let local = socket.local_addr().expect("bound socket");
            let mut buf = vec![0u8; 2000];
            while let Ok((n, from)) = socket.recv_from(&mut buf).await {
                if tx.send((buf[..n].to_vec(), from, local)).await.is_err() {
                    break;
                }
            }
        }));
    }
    let _abort_readers = AbortOnDrop(readers);

    let (keyframe_tx, mut keyframe_requests) = mpsc::unbounded_channel::<()>();
    let decoder = DecoderThread::spawn(hub, keyframe_tx)?;
    let mut channel: Option<ChannelId> = None;
    let mut video: Option<Mid> = None;

    loop {
        let deadline = loop {
            match rtc.poll_output()? {
                Output::Timeout(at) => break at,
                Output::Transmit(t) => {
                    if let Some(s) = sockets.iter().find(|s| s.local_addr().ok() == Some(t.source)) {
                        let _ = s.send_to(&t.contents, t.destination).await;
                    }
                }
                Output::Event(event) => match event {
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return Ok(()),
                    Event::Connected => tracing::info!("WebRTC connected"),
                    Event::ChannelOpen(id, _label) => channel = Some(id),
                    Event::MediaAdded(media) => video = Some(media.mid),
                    Event::MediaData(data) => decoder.push(data.data.clone(), data.contiguous, data.is_keyframe()),
                    _ => {}
                },
            }
        };
        tokio::select! {
            Some((contents, source, destination)) = packets.recv() => {
                let input = Input::Receive(
                    Instant::now(),
                    Receive { proto: Protocol::Udp, source, destination, contents: contents.as_slice().try_into()? },
                );
                if rtc.accepts(&input) {
                    rtc.handle_input(input)?;
                }
            }
            _ = tokio::time::sleep_until(deadline.into()) => rtc.handle_input(Input::Timeout(Instant::now()))?,
            Some(()) = keyframe_requests.recv() => {
                if let Some(mut writer) = video.and_then(|mid| rtc.writer(mid)) {
                    let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
                }
            }
            command = commands.recv() => match command {
                Some(Command::Send(message)) => {
                    if let Some(mut ch) = channel.and_then(|id| rtc.channel(id)) {
                        let _ = ch.write(false, serde_json::to_string(&message)?.as_bytes());
                    }
                }
                Some(Command::Close) | None => {
                    // Flush what's queued (e.g. a "replaced" message) before closing.
                    let flush_until = Instant::now() + std::time::Duration::from_millis(200);
                    while Instant::now() < flush_until {
                        match rtc.poll_output()? {
                            Output::Transmit(t) => {
                                if let Some(s) = sockets.iter().find(|s| s.local_addr().ok() == Some(t.source)) {
                                    let _ = s.send_to(&t.contents, t.destination).await;
                                }
                            }
                            Output::Timeout(_) => break,
                            Output::Event(_) => {}
                        }
                    }
                    rtc.disconnect();
                    return Ok(());
                }
            },
        }
        if !rtc.is_alive() {
            return Ok(());
        }
    }
}

struct AbortOnDrop(Vec<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Decodes frames on a dedicated thread so the network loop never waits for it.
struct DecoderThread {
    frames: std_mpsc::Sender<Encoded>,
    backlog: Arc<AtomicUsize>,
}

impl DecoderThread {
    fn spawn(hub: Arc<FrameHub>, keyframe_requests: mpsc::UnboundedSender<()>) -> Result<Self> {
        let mut decoder = decode::new_decoder()?;
        let (frames, rx) = std_mpsc::channel::<Encoded>();
        let backlog = Arc::new(AtomicUsize::new(0));
        let pending = backlog.clone();
        std::thread::Builder::new().name("h264-decoder".into()).spawn(move || {
            // After a gap or when we fall behind, frames are useless until a keyframe.
            let mut need_keyframe = false;
            while let Ok(Encoded { data, contiguous, keyframe }) = rx.recv() {
                let behind = pending.fetch_sub(1, Ordering::Relaxed) > MAX_BACKLOG;
                if !contiguous || behind {
                    if !need_keyframe {
                        let _ = keyframe_requests.send(());
                    }
                    need_keyframe = true;
                }
                if need_keyframe {
                    if !keyframe {
                        continue;
                    }
                    need_keyframe = false;
                }
                let bytes = data.len();
                if let Err(err) = decoder.decode(&data, &mut |picture| hub.push_yuv(picture, bytes)) {
                    tracing::debug!("decode error: {err:#}");
                    hub.count_decode_error();
                    need_keyframe = true;
                    let _ = keyframe_requests.send(());
                }
            }
        })?;
        Ok(Self { frames, backlog })
    }

    fn push(&self, data: Arc<[u8]>, contiguous: bool, keyframe: bool) {
        self.backlog.fetch_add(1, Ordering::Relaxed);
        let _ = self.frames.send(Encoded { data, contiguous, keyframe });
    }
}

struct Encoded {
    data: Arc<[u8]>,
    contiguous: bool,
    keyframe: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_bitrate_hint_is_added_to_every_video_codec() {
        let sdp = "v=0\r\nm=audio 9 UDP 0\r\na=rtpmap:0 PCMU/8000\r\n\
                   m=video 9 UDP 96 97 102\r\na=rtpmap:96 VP8/90000\r\na=rtcp-fb:96 nack\r\n\
                   a=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=96\r\n\
                   a=rtpmap:102 H264/90000\r\na=fmtp:102 packetization-mode=1\r\n";
        let out = with_start_bitrate(sdp, 2500);
        let lines: Vec<&str> = out.split("\r\n").collect();
        let vp8 = lines.iter().position(|l| *l == "a=rtpmap:96 VP8/90000").unwrap();
        assert_eq!(lines[vp8 + 1], "a=fmtp:96 x-google-start-bitrate=2500");
        assert!(lines.contains(&"a=fmtp:102 packetization-mode=1;x-google-start-bitrate=2500"));
        assert!(lines.contains(&"a=fmtp:97 apt=96"));
        assert!(!lines.iter().any(|l| l.starts_with("a=fmtp:0")));
    }
}
