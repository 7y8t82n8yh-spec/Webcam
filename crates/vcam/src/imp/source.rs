//! The media source (one video stream) the Frame Server pulls frames from.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{implement, Interface, GUID, HRESULT};
use windows::Win32::Foundation::{ERROR_SET_NOT_FOUND, E_INVALIDARG, E_POINTER, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{ComObject, IUnknown, Ref};

use super::frames::FrameReader;
use super::vlog;
use crate::convert;

const FPS: u32 = 30;
const FRAME_DURATION: i64 = 10_000_000 / FPS as i64; // 100 ns units
/// Offered sizes; the first (NV12 1280x720) is the default.
const SIZES: [(u32, u32); 3] = [(1280, 720), (1920, 1080), (640, 360)];

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn shutdown_error() -> windows::core::Error {
    MF_E_SHUTDOWN.into()
}

unsafe fn create_video_type(subtype: &GUID, w: u32, h: u32) -> windows::core::Result<IMFMediaType> {
    let nv12 = *subtype == MFVideoFormat_NV12;
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
    t.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1)?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
    t.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
    t.SetUINT32(&MF_MT_DEFAULT_STRIDE, if nv12 { w } else { w * 4 })?;
    t.SetUINT32(&MF_MT_SAMPLE_SIZE, convert::frame_bytes(w, h, nv12) as u32)?;
    if nv12 {
        // convert() produces BT.601 limited range.
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(t)
}

// ---------------------------------------------------------------- stream

/// State shared between the stream's COM methods and its worker thread.
struct Shared {
    inner: Mutex<StreamInner>,
    wake: Condvar,
}

struct StreamInner {
    queue: Option<IMFMediaEventQueue>,
    descriptor: Option<IMFStreamDescriptor>,
    state: MF_STREAM_STATE,
    requests: VecDeque<Option<IUnknown>>,
    stopping: bool,
}

// Media Foundation event queues, descriptors and tokens are free-threaded.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

#[implement(IMFMediaStream2)]
pub struct Stream {
    shared: Arc<Shared>,
    /// Back reference; cleared on shutdown to break the source <-> stream cycle.
    source: Mutex<Option<IMFMediaSource>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Stream {
    fn new(descriptor: IMFStreamDescriptor) -> windows::core::Result<Self> {
        let queue = unsafe { MFCreateEventQueue()? };
        let inner = StreamInner {
            queue: Some(queue),
            descriptor: Some(descriptor),
            state: MF_STREAM_STATE_STOPPED,
            requests: VecDeque::new(),
            stopping: false,
        };
        let shared = Arc::new(Shared { inner: Mutex::new(inner), wake: Condvar::new() });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("vcam-stream".into())
            .spawn(move || worker(worker_shared))
            .map_err(|_| windows::core::Error::from(E_POINTER))?;
        Ok(Self { shared, source: Mutex::new(None), worker: Mutex::new(Some(worker)) })
    }

    fn start(&self, start_time: &PROPVARIANT) -> windows::core::Result<()> {
        let mut inner = lock(&self.shared.inner);
        let queue = inner.queue.clone().ok_or_else(shutdown_error)?;
        inner.state = MF_STREAM_STATE_RUNNING;
        unsafe { queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, start_time) }
    }

    fn stop(&self) -> windows::core::Result<()> {
        let mut inner = lock(&self.shared.inner);
        let queue = inner.queue.clone().ok_or_else(shutdown_error)?;
        inner.state = MF_STREAM_STATE_STOPPED;
        inner.requests.clear();
        unsafe { queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null()) }
    }

    fn shutdown(&self) {
        let mut inner = lock(&self.shared.inner);
        inner.stopping = true;
        if let Some(queue) = inner.queue.take() {
            let _ = unsafe { queue.Shutdown() };
        }
        inner.requests.clear();
        inner.descriptor = None;
        drop(inner);
        self.shared.wake.notify_all();
        self.join_worker();
        *lock(&self.source) = None;
    }

    /// Wait for the worker to exit (unless this is the worker), holding the worker lock.
    fn join_worker(&self) {
        let mut slot = lock(&self.worker);
        let Some(worker) = slot.take() else { return };
        if worker.thread().id() != std::thread::current().id() {
            let _ = worker.join();
        }
    }

    fn queue(&self) -> windows::core::Result<IMFMediaEventQueue> {
        lock(&self.shared.inner).queue.clone().ok_or_else(shutdown_error)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Answers sample requests, paced to the frame rate.
fn worker(shared: Arc<Shared>) {
    let mut reader = FrameReader::new();
    let mut due: i64 = 0;
    let mut logged = 0;
    loop {
        let Some((token, descriptor)) = next_request(&shared) else { return };
        pace(&mut due);

        let sample = descriptor.ok_or_else(shutdown_error).and_then(|d| unsafe { create_sample(&d, &mut reader) });
        let sample = sample.and_then(|s| unsafe { attach_token(s, token.as_ref()) });
        if logged < 3 || sample.is_err() {
            logged += 1;
            let outcome = sample.as_ref().map(|_| ()).map_err(|e| e.code());
            vlog!("stream sample: {:?}", outcome);
        }
        deliver(&shared, sample);
    }
}

/// Wait for a sample request: its token and the current descriptor, or `None` once stopping.
fn next_request(shared: &Shared) -> Option<(Option<IUnknown>, Option<IMFStreamDescriptor>)> {
    let mut inner = lock(&shared.inner);
    while !inner.stopping && inner.requests.is_empty() {
        inner = shared.wake.wait(inner).unwrap_or_else(|e| e.into_inner());
    }
    if inner.stopping {
        return None;
    }
    let token = inner.requests.pop_front().flatten();
    Some((token, inner.descriptor.clone()))
}

/// Sleep until the next frame is due and schedule the one after.
fn pace(due: &mut i64) {
    let now = unsafe { MFGetSystemTime() };
    if *due > now {
        let wait = Duration::from_nanos(((*due - now) * 100) as u64);
        std::thread::sleep(wait);
    }
    *due = (*due).max(now) + FRAME_DURATION;
}

unsafe fn attach_token(sample: IMFSample, token: Option<&IUnknown>) -> windows::core::Result<IMFSample> {
    if let Some(token) = token {
        sample.SetUnknown(&MFSampleExtension_Token, token)?;
    }
    Ok(sample)
}

/// Queue the sample (or the error making it) if the stream is still running.
fn deliver(shared: &Shared, sample: windows::core::Result<IMFSample>) {
    let inner = lock(&shared.inner);
    let Some(queue) = inner.queue.clone() else { return };
    if inner.state != MF_STREAM_STATE_RUNNING {
        return;
    }
    drop(inner);
    unsafe {
        let _ = match sample {
            Ok(sample) => queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample),
            Err(e) => queue.QueueEventParamVar(MEError.0 as u32, &GUID::zeroed(), e.code(), std::ptr::null()),
        };
    }
}

unsafe fn create_sample(
    descriptor: &IMFStreamDescriptor,
    reader: &mut FrameReader,
) -> windows::core::Result<IMFSample> {
    let media_type = descriptor.GetMediaTypeHandler()?.GetCurrentMediaType()?;
    let subtype = media_type.GetGUID(&MF_MT_SUBTYPE)?;
    let size = media_type.GetUINT64(&MF_MT_FRAME_SIZE)?;
    let (w, h) = ((size >> 32) as u32, size as u32);
    if w == 0 || h == 0 {
        return Err(MF_E_INVALIDMEDIATYPE.into());
    }
    let nv12 = subtype == MFVideoFormat_NV12;
    let len = convert::frame_bytes(w, h, nv12);
    let buffer = MFCreateMemoryBuffer(len as u32)?;
    let mut data = std::ptr::null_mut();
    buffer.Lock(&mut data, None, None)?;
    let dst = std::slice::from_raw_parts_mut(data, len);
    match reader.read() {
        Some((rgb, sw, sh)) => convert::convert(rgb, sw, sh, dst, w, h, nv12),
        None => convert::fill_black(dst, w, h, nv12),
    }
    buffer.Unlock()?;
    buffer.SetCurrentLength(len as u32)?;
    let sample = MFCreateSample()?;
    sample.AddBuffer(&buffer)?;
    sample.SetSampleTime(MFGetSystemTime())?;
    sample.SetSampleDuration(FRAME_DURATION)?;
    Ok(sample)
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
        let queue = self.queue()?; // may block: not under the lock
        unsafe { queue.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> windows::core::Result<()> {
        unsafe { self.queue()?.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        unsafe { self.queue()?.EndGetEvent(result.as_ref()) }
    }
    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        status: HRESULT,
        value: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        unsafe { self.queue()?.QueueEventParamVar(met, ext, status, value) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> windows::core::Result<IMFMediaSource> {
        self.queue()?;
        lock(&self.source).clone().ok_or_else(shutdown_error)
    }
    fn GetStreamDescriptor(&self) -> windows::core::Result<IMFStreamDescriptor> {
        lock(&self.shared.inner).descriptor.clone().ok_or_else(shutdown_error)
    }
    fn RequestSample(&self, token: Ref<IUnknown>) -> windows::core::Result<()> {
        {
            let mut inner = lock(&self.shared.inner);
            if inner.queue.is_none() {
                return Err(shutdown_error());
            }
            if inner.state != MF_STREAM_STATE_RUNNING {
                return Err(MF_E_MEDIA_SOURCE_WRONGSTATE.into());
            }
            inner.requests.push_back(token.cloned());
        }
        self.shared.wake.notify_one();
        Ok(())
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> windows::core::Result<()> {
        vlog!("stream SetStreamState {}", value.0);
        let mut inner = lock(&self.shared.inner);
        if inner.queue.is_none() {
            return Err(shutdown_error());
        }
        inner.state = value; // paused behaves like stopped for a live source
        if value != MF_STREAM_STATE_RUNNING {
            inner.requests.clear();
        }
        Ok(())
    }
    fn GetStreamState(&self) -> windows::core::Result<MF_STREAM_STATE> {
        let inner = lock(&self.shared.inner);
        if inner.queue.is_none() {
            return Err(shutdown_error());
        }
        Ok(inner.state)
    }
}

// ---------------------------------------------------------------- source

/// Every offered format: NV12 then RGB32, each in all `SIZES`.
unsafe fn create_video_types() -> windows::core::Result<Vec<Option<IMFMediaType>>> {
    let mut types = Vec::new();
    for subtype in [MFVideoFormat_NV12, MFVideoFormat_RGB32] {
        for (w, h) in SIZES {
            types.push(Some(create_video_type(&subtype, w, h)?));
        }
    }
    Ok(types)
}

unsafe fn create_stream_descriptor() -> windows::core::Result<IMFStreamDescriptor> {
    let types = create_video_types()?;
    let descriptor = MFCreateStreamDescriptor(0, &types)?;
    descriptor.GetMediaTypeHandler()?.SetCurrentMediaType(types[0].as_ref())?;
    // What the Frame Server needs to treat the stream as a colour camera.
    descriptor.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
    descriptor.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
    descriptor.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
    descriptor.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
    Ok(descriptor)
}

#[implement(IMFMediaSourceEx, IKsControl)]
pub struct Source {
    inner: Mutex<SourceInner>,
    stream: ComObject<Stream>,
}

struct SourceInner {
    queue: Option<IMFMediaEventQueue>,
    attributes: IMFAttributes,
    presentation: IMFPresentationDescriptor,
    started: bool,
}

impl Source {
    pub fn create() -> windows::core::Result<ComObject<Source>> {
        unsafe {
            let descriptor = create_stream_descriptor()?;
            let presentation = MFCreatePresentationDescriptor(Some(&[Some(descriptor.clone())]))?;
            presentation.SelectStream(0)?;
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 1)?;

            let stream = ComObject::new(Stream::new(descriptor)?);
            let queue = MFCreateEventQueue()?;
            let attributes = attributes.ok_or(E_POINTER)?;
            let inner = SourceInner { queue: Some(queue), attributes, presentation, started: false };
            let source = ComObject::new(Source { inner: Mutex::new(inner), stream: stream.clone() });
            let source_interface: IMFMediaSource = source.to_interface::<IMFMediaSourceEx>().into();
            *lock(&stream.source) = Some(source_interface);
            vlog!("source created");
            Ok(source)
        }
    }

    pub fn is_shut_down(&self) -> bool {
        lock(&self.inner).queue.is_none()
    }

    pub fn shutdown(&self) {
        vlog!("source Shutdown");
        if let Some(queue) = lock(&self.inner).queue.take() {
            let _ = unsafe { queue.Shutdown() };
        }
        self.stream.shutdown(); // joins its worker: not under our lock
    }

    fn queue(&self) -> windows::core::Result<IMFMediaEventQueue> {
        lock(&self.inner).queue.clone().ok_or_else(shutdown_error)
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stream.shutdown();
    }
}

impl IMFMediaEventGenerator_Impl for Source_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
        let queue = self.queue()?;
        unsafe { queue.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> windows::core::Result<()> {
        unsafe { self.queue()?.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        unsafe { self.queue()?.EndGetEvent(result.as_ref()) }
    }
    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        status: HRESULT,
        value: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        unsafe { self.queue()?.QueueEventParamVar(met, ext, status, value) }
    }
}

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> windows::core::Result<u32> {
        self.queue()?;
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> windows::core::Result<IMFPresentationDescriptor> {
        let inner = lock(&self.inner);
        if inner.queue.is_none() {
            return Err(shutdown_error());
        }
        unsafe { inner.presentation.Clone() }
    }

    fn Start(
        &self,
        descriptor: Ref<IMFPresentationDescriptor>,
        time_format: *const GUID,
        _start_position: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        vlog!("source Start");
        let mut inner = lock(&self.inner);
        let queue = inner.queue.clone().ok_or_else(shutdown_error)?;
        let descriptor = descriptor.as_ref().ok_or(E_INVALIDARG)?;
        if !time_format.is_null() && unsafe { *time_format } != GUID::zeroed() {
            return Err(MF_E_UNSUPPORTED_TIME_FORMAT.into());
        }
        let mut selected = windows::core::BOOL::default();
        let mut stream_descriptor = None;
        unsafe { descriptor.GetStreamDescriptorByIndex(0, &mut selected, &mut stream_descriptor)? };
        let start = PROPVARIANT::from(unsafe { MFGetSystemTime() });
        if selected.as_bool() {
            let event = if inner.started { MEUpdatedStream } else { MENewStream };
            let stream: IMFMediaStream2 = self.stream.to_interface();
            unsafe { queue.QueueEventParamUnk(event.0 as u32, &GUID::zeroed(), S_OK, &stream)? };
            self.stream.start(&start)?;
            inner.started = true;
        }
        unsafe { queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, &start) }
    }

    fn Stop(&self) -> windows::core::Result<()> {
        vlog!("source Stop");
        let queue = self.queue()?;
        self.stream.stop()?;
        unsafe { queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null()) }
    }

    fn Pause(&self) -> windows::core::Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into()) // live source
    }

    fn Shutdown(&self) -> windows::core::Result<()> {
        self.queue()?;
        self.shutdown();
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> windows::core::Result<IMFAttributes> {
        let inner = lock(&self.inner);
        if inner.queue.is_none() {
            return Err(shutdown_error());
        }
        Ok(inner.attributes.clone())
    }

    fn GetStreamAttributes(&self, stream_id: u32) -> windows::core::Result<IMFAttributes> {
        self.queue()?;
        if stream_id != 0 {
            return Err(MF_E_INVALIDSTREAMNUMBER.into());
        }
        // A stream descriptor is an attribute store carrying the stream attributes.
        let inner = lock(&self.stream.shared.inner);
        let descriptor = inner.descriptor.as_ref().ok_or_else(shutdown_error)?;
        descriptor.cast()
    }

    fn SetD3DManager(&self, _manager: Ref<IUnknown>) -> windows::core::Result<()> {
        Ok(()) // we produce system-memory frames
    }
}

impl IKsControl_Impl for Source_Impl {
    // No custom properties, methods or events. The Frame Server requires the interface.
    fn KsProperty(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut core::ffi::c_void,
        _: u32,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        Err(ERROR_SET_NOT_FOUND.to_hresult().into())
    }
    fn KsMethod(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut core::ffi::c_void,
        _: u32,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        Err(ERROR_SET_NOT_FOUND.to_hresult().into())
    }
    fn KsEvent(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut core::ffi::c_void,
        _: u32,
        _: *mut u32,
    ) -> windows::core::Result<()> {
        Err(ERROR_SET_NOT_FOUND.to_hresult().into())
    }
}
