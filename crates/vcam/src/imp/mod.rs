//! Windows implementation: COM entry points, registration and diagnostics.

mod activate;
mod frames;
mod source;

use std::ffi::c_void;
use std::io::Write;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{implement, Interface, BOOL, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, E_POINTER, HINSTANCE,
    S_FALSE, S_OK,
};
use windows::Win32::Media::MediaFoundation::IMFActivate;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_core::Ref;

const CLSID: GUID = GUID::from_u128(protocol::frame_section::VCAM_CLSID_U128);

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- diagnostics

/// Optional log: set HKLM\SOFTWARE\iPhoneWebcam\VCamLog (DWORD) to 1 and every process
/// that loads the DLL appends to iPhoneWebcamVCam-<pid>.log in VCamLogDir (a folder
/// the camera service can write to) or else its temp folder.
pub(crate) fn log(message: std::fmt::Arguments) {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    let file = FILE.get_or_init(|| {
        let mut enabled = 0u32;
        let mut size = 4u32;
        let ok = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                PCWSTR(wide(r"SOFTWARE\iPhoneWebcam").as_ptr()),
                PCWSTR(wide("VCamLog").as_ptr()),
                RRF_RT_REG_DWORD,
                None,
                Some(&mut enabled as *mut u32 as *mut c_void),
                Some(&mut size),
            )
        };
        if ok != ERROR_SUCCESS || enabled == 0 {
            return None;
        }
        let dir = read_registry_string(r"SOFTWARE\iPhoneWebcam", "VCamLogDir")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = dir.join(format!("iPhoneWebcamVCam-{}.log", std::process::id()));
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    });
    if let Some(file) = file {
        if let Ok(mut f) = file.lock() {
            let _ = writeln!(f, "{:?} [{:?}] {}", std::time::SystemTime::now(), std::thread::current().id(), message);
        }
    }
}

macro_rules! vlog {
    ($($arg:tt)*) => { $crate::imp::log(format_args!($($arg)*)) };
}
pub(crate) use vlog;

fn read_registry_string(key: &str, value: &str) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(wide(key).as_ptr()),
            PCWSTR(wide(value).as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    (ok == ERROR_SUCCESS).then(|| {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    })
}

// ---------------------------------------------------------------- class factory

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
        unsafe { *object = std::ptr::null_mut() };
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let activate: IMFActivate = activate::Activate::new()?.into();
        unsafe { activate.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- DLL exports

#[no_mangle]
extern "system" fn DllMain(instance: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(instance.0, Ordering::Relaxed);
    }
    true.into()
}

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

fn clsid_key() -> String {
    format!(r"SOFTWARE\Classes\CLSID\{}", protocol::frame_section::VCAM_CLSID)
}

fn set_string(key: &str, name: Option<&str>, value: &str) -> windows::core::Result<()> {
    unsafe {
        let mut hkey = HKEY::default();
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(wide(key).as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .ok()?;
        let data = wide(value);
        let name = name.map(wide);
        let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
        let result = RegSetValueExW(
            hkey,
            name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())),
            None,
            REG_SZ,
            Some(bytes),
        );
        let _ = windows::Win32::System::Registry::RegCloseKey(hkey);
        result.ok()
    }
}

/// `regsvr32`: register the COM class machine-wide so the Frame Server service can load it.
#[no_mangle]
extern "system" fn DllRegisterServer() -> HRESULT {
    let mut path = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(HINSTANCE(MODULE.load(Ordering::Relaxed)).into()), &mut path) } as usize;
    if len == 0 {
        return windows::core::Error::from_thread().code();
    }
    let path = String::from_utf16_lossy(&path[..len]);
    let server = format!(r"{}\InprocServer32", clsid_key());
    let result = set_string(&clsid_key(), None, "iPhone Webcam virtual camera")
        .and_then(|_| set_string(&server, None, &path))
        .and_then(|_| set_string(&server, Some("ThreadingModel"), "Both"));
    match result {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

#[no_mangle]
extern "system" fn DllUnregisterServer() -> HRESULT {
    let status = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(wide(&clsid_key()).as_ptr())) };
    if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
        S_OK
    } else {
        status.to_hresult()
    }
}
