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
use str0m::net::{Protocol, Receive, Transmit};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::decode::H264Decoder;
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

/// Add libwebrtc's start-bitrate hint to the video codecs of our SDP answer.
///
/// Without it the phone starts around 300 kbit/s and needs ~30 s of bandwidth probing
/// to reach full quality. Senders still back off quickly if the network can't carry it.
/// Every codec in our answer has an fmtp line (H.264 always carries its profile);
/// the retransmission (rtx) ones are recognisable by their `apt=`.
pub fn with_start_bitrate(sdp: &str, kbps: u32) -> String {
    let mut out = String::new();
    for line in sdp.lines() {
        out.push_str(line);
        if line.starts_with("a=fmtp:") && !line.contains("apt=") {
            out.push_str(&format!(";x-google-start-bitrate={kbps}"));
        }
        out.push_str("\r\n");
    }
    out
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

    let sockets = bind_candidates(&mut rtc, ips).await?;
    anyhow::ensure!(!sockets.is_empty(), "no usable network address for WebRTC");

    let answer = rtc.sdp_api().accept_offer(offer).context("offer not acceptable (no H.264?)")?;
    let answer = with_start_bitrate(&answer.to_sdp_string(), start_bitrate / 1000);

    static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u64;
    let (commands, command_rx) = mpsc::unbounded_channel();
    let session = RtcSession { id, client_id: client_id.clone(), commands };
    let session_task = async move {
        if let Err(err) = run(rtc, sockets, command_rx, hub).await {
            tracing::warn!("WebRTC session {client_id} ended: {err:#}");
        } else {
            tracing::info!("WebRTC session {client_id} ended");
        }
        on_end(id);
    };
    tokio::spawn(session_task);
    Ok((answer, session))
}

/// One UDP socket per local address, so we know which address a packet arrived on.
/// Each one becomes a host candidate of `rtc`.
async fn bind_candidates(rtc: &mut Rtc, ips: &[IpAddr]) -> Result<Vec<Arc<UdpSocket>>> {
    let loopback = IpAddr::from([127, 0, 0, 1]);
    let mut sockets = Vec::new();
    for ip in ips.iter().chain(std::iter::once(&loopback)) {
        let Ok(socket) = UdpSocket::bind(SocketAddr::new(*ip, 0)).await else { continue };
        let addr = socket.local_addr()?;
        if let Ok(candidate) = Candidate::host(addr, "udp") {
            rtc.add_local_candidate(candidate);
            sockets.push(Arc::new(socket));
        }
    }
    Ok(sockets)
}

async fn run(
    rtc: Rtc,
    sockets: Vec<Arc<UdpSocket>>,
    mut commands: mpsc::UnboundedReceiver<Command>,
    hub: Arc<FrameHub>,
) -> Result<()> {
    // Datagrams from all sockets are funnelled into one channel. The readers stop when
    // `readers` is dropped.
    let (packet_tx, mut packets) = mpsc::channel::<Packet>(256);
    let mut readers = tokio::task::JoinSet::new();
    for socket in &sockets {
        readers.spawn(read_socket(socket.clone(), packet_tx.clone()));
    }

    let (keyframe_tx, mut keyframe_requests) = mpsc::unbounded_channel::<()>();
    let decoder = DecoderThread::spawn(hub, keyframe_tx)?;
    let mut conn = Connection { rtc, sockets, decoder, channel: None, video: None };

    loop {
        let Some(deadline) = conn.poll().await? else { return Ok(()) };
        tokio::select! {
            Some(packet) = packets.recv() => conn.receive(packet)?,
            _ = tokio::time::sleep_until(deadline.into()) => conn.rtc.handle_input(Input::Timeout(Instant::now()))?,
            Some(()) = keyframe_requests.recv() => conn.request_keyframe(),
            command = commands.recv() => match command {
                Some(Command::Send(message)) => conn.send(&message)?,
                Some(Command::Close) | None => return conn.close().await,
            },
        }
        if !conn.rtc.is_alive() {
            return Ok(());
        }
    }
}

/// A datagram: (contents, source, destination).
type Packet = (Vec<u8>, SocketAddr, SocketAddr);

/// Forward the datagrams arriving on `socket` to `tx`.
async fn read_socket(socket: Arc<UdpSocket>, tx: mpsc::Sender<Packet>) {
    let local = socket.local_addr().expect("bound socket");
    let mut buf = vec![0u8; 2000];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            // Windows reports an ICMP "port unreachable" for an earlier send (e.g. an ICE
            // check to a candidate that's gone) as an error on the next receive. The
            // socket is fine; keep reading.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(_) => break,
        };
        if tx.send((buf[..n].to_vec(), from, local)).await.is_err() {
            break;
        }
    }
}

/// The state of a running session's network loop.
struct Connection {
    rtc: Rtc,
    sockets: Vec<Arc<UdpSocket>>,
    decoder: DecoderThread,
    channel: Option<ChannelId>,
    video: Option<Mid>,
}

impl Connection {
    /// Handle str0m's output until it has to wait. Returns when to wake it up, or None
    /// once the phone disconnected.
    async fn poll(&mut self) -> Result<Option<Instant>> {
        loop {
            match self.rtc.poll_output()? {
                Output::Timeout(at) => return Ok(Some(at)),
                Output::Transmit(t) => self.transmit(t).await,
                Output::Event(event) => {
                    if !self.on_event(event) {
                        return Ok(None);
                    }
                }
            }
        }
    }

    /// False if the event ends the session.
    fn on_event(&mut self, event: Event) -> bool {
        match event {
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return false,
            Event::Connected => tracing::info!("WebRTC connected"),
            Event::ChannelOpen(id, _label) => self.channel = Some(id),
            Event::MediaAdded(media) => self.video = Some(media.mid),
            Event::MediaData(data) => self.decoder.push(data.data.clone(), data.contiguous, data.is_keyframe()),
            _ => {}
        }
        true
    }

    async fn transmit(&self, t: Transmit) {
        if let Some(s) = self.sockets.iter().find(|s| s.local_addr().ok() == Some(t.source)) {
            let _ = s.send_to(&t.contents, t.destination).await;
        }
    }

    fn receive(&mut self, (contents, source, destination): Packet) -> Result<()> {
        let receive = Receive { proto: Protocol::Udp, source, destination, contents: contents.as_slice().try_into()? };
        let input = Input::Receive(Instant::now(), receive);
        if self.rtc.accepts(&input) {
            self.rtc.handle_input(input)?;
        }
        Ok(())
    }

    fn request_keyframe(&mut self) {
        if let Some(mut writer) = self.video.and_then(|mid| self.rtc.writer(mid)) {
            let _ = writer.request_keyframe(None, KeyframeRequestKind::Pli);
        }
    }

    fn send(&mut self, message: &ServerMessage) -> Result<()> {
        if let Some(mut ch) = self.channel.and_then(|id| self.rtc.channel(id)) {
            let _ = ch.write(false, serde_json::to_string(message)?.as_bytes());
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        // Flush what's queued (e.g. a "replaced" message) before closing.
        let flush_until = Instant::now() + std::time::Duration::from_millis(200);
        while Instant::now() < flush_until {
            match self.rtc.poll_output()? {
                Output::Transmit(t) => self.transmit(t).await,
                Output::Timeout(_) => break,
                Output::Event(_) => {}
            }
        }
        self.rtc.disconnect();
        Ok(())
    }
}

/// Decodes frames on a dedicated thread so the network loop never waits for it.
struct DecoderThread {
    frames: std_mpsc::Sender<Encoded>,
    backlog: Arc<AtomicUsize>,
}

impl DecoderThread {
    fn spawn(hub: Arc<FrameHub>, keyframe_requests: mpsc::UnboundedSender<()>) -> Result<Self> {
        let decoder = H264Decoder::new()?;
        let (frames, rx) = std_mpsc::channel::<Encoded>();
        let backlog = Arc::new(AtomicUsize::new(0));
        let pending = backlog.clone();
        let decode_frames = move || decode_loop(decoder, rx, &pending, &hub, &keyframe_requests);
        std::thread::Builder::new().name("h264-decoder".into()).spawn(decode_frames)?;
        Ok(Self { frames, backlog })
    }

    fn push(&self, data: Arc<[u8]>, contiguous: bool, keyframe: bool) {
        self.backlog.fetch_add(1, Ordering::Relaxed);
        let _ = self.frames.send(Encoded { data, contiguous, keyframe });
    }
}

/// Body of the decoder thread.
fn decode_loop(
    mut decoder: H264Decoder,
    rx: std_mpsc::Receiver<Encoded>,
    pending: &AtomicUsize,
    hub: &FrameHub,
    keyframe_requests: &mpsc::UnboundedSender<()>,
) {
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
        if let Err(err) = decoder.decode(&data, |picture| hub.push_yuv(picture, data.len())) {
            tracing::debug!("decode error: {err:#}");
            hub.count_decode_error();
            need_keyframe = true;
            let _ = keyframe_requests.send(());
        }
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
    use str0m::media::{Direction, MediaKind};

    #[test]
    fn our_answer_gets_the_start_bitrate_hint() {
        // An offer like the phone's, answered the way `start` does.
        let mut phone = Rtc::new(Instant::now());
        let mut changes = phone.sdp_api();
        changes.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
        let (offer, _) = changes.apply().unwrap();
        let answer = rtc_config().build(Instant::now()).sdp_api().accept_offer(offer).unwrap();
        let sdp = with_start_bitrate(&answer.to_sdp_string(), 2500);

        let fmtp: Vec<&str> = sdp.lines().filter(|l| l.starts_with("a=fmtp:")).collect();
        let h264 = fmtp.iter().filter(|l| l.contains("profile-level-id")).count();
        assert!(h264 > 0, "{sdp}");
        for line in fmtp {
            assert_eq!(line.contains("x-google-start-bitrate=2500"), !line.contains("apt="), "{line}");
        }
    }
}
