//! Pushes frames from the hub to the virtual camera at a fixed rate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::hub::{Frame, FrameHub};

/// Where frames go.
pub trait CameraSink {
    fn send(&mut self, frame: &Frame) -> anyhow::Result<()>;
    fn describe(&self) -> String;
}

/// The virtual camera to open.
pub struct CameraConfig {
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// Open the virtual camera.
fn open(config: &CameraConfig) -> anyhow::Result<Box<dyn CameraSink>> {
    let CameraConfig { name, width, height } = config;
    #[cfg(windows)]
    {
        return Ok(Box::new(crate::vcam_client::VirtualCamera::create(name, *width, *height)?));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(Box::new(crate::v4l2::LoopbackCamera::open(name, *width, *height)?));
    }
    #[allow(unreachable_code)]
    {
        let _ = (name, width, height);
        anyhow::bail!("virtual cameras need Windows 11 or Linux")
    }
}

pub struct OutputThread {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl OutputThread {
    /// Open the camera (`None`: none wanted) on a new thread and send it the latest
    /// frame `fps` times a second. Returns once the camera is open (or failed to), with
    /// the outcome in `status`.
    ///
    /// The camera is opened on the thread that uses it: the Windows one is a COM object,
    /// which then never has to move between threads.
    pub fn spawn(hub: Arc<FrameHub>, camera: Option<CameraConfig>, fps: u32, status: Arc<Mutex<String>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        let (opened_tx, opened_rx) = mpsc::channel();
        let output = move || {
            let sink = open_and_report(camera, &status);
            let _ = opened_tx.send(());
            output_loop(&hub, sink, interval, &stopping, &status);
        };
        let thread =
            std::thread::Builder::new().name("virtual-camera".into()).spawn(output).expect("spawn camera thread");
        let _ = opened_rx.recv();
        Self { stop, thread }
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

fn open_and_report(camera: Option<CameraConfig>, status: &Mutex<String>) -> Option<Box<dyn CameraSink>> {
    let Some(camera) = camera else {
        *status.lock().unwrap() = "disabled (--no-camera)".into();
        return None;
    };
    match open(&camera) {
        Ok(sink) => {
            *status.lock().unwrap() = sink.describe();
            Some(sink)
        }
        Err(err) => {
            tracing::error!("virtual camera unavailable: {err:#}");
            *status.lock().unwrap() = format!("unavailable: {err:#}");
            None
        }
    }
}

/// Body of the output thread: send the latest frame every `interval` until `stopping`.
fn output_loop(
    hub: &FrameHub,
    mut sink: Option<Box<dyn CameraSink>>,
    interval: Duration,
    stopping: &AtomicBool,
    status: &Mutex<String>,
) {
    let mut next = Instant::now();
    while !stopping.load(Ordering::Relaxed) {
        if let Some(camera) = sink.as_mut() {
            if let Err(err) = camera.send(&hub.latest()) {
                // Surface it instead of dying silently.
                tracing::error!("virtual camera stopped: {err:#}");
                *status.lock().unwrap() = format!("stopped after an error: {err:#}");
                sink = None;
            }
        }
        hub.count_output_frame();
        next += interval;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // fell behind; don't try to catch up
        }
    }
}
