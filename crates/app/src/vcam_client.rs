//! App side of the Windows 11 virtual camera: creates it for the lifetime of this
//! process and hands it frames through the shared section (see
//! `protocol::frame_section`). The camera itself is the media source in `crates/vcam`,
//! which runs inside Windows' camera service.

use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Context, Result};
use protocol::frame_section::{self, FrameWriter, VCAM_CLSID};
use windows::core::HSTRING;
use windows::Win32::Foundation::E_ACCESSDENIED;
use windows::Win32::Media::KernelStreaming::{KSCATEGORY_CAPTURE, KSCATEGORY_VIDEO, KSCATEGORY_VIDEO_CAMERA};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoIncrementMTAUsage;

use crate::camera::CameraSink;
use crate::hub::Frame;

pub struct VirtualCamera {
    camera: IMFVirtualCamera,
    name: String,
    section: Option<FrameWriter>,
    last_open_attempt: Option<Instant>,
}

impl VirtualCamera {
    pub fn create(name: &str, width: u32, height: u32) -> Result<Self> {
        ensure!(frame_section::fits(width, height), "the virtual camera supports up to 1920x1080 (or 1080x1920)");
        // SAFETY (all blocks here): plain Media Foundation calls with valid arguments.
        // Keep a multithreaded apartment for this thread without initialising COM on
        // it, and never shut Media Foundation down: doing that while any of its objects
        // are alive crashes.
        unsafe { CoIncrementMTAUsage() }.context("CoIncrementMTAUsage")?;
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.context("MFStartup")?;
        // VIDEO and CAPTURE as well as VIDEO_CAMERA, so DirectShow-based apps list it too.
        let categories = [KSCATEGORY_VIDEO_CAMERA, KSCATEGORY_VIDEO, KSCATEGORY_CAPTURE];
        let camera = unsafe {
            MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                &HSTRING::from(name),
                &HSTRING::from(VCAM_CLSID),
                Some(&categories),
            )
        };
        let camera = camera.map_err(|e| {
            if e.code() == E_ACCESSDENIED {
                anyhow!(
                    "Windows blocked camera access ({e}). Turn on Settings > Privacy & security > Camera > \
                     'Camera access' and 'Let desktop apps access your camera'."
                )
            } else {
                anyhow!("MFCreateVirtualCamera failed ({e}); run `iphone-webcam install` as administrator")
            }
        })?;
        unsafe { camera.Start(None) }.context("starting the virtual camera")?;
        Ok(Self { camera, name: name.to_string(), section: None, last_open_attempt: None })
    }
}

impl CameraSink for VirtualCamera {
    fn send(&mut self, frame: &Frame) -> Result<()> {
        // The camera service creates the section once an app opens the camera. Until
        // then nobody is watching; retry often so the first frames are prompt.
        if self.section.is_none() && self.last_open_attempt.is_none_or(|t| t.elapsed() > Duration::from_millis(100)) {
            self.last_open_attempt = Some(Instant::now());
            self.section = FrameWriter::open().ok();
        }
        // Any size: the camera fits what it gets into the size the video app chose.
        if let (Some(section), true) = (&self.section, frame_section::fits(frame.width, frame.height)) {
            section.write(frame.width, frame.height, &frame.rgb);
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} (Windows 11 virtual camera)", self.name)
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        self.section = None;
        // SAFETY: plain calls on a valid camera object.
        unsafe {
            let _ = self.camera.Remove();
            let _ = self.camera.Shutdown();
        }
    }
}
