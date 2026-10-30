//! The admin page's server: only for the page itself, on this PC.

mod common;

use common::test_app::{TestApp, OUTPUT_HEIGHT, OUTPUT_WIDTH};
use protocol::AdminState;
use reqwest::StatusCode;

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn state_and_rotation() {
    let test_app = TestApp::start();
    let url = &test_app.app.admin_url;
    let state: AdminState = client().get(format!("{url}api/state")).send().await.unwrap().json().await.unwrap();
    assert_eq!(state.urls, test_app.app.urls);
    assert_eq!((state.output_width, state.output_height, state.rotation), (OUTPUT_WIDTH, OUTPUT_HEIGHT, 0));

    let rotate = client()
        .post(format!("{url}api/rotate"))
        .header(protocol::ADMIN_HEADER, "1")
        .json(&protocol::RotateRequest { quarters: -1 })
        .send()
        .await
        .unwrap();
    let state: AdminState = rotate.json().await.unwrap();
    assert_eq!((state.output_width, state.output_height, state.rotation), (OUTPUT_HEIGHT, OUTPUT_WIDTH, 270));

    for path in ["", "qr.svg", "preview.jpg", "pkg/web.js"] {
        let response = client().get(format!("{url}{path}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn other_websites_are_refused() {
    let test_app = TestApp::start();
    let url = &test_app.app.admin_url;
    // A page from another site posting to us can't add the admin header.
    let response =
        client().post(format!("{url}api/rotate")).json(&protocol::RotateRequest { quarters: 1 }).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(test_app.app.hub.rotation().degrees(), 0);
    // A site whose name points at 127.0.0.1 (DNS rebinding) can't read the pairing key.
    let response = client().get(format!("{url}api/state")).header("Host", "evil.example").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
