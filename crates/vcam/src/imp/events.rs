//! Media Foundation's event queue, which both the source and its stream hand events
//! out of, behind safe methods.

use windows::core::{Interface, Result, GUID, HRESULT};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{IUnknown, Ref};

#[derive(Clone)]
pub struct EventQueue(IMFMediaEventQueue);

// SAFETY (for every unsafe block below): plain calls on a valid queue, with arguments
// that are valid references; the queue is free-threaded.
impl EventQueue {
    pub fn new() -> Result<Self> {
        unsafe { MFCreateEventQueue() }.map(EventQueue)
    }

    /// Queue `event`, with `value` as its data (or none).
    pub fn send(&self, event: MF_EVENT_TYPE, value: Option<&PROPVARIANT>) -> Result<()> {
        let value = value.map_or(std::ptr::null(), |value| value as *const PROPVARIANT);
        unsafe { self.0.QueueEventParamVar(event.0 as u32, &GUID::zeroed(), S_OK, value) }
    }

    /// Queue `event` carrying a COM object (a sample, or the stream).
    pub fn send_object(&self, event: MF_EVENT_TYPE, object: &impl Interface) -> Result<()> {
        let object: IUnknown = object.cast()?;
        unsafe { self.0.QueueEventParamUnk(event.0 as u32, &GUID::zeroed(), S_OK, &object) }
    }

    pub fn send_error(&self, status: HRESULT) -> Result<()> {
        unsafe { self.0.QueueEventParamVar(MEError.0 as u32, &GUID::zeroed(), status, std::ptr::null()) }
    }

    pub fn shutdown(&self) {
        let _ = unsafe { self.0.Shutdown() };
    }

    // The IMFMediaEventGenerator methods the source and stream forward to the queue.

    pub fn get_event(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        unsafe { self.0.GetEvent(flags.0) }
    }

    pub fn begin_get_event(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        unsafe { self.0.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }

    pub fn end_get_event(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.0.EndGetEvent(result.as_ref()) }
    }

    /// `IMFMediaEventGenerator::QueueEvent`, called by a client.
    ///
    /// # Safety
    /// `ext` and `value` must be valid or null, as COM guarantees for its callers.
    pub unsafe fn queue_event(
        &self,
        met: u32,
        ext: *const GUID,
        status: HRESULT,
        value: *const PROPVARIANT,
    ) -> Result<()> {
        self.0.QueueEventParamVar(met, ext, status, value)
    }
}
