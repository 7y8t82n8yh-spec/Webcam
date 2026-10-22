//! Linux virtual camera: frames are written to a v4l2loopback device, which video apps
//! then open like any webcam. `iphone-webcam install` loads the module with a device
//! labelled after the camera; any other loopback device works too.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use anyhow::{bail, ensure, Context, Result};

use crate::camera::CameraSink;
use crate::hub::Frame;

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
    file: File,
    device: LoopbackDevice,
    width: u32,
    height: u32,
    yuyv: Vec<u8>,
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

        let file =
            OpenOptions::new().write(true).open(&device.path).with_context(|| device.path.display().to_string())?;
        let mut format = Format::yuyv(width, height);
        set_format(&file, &mut format).with_context(|| format!("setting the format of {}", device.path.display()))?;
        // The device may adjust the request; the frames we write must match what it settled on.
        ensure!(
            format.width == width && format.height == height && format.pixelformat == PIX_FMT_YUYV,
            "{} refused {width}x{height} YUYV (in use by another program?)",
            device.path.display()
        );
        Ok(Self { file, device, width, height, yuyv: vec![0; (width * height * 2) as usize] })
    }
}

impl CameraSink for LoopbackCamera {
    fn send(&mut self, frame: &Frame) -> Result<()> {
        ensure!(frame.width == self.width && frame.height == self.height, "frame size changed");
        rgb_to_yuyv(&frame.rgb, &mut self.yuyv);
        self.file.write_all(&self.yuyv).with_context(|| format!("writing to {}", self.device.path.display()))
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

const PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");

/// `struct v4l2_format` holding a `v4l2_pix_format`. The kernel's union is 200 bytes
/// and pointer aligned, hence the padding after `kind`.
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
    fn yuyv_colours() {
        let mut out = [0u8; 8];
        rgb_to_yuyv(&[255, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0], &mut out);
        assert_eq!(out, [235, 128, 235, 128, 16, 128, 16, 128]);
    }
}
