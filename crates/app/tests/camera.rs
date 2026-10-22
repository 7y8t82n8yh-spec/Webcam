//! End to end through the real virtual camera: fake phone → WebRTC → H.264 decoder →
//! virtual camera → read back the way video apps read it.
//!
//! - Windows 11: read through Media Foundation by `vcam-probe`; set VCAM_PROBE to its path.
//! - Linux: read from the v4l2loopback device with ffmpeg; set CAMERA_E2E=1.
//!
//! Needs `iphone-webcam install` first; skipped unless the variable is set. CI runs it.

mod common;

use std::process::Command;
use std::time::Duration;

const COLOR: [u8; 3] = [40, 160, 220];

/// Average RGB the camera shows, as an ordinary client sees it.
#[cfg(windows)]
fn read_camera() -> Option<Result<Vec<f64>, String>> {
    let probe = std::env::var_os("VCAM_PROBE")?;
    let output = Command::new(probe).args(["iPhone Webcam", "60"]).output().expect("run vcam-probe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("probe stdout:\n{stdout}\nprobe stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        return Some(Err(format!("vcam-probe failed ({})", output.status)));
    }
    let last_line = stdout.lines().last().unwrap_or_default();
    let average: Vec<f64> = last_line.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    Some(Ok(average))
}

#[cfg(target_os = "linux")]
fn read_camera() -> Option<Result<Vec<f64>, String>> {
    std::env::var_os("CAMERA_E2E")?;
    let devices = iphone_webcam::v4l2::loopback_devices();
    let (device, _) = devices.iter().find(|(_, label)| label == "iPhone Webcam").or(devices.first())?;
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "v4l2", "-i"])
        .arg(device)
        .args(["-frames:v", "30", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("run ffmpeg");
    eprintln!("ffmpeg stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    let frame = 1280 * 720 * 3;
    if !output.status.success() || output.stdout.len() < frame {
        return Some(Err(format!("ffmpeg failed ({}, {} bytes)", output.status, output.stdout.len())));
    }
    let last = &output.stdout[output.stdout.len() - frame..];
    let mut sum = [0f64; 3];
    for px in last.chunks_exact(3) {
        for c in 0..3 {
            sum[c] += px[c] as f64;
        }
    }
    let average: Vec<f64> = sum.iter().map(|s| s / (1280.0 * 720.0)).collect();
    Some(Ok(average))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn read_camera() -> Option<Result<Vec<f64>, String>> {
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn phone_video_reaches_video_apps() {
    if std::env::var_os("VCAM_PROBE").is_none() && std::env::var_os("CAMERA_E2E").is_none() {
        eprintln!("skipping: set VCAM_PROBE (Windows) or CAMERA_E2E (Linux)");
        return;
    }
    let t = common::start_with(true);
    let target = t.target();
    let phone = tokio::spawn(async move { common::webrtc_phone(&target, "phone", COLOR, 40.0).await });
    let status = t.wait_for_frames(30, Duration::from_secs(20)).await;
    assert_eq!(status["transport"], "WebRTC", "{status}");
    let camera = t.app.camera_status.lock().unwrap().clone();
    eprintln!("camera: {camera}");

    let average = tokio::task::spawn_blocking(read_camera).await.unwrap().expect("camera reader configured");
    let average = average.unwrap_or_else(|e| panic!("{e}; camera: {camera}"));
    assert_eq!(average.len(), 3, "{average:?}");
    // H.264, then the camera's YUV format, then back to RGB in the reader: allow some error.
    for (got, want) in average.iter().zip(COLOR) {
        assert!((got - want as f64).abs() <= 25.0, "camera shows {average:?}, phone sent {COLOR:?}");
    }
    phone.abort();
}
