//! End-to-end tests against the app running on localhost: the pairing key, and video
//! arriving as JPEG over HTTPS posts, JPEG over a WebSocket, and H.264 over WebRTC.
//!
//! Never skipped: everything they need runs in-process.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::fake_phone::{self, FRAME_HEIGHT, FRAME_WIDTH};
use common::images::{is_close, solid_jpeg, Rgb};
use common::test_app::{AppClient, TestApp, OUTPUT_HEIGHT, OUTPUT_WIDTH};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};

const SKY_BLUE: Rgb = [40, 180, 220];
const ORANGE: Rgb = [220, 120, 40];
const GREEN: Rgb = [10, 200, 10];
const RED: Rgb = [200, 10, 10];
const BLACK: Rgb = [0, 0, 0];

/// JPEG is lossy: allowed error per channel in the output's average colour.
const JPEG_TOLERANCE: f64 = 8.0;
/// H.264 is lossier still.
const H264_TOLERANCE: f64 = 12.0;
/// After a takeover the output may still hold a little of the first phone's picture.
const TAKEOVER_TOLERANCE: f64 = 15.0;

#[tokio::test(flavor = "multi_thread")]
async fn endpoints_require_the_pairing_key() {
    let test_app = TestApp::start();
    let client = &test_app.client;

    for path in ["/status", "/status?k=wrong"] {
        let response = client.get(path).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    for path in ["/frame", "/offer"] {
        let response = client.http.post(client.url(path)).body("x").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }
    // The page, its code and the CA certificate stay public: the phone needs them before pairing.
    for path in ["/", "/pkg/web.js", "/ca.crt", "/config"] {
        let response = client.get(path).await;
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn jpeg_frames_over_https_post() {
    const FRAMES: u64 = 5;
    let test_app = TestApp::start();
    let portrait_jpeg = solid_jpeg(OUTPUT_HEIGHT, OUTPUT_WIDTH, SKY_BLUE);

    for _ in 0..FRAMES {
        let reply = post_frame(&test_app.client, "phone-1", portrait_jpeg.clone()).await;
        assert_eq!(reply["ok"], true);
    }

    let status = test_app.client.status().await;
    assert_eq!(status["framesIn"], FRAMES);
    assert_eq!(status["transport"], "JPEG");
    assert_eq!(status["sourceSize"], serde_json::json!([OUTPUT_HEIGHT, OUTPUT_WIDTH]));
    // Letterboxed into landscape: black bars at the sides, the picture in the middle.
    let frame = test_app.app.hub.latest();
    assert_eq!(&frame.rgb[..3], &BLACK);
    let center = ((OUTPUT_HEIGHT / 2 * OUTPUT_WIDTH + OUTPUT_WIDTH / 2) * 3) as usize;
    let center_pixel = &frame.rgb[center..center + 3];
    assert!(center_pixel[2] > 200, "{center_pixel:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn jpeg_frames_over_websocket_with_acks() {
    const FRAMES: usize = 3;
    let test_app = TestApp::start();
    let mut socket = connect_websocket(&test_app.client, "phone-ws").await;
    let greeting = next_text(&mut socket).await;
    assert!(greeting.contains(r#""type":"config""#), "{greeting}");

    let jpeg = solid_jpeg(OUTPUT_WIDTH, OUTPUT_HEIGHT, ORANGE);
    for _ in 0..FRAMES {
        socket.send(Message::Binary(jpeg.clone().into())).await.unwrap();
        assert_eq!(next_text(&mut socket).await, protocol::ACK);
    }
    socket.send(Message::Binary(b"garbage".to_vec().into())).await.unwrap();
    assert_eq!(next_text(&mut socket).await, protocol::BAD);

    let color = test_app.output_color();
    assert!(is_close(color, ORANGE, JPEG_TOLERANCE), "{color:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn webrtc_h264_stream_reaches_the_camera_output() {
    let test_app = TestApp::start();
    let client = test_app.client.clone();
    let phone =
        tokio::spawn(
            async move { fake_phone::stream_webrtc(&client, "phone-rtc", ORANGE, Duration::from_secs(6)).await },
        );

    // Check while the phone still streams: once it stops, the output turns to the
    // "waiting" image after a few seconds.
    let status = test_app.client.wait_for_frames(20, Duration::from_secs(5)).await;
    assert_eq!(status["transport"], "WebRTC", "{status}");
    assert_eq!(status["sourceSize"], serde_json::json!([FRAME_WIDTH, FRAME_HEIGHT]));
    assert_eq!(status["decodeErrors"], 0);
    // The phone's 16:9 frames scale up to fill the output.
    let color = test_app.output_color();
    assert!(is_close(color, ORANGE, H264_TOLERANCE), "{color:?}");

    let report = phone.await.unwrap();
    assert!(report.connected, "ICE/DTLS connected");
    assert!(report.frames_sent > 30);
}

#[tokio::test(flavor = "multi_thread")]
async fn second_phone_takes_over_and_first_is_told() {
    let test_app = TestApp::start();
    let client = test_app.client.clone();
    let first =
        tokio::spawn(async move { fake_phone::stream_webrtc(&client, "phone-a", GREEN, Duration::from_secs(4)).await });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let second = fake_phone::stream_webrtc(&test_app.client, "phone-b", RED, Duration::from_secs(2)).await;
    let first = first.await.unwrap();

    assert!(second.connected);
    let first_was_told = first.control_messages.iter().any(|message| message.contains(r#""type":"replaced""#));
    assert!(first_was_told, "first phone got {:?}", first.control_messages);
    let color = test_app.output_color();
    assert!(is_close(color, RED, TAKEOVER_TOLERANCE), "{color:?}");
}

/// POST one JPEG to `/frame` as `client_id`; returns the app's JSON reply.
async fn post_frame(client: &AppClient, client_id: &str, jpeg: Vec<u8>) -> serde_json::Value {
    let request = client
        .http
        .post(client.url("/frame"))
        .header(protocol::KEY_HEADER, &client.key)
        .header(protocol::CLIENT_ID_HEADER, client_id)
        .body(jpeg);
    let response = request.send().await.unwrap();
    response.json().await.unwrap()
}

type WebSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_websocket(client: &AppClient, client_id: &str) -> WebSocket {
    let https_url = client.url(&format!("/ws?id={client_id}&k={}", client.key));
    let url = https_url.replace("https", "wss");
    let connector = trust_any_certificate();
    let connecting = tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector));
    let (socket, _response) = connecting.await.unwrap();
    socket
}

/// The next message, which must be text.
async fn next_text(socket: &mut WebSocket) -> Utf8Bytes {
    let message = socket.next().await.unwrap().unwrap();
    message.into_text().unwrap()
}

/// TLS that accepts the app's self-signed certificate.
fn trust_any_certificate() -> tokio_tungstenite::Connector {
    let tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate))
        .with_no_client_auth();
    tokio_tungstenite::Connector::Rustls(Arc::new(tls))
}

#[derive(Debug)]
struct AcceptAnyCertificate;

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}
