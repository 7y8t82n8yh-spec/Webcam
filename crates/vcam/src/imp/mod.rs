//! Windows implementation: the COM entry points the camera service calls, and an
//! optional diagnostic log. `iphone-webcam install` registers the class.

mod activate;
mod frames;
mod source;

use std::ffi::c_void;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use windows::core::{implement, Interface, BOOL, GUID, HRESULT};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, S_FALSE};
use windows::Win32::Media::MediaFoundation::IMFActivate;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows_core::Ref;

const CLSID: GUID = GUID::from_u128(protocol::frame_section::VCAM_CLSID_U128);

// ---------------------------------------------------------------- diagnostics

/// Optional log: set HKLM\SOFTWARE\iPhoneWebcam\VCamLog (DWORD) to 1 and every process
/// that loads the DLL appends to iPhoneWebcamVCam-<pid>.log in VCamLogDir (a folder
/// the camera service can write to) or else its temp folder.
pub(crate) fn log(message: std::fmt::Arguments) {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let Some(file) = FILE.get_or_init(open_log_file) else { return };
    let Ok(mut file) = file.lock() else { return };
    let _ = writeln!(file, "{:?} [{:?}] {}", std::time::SystemTime::now(), std::thread::current().id(), message);
}

fn open_log_file() -> Option<Mutex<std::fs::File>> {
    let settings = windows_registry::LOCAL_MACHINE.open(r"SOFTWARE\iPhoneWebcam").ok()?;
    if settings.get_u32("VCamLog").unwrap_or(0) == 0 {
        return None;
    }
    let dir = settings.get_string("VCamLogDir").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    let path = dir.join(format!("iPhoneWebcamVCam-{}.log", std::process::id()));
    std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
}

macro_rules! vlog {
    ($($arg:tt)*) => { $crate::imp::log(format_args!($($arg)*)) };
}
pub(crate) use vlog;

// ---------------------------------------------------------------- COM entry points

/// Makes the `IMFActivate` the camera service asks for.
#[implement(IClassFactory)]
struct Factory;

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<windows::core::IUnknown>,
        iid: *const GUID,
        object: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if object.is_null() {
            return Err(E_POINTER.into());
        }
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let activate: IMFActivate = activate::Activate::new()?.into();
        // SAFETY: COM passes a valid interface id and out pointer (checked above).
        unsafe { activate.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// COM asks the DLL for the factory of a class.
///
/// # Safety
/// Called by COM with valid pointers (null ones are rejected).
#[no_mangle]
unsafe extern "system" fn DllGetClassObject(clsid: *const GUID, iid: *const GUID, object: *mut *mut c_void) -> HRESULT {
    if clsid.is_null() || iid.is_null() || object.is_null() {
        return E_POINTER;
    }
    *object = std::ptr::null_mut();
    if *clsid != CLSID {
        return CLASS_E_CLASSNOTAVAILABLE;
    }
    let factory: IClassFactory = Factory.into();
    factory.query(iid, object)
}

/// Never unload: the camera service may keep threads of ours around briefly.
#[no_mangle]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
