//! Linux virtual camera: frames are written to a v4l2loopback device, which video apps
//! then open like any webcam. `iphone-webcam install` loads the module with a device
//! labelled after the camera; any other loopback device works too.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context, Result};

use crate::camera::CameraSink;
use crate::hub::{self, Frame};

pub struct LoopbackDevice {
    pub path: PathBuf,
    pub label: String,
}

/// The v4l2loopback devices, in device order.
///
/// sysfs says which they are: only v4l2loopback devices have a `max_openers` attribute.
pub fn loopback_devices() -> Vec<LoopbackDevice> {
    let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") else { return Vec::new() };
    let mut devices = Vec::new();
    for entry in entries.flatten() {
        let sysfs = entry.path();
        if !sysfs.join("max_openers").exists() {
            continue;
        }
        let label = std::fs::read_to_string(sysfs.join("name")).unwrap_or_default();
        devices
            .push(LoopbackDevice { path: PathBuf::from("/dev").join(entry.file_name()), label: label.trim().into() });
    }
    // video2 before video10.
    devices.sort_by_key(|device| {
        let name = device.path.to_string_lossy();
        name.trim_start_matches("/dev/video").parse::<u32>().unwrap_or(u32::MAX)
    });
    devices
}

pub struct LoopbackCamera {
    /// None only if reopening it after a resize failed.
    file: Option<File>,
    device: LoopbackDevice,
    width: u32,
    height: u32,
    yuyv: Vec<u8>,
    last_resize_attempt: Option<Instant>,
}

impl LoopbackCamera {
    /// Open the loopback device labelled `name`, else the first one there is.
    pub fn open(name: &str, width: u32, height: u32) -> Result<Self> {
        let mut devices = loopback_devices();
        if devices.is_empty() {
            bail!("no v4l2loopback device: run `sudo iphone-webcam install` (needs the v4l2loopback module)");
        }
        let index = devices.iter().position(|device| device.label == name).unwrap_or(0);
        let device = devices.swap_remove(index);
        let file = open_writer(&device.path, width, height)?;
        let yuyv = vec![0; (width * height * 2) as usize];
        Ok(Self { file: Some(file), device, width, height, yuyv, last_resize_attempt: None })
    }

    /// Switch the device to `width x height` (after a rotation). The driver keeps a
    /// device's size while anyone has it open, us included, so this waits until no
    /// video app has it open (checked once a second), then reopens it.
    fn try_resize(&mut self, width: u32, height: u32) {
        if self.last_resize_attempt.is_some_and(|at| at.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.last_resize_attempt = Some(Instant::now());
        let path = &self.device.path;
        if open_in_other_processes(path) {
            return;
        }
        self.file = None;
        match open_writer(path, width, height) {
            Ok(file) => {
                tracing::info!("{} is now {width}x{height}", path.display());
                self.file = Some(file);
                (self.width, self.height) = (width, height);
                self.yuyv.resize((width * height * 2) as usize, 0);
            }
            Err(err) => {
                tracing::warn!("could not resize {}: {err:#}", path.display());
                self.file = open_writer(path, self.width, self.height).ok();
            }
        }
    }
}

/// Open the device at `path` for writing `width x height` YUYV frames.
fn open_writer(path: &Path, width: u32, height: u32) -> Result<File> {
    let file = OpenOptions::new().write(true).open(path).with_context(|| path.display().to_string())?;
    let mut format = Format::yuyv(width, height);
    set_format(&file, &mut format).with_context(|| format!("setting the format of {}", path.display()))?;
    // The device may adjust the request; the frames we write must match what it settled on.
    ensure!(
        format.width == width && format.height == height && format.pixelformat == PIX_FMT_YUYV,
        "{} refused {width}x{height} YUYV (in use by another program?)",
        path.display()
    );
    Ok(file)
}

/// Whether another process has `path` open, going by the open files /proc lists (of
/// the processes we may look at: ours, so the video apps the user runs).
fn open_in_other_processes(path: &Path) -> bool {
    let own_pid = std::process::id().to_string();
    let Ok(processes) = std::fs::read_dir("/proc") else { return false };
    let others = processes.flatten().filter(|process| process.file_name() != own_pid.as_str());
    let open_files = others.filter_map(|process| std::fs::read_dir(process.path().join("fd")).ok()).flatten();
    open_files.flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == path))
}

impl CameraSink for LoopbackCamera {
    fn send(&mut self, frame: &Frame) -> Result<()> {
        if (frame.width, frame.height) != (self.width, self.height) {
            self.try_resize(frame.width, frame.height);
        }
        let letterboxed;
        let frame = if (frame.width, frame.height) == (self.width, self.height) {
            frame
        } else {
            // Still the old size: a video app has the camera open.
            letterboxed = hub::letterbox(frame, self.width, self.height);
            &letterboxed
        };
        let path = &self.device.path;
        let Some(file) = self.file.as_mut() else { bail!("{} could not be reopened", path.display()) };
        rgb_to_yuyv(&frame.rgb, &mut self.yuyv);
        file.write_all(&self.yuyv).with_context(|| format!("writing to {}", path.display()))
    }

    fn describe(&self) -> String {
        format!("{} ({})", self.device.label, self.device.path.display())
    }
}

/// RGB24 to YUYV (BT.601, limited range); the two pixels of a pair share U and V.
pub fn rgb_to_yuyv(rgb: &[u8], yuyv: &mut [u8]) {
    let luma = |r: i32, g: i32, b: i32| (16 + ((66 * r + 129 * g + 25 * b + 128) >> 8)) as u8;
    for (pair, out) in rgb.chunks_exact(6).zip(yuyv.chunks_exact_mut(4)) {
        let [r0, g0, b0, r1, g1, b1] = [0, 1, 2, 3, 4, 5].map(|i| pair[i] as i32);
        let (r, g, b) = ((r0 + r1) / 2, (g0 + g1) / 2, (b0 + b1) / 2);
        out[0] = luma(r0, g0, b0);
        out[1] = (128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8)) as u8;
        out[2] = luma(r1, g1, b1);
        out[3] = (128 + ((112 * r - 94 * g - 18 * b + 128) >> 8)) as u8;
    }
}

// ---------------------------------------------------------------- the one ioctl

#[cfg(not(target_pointer_width = "64"))]
compile_error!("the v4l2 format layout below is the 64-bit one");

const PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");

/// `struct v4l2_format` holding a `v4l2_pix_format`, as on 64-bit Linux: the kernel's
/// union is 200 bytes and pointer aligned, hence the padding after `kind`.
#[repr(C)]
struct Format {
    kind: u32,
    _padding: u32,
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
    _rest: [u8; 152],
}

impl Format {
    fn yuyv(width: u32, height: u32) -> Self {
        Format {
            kind: 2, // V4L2_BUF_TYPE_VIDEO_OUTPUT
            _padding: 0,
            width,
            height,
            pixelformat: PIX_FMT_YUYV,
            field: 1, // V4L2_FIELD_NONE
            bytesperline: width * 2,
            sizeimage: width * height * 2,
            colorspace: 1, // V4L2_COLORSPACE_SMPTE170M: BT.601, as rgb_to_yuyv converts
            private: 0,
            flags: 0,
            ycbcr_enc: 0,
            quantization: 0,
            xfer_func: 0,
            _rest: [0; 152],
        }
    }
}

/// `_IOWR('V', 5, struct v4l2_format)`
const VIDIOC_S_FMT: u64 = (3 << 30) | ((std::mem::size_of::<Format>() as u64) << 16) | ((b'V' as u64) << 8) | 5;

fn set_format(device: &File, format: &mut Format) -> std::io::Result<()> {
    // SAFETY: VIDIOC_S_FMT reads and writes exactly one `struct v4l2_format`, which
    // `Format` matches (checked in the tests), and `format` is valid for the call.
    let result = unsafe { libc::ioctl(device.as_raw_fd(), VIDIOC_S_FMT as _, format as *mut Format) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_matches_the_kernel() {
        assert_eq!(std::mem::size_of::<Format>(), 208);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
    }

    #[test]
    fn finds_files_other_processes_have_open() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        // Whatever the child's stdin is: a terminal, a pipe or /dev/null.
        let target = std::fs::read_link(format!("/proc/{}/fd/0", child.id())).unwrap();
        assert!(open_in_other_processes(&target));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!open_in_other_processes(Path::new("/nonexistent/device")));
    }

    #[test]
    fn yuyv_colours() {
        let mut out = [0u8; 8];
        rgb_to_yuyv(&[255, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0], &mut out);
        assert_eq!(out, [235, 128, 235, 128, 16, 128, 16, 128]);
    }
}
