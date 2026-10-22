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

use common::fake_phone;
use common::images::Rgb;
use common::test_app::{TestApp, CAMERA_NAME};

const PHONE_COLOR: Rgb = [40, 160, 220];
/// Long enough to outlast the camera reader; the test aborts the phone when done.
const PHONE_STREAM_DURATION: Duration = Duration::from_secs(40);
const FRAMES_BEFORE_READING: u64 = 30;
const FRAMES_TIMEOUT: Duration = Duration::from_secs(20);
/// H.264, then the camera's YUV format, then back to RGB in the reader: allowed error per channel.
const COLOR_TOLERANCE: f64 = 25.0;

/// The camera's average colour, or why it couldn't be read.
type CameraReading = Result<Vec<f64>, String>;

#[tokio::test(flavor = "multi_thread")]
async fn phone_video_reaches_video_apps() {
    if std::env::var_os("VCAM_PROBE").is_none() && std::env::var_os("CAMERA_E2E").is_none() {
        eprintln!("skipping: set VCAM_PROBE (Windows) or CAMERA_E2E (Linux)");
        return;
    }
    let test_app = TestApp::start_with_virtual_camera();
    let client = test_app.client.clone();
    let phone =
        tokio::spawn(
            async move { fake_phone::stream_webrtc(&client, "phone", PHONE_COLOR, PHONE_STREAM_DURATION).await },
        );
    let status = test_app.client.wait_for_frames(FRAMES_BEFORE_READING, FRAMES_TIMEOUT).await;
    assert_eq!(status["transport"], "WebRTC", "{status}");
    let camera = test_app.app.camera_status.lock().unwrap().clone();
    eprintln!("camera: {camera}");

    let reading = tokio::task::spawn_blocking(read_camera).await.unwrap().expect("camera reader configured");
    let average = reading.unwrap_or_else(|error| panic!("{error}; camera: {camera}"));

    assert_eq!(average.len(), 3, "{average:?}");
    for (got, want) in average.iter().zip(PHONE_COLOR) {
        assert!((got - want as f64).abs() <= COLOR_TOLERANCE, "camera shows {average:?}, phone sent {PHONE_COLOR:?}");
    }
    phone.abort();
}

/// Average RGB the camera shows, as an ordinary client sees it; `None` when not configured.
#[cfg(windows)]
fn read_camera() -> Option<CameraReading> {
    const PROBE_FRAMES: &str = "60";
    let probe = std::env::var_os("VCAM_PROBE")?;
    let output = Command::new(probe).args([CAMERA_NAME, PROBE_FRAMES]).output().expect("run vcam-probe");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("probe stdout:\n{stdout}\nprobe stderr:\n{stderr}");
    if !output.status.success() {
        return Some(Err(format!("vcam-probe failed ({})", output.status)));
    }
    // The probe prints the average colour of the frames it read on its last line.
    let last_line = stdout.lines().last().unwrap_or_default();
    let average: Vec<f64> = last_line.split_whitespace().filter_map(|value| value.parse().ok()).collect();
    Some(Ok(average))
}

/// Average RGB the camera shows, as an ordinary client sees it; `None` when not configured.
#[cfg(target_os = "linux")]
fn read_camera() -> Option<CameraReading> {
    use common::images::average_rgb;
    use common::test_app::{OUTPUT_HEIGHT, OUTPUT_WIDTH};

    const FFMPEG_FRAMES: &str = "30";
    const FRAME_BYTES: usize = (OUTPUT_WIDTH * OUTPUT_HEIGHT * 3) as usize;
    std::env::var_os("CAMERA_E2E")?;
    let devices = iphone_webcam::v4l2::loopback_devices();
    // Prefer our device by its label; otherwise the first loopback device.
    let ours = devices.iter().find(|(_, label)| label == CAMERA_NAME);
    let (device, _) = ours.or(devices.first())?;
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "v4l2", "-i"])
        .arg(device)
        .args(["-frames:v", FFMPEG_FRAMES, "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("run ffmpeg");
    eprintln!("ffmpeg stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() || output.stdout.len() < FRAME_BYTES {
        return Some(Err(format!("ffmpeg failed ({}, {} bytes)", output.status, output.stdout.len())));
    }
    let last_frame = &output.stdout[output.stdout.len() - FRAME_BYTES..];
    Some(Ok(average_rgb(last_frame).to_vec()))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn read_camera() -> Option<CameraReading> {
    None
}
