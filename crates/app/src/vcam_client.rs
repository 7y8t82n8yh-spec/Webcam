//! App side of the Windows 11 virtual camera: creates it for the lifetime of this
//! process and hands it frames through the shared section (see
//! `protocol::frame_section`). The camera itself is the media source in `crates/vcam`,
//! which runs inside the Windows Camera Frame Server service.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use protocol::frame_section::{self as fs, Header};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HANDLE};
use windows::Win32::Media::KernelStreaming::{KSCATEGORY_CAPTURE, KSCATEGORY_VIDEO, KSCATEGORY_VIDEO_CAMERA};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::camera::CameraSink;
use crate::hub::Frame;

pub struct VirtualCamera {
    camera: IMFVirtualCamera,
    name: String,
    section: Option<(HANDLE, MEMORY_MAPPED_VIEW_ADDRESS)>,
    last_open_attempt: Option<Instant>,
}

// COM objects from the multithreaded apartment may be used from any MTA thread.
unsafe impl Send for VirtualCamera {}

impl VirtualCamera {
    pub fn create(name: &str, width: u32, height: u32) -> Result<Self> {
        anyhow::ensure!(
            width <= fs::MAX_WIDTH && height <= fs::MAX_HEIGHT,
            "the virtual camera supports up to {}x{}",
            fs::MAX_WIDTH,
            fs::MAX_HEIGHT
        );
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).context("MFStartup")?;
            // VIDEO and CAPTURE as well as VIDEO_CAMERA, so DirectShow-based apps list it too.
            let categories = [KSCATEGORY_VIDEO_CAMERA, KSCATEGORY_VIDEO, KSCATEGORY_CAPTURE];
            let camera = MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                &HSTRING::from(name),
                &HSTRING::from(fs::VCAM_CLSID),
                Some(&categories),
            )
            .map_err(|e| {
                if e.code() == E_ACCESSDENIED {
                    anyhow::anyhow!(
                        "Windows blocked camera access ({e}). Turn on Settings > Privacy & security > Camera > \
                         'Camera access' and 'Let desktop apps access your camera'."
                    )
                } else {
                    anyhow::anyhow!("MFCreateVirtualCamera failed ({e}); run `iphone-webcam install` as administrator")
                }
            })?;
            camera.Start(None).context("starting the virtual camera")?;
            Ok(Self { camera, name: name.to_string(), section: None, last_open_attempt: None })
        }
    }

    /// The section only exists once an app has opened the camera (the service creates
    /// it). Retrying is cheap, so do it often to keep the first frames prompt.
    fn open_section(&mut self) -> Option<*mut u8> {
        if let Some((_, view)) = self.section {
            return Some(view.Value as *mut u8);
        }
        if self.last_open_attempt.is_some_and(|t| t.elapsed() < Duration::from_millis(100)) {
            return None;
        }
        self.last_open_attempt = Some(Instant::now());
        unsafe {
            let name = HSTRING::from(fs::SECTION_NAME);
            let handle = OpenFileMappingW((FILE_MAP_READ | FILE_MAP_WRITE).0, false, PCWSTR(name.as_ptr())).ok()?;
            let view = MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, fs::SECTION_SIZE);
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                return None;
            }
            self.section = Some((handle, view));
            Some(view.Value as *mut u8)
        }
    }
}

impl CameraSink for VirtualCamera {
    fn send(&mut self, frame: &Frame) -> Result<()> {
        let Some(base) = self.open_section() else { return Ok(()) }; // nobody watching yet
        anyhow::ensure!(frame.width <= fs::MAX_WIDTH && frame.height <= fs::MAX_HEIGHT, "frame too large");
        unsafe {
            let header = base as *mut Header;
            let sequence = &*(std::ptr::addr_of_mut!((*header).sequence) as *const AtomicI64);
            let mut seq = sequence.load(Ordering::Acquire);
            if seq & 1 == 1 {
                seq += 1; // a previous writer died mid-frame
            }
            sequence.store(seq + 1, Ordering::Release);
            std::sync::atomic::fence(Ordering::SeqCst);
            (*header).magic = fs::MAGIC;
            (*header).version = fs::VERSION;
            (*header).width = frame.width;
            (*header).height = frame.height;
            std::ptr::copy_nonoverlapping(frame.rgb.as_ptr(), base.add(fs::HEADER_SIZE), frame.rgb.len());
            (*header).write_tick = GetTickCount64();
            sequence.store(seq + 2, Ordering::Release);
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} (Windows 11 virtual camera)", self.name)
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        unsafe {
            if let Some((handle, view)) = self.section.take() {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(handle);
            }
            let _ = self.camera.Remove();
            let _ = self.camera.Shutdown();
        }
    }
}
