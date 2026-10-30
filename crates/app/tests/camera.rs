//! End to end through the real virtual camera: fake phone → WebRTC → H.264 decoder →
//! virtual camera → read back the way video apps read it. Then turned a quarter: the
//! camera must come back portrait when it is opened again.
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
use common::test_app::{TestApp, CAMERA_NAME, OUTPUT_HEIGHT, OUTPUT_WIDTH};

const PHONE_COLOR: Rgb = [40, 160, 220];
/// Long enough to outlast the camera reader; the test aborts the phone when done.
const PHONE_STREAM_DURATION: Duration = Duration::from_secs(40);
const FRAMES_BEFORE_READING: u64 = 30;
const FRAMES_TIMEOUT: Duration = Duration::from_secs(20);
/// H.264, then the camera's YUV format, then back to RGB in the reader: allowed error per channel.
const COLOR_TOLERANCE: f64 = 25.0;

/// What a video app gets from the camera: its size and average colour.
#[derive(Debug)]
struct Reading {
    size: (u32, u32),
    average: Vec<f64>,
}

/// A reading, or why the camera couldn't be read.
type CameraReading = Result<Reading, String>;

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

    check_camera((OUTPUT_WIDTH, OUTPUT_HEIGHT), &camera).await;

    // A quarter turn: the next time a video app opens the camera, it is portrait.
    let hub = &test_app.app.hub;
    hub.set_rotation(hub.rotation().turned(1));
    tokio::time::sleep(Duration::from_secs(2)).await;
    check_camera((OUTPUT_HEIGHT, OUTPUT_WIDTH), &camera).await;
    phone.abort();
}

/// Open the camera as a video app does, and check it shows the phone's colour at `size`.
async fn check_camera(size: (u32, u32), camera: &str) {
    let reading = tokio::task::spawn_blocking(read_camera).await.unwrap().expect("camera reader configured");
    let reading = reading.unwrap_or_else(|error| panic!("{error}; camera: {camera}"));
    assert_eq!(reading.size, size, "{reading:?}");
    let average = &reading.average;
    assert_eq!(average.len(), 3, "{average:?}");
    for (got, want) in average.iter().zip(PHONE_COLOR) {
        assert!((got - want as f64).abs() <= COLOR_TOLERANCE, "camera shows {average:?}, phone sent {PHONE_COLOR:?}");
    }
}

/// Average RGB the camera shows, as an ordinary client sees it; `None` when not configured.
#[cfg(windows)]
fn read_camera() -> Option<CameraReading> {
    const PROBE_FRAMES: &str = "60";
    let probe = std::env::var_os("VCAM_PROBE")?;
    const CAMERA_NOT_FOUND: i32 = 2;
    // After a rotation the app restarts the camera, which takes Windows a moment.
    let mut output = Command::new(&probe).args([CAMERA_NAME, PROBE_FRAMES]).output().expect("run vcam-probe");
    for _ in 0..10 {
        if output.status.code() != Some(CAMERA_NOT_FOUND) {
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
        output = Command::new(&probe).args([CAMERA_NAME, PROBE_FRAMES]).output().expect("run vcam-probe");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("probe stdout:\n{stdout}\nprobe stderr:\n{stderr}");
    if !output.status.success() {
        return Some(Err(format!("vcam-probe failed ({})", output.status)));
    }
    // The probe prints "reading WxH RGB32", and the average colour on its last line.
    let numbers = |line: &str| -> Vec<u32> { line.split(['x', ' ']).filter_map(|n| n.parse().ok()).collect() };
    let size = stdout.lines().find(|line| line.starts_with("reading ")).map(numbers).unwrap_or_default();
    let last_line = stdout.lines().last().unwrap_or_default();
    let average: Vec<f64> = last_line.split_whitespace().filter_map(|value| value.parse().ok()).collect();
    let size = (size.first().copied().unwrap_or(0), size.get(1).copied().unwrap_or(0));
    Some(Ok(Reading { size, average }))
}

/// Average RGB the camera shows, as an ordinary client sees it; `None` when not configured.
#[cfg(target_os = "linux")]
fn read_camera() -> Option<CameraReading> {
    use common::images::average_rgb;

    const FFMPEG_FRAMES: &str = "30";
    std::env::var_os("CAMERA_E2E")?;
    let devices = iphone_webcam::v4l2::loopback_devices();
    // Prefer our device by its label; otherwise the first loopback device.
    let ours = devices.iter().find(|device| device.label == CAMERA_NAME);
    let device = ours.or(devices.first())?;
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-f", "v4l2", "-i"])
        .arg(&device.path)
        // PPM pictures, whose headers give the size.
        .args(["-frames:v", FFMPEG_FRAMES, "-f", "image2pipe", "-vcodec", "ppm", "-"])
        .output()
        .expect("run ffmpeg");
    eprintln!("ffmpeg stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    let size = ppm_size(&output.stdout).filter(|(w, h)| output.stdout.len() >= (w * h * 3) as usize);
    let Some(size) = size else {
        return Some(Err(format!("ffmpeg failed ({}, {} bytes)", output.status, output.stdout.len())));
    };
    let last_frame = &output.stdout[output.stdout.len() - (size.0 * size.1 * 3) as usize..];
    Some(Ok(Reading { size, average: average_rgb(last_frame).to_vec() }))
}

/// The size in the header of the PPM picture `data` starts with ("P6\nW H\n255\n").
#[cfg(target_os = "linux")]
fn ppm_size(data: &[u8]) -> Option<(u32, u32)> {
    let header = String::from_utf8_lossy(data.get(..20)?);
    let mut fields = header.split_ascii_whitespace().skip(1).map(|field| field.parse::<u32>().ok());
    Some((fields.next()??, fields.next()??))
}

#[cfg(not(any(windows, target_os = "linux")))]
fn read_camera() -> Option<CameraReading> {
    None
}
