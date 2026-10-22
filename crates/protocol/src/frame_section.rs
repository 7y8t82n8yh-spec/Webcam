//! Layout of the shared memory section that carries frames from the app to the
//! Windows 11 camera's media source (which runs in the Frame Server service).
//!
//! The service creates the named section in the `Global\` namespace (which needs a
//! privilege services have and users don't) and the app opens it. The frame is
//! guarded by a sequence lock: the writer makes `sequence` odd while it copies and
//! even when done; a reader retries if the value was odd or changed meanwhile.

/// Name of the section, as a NUL-terminated UTF-16 string is built from this.
pub const SECTION_NAME: &str = r"Global\iPhoneWebcam_Frames";
pub const MAGIC: u32 = 0x4357_5049; // "IPWC"
pub const VERSION: u32 = 1;
pub const MAX_WIDTH: u32 = 1920;
pub const MAX_HEIGHT: u32 = 1080;

/// Header at offset 0; an RGB24 frame (`width * height * 3` bytes, rows top-down,
/// tightly packed) follows at [`HEADER_SIZE`].
#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    /// Access atomically (`AtomicI64`).
    pub sequence: i64,
    pub width: u32,
    pub height: u32,
    /// `GetTickCount64()` of the last completed write.
    pub write_tick: u64,
    pub reserved: [u8; 32],
}

pub const HEADER_SIZE: usize = 64;
pub const SECTION_SIZE: usize = HEADER_SIZE + (MAX_WIDTH as usize) * (MAX_HEIGHT as usize) * 3;

const _: () = assert!(core::mem::size_of::<Header>() == HEADER_SIZE);

/// COM class of the camera's media source (crates/vcam). The app passes it to
/// `MFCreateVirtualCamera`; the installer registers it.
pub const VCAM_CLSID: &str = "{8E3B5C1A-6F42-4D8B-9C77-2A1D5E0F4B93}";
pub const VCAM_CLSID_U128: u128 = 0x8E3B5C1A_6F42_4D8B_9C77_2A1D5E0F4B93;
