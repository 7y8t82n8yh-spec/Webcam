//! End-to-end tests against a running app on localhost.

mod common;

use std::time::Duration;

use common::{solid_jpeg, start, webrtc_phone};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;

fn close_to(got: [f64; 3], want: [u8; 3], tolerance: f64) -> bool {
    got.iter().zip(want).all(|(g, w)| (g - w as f64).abs() <= tolerance)
}

#[tokio::test(flavor = "multi_thread")]
async fn endpoints_require_the_pairing_key() {
    let t = start();
    for path in ["/status", "/status?k=wrong"] {
        let r = t.client.get(format!("{}{path}", t.base)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{path}");
    }
    for path in ["/frame", "/offer"] {
        let r = t.client.post(format!("{}{path}", t.base)).body("x").send().await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{path}");
    }
    // The page, its code and the CA certificate stay public.
    for path in ["/", "/pkg/web.js", "/ca.crt", "/config"] {
        let r = t.client.get(format!("{}{path}", t.base)).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn jpeg_frames_over_https_post() {
    let t = start();
    let jpeg = solid_jpeg(720, 1280, [40, 180, 220]); // portrait phone
    for _ in 0..5 {
        let r = t
            .client
            .post(format!("{}/frame", t.base))
            .header(protocol::KEY_HEADER, &t.key)
            .header(protocol::CLIENT_ID_HEADER, "phone-1")
            .body(jpeg.clone())
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(body["ok"], true);
    }
    let status = t.status().await;
    assert_eq!(status["framesIn"], 5);
    assert_eq!(status["transport"], "JPEG");
    assert_eq!(status["sourceSize"], serde_json::json!([720, 1280]));
    // Letterboxed into 1280x720: 405 of 1280 columns are picture, the rest black bars.
    let frame = t.app.hub.latest();
    assert_eq!(&frame.rgb[..3], &[0, 0, 0]);
    let mid = ((360 * 1280 + 640) * 3) as usize;
    assert!(frame.rgb[mid + 2] > 200, "{:?}", &frame.rgb[mid..mid + 3]);
}

#[tokio::test(flavor = "multi_thread")]
async fn jpeg_frames_over_websocket_with_acks() {
    let t = start();
    let url = format!("{}/ws?id=phone-ws&k={}", t.base.replace("https", "wss"), t.key);
    let connector = trust_any_certificate();
    let (mut ws, _) =
        tokio_tungstenite::connect_async_tls_with_config(url, None, false, Some(connector)).await.unwrap();

    let first = next_text(&mut ws).await;
    assert!(first.contains(r#""type":"config""#), "{first}");
    let jpeg = solid_jpeg(1280, 720, [220, 120, 40]);
    for _ in 0..3 {
        ws.send(tokio_tungstenite::tungstenite::Message::Binary(jpeg.clone().into())).await.unwrap();
        assert_eq!(next_text(&mut ws).await, protocol::ACK);
    }
    ws.send(tokio_tungstenite::tungstenite::Message::Binary(b"garbage".to_vec().into())).await.unwrap();
    assert_eq!(next_text(&mut ws).await, protocol::BAD);
    let color = t.output_color();
    assert!(close_to(color, [220, 120, 40], 8.0), "{color:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn webrtc_h264_stream_reaches_the_camera_output() {
    let t = start();
    let target = t.target();
    // Check while the phone still streams: once it stops, the output turns to the
    // "waiting" image after a few seconds.
    let phone = tokio::spawn(async move { webrtc_phone(&target, "phone-rtc", [220, 120, 40], 6.0).await });
    let status = t.wait_for_frames(20, Duration::from_secs(5)).await;
    assert_eq!(status["transport"], "WebRTC", "{status}");
    assert_eq!(status["sourceSize"], serde_json::json!([640, 360]));
    assert_eq!(status["decodeErrors"], 0);
    // 640x360 scaled into 1280x720 fills the frame; H.264 is lossy, allow some error.
    let color = t.output_color();
    assert!(close_to(color, [220, 120, 40], 12.0), "{color:?}");
    let report = phone.await.unwrap();
    assert!(report.connected, "ICE/DTLS connected");
    assert!(report.frames_sent > 30);
}

#[tokio::test(flavor = "multi_thread")]
async fn second_phone_takes_over_and_first_is_told() {
    let t = start();
    let target = t.target();
    let first = tokio::spawn(async move { webrtc_phone(&target, "phone-a", [10, 200, 10], 4.0).await });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let second = webrtc_phone(&t.target(), "phone-b", [200, 10, 10], 2.0).await;
    let first = first.await.unwrap();
    assert!(second.connected);
    assert!(
        first.control_messages.iter().any(|m| m.contains(r#""type":"replaced""#)),
        "first phone got {:?}",
        first.control_messages
    );
    let color = t.output_color();
    assert!(close_to(color, [200, 10, 10], 15.0), "{color:?}");
}

type WebSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The next message, which must be text.
async fn next_text(ws: &mut WebSocket) -> tokio_tungstenite::tungstenite::Utf8Bytes {
    let message = ws.next().await.unwrap().unwrap();
    message.into_text().unwrap()
}

/// TLS that accepts the app's self-signed certificate.
fn trust_any_certificate() -> tokio_tungstenite::Connector {
    let tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(NoVerify))
        .with_no_client_auth();
    tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(tls))
}

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}
