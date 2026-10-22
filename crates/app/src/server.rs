//! HTTPS + WebSocket server the phone connects to.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use protocol::{ClientInfo, ClientMessage, OfferRequest, OfferResponse, ServerMessage, StreamConfig};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::hub::FrameHub;
use crate::webrtc::{self, RtcSession};
use crate::{decode, web_assets};

/// WebRTC start bitrate (bit/s): a clear 720p picture from the first second, low
/// enough that a weak Wi-Fi link recovers quickly.
const START_BITRATE: u32 = 2_500_000;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub struct Server {
    pub hub: Arc<FrameHub>,
    pub key: String,
    pub config: StreamConfig,
    pub ips: Vec<IpAddr>,
    pub ca_cert_der: Vec<u8>,
    pub client_info: Mutex<ClientInfo>,
    active: Mutex<Active>,
}

/// The one phone currently streaming.
#[derive(Default)]
struct Active {
    client: Option<String>,
    conn: Option<Conn>,
}

enum Conn {
    Ws { id: u64, commands: mpsc::UnboundedSender<WsCommand> },
    Rtc(RtcSession),
    Http,
}

enum WsCommand {
    Replaced,
    Close,
}

impl Conn {
    fn id(&self) -> Option<u64> {
        match self {
            Conn::Ws { id, .. } => Some(*id),
            Conn::Rtc(session) => Some(session.id + (1 << 62)), // separate id spaces
            Conn::Http => None,
        }
    }

    fn end(&self, tell_replaced: bool) {
        match self {
            Conn::Ws { commands, .. } => {
                if tell_replaced {
                    let _ = commands.send(WsCommand::Replaced);
                }
                let _ = commands.send(WsCommand::Close);
            }
            Conn::Rtc(session) => {
                if tell_replaced {
                    session.send(ServerMessage::Replaced);
                }
                session.close();
            }
            Conn::Http => {}
        }
    }
}

impl Server {
    pub fn new(hub: Arc<FrameHub>, key: String, config: StreamConfig, ips: Vec<IpAddr>, ca_cert_der: Vec<u8>) -> Self {
        Self {
            hub,
            key,
            config,
            ips,
            ca_cert_der,
            client_info: Mutex::new(ClientInfo::default()),
            active: Mutex::new(Active::default()),
        }
    }

    /// Make `client` the streaming phone, ending any previous connection. Only a
    /// *different* device is told it was replaced; a reconnecting page shouldn't give up.
    fn take_over(&self, client: &str, conn: Conn) {
        let mut active = self.active.lock().unwrap();
        let replaced = active.client.as_deref() != Some(client);
        if replaced {
            tracing::info!("Phone connected ({client})");
        }
        if let Some(old) = active.conn.take() {
            old.end(replaced);
        }
        active.client = Some(client.to_string());
        active.conn = Some(conn);
    }

    /// A connection ended by itself; forget it if it's still the active one.
    fn ended(&self, id: u64) {
        let mut active = self.active.lock().unwrap();
        if active.conn.as_ref().and_then(Conn::id) == Some(id) {
            active.conn = None;
            tracing::info!("Phone disconnected");
        }
    }

    pub fn shutdown(&self) {
        if let Some(conn) = self.active.lock().unwrap().conn.take() {
            conn.end(false);
        }
    }

    pub fn router(self: Arc<Self>) -> Router {
        let protected = Router::new()
            .route("/offer", post(offer))
            .route("/ws", get(websocket))
            .route("/frame", post(frame_post))
            .route("/status", get(status))
            .route_layer(middleware::from_fn_with_state(self.clone(), check_key));
        Router::new()
            .route("/", get(|| async { web_assets::serve("index.html") }))
            .route(
                "/pkg/{file}",
                get(|axum::extract::Path(file): axum::extract::Path<String>| async move {
                    web_assets::serve(&format!("pkg/{file}"))
                }),
            )
            .route("/config", get(config))
            .route("/ca.crt", get(ca_cert))
            .merge(protected)
            .layer(axum::extract::DefaultBodyLimit::max(MAX_FRAME_BYTES))
            .with_state(self)
    }
}

/// Endpoints that carry camera images need the pairing key from the QR code, so other
/// devices on the network can neither watch nor inject video.
async fn check_key(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let from_query =
        request.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("k="))).map(|k| k.to_string());
    let from_header = request.headers().get(protocol::KEY_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
    let given = from_query.or(from_header).unwrap_or_default();
    if !constant_time_eq(given.as_bytes(), server.key.as_bytes()) {
        return (StatusCode::FORBIDDEN, "missing or wrong pairing key - scan the QR code again").into_response();
    }
    next.run(request).await
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn config(State(server): State<Arc<Server>>) -> Json<StreamConfig> {
    Json(server.config.clone())
}

async fn ca_cert(State(server): State<Arc<Server>>) -> Response {
    // DER is the most widely accepted form for iOS "install profile" downloads.
    (
        [
            (header::CONTENT_TYPE, "application/x-x509-ca-cert"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"iPhoneWebcamCA.crt\""),
        ],
        server.ca_cert_der.clone(),
    )
        .into_response()
}

async fn status(State(server): State<Arc<Server>>) -> Response {
    let s = server.hub.stats();
    Json(serde_json::json!({
        "streaming": server.hub.streaming(),
        "client": *server.client_info.lock().unwrap(),
        "framesIn": s.frames_in,
        "framesOut": s.frames_out,
        "decodeErrors": s.decode_errors,
        "transport": s.transport,
        "sourceSize": [s.source_size.0, s.source_size.1],
        "outputSize": [server.hub.width, server.hub.height],
    }))
    .into_response()
}

/// WebRTC signalling: take the phone's offer, return our answer plus settings.
async fn offer(State(server): State<Arc<Server>>, Json(request): Json<OfferRequest>) -> Response {
    if !decode::available() {
        // The page falls back to JPEG frames.
        return (StatusCode::NOT_IMPLEMENTED, "no H.264 decoder in this build").into_response();
    }
    *server.client_info.lock().unwrap() = request.info.clone();
    let weak = Arc::downgrade(&server);
    let on_end = move |id: u64| {
        if let Some(server) = weak.upgrade() {
            server.ended(id + (1 << 62));
        }
    };
    let started = webrtc::start(
        &request.sdp,
        request.id.clone(),
        &server.ips,
        server.hub.clone(),
        START_BITRATE.min(server.config.max_bitrate),
        on_end,
    )
    .await;
    match started {
        // If the phone gave up waiting, this handler is dropped before getting here, so
        // an abandoned attempt can't take over from a fallback connection.
        Ok((sdp, session)) => {
            server.take_over(&request.id, Conn::Rtc(session));
            Json(OfferResponse { sdp, config: server.config.clone() }).into_response()
        }
        Err(err) => {
            tracing::warn!("WebRTC negotiation failed: {err:#}");
            (StatusCode::BAD_REQUEST, format!("negotiation failed: {err:#}")).into_response()
        }
    }
}

/// JPEG fallback over plain HTTPS, for when WebSockets can't connect either.
async fn frame_post(State(server): State<Arc<Server>>, headers: HeaderMap, body: Bytes) -> Response {
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty frame").into_response();
    }
    let client = headers.get(protocol::CLIENT_ID_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("http").to_string();
    let is_active = server.active.lock().unwrap().client.as_deref() == Some(client.as_str());
    if !is_active {
        server.take_over(&client, Conn::Http);
    }
    let hub = server.hub.clone();
    let ok = tokio::task::spawn_blocking(move || hub.push_jpeg(&body, "JPEG")).await.unwrap_or(false);
    Json(serde_json::json!({ "ok": ok })).into_response()
}

#[derive(Deserialize)]
struct WsQuery {
    id: Option<String>,
}

async fn websocket(State(server): State<Arc<Server>>, Query(query): Query<WsQuery>, ws: WebSocketUpgrade) -> Response {
    let client = query.id.unwrap_or_else(|| "websocket".into());
    ws.max_message_size(MAX_FRAME_BYTES).on_upgrade(move |socket| handle_socket(server, client, socket))
}

async fn handle_socket(server: Arc<Server>, client: String, socket: WebSocket) {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (commands, mut command_rx) = mpsc::unbounded_channel();
    server.take_over(&client, Conn::Ws { id, commands });

    let (mut tx, mut rx) = socket.split();
    let config = serde_json::to_string(&ServerMessage::Config(server.config.clone())).unwrap();
    if tx.send(Message::Text(config.into())).await.is_err() {
        server.ended(id);
        return;
    }
    loop {
        tokio::select! {
            message = rx.next() => match message {
                Some(Ok(Message::Binary(data))) => {
                    let hub = server.hub.clone();
                    let ok = tokio::task::spawn_blocking(move || hub.push_jpeg(&data, "JPEG")).await.unwrap_or(false);
                    let reply = if ok { protocol::ACK } else { protocol::BAD };
                    if tx.send(Message::Text(reply.into())).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ClientMessage::Hello(info)) = serde_json::from_str(&text) {
                        tracing::info!("Phone info: {info:?}");
                        *server.client_info.lock().unwrap() = info;
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            command = command_rx.recv() => match command {
                Some(WsCommand::Replaced) => {
                    let msg = serde_json::to_string(&ServerMessage::Replaced).unwrap();
                    let _ = tx.send(Message::Text(msg.into())).await;
                }
                Some(WsCommand::Close) | None => {
                    let _ = tx.send(Message::Close(None)).await;
                    break;
                }
            },
        }
    }
    server.ended(id);
}

/// Serve `router` over HTTPS until `shutdown` resolves.
pub async fn serve(
    router: Router,
    addr: SocketAddr,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    started: impl FnOnce(SocketAddr) + Send,
) -> anyhow::Result<()> {
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem).await?;
    let handle = axum_server::Handle::new();
    let h = handle.clone();
    tokio::spawn(async move {
        shutdown.await;
        h.graceful_shutdown(Some(std::time::Duration::from_secs(1)));
    });
    let server = axum_server::bind_rustls(addr, tls).handle(handle.clone());
    let serving = tokio::spawn(server.serve(router.into_make_service()));
    if let Some(bound) = handle.listening().await {
        started(bound);
    }
    serving.await??;
    Ok(())
}
