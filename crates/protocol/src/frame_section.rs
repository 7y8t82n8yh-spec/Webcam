//! The shared memory section that carries frames from the app to the Windows 11
//! camera's media source (which runs in Windows' camera service).
//!
//! The service creates the named section in the `Global\` namespace (which needs a
//! privilege services have and users don't) and the app opens it. The app writes
//! frames with a [`FrameWriter`]; the media source reads them with a [`FrameReader`].
//!
//! The frame is guarded by a sequence lock: the writer makes `sequence` odd while it
//! copies and even when done; a reader retries if the value was odd or changed
//! meanwhile.

#[cfg(any(windows, test))]
use std::sync::atomic::{fence, Ordering};
use std::sync::atomic::{AtomicU32, AtomicU64};

pub const SECTION_NAME: &str = r"Global\iPhoneWebcam_Frames";
/// The largest frame: 1080p, landscape or portrait.
pub const MAX_SIDE: u32 = 1920;
pub const MAX_PIXELS: u32 = 1920 * 1080;

/// Whether a `width x height` frame fits in the section.
pub fn fits(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && width <= MAX_SIDE && height <= MAX_SIDE && width * height <= MAX_PIXELS
}

/// COM class of the camera's media source (crates/vcam). The app passes it to
/// `MFCreateVirtualCamera`; the installer registers it.
pub const VCAM_CLSID: &str = "{8E3B5C1A-6F42-4D8B-9C77-2A1D5E0F4B93}";
pub const VCAM_CLSID_U128: u128 = 0x8E3B5C1A_6F42_4D8B_9C77_2A1D5E0F4B93;

/// At offset 0 of the section; an RGB24 frame (`width * height * 3` bytes, rows
/// top-down, tightly packed) follows at [`HEADER_SIZE`]. Only atomics: the other
/// process may change any of it at any time.
#[repr(C)]
struct Header {
    /// Odd while a frame is being written; grows by 2 with every frame.
    sequence: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
}

const HEADER_SIZE: usize = 64;
const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_SIZE);

pub const SECTION_SIZE: usize = HEADER_SIZE + MAX_PIXELS as usize * 3;

/// What [`FrameReader::read_into`] found.
pub enum Read {
    /// No frame has been written yet.
    Empty,
    /// The writer was busy, or changed the frame while we copied it: try again.
    Busy,
    /// A complete frame is now in the buffer. `sequence` changes with every frame.
    Frame { width: u32, height: u32, sequence: u64 },
}

/// Pointer to a mapping of the section: at least `SECTION_SIZE` bytes, 8-byte
/// aligned, mapped for as long as this value is used. Only the two owning types below
/// (and the tests) create one.
#[cfg(any(windows, test))]
struct Section(*mut u8);

#[cfg(any(windows, test))]
impl Section {
    fn header(&self) -> &Header {
        // SAFETY: the mapping starts with the header (see `Section`), and every field
        // is atomic, so sharing it with the other process is fine.
        unsafe { &*(self.0 as *const Header) }
    }

    fn write(&self, width: u32, height: u32, rgb: &[u8]) {
        assert!(fits(width, height) && rgb.len() == (width * height * 3) as usize);
        let header = self.header();
        let mut sequence = header.sequence.load(Ordering::Relaxed);
        if sequence % 2 == 1 {
            sequence += 1; // an earlier writer died mid-frame
        }
        header.sequence.store(sequence + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        header.width.store(width, Ordering::Relaxed);
        header.height.store(height, Ordering::Relaxed);
        // SAFETY: the frame fits the section (asserted above). The reader may copy the
        // pixels while they change; the odd sequence makes it discard that copy.
        unsafe { std::ptr::copy_nonoverlapping(rgb.as_ptr(), self.0.add(HEADER_SIZE), rgb.len()) };
        header.sequence.store(sequence + 2, Ordering::Release);
    }

    /// Only loads, so it works on a read-only mapping. (`Relaxed` loads plus fences:
    /// Rust only promises read-only memory works for those.)
    fn read_into(&self, rgb: &mut Vec<u8>) -> Read {
        let header = self.header();
        let before = header.sequence.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        if before == 0 {
            return Read::Empty;
        }
        if before % 2 == 1 {
            return Read::Busy;
        }
        let (width, height) = (header.width.load(Ordering::Relaxed), header.height.load(Ordering::Relaxed));
        if !fits(width, height) {
            return Read::Busy; // read mid-update
        }
        let len = (width * height * 3) as usize;
        rgb.resize(len, 0);
        // SAFETY: the size was checked against the section above. The writer may change
        // the pixels while we copy them (the usual sequence lock race); the check
        // below then discards this copy.
        unsafe { std::ptr::copy_nonoverlapping(self.0.add(HEADER_SIZE), rgb.as_mut_ptr(), len) };
        fence(Ordering::Acquire);
        if header.sequence.load(Ordering::Relaxed) != before {
            return Read::Busy;
        }
        Read::Frame { width, height, sequence: before }
    }
}

#[cfg(windows)]
pub use mapping::{FrameReader, FrameWriter};

#[cfg(windows)]
mod mapping {
    use windows::core::{w, Result, HSTRING};
    use windows::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP, FILE_MAP_READ, FILE_MAP_WRITE,
        MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    };

    use super::{Read, Section, SECTION_NAME, SECTION_SIZE};

    /// The app's side: the section opened for writing.
    pub struct FrameWriter(Mapping);

    impl FrameWriter {
        /// Open the section, which exists once an app has opened the camera.
        pub fn open() -> Result<Self> {
            let access = FILE_MAP_READ | FILE_MAP_WRITE;
            // SAFETY: plain call with a valid name.
            let handle = unsafe { OpenFileMappingW(access.0, false, &HSTRING::from(SECTION_NAME))? };
            Mapping::map(handle, access).map(FrameWriter)
        }

        /// Publish a frame.
        pub fn write(&self, width: u32, height: u32, rgb: &[u8]) {
            self.0.section.write(width, height, rgb);
        }
    }

    /// The camera's side: the section mapped read-only.
    pub struct FrameReader(Mapping);

    impl FrameReader {
        /// Create the section, or open the one that exists.
        pub fn create_or_open() -> Result<Self> {
            let name = HSTRING::from(SECTION_NAME);
            let handle = match create(&name) {
                Ok(handle) => handle,
                // Outside the service we can't create Global objects: open what's there.
                // SAFETY: plain call with a valid name.
                Err(_) => unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, &name)? },
            };
            Mapping::map(handle, FILE_MAP_READ).map(FrameReader)
        }

        /// Copy the current frame into `rgb`.
        pub fn read_into(&self, rgb: &mut Vec<u8>) -> Read {
            self.0.section.read_into(rgb)
        }
    }

    /// Create the section so that System, LocalService, Administrators and interactive
    /// users may use it. The low integrity label lets the (medium integrity) app write
    /// to a section created by a service.
    fn create(name: &HSTRING) -> Result<HANDLE> {
        let sddl = w!("D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;BA)(A;;GRGW;;;IU)S:(ML;;NW;;;LW)");
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a valid constant string and out pointer.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, SDDL_REVISION_1, &mut descriptor, None)? };
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let size = SECTION_SIZE as u32;
        // SAFETY: valid attributes and name; the descriptor is freed once, after use.
        unsafe {
            let created = CreateFileMappingW(INVALID_HANDLE_VALUE, Some(&attributes), PAGE_READWRITE, 0, size, name);
            let _ = LocalFree(Some(HLOCAL(descriptor.0)));
            created
        }
    }

    /// A mapped view of the section, unmapped on drop.
    struct Mapping {
        handle: HANDLE,
        address: MEMORY_MAPPED_VIEW_ADDRESS,
        section: Section,
    }

    // The mapping belongs to the whole process; any thread may use it.
    unsafe impl Send for Mapping {}

    impl Mapping {
        fn map(handle: HANDLE, access: FILE_MAP) -> Result<Self> {
            // SAFETY: a valid section handle; the section is SECTION_SIZE bytes.
            let address = unsafe { MapViewOfFile(handle, access, 0, 0, SECTION_SIZE) };
            if address.Value.is_null() {
                let error = windows::core::Error::from_thread();
                // SAFETY: our own handle, closed once.
                let _ = unsafe { CloseHandle(handle) };
                return Err(error);
            }
            // A view is SECTION_SIZE bytes, page aligned, and stays mapped until drop.
            let section = Section(address.Value as *mut u8);
            Ok(Mapping { handle, address, section })
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: our own view and handle, released once.
            unsafe {
                let _ = UnmapViewOfFile(self.address);
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_frame_reads_back() {
        let mut memory = vec![0u64; SECTION_SIZE / 8]; // 8-byte aligned, like a mapping
        let section = Section(memory.as_mut_ptr() as *mut u8);
        let mut rgb = Vec::new();
        assert!(matches!(section.read_into(&mut rgb), Read::Empty));
        let frame: Vec<u8> = (0..4 * 2 * 3).map(|i| i as u8).collect();
        section.write(4, 2, &frame);
        let Read::Frame { width, height, sequence } = section.read_into(&mut rgb) else { panic!("no frame") };
        assert_eq!((width, height, sequence), (4, 2, 2));
        assert_eq!(rgb, frame);
    }

    #[test]
    fn clsid_constants_agree() {
        let hex = format!("{VCAM_CLSID_U128:032X}");
        let formatted = format!("{{{}-{}-{}-{}-{}}}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
        assert_eq!(formatted, VCAM_CLSID);
    }
}
