//! End to end on Windows 11: fake phone → WebRTC → Media Foundation decoder → virtual
//! camera → read back by an ordinary Media Foundation client (`vcam-probe`).
//!
//! Needs the camera installed (`iphone-webcam install`) and VCAM_PROBE set to the
//! probe's path; skipped otherwise. CI runs it on a Windows runner.

mod common;

use std::time::Duration;

const COLOR: [u8; 3] = [40, 160, 220];

#[tokio::test(flavor = "multi_thread")]
async fn phone_video_reaches_media_foundation_apps() {
    let Some(probe) = std::env::var_os("VCAM_PROBE") else {
        eprintln!("skipping: VCAM_PROBE not set");
        return;
    };
    let t = common::start_with(true);
    let target = t.target();
    let phone = tokio::spawn(async move { common::webrtc_phone(&target, "phone", COLOR, 40.0).await });
    let status = t.wait_for_frames(30, Duration::from_secs(20)).await;
    assert_eq!(status["transport"], "WebRTC", "{status}");
    let camera = t.app.camera_status.lock().unwrap().clone();
    eprintln!("camera: {camera}");

    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(probe).args(["iPhone Webcam", "60"]).output().expect("run vcam-probe")
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("probe stdout:\n{stdout}\nprobe stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success(), "vcam-probe failed ({}); camera: {camera}", output.status);
    let average: Vec<f64> =
        stdout.lines().last().unwrap_or_default().split_whitespace().filter_map(|v| v.parse().ok()).collect();
    assert_eq!(average.len(), 3, "probe output: {stdout}");
    // H.264, then BT.601 NV12 in the camera, then back to RGB in the reader: allow some error.
    for (got, want) in average.iter().zip(COLOR) {
        assert!((got - want as f64).abs() <= 25.0, "camera shows {average:?}, phone sent {COLOR:?}");
    }
    phone.abort();
}
