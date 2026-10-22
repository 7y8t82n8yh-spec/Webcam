//! Reads frames from the shared section (see `protocol::frame_section`).

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use protocol::frame_section::{self as fs, Header};
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

/// The outcome of one attempt at copying the frame out of the section.
enum Attempt {
    Copied,
    Retry,
    NeverWritten,
}

pub struct FrameReader {
    section: Option<(HANDLE, MEMORY_MAPPED_VIEW_ADDRESS)>,
    last_attempt: Option<Instant>,
    scratch: Vec<u8>,
    last: Vec<u8>,
    last_size: (u32, u32),
    last_tick: u64,
}

impl FrameReader {
    pub fn new() -> Self {
        Self {
            section: None,
            last_attempt: None,
            scratch: Vec::new(),
            last: Vec::new(),
            last_size: (0, 0),
            last_tick: 0,
        }
    }

    /// The latest frame as (rgb, width, height), or `None` if there is none (yet) or the
    /// app stopped writing. If the writer is mid-frame every time we look, the previous
    /// frame is returned again rather than black.
    pub fn read(&mut self) -> Option<(&[u8], u32, u32)> {
        let base = self.view()?;
        for _ in 0..4 {
            match unsafe { self.try_copy(base) } {
                Attempt::Copied => break,
                Attempt::Retry => continue,
                Attempt::NeverWritten => return None,
            }
        }
        if self.last.is_empty() || unsafe { GetTickCount64() }.saturating_sub(self.last_tick) > STALE_MS {
            return None;
        }
        Some((&self.last, self.last_size.0, self.last_size.1))
    }

    /// Copy the frame at `base` into `last` if it is complete and unchanged while copying.
    unsafe fn try_copy(&mut self, base: *const u8) -> Attempt {
        let header = base as *const Header;
        // Plain atomic loads only: the view is read-only, and a compare-exchange
        // would write (and fault).
        let sequence = &*(std::ptr::addr_of!((*header).sequence) as *const AtomicI64);
        let before = sequence.load(Ordering::Acquire);
        if before == 0 {
            return Attempt::NeverWritten;
        }
        if before & 1 == 1 {
            std::thread::sleep(Duration::from_millis(1));
            return Attempt::Retry;
        }
        let (w, h) = ((*header).width, (*header).height);
        let tick = (*header).write_tick;
        if w == 0 || h == 0 || w > fs::MAX_WIDTH || h > fs::MAX_HEIGHT {
            return Attempt::Retry; // torn read
        }
        let len = (w * h * 3) as usize;
        self.scratch.resize(len, 0);
        std::ptr::copy_nonoverlapping(base.add(fs::HEADER_SIZE), self.scratch.as_mut_ptr(), len);
        std::sync::atomic::fence(Ordering::Acquire);
        if sequence.load(Ordering::Acquire) != before {
            return Attempt::Retry;
        }
        std::mem::swap(&mut self.last, &mut self.scratch);
        self.last_size = (w, h);
        self.last_tick = tick;
        Attempt::Copied
    }

    fn view(&mut self) -> Option<*const u8> {
        if let Some((_, view)) = self.section {
            return Some(view.Value as *const u8);
        }
        // Throttle: this runs for every frame while the app isn't writing yet.
        if self.last_attempt.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return None;
        }
        self.last_attempt = Some(Instant::now());
        unsafe {
            let handle = create_or_open_section()?;
            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, fs::SECTION_SIZE);
            if view.Value.is_null() {
                let _ = CloseHandle(handle);
                vlog!("frames: MapViewOfFile failed");
                return None;
            }
            vlog!("frames: section mapped");
            self.section = Some((handle, view));
            Some(view.Value as *const u8)
        }
    }
}

unsafe fn create_or_open_section() -> Option<HANDLE> {
    // System, LocalService, Administrators and interactive users may use it. The
    // low integrity label lets the (medium integrity) app write to a section
    // created by a service. We only ever read it.
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let sddl = w!("D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;IU)S:(ML;;NW;;;LW)");
    let _ = ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, SDDL_REVISION_1, &mut descriptor, None);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let name = HSTRING::from(fs::SECTION_NAME);
    let created =
        CreateFileMappingW(INVALID_HANDLE_VALUE, Some(&attributes), PAGE_READWRITE, 0, fs::SECTION_SIZE as u32, &name);
    if !descriptor.0.is_null() {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    // The app may keep an earlier section alive, or we're not in the service and
    // can't create Global objects: open what's there, read-only.
    match created {
        Ok(h) => Some(h),
        Err(_) => OpenFileMappingW(FILE_MAP_READ.0, false, &name).ok(),
    }
}

impl Drop for FrameReader {
    fn drop(&mut self) {
        if let Some((handle, view)) = self.section.take() {
            unsafe {
                let _ = UnmapViewOfFile(view);
                let _ = CloseHandle(handle);
            }
        }
    }
}
