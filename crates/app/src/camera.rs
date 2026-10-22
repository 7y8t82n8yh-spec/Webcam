//! Pushes frames from the hub to the virtual camera at a fixed rate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::hub::{Frame, FrameHub};

/// Where frames go.
pub trait CameraSink: Send {
    fn send(&mut self, frame: &Frame) -> anyhow::Result<()>;
    fn describe(&self) -> String;
}

/// Open the virtual camera.
pub fn open(name: &str, width: u32, height: u32) -> anyhow::Result<Box<dyn CameraSink>> {
    #[cfg(windows)]
    {
        return Ok(Box::new(crate::vcam_client::VirtualCamera::create(name, width, height)?));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(Box::new(crate::v4l2::LoopbackCamera::open(name, width, height)?));
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
    pub fn spawn(hub: Arc<FrameHub>, sink: Option<Box<dyn CameraSink>>, fps: u32, status: Arc<Mutex<String>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let interval = Duration::from_secs_f64(1.0 / fps.max(1) as f64);
        let output = move || output_loop(&hub, sink, interval, &stopping, &status);
        let thread =
            std::thread::Builder::new().name("virtual-camera".into()).spawn(output).expect("spawn camera thread");
        Self { stop, thread }
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
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
