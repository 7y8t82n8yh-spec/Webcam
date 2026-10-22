//! Reads frames from the shared section (see `protocol::frame_section`).

use std::time::{Duration, Instant};

use protocol::frame_section::{Read, SectionView, SECTION_NAME, SECTION_SIZE};
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READWRITE,
};
use windows::Win32::System::SystemInformation::GetTickCount64;

use super::vlog;

/// After this long without a write the app is gone: show black, not a frozen frame.
const STALE_MS: u64 = 3000;

pub struct FrameReader {
    section: Option<Section>,
    last_attempt: Option<Instant>,
    /// The last complete frame, kept for when the writer is busy every time we look.
    rgb: Vec<u8>,
    scratch: Vec<u8>,
    size: (u32, u32),
    write_tick: u64,
}

/// The mapped section, unmapped on drop.
struct Section {
    handle: HANDLE,
    address: MEMORY_MAPPED_VIEW_ADDRESS,
    view: SectionView,
}

impl FrameReader {
    pub fn new() -> Self {
        Self { section: None, last_attempt: None, rgb: Vec::new(), scratch: Vec::new(), size: (0, 0), write_tick: 0 }
    }

    /// The latest frame as (rgb, width, height), or `None` if there is none yet or the
    /// app stopped writing.
    pub fn read(&mut self) -> Option<(&[u8], u32, u32)> {
        // Throttled: this runs for every frame while the app isn't writing yet.
        if self.section.is_none() && self.last_attempt.is_none_or(|t| t.elapsed() > Duration::from_secs(1)) {
            self.last_attempt = Some(Instant::now());
            self.section = map_section();
        }
        let view = &self.section.as_ref()?.view;
        for _ in 0..4 {
            match view.read_into(&mut self.scratch) {
                Read::Empty => return None,
                Read::Busy => std::thread::sleep(Duration::from_millis(1)),
                Read::Frame { width, height, write_tick } => {
                    std::mem::swap(&mut self.rgb, &mut self.scratch);
                    self.size = (width, height);
                    self.write_tick = write_tick;
                    break;
                }
            }
        }
        // SAFETY: no arguments; reads the system tick count.
        let now = unsafe { GetTickCount64() };
        if self.rgb.is_empty() || now.saturating_sub(self.write_tick) > STALE_MS {
            return None;
        }
        Some((&self.rgb, self.size.0, self.size.1))
    }
}

/// Create the section, or open the one that exists, and map it read-only.
fn map_section() -> Option<Section> {
    // SAFETY: plain Win32 calls with valid arguments; the view is SECTION_SIZE bytes,
    // page aligned, read-only (SectionView only reads), and stays mapped until
    // `Section` is dropped.
    unsafe {
        // System, LocalService, Administrators and interactive users may use it. The
        // low integrity label lets the (medium integrity) app write to a section
        // created by a service.
        let sddl = w!("D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;IU)S:(ML;;NW;;;LW)");
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let _ = ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, SDDL_REVISION_1, &mut descriptor, None);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let name = HSTRING::from(SECTION_NAME);
        let created =
            CreateFileMappingW(INVALID_HANDLE_VALUE, Some(&attributes), PAGE_READWRITE, 0, SECTION_SIZE as u32, &name);
        if !descriptor.0.is_null() {
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        }
        // Outside the service we can't create Global objects: open what's there.
        let handle = created.or_else(|_| OpenFileMappingW(FILE_MAP_READ.0, false, &name)).ok()?;
        let address = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, SECTION_SIZE);
        if address.Value.is_null() {
            let _ = CloseHandle(handle);
            vlog!("frames: MapViewOfFile failed");
            return None;
        }
        vlog!("frames: section mapped");
        let view = SectionView::new(address.Value as *mut u8);
        Some(Section { handle, address, view })
    }
}

impl Drop for Section {
    fn drop(&mut self) {
        // SAFETY: the view and handle came from MapViewOfFile and CreateFileMappingW /
        // OpenFileMappingW and are released once.
        unsafe {
            let _ = UnmapViewOfFile(self.address);
            let _ = CloseHandle(self.handle);
        }
    }
}
