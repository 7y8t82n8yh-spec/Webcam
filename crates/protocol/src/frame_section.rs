//! The shared memory section that carries frames from the app to the Windows 11
//! camera's media source (which runs in Windows' camera service).
//!
//! The service creates the named section in the `Global\` namespace (which needs a
//! privilege services have and users don't) and the app opens it. The app writes
//! frames; the media source reads them.
//!
//! The frame is guarded by a sequence lock: the writer makes `sequence` odd while it
//! copies and even when done; a reader retries if the value was odd or changed
//! meanwhile.

use std::sync::atomic::{fence, AtomicI64, AtomicU32, AtomicU64, Ordering};

pub const SECTION_NAME: &str = r"Global\iPhoneWebcam_Frames";
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;

/// COM class of the camera's media source (crates/vcam). The app passes it to
/// `MFCreateVirtualCamera`; the installer registers it.
pub const VCAM_CLSID: &str = "{8E3B5C1A-6F42-4D8B-9C77-2A1D5E0F4B93}";
pub const VCAM_CLSID_U128: u128 = 0x8E3B5C1A_6F42_4D8B_9C77_2A1D5E0F4B93;

/// At offset 0 of the section; an RGB24 frame (`width * height * 3` bytes, rows
/// top-down, tightly packed) follows at [`HEADER_SIZE`].
#[repr(C)]
struct Header {
    sequence: AtomicI64,
    width: AtomicU32,
    height: AtomicU32,
    /// When the last frame was completed, in the writer's milliseconds (Windows
    /// `GetTickCount64`, which both processes share).
    write_tick: AtomicU64,
    _reserved: [u8; 40],
}

const HEADER_SIZE: usize = 64;
const _: () = assert!(std::mem::size_of::<Header>() == HEADER_SIZE);

pub const SECTION_SIZE: usize = HEADER_SIZE + (MAX_WIDTH as usize) * (MAX_HEIGHT as usize) * 3;

/// What [`SectionView::read_into`] found.
pub enum Read {
    /// No frame has been written yet.
    Empty,
    /// The writer was busy, or changed the frame while we copied it: try again.
    Busy,
    /// A complete frame is now in the buffer.
    Frame { width: u32, height: u32, write_tick: u64 },
}

/// A mapped view of the section.
pub struct SectionView {
    base: *mut u8,
}

// The view is shared memory that the other process changes anyway; every access goes
// through the sequence lock.
unsafe impl Send for SectionView {}

impl SectionView {
    /// # Safety
    /// `base` must point to a mapping of the section that is at least [`SECTION_SIZE`]
    /// bytes, aligned to 8 bytes, and stays mapped as long as the view is used. A
    /// read-only mapping may only be used with [`SectionView::read_into`].
    pub unsafe fn new(base: *mut u8) -> Self {
        SectionView { base }
    }

    fn header(&self) -> &Header {
        // SAFETY: `new` requires a mapping that starts with the header, and every
        // header field is atomic, so the other process may change it at any time.
        unsafe { &*(self.base as *const Header) }
    }

    /// Publish a frame (the app's side).
    pub fn write(&self, width: u32, height: u32, rgb: &[u8], write_tick: u64) {
        assert!(width <= MAX_WIDTH && height <= MAX_HEIGHT && rgb.len() == (width * height * 3) as usize);
        let header = self.header();
        let mut sequence = header.sequence.load(Ordering::Acquire);
        if sequence & 1 == 1 {
            sequence += 1; // an earlier writer died mid-frame
        }
        header.sequence.store(sequence + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        header.width.store(width, Ordering::Relaxed);
        header.height.store(height, Ordering::Relaxed);
        // SAFETY: the frame fits the section (asserted above). Readers may look at the
        // pixels while we write them; the odd sequence makes them throw that copy away.
        unsafe { std::ptr::copy_nonoverlapping(rgb.as_ptr(), self.base.add(HEADER_SIZE), rgb.len()) };
        header.write_tick.store(write_tick, Ordering::Relaxed);
        header.sequence.store(sequence + 2, Ordering::Release);
    }

    /// Copy the current frame into `rgb` (the camera's side). Only loads, so it works
    /// on a read-only mapping.
    pub fn read_into(&self, rgb: &mut Vec<u8>) -> Read {
        let header = self.header();
        let before = header.sequence.load(Ordering::Acquire);
        if before == 0 {
            return Read::Empty;
        }
        if before & 1 == 1 {
            return Read::Busy;
        }
        let (width, height) = (header.width.load(Ordering::Relaxed), header.height.load(Ordering::Relaxed));
        let write_tick = header.write_tick.load(Ordering::Relaxed);
        if width == 0 || height == 0 || width > MAX_WIDTH || height > MAX_HEIGHT {
            return Read::Busy; // read mid-update
        }
        let len = (width * height * 3) as usize;
        rgb.resize(len, 0);
        // SAFETY: the size was checked against the section above. If the writer changes
        // the pixels meanwhile, the sequence check below discards this copy.
        unsafe { std::ptr::copy_nonoverlapping(self.base.add(HEADER_SIZE), rgb.as_mut_ptr(), len) };
        fence(Ordering::Acquire);
        if header.sequence.load(Ordering::Relaxed) != before {
            return Read::Busy;
        }
        Read::Frame { width, height, write_tick }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_frame_reads_back() {
        let mut memory = vec![0u64; SECTION_SIZE / 8]; // 8-byte aligned, like a mapping
        let view = unsafe { SectionView::new(memory.as_mut_ptr() as *mut u8) };
        let mut rgb = Vec::new();
        assert!(matches!(view.read_into(&mut rgb), Read::Empty));
        let frame: Vec<u8> = (0..4 * 2 * 3).map(|i| i as u8).collect();
        view.write(4, 2, &frame, 42);
        let Read::Frame { width, height, write_tick } = view.read_into(&mut rgb) else { panic!("no frame") };
        assert_eq!((width, height, write_tick), (4, 2, 42));
        assert_eq!(rgb, frame);
    }
}
