//! Linux virtual camera: frames are written to a v4l2loopback device, which video apps
//! then open like any webcam. `iphone-webcam install` loads the module with a device
//! labelled after the camera; any other loopback device works too.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::camera::CameraSink;
use crate::hub::Frame;

const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const FIELD_NONE: u32 = 1;
const COLORSPACE_SMPTE170M: u32 = 1; // BT.601, matching the conversion below
const PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");

/// `struct v4l2_capability`.
#[repr(C)]
struct Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

/// `struct v4l2_format` with the `pix` member of its union (the union is 200 bytes and
/// pointer aligned, hence the padding after `type`).
#[repr(C)]
struct Format {
    kind: u32,
    _pad: u32,
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
    _rest: [u8; 200 - 48],
}

const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((b'V' as u64) << 8) | nr
}
const VIDIOC_QUERYCAP: u64 = ioc(2, 0, std::mem::size_of::<Capability>());
const VIDIOC_S_FMT: u64 = ioc(3, 5, std::mem::size_of::<Format>());

fn c_str(bytes: &[u8]) -> String {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// Driver and card name of a video device.
fn query(file: &File) -> Option<(String, String)> {
    let mut cap: Capability = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_QUERYCAP as _, &mut cap) } == 0;
    ok.then(|| (c_str(&cap.driver), c_str(&cap.card)))
}

/// v4l2loopback devices as (path, label), in device order.
pub fn loopback_devices() -> Vec<(PathBuf, String)> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir("/dev")
        .map(|dir| dir.flatten().map(|e| e.path()).filter(|p| p.to_string_lossy().starts_with("/dev/video")).collect())
        .unwrap_or_default();
    paths.sort_by_key(|p| p.to_string_lossy().trim_start_matches("/dev/video").parse::<u32>().unwrap_or(u32::MAX));
    paths
        .into_iter()
        .filter_map(|path| {
            let file = OpenOptions::new().read(true).write(true).open(&path).ok()?;
            let (driver, card) = query(&file)?;
            (driver == "v4l2 loopback").then_some((path, card))
        })
        .collect()
}

pub struct LoopbackCamera {
    file: File,
    path: PathBuf,
    label: String,
    width: u32,
    height: u32,
    buffer: Vec<u8>,
}

impl LoopbackCamera {
    /// Open the loopback device labelled `name`, else the first one there is.
    pub fn open(name: &str, width: u32, height: u32) -> Result<Self> {
        let devices = loopback_devices();
        let Some((path, label)) = devices.iter().find(|(_, label)| label == name).or_else(|| devices.first()).cloned()
        else {
            bail!("no v4l2loopback device: run `sudo iphone-webcam install` (needs the v4l2loopback module)");
        };
        Self::open_device(&path, &label, width, height)
    }

    fn open_device(path: &Path, label: &str, width: u32, height: u32) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path).with_context(|| path.display().to_string())?;
        let mut format: Format = unsafe { std::mem::zeroed() };
        format.kind = BUF_TYPE_VIDEO_OUTPUT;
        format.width = width;
        format.height = height;
        format.pixelformat = PIX_FMT_YUYV;
        format.field = FIELD_NONE;
        format.bytesperline = width * 2;
        format.sizeimage = width * height * 2;
        format.colorspace = COLORSPACE_SMPTE170M;
        if unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_S_FMT as _, &mut format) } != 0 {
            return Err(std::io::Error::last_os_error()).context(format!("setting the format of {}", path.display()));
        }
        // The device may round the size; the writes must match what it settled on.
        anyhow::ensure!(
            format.width == width && format.height == height && format.pixelformat == PIX_FMT_YUYV,
            "{} refused {width}x{height} YUYV (in use by another program?)",
            path.display()
        );
        Ok(Self {
            file,
            path: path.to_path_buf(),
            label: label.to_string(),
            width,
            height,
            buffer: vec![0; (width * height * 2) as usize],
        })
    }
}

/// RGB24 to YUYV (BT.601, limited range); the two pixels of a pair share U and V.
pub fn rgb_to_yuyv(rgb: &[u8], out: &mut [u8]) {
    for (pair, dst) in rgb.chunks_exact(6).zip(out.chunks_exact_mut(4)) {
        let y = |r: i32, g: i32, b: i32| (16 + ((66 * r + 129 * g + 25 * b + 128) >> 8)) as u8;
        let (r0, g0, b0) = (pair[0] as i32, pair[1] as i32, pair[2] as i32);
        let (r1, g1, b1) = (pair[3] as i32, pair[4] as i32, pair[5] as i32);
        let (r, g, b) = ((r0 + r1) / 2, (g0 + g1) / 2, (b0 + b1) / 2);
        dst[0] = y(r0, g0, b0);
        dst[1] = (128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8)) as u8;
        dst[2] = y(r1, g1, b1);
        dst[3] = (128 + ((112 * r - 94 * g - 18 * b + 128) >> 8)) as u8;
    }
}

impl CameraSink for LoopbackCamera {
    fn send(&mut self, frame: &Frame) -> Result<()> {
        anyhow::ensure!(frame.width == self.width && frame.height == self.height, "frame size changed");
        rgb_to_yuyv(&frame.rgb, &mut self.buffer);
        self.file.write_all(&self.buffer).with_context(|| format!("writing to {}", self.path.display()))
    }

    fn describe(&self) -> String {
        format!("{} ({})", self.label, self.path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_the_kernel() {
        assert_eq!(std::mem::size_of::<Capability>(), 104);
        assert_eq!(std::mem::size_of::<Format>(), 208);
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
    }

    #[test]
    fn yuyv_colours() {
        let mut out = [0u8; 8];
        rgb_to_yuyv(&[255, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0], &mut out);
        assert_eq!(out, [235, 128, 235, 128, 16, 128, 16, 128]);
    }
}
