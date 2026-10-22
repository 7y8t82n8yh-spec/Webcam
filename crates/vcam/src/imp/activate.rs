//! The COM class: an `IMFActivate` the Frame Server asks for the media source.

use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::{implement, Interface, BOOL, GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::Ref;

use super::source::Source;
use super::vlog;

#[implement(IMFActivate)]
pub struct Activate {
    attributes: IMFAttributes,
    source: Mutex<Option<windows_core::ComObject<Source>>>,
}

impl Activate {
    pub fn new() -> windows::core::Result<Self> {
        Ok(Self { attributes: unsafe { create_attributes()? }, source: Mutex::new(None) })
    }
}

unsafe fn create_attributes() -> windows::core::Result<IMFAttributes> {
    let mut attributes = None;
    MFCreateAttributes(&mut attributes, 1)?;
    attributes.ok_or_else(|| E_POINTER.into())
}

impl IMFActivate_Impl for Activate_Impl {
    fn ActivateObject(&self, iid: *const GUID, object: *mut *mut c_void) -> windows::core::Result<()> {
        vlog!("activate ActivateObject");
        let mut source = self.source.lock().unwrap_or_else(|e| e.into_inner());
        // The client may have shut the previous source down.
        if source.as_ref().is_none_or(|s| s.is_shut_down()) {
            *source = Some(Source::create()?);
        }
        let source: IMFMediaSourceEx = source.as_ref().map(|s| s.to_interface()).ok_or(E_POINTER)?;
        unsafe { source.query(iid, object).ok() }
    }

    fn ShutdownObject(&self) -> windows::core::Result<()> {
        vlog!("activate ShutdownObject");
        if let Some(source) = self.source.lock().unwrap_or_else(|e| e.into_inner()).take() {
            source.shutdown();
        }
        Ok(())
    }

    fn DetachObject(&self) -> windows::core::Result<()> {
        *self.source.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(())
    }
}

/// Forward an `IMFAttributes` method to the inner attribute store through its vtable.
macro_rules! forward {
    ($self:ident . $method:ident ( $($arg:expr),* )) => {{
        let inner = &$self.attributes;
        unsafe { (Interface::vtable(inner).$method)(Interface::as_raw(inner), $($arg),*).ok() }
    }};
}

/// Same, for methods whose last parameter is an out value returned as `Result<T>`.
macro_rules! forward_out {
    ($self:ident . $method:ident ( $($arg:expr),* ) -> $ty:ty) => {{
        let inner = &$self.attributes;
        let mut out: $ty = Default::default();
        unsafe { (Interface::vtable(inner).$method)(Interface::as_raw(inner), $($arg,)* &mut out).map(|| out) }
    }};
}

impl IMFAttributes_Impl for Activate_Impl {
    fn GetItem(&self, key: *const GUID, value: *mut PROPVARIANT) -> windows::core::Result<()> {
        forward!(self.GetItem(key, value))
    }
    fn GetItemType(&self, key: *const GUID) -> windows::core::Result<MF_ATTRIBUTE_TYPE> {
        forward_out!(self.GetItemType(key) -> MF_ATTRIBUTE_TYPE)
    }
    fn CompareItem(&self, key: *const GUID, value: *const PROPVARIANT) -> windows::core::Result<BOOL> {
        forward_out!(self.CompareItem(key, value) -> BOOL)
    }
    fn Compare(&self, theirs: Ref<IMFAttributes>, match_type: MF_ATTRIBUTES_MATCH_TYPE) -> windows::core::Result<BOOL> {
        forward_out!(self.Compare(theirs.as_ref().map_or(std::ptr::null_mut(), |a| a.as_raw()), match_type) -> BOOL)
    }
    fn GetUINT32(&self, key: *const GUID) -> windows::core::Result<u32> {
        forward_out!(self.GetUINT32(key) -> u32)
    }
    fn GetUINT64(&self, key: *const GUID) -> windows::core::Result<u64> {
        forward_out!(self.GetUINT64(key) -> u64)
    }
    fn GetDouble(&self, key: *const GUID) -> windows::core::Result<f64> {
        forward_out!(self.GetDouble(key) -> f64)
    }
    fn GetGUID(&self, key: *const GUID) -> windows::core::Result<GUID> {
        forward_out!(self.GetGUID(key) -> GUID)
    }
    fn GetStringLength(&self, key: *const GUID) -> windows::core::Result<u32> {
        forward_out!(self.GetStringLength(key) -> u32)
    }
    fn GetString(&self, key: *const GUID, value: PWSTR, size: u32, length: *mut u32) -> windows::core::Result<()> {
        forward!(self.GetString(key, value, size, length))
    }
    fn GetAllocatedString(&self, key: *const GUID, value: *mut PWSTR, length: *mut u32) -> windows::core::Result<()> {
        forward!(self.GetAllocatedString(key, value, length))
    }
    fn GetBlobSize(&self, key: *const GUID) -> windows::core::Result<u32> {
        forward_out!(self.GetBlobSize(key) -> u32)
    }
    fn GetBlob(&self, key: *const GUID, buf: *mut u8, size: u32, blob_size: *mut u32) -> windows::core::Result<()> {
        forward!(self.GetBlob(key, buf, size, blob_size))
    }
    fn GetAllocatedBlob(&self, key: *const GUID, buf: *mut *mut u8, size: *mut u32) -> windows::core::Result<()> {
        forward!(self.GetAllocatedBlob(key, buf, size))
    }
    fn GetUnknown(&self, key: *const GUID, iid: *const GUID, object: *mut *mut c_void) -> windows::core::Result<()> {
        forward!(self.GetUnknown(key, iid, object))
    }
    fn SetItem(&self, key: *const GUID, value: *const PROPVARIANT) -> windows::core::Result<()> {
        forward!(self.SetItem(key, value))
    }
    fn DeleteItem(&self, key: *const GUID) -> windows::core::Result<()> {
        forward!(self.DeleteItem(key))
    }
    fn DeleteAllItems(&self) -> windows::core::Result<()> {
        forward!(self.DeleteAllItems())
    }
    fn SetUINT32(&self, key: *const GUID, value: u32) -> windows::core::Result<()> {
        forward!(self.SetUINT32(key, value))
    }
    fn SetUINT64(&self, key: *const GUID, value: u64) -> windows::core::Result<()> {
        forward!(self.SetUINT64(key, value))
    }
    fn SetDouble(&self, key: *const GUID, value: f64) -> windows::core::Result<()> {
        forward!(self.SetDouble(key, value))
    }
    fn SetGUID(&self, key: *const GUID, value: *const GUID) -> windows::core::Result<()> {
        forward!(self.SetGUID(key, value))
    }
    fn SetString(&self, key: *const GUID, value: &PCWSTR) -> windows::core::Result<()> {
        forward!(self.SetString(key, *value))
    }
    fn SetBlob(&self, key: *const GUID, buf: *const u8, size: u32) -> windows::core::Result<()> {
        forward!(self.SetBlob(key, buf, size))
    }
    fn SetUnknown(&self, key: *const GUID, value: Ref<windows::core::IUnknown>) -> windows::core::Result<()> {
        forward!(self.SetUnknown(key, value.as_ref().map_or(std::ptr::null_mut(), |u| u.as_raw())))
    }
    fn LockStore(&self) -> windows::core::Result<()> {
        forward!(self.LockStore())
    }
    fn UnlockStore(&self) -> windows::core::Result<()> {
        forward!(self.UnlockStore())
    }
    fn GetCount(&self) -> windows::core::Result<u32> {
        forward_out!(self.GetCount() -> u32)
    }
    fn GetItemByIndex(&self, index: u32, key: *mut GUID, value: *mut PROPVARIANT) -> windows::core::Result<()> {
        forward!(self.GetItemByIndex(index, key, value))
    }
    fn CopyAllItems(&self, destination: Ref<IMFAttributes>) -> windows::core::Result<()> {
        forward!(self.CopyAllItems(destination.as_ref().map_or(std::ptr::null_mut(), |a| a.as_raw())))
    }
}
