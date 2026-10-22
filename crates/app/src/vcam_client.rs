//! App side of the Windows 11 virtual camera: creates it for the lifetime of this
//! process and hands it frames through the shared section (see
//! `protocol::frame_section`). The camera itself is the media source in `crates/vcam`,
//! which runs inside Windows' camera service.

use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Context, Result};
use protocol::frame_section::{SectionView, MAX_HEIGHT, MAX_WIDTH, SECTION_NAME, SECTION_SIZE, VCAM_CLSID};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HANDLE};
use windows::Win32::Media::KernelStreaming::{KSCATEGORY_CAPTURE, KSCATEGORY_VIDEO, KSCATEGORY_VIDEO_CAMERA};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::camera::CameraSink;
use crate::hub::Frame;

pub struct VirtualCamera {
    camera: IMFVirtualCamera,
    name: String,
    section: Option<Section>,
    last_open_attempt: Option<Instant>,
}

/// The mapped section, unmapped on drop.
struct Section {
    handle: HANDLE,
    address: MEMORY_MAPPED_VIEW_ADDRESS,
    view: SectionView,
}

// COM objects from the multithreaded apartment may be used from any thread in it, and
// the output thread is the only one using this.
unsafe impl Send for VirtualCamera {}

impl VirtualCamera {
    pub fn create(name: &str, width: u32, height: u32) -> Result<Self> {
        ensure!(
            width <= MAX_WIDTH && height <= MAX_HEIGHT,
            "the virtual camera supports up to {MAX_WIDTH}x{MAX_HEIGHT}"
        );
        // VIDEO and CAPTURE as well as VIDEO_CAMERA, so DirectShow-based apps list it too.
        let categories = [KSCATEGORY_VIDEO_CAMERA, KSCATEGORY_VIDEO, KSCATEGORY_CAPTURE];
        // SAFETY: plain Media Foundation calls with valid arguments.
        let camera = unsafe {
            // Keep the multithreaded apartment alive without putting this (the GUI)
            // thread in it: the window library needs it single-threaded.
            CoIncrementMTAUsage().context("CoIncrementMTAUsage")?;
            // Never shut down: shutting Media Foundation down while any of its objects
            // are alive crashes.
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).context("MFStartup")?;
            let camera = MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                &HSTRING::from(name),
                &HSTRING::from(VCAM_CLSID),
                Some(&categories),
            )
            .map_err(|e| {
                if e.code() == E_ACCESSDENIED {
                    anyhow!(
                        "Windows blocked camera access ({e}). Turn on Settings > Privacy & security > Camera > \
                         'Camera access' and 'Let desktop apps access your camera'."
                    )
                } else {
                    anyhow!("MFCreateVirtualCamera failed ({e}); run `iphone-webcam install` as administrator")
                }
            })?;
            camera.Start(None).context("starting the virtual camera")?;
            camera
        };
        Ok(Self { camera, name: name.to_string(), section: None, last_open_attempt: None })
    }
}

impl CameraSink for VirtualCamera {
    fn send(&mut self, frame: &Frame) -> Result<()> {
        // The camera service creates the section once an app opens the camera. Until
        // then nobody is watching; retry often so the first frames are prompt.
        if self.section.is_none() && self.last_open_attempt.is_none_or(|t| t.elapsed() > Duration::from_millis(100)) {
            self.last_open_attempt = Some(Instant::now());
            self.section = open_section();
        }
        if let Some(section) = &self.section {
            // SAFETY: no arguments; reads the system tick count.
            let now = unsafe { GetTickCount64() };
            section.view.write(frame.width, frame.height, &frame.rgb, now);
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} (Windows 11 virtual camera)", self.name)
    }
}

fn open_section() -> Option<Section> {
    let access = FILE_MAP_READ | FILE_MAP_WRITE;
    // SAFETY: the view is SECTION_SIZE bytes (the service creates the section with that
    // size), page aligned, and stays mapped until `Section` is dropped.
    unsafe {
        let handle = OpenFileMappingW(access.0, false, &HSTRING::from(SECTION_NAME)).ok()?;
        let address = MapViewOfFile(handle, access, 0, 0, SECTION_SIZE);
        if address.Value.is_null() {
            let _ = CloseHandle(handle);
            return None;
        }
        let view = SectionView::new(address.Value as *mut u8);
        Some(Section { handle, address, view })
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the view and handle came from MapViewOfFile/OpenFileMappingW and are
        // released once.
        unsafe {
            let _ = UnmapViewOfFile(self.address);
            let _ = CloseHandle(self.handle);
        }
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
