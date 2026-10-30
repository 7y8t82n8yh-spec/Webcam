//! The media source (one video stream) the Frame Server pulls frames from.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{implement, Interface, GUID, HRESULT};
use windows::Win32::Foundation::{ERROR_SET_NOT_FOUND, E_INVALIDARG, E_OUTOFMEMORY};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{ComObject, IUnknown, Ref};

use super::events::EventQueue;
use super::frames::FrameReader;
use super::{lock, new_attributes, vlog};
use crate::convert;

const FPS: u32 = 30;
const FRAME_DURATION: i64 = 10_000_000 / FPS as i64; // 100 ns units
const FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / FPS as u64);
/// Offered sizes, each also turned to portrait. Those in the orientation of the app's
/// picture come first (see `create_stream_descriptor`).
const SIZES: [(u32, u32); 3] = [(1280, 720), (1920, 1080), (640, 360)];

fn shutdown_error() -> windows::core::Error {
    MF_E_SHUTDOWN.into()
}

// SAFETY (for the unsafe blocks in this file unless they say otherwise): Media
// Foundation calls on live objects we hold, with valid arguments.

fn create_video_type(subtype: &GUID, w: u32, h: u32) -> windows::core::Result<IMFMediaType> {
    let nv12 = *subtype == MFVideoFormat_NV12;
    let stride = if nv12 { w } else { w * 4 };
    let sample_size = convert::frame_bytes(w, h, nv12) as u32;
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        t.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride)?;
        t.SetUINT32(&MF_MT_SAMPLE_SIZE, sample_size)?;
        if nv12 {
            // convert() produces BT.601 limited range.
            t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32)?;
            t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        }
        Ok(t)
    }
}

// ---------------------------------------------------------------- stream

/// State shared between the stream's COM methods and its worker thread.
struct Shared {
    inner: Mutex<StreamInner>,
    wake: Condvar,
}

struct StreamInner {
    queue: Option<EventQueue>,
    descriptor: Option<IMFStreamDescriptor>,
    state: MF_STREAM_STATE,
    requests: VecDeque<Option<IUnknown>>,
}

// SAFETY: the camera service runs us in the multithreaded apartment, where any COM
// interface pointer (queue, descriptor, request tokens) may be used from any thread;
// the mutex serialises access.
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
        let queue = EventQueue::new()?;
        let inner = StreamInner {
            queue: Some(queue),
            descriptor: Some(descriptor),
            state: MF_STREAM_STATE_STOPPED,
            requests: VecDeque::new(),
        };
        let shared = Arc::new(Shared { inner: Mutex::new(inner), wake: Condvar::new() });
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("vcam-stream".into())
            .spawn(move || worker(worker_shared))
            .map_err(|_| windows::core::Error::from(E_OUTOFMEMORY))?;
        Ok(Self { shared, source: Mutex::new(None), worker: Mutex::new(Some(worker)) })
    }

    /// The stream's state, unless it was shut down.
    fn live(&self) -> windows::core::Result<MutexGuard<'_, StreamInner>> {
        let inner = lock(&self.shared.inner);
        if inner.queue.is_none() {
            return Err(shutdown_error());
        }
        Ok(inner)
    }

    fn start(&self, start_time: &PROPVARIANT) -> windows::core::Result<()> {
        let mut inner = self.live()?;
        inner.state = MF_STREAM_STATE_RUNNING;
        inner.queue.as_ref().ok_or_else(shutdown_error)?.send(MEStreamStarted, Some(start_time))
    }

    fn stop(&self) -> windows::core::Result<()> {
        let mut inner = self.live()?;
        inner.state = MF_STREAM_STATE_STOPPED;
        inner.requests.clear();
        inner.queue.as_ref().ok_or_else(shutdown_error)?.send(MEStreamStopped, None)
    }

    fn shutdown(&self) {
        let mut inner = lock(&self.shared.inner);
        if let Some(queue) = inner.queue.take() {
            queue.shutdown();
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

    fn queue(&self) -> windows::core::Result<EventQueue> {
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
    let mut due = Instant::now();
    let mut logged = 0;
    loop {
        let Some((token, descriptor)) = next_request(&shared) else { return };
        pace(&mut due);
        let sample = create_sample(&descriptor, token.as_ref(), &mut reader);
        if logged < 3 || sample.is_err() {
            logged += 1;
            vlog!("stream sample: {:?}", sample.as_ref().map(|_| ()).map_err(|e| e.code()));
        }
        deliver(&shared, sample);
    }
}

/// Wait for a sample request: its token and the stream descriptor, or `None` once the
/// stream is shut down.
fn next_request(shared: &Shared) -> Option<(Option<IUnknown>, IMFStreamDescriptor)> {
    let mut inner = lock(&shared.inner);
    while inner.queue.is_some() && inner.requests.is_empty() {
        inner = shared.wake.wait(inner).unwrap_or_else(|e| e.into_inner());
    }
    inner.queue.as_ref()?;
    let token = inner.requests.pop_front().flatten();
    Some((token, inner.descriptor.clone()?))
}

/// Sleep until the next frame is due and schedule the one after.
fn pace(due: &mut Instant) {
    let now = Instant::now();
    if *due > now {
        std::thread::sleep(*due - now);
    }
    *due = (*due).max(now) + FRAME_INTERVAL;
}

/// Queue the sample (or the error making it) if the stream is still running.
fn deliver(shared: &Shared, sample: windows::core::Result<IMFSample>) {
    let inner = lock(&shared.inner);
    let Some(queue) = inner.queue.clone() else { return };
    if inner.state != MF_STREAM_STATE_RUNNING {
        return;
    }
    drop(inner);
    let _ = match sample {
        Ok(sample) => queue.send_object(MEMediaSample, &sample),
        Err(e) => queue.send_error(e.code()),
    };
}

/// The next frame, converted to the stream's current format, carrying the request's
/// token (the client matches samples to requests by it).
fn create_sample(
    descriptor: &IMFStreamDescriptor,
    token: Option<&IUnknown>,
    reader: &mut FrameReader,
) -> windows::core::Result<IMFSample> {
    let media_type = unsafe { descriptor.GetMediaTypeHandler()?.GetCurrentMediaType()? };
    let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE)? };
    let size = unsafe { media_type.GetUINT64(&MF_MT_FRAME_SIZE)? };
    let (w, h) = ((size >> 32) as u32, size as u32);
    if w == 0 || h == 0 {
        return Err(MF_E_INVALIDMEDIATYPE.into());
    }
    let nv12 = subtype == MFVideoFormat_NV12;
    let len = convert::frame_bytes(w, h, nv12);
    let buffer = unsafe { MFCreateMemoryBuffer(len as u32)? };
    fill(&buffer, len, |pixels| match reader.read() {
        Some((rgb, sw, sh)) => convert::convert(rgb, sw, sh, pixels, w, h, nv12),
        None => convert::fill_black(pixels, w, h, nv12),
    })?;
    unsafe {
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(MFGetSystemTime())?;
        sample.SetSampleDuration(FRAME_DURATION)?;
        if let Some(token) = token {
            sample.SetUnknown(&MFSampleExtension_Token, token)?;
        }
        Ok(sample)
    }
}

/// Write the first `len` bytes of `buffer` (made with at least that capacity) with `f`.
fn fill(buffer: &IMFMediaBuffer, len: usize, f: impl FnOnce(&mut [u8])) -> windows::core::Result<()> {
    let mut data = std::ptr::null_mut();
    let mut capacity = 0u32;
    unsafe { buffer.Lock(&mut data, Some(&mut capacity), None)? };
    if capacity as usize >= len {
        // SAFETY: Lock gives `capacity` writable bytes at `data` until Unlock, and nothing
        // else touches the buffer meanwhile.
        f(unsafe { std::slice::from_raw_parts_mut(data, len) });
    }
    unsafe {
        buffer.Unlock()?;
        buffer.SetCurrentLength(len as u32)
    }
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
        self.queue()?.get_event(flags) // may block: the lock isn't held
    }
    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> windows::core::Result<()> {
        self.queue()?.begin_get_event(callback, state)
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        self.queue()?.end_get_event(result)
    }
    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        status: HRESULT,
        value: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        let queue = self.queue()?;
        // SAFETY: COM passes valid (or null) pointers.
        unsafe { queue.queue_event(met, ext, status, value) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> windows::core::Result<IMFMediaSource> {
        drop(self.live()?);
        lock(&self.source).clone().ok_or_else(shutdown_error)
    }
    fn GetStreamDescriptor(&self) -> windows::core::Result<IMFStreamDescriptor> {
        lock(&self.shared.inner).descriptor.clone().ok_or_else(shutdown_error)
    }
    fn RequestSample(&self, token: Ref<IUnknown>) -> windows::core::Result<()> {
        {
            let mut inner = self.live()?;
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
        let mut inner = self.live()?;
        inner.state = value; // paused behaves like stopped for a live source
        if value != MF_STREAM_STATE_RUNNING {
            inner.requests.clear();
        }
        Ok(())
    }
    fn GetStreamState(&self) -> windows::core::Result<MF_STREAM_STATE> {
        Ok(self.live()?.state)
    }
}

// ---------------------------------------------------------------- source

/// Every offered format: NV12 then RGB32, each in all `SIZES`, landscape and portrait
/// (portrait first if `portrait`).
fn create_video_types(portrait: bool) -> windows::core::Result<Vec<Option<IMFMediaType>>> {
    let turned = SIZES.map(|(w, h)| (h, w));
    let sizes = if portrait { [turned, SIZES] } else { [SIZES, turned] }.concat();
    let mut types = Vec::new();
    for subtype in [MFVideoFormat_NV12, MFVideoFormat_RGB32] {
        for &(w, h) in &sizes {
            types.push(Some(create_video_type(&subtype, w, h)?));
        }
    }
    Ok(types)
}

fn create_stream_descriptor() -> windows::core::Result<IMFStreamDescriptor> {
    // Video apps take the first size offered: make it portrait while the app's picture
    // is (it was turned a quarter). The camera service asks a source for its formats
    // once, so the app restarts the camera when the orientation changes, and the new
    // source made then looks again.
    let portrait = FrameReader::new().read().is_some_and(|(_, w, h)| h > w);
    vlog!("offering {} sizes first", if portrait { "portrait" } else { "landscape" });
    let types = create_video_types(portrait)?;
    unsafe {
        let descriptor = MFCreateStreamDescriptor(0, &types)?;
        descriptor.GetMediaTypeHandler()?.SetCurrentMediaType(types[0].as_ref())?;
        // What the Frame Server needs to treat the stream as a colour camera.
        descriptor.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        descriptor.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
        descriptor.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        descriptor.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
        Ok(descriptor)
    }
}

#[implement(IMFMediaSourceEx, IKsControl)]
pub struct Source {
    inner: Mutex<SourceInner>,
    stream: ComObject<Stream>,
}

struct SourceInner {
    queue: Option<EventQueue>,
    attributes: IMFAttributes,
    presentation: IMFPresentationDescriptor,
    started: bool,
}

impl Source {
    pub fn create() -> windows::core::Result<ComObject<Source>> {
        let descriptor = create_stream_descriptor()?;
        let presentation = unsafe { MFCreatePresentationDescriptor(Some(&[Some(descriptor.clone())]))? };
        unsafe { presentation.SelectStream(0)? };
        let stream = ComObject::new(Stream::new(descriptor)?);
        let inner = SourceInner {
            queue: Some(EventQueue::new()?),
            attributes: new_attributes()?,
            presentation,
            started: false,
        };
        let source = ComObject::new(Source { inner: Mutex::new(inner), stream: stream.clone() });
        let source_interface: IMFMediaSource = source.to_interface::<IMFMediaSourceEx>().into();
        *lock(&stream.source) = Some(source_interface);
        vlog!("source created");
        Ok(source)
    }

    pub fn is_shut_down(&self) -> bool {
        lock(&self.inner).queue.is_none()
    }

    pub fn shutdown(&self) {
        vlog!("source Shutdown");
        if let Some(queue) = lock(&self.inner).queue.take() {
            queue.shutdown();
        }
        self.stream.shutdown(); // joins its worker: not under our lock
    }

    fn queue(&self) -> windows::core::Result<EventQueue> {
        lock(&self.inner).queue.clone().ok_or_else(shutdown_error)
    }

    /// The source's state, unless it was shut down.
    fn live(&self) -> windows::core::Result<MutexGuard<'_, SourceInner>> {
        let inner = lock(&self.inner);
        if inner.queue.is_none() {
            return Err(shutdown_error());
        }
        Ok(inner)
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stream.shutdown();
    }
}

impl IMFMediaEventGenerator_Impl for Source_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows::core::Result<IMFMediaEvent> {
        self.queue()?.get_event(flags)
    }
    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> windows::core::Result<()> {
        self.queue()?.begin_get_event(callback, state)
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows::core::Result<IMFMediaEvent> {
        self.queue()?.end_get_event(result)
    }
    fn QueueEvent(
        &self,
        met: u32,
        ext: *const GUID,
        status: HRESULT,
        value: *const PROPVARIANT,
    ) -> windows::core::Result<()> {
        let queue = self.queue()?;
        // SAFETY: COM passes valid (or null) pointers.
        unsafe { queue.queue_event(met, ext, status, value) }
    }
}

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> windows::core::Result<u32> {
        drop(self.live()?);
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> windows::core::Result<IMFPresentationDescriptor> {
        unsafe { self.live()?.presentation.Clone() }
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
        // SAFETY: COM passes a valid or null pointer.
        if unsafe { time_format.as_ref() }.is_some_and(|format| *format != GUID::zeroed()) {
            return Err(MF_E_UNSUPPORTED_TIME_FORMAT.into());
        }
        let mut selected = windows::core::BOOL::default();
        let mut stream_descriptor = None;
        unsafe { descriptor.GetStreamDescriptorByIndex(0, &mut selected, &mut stream_descriptor)? };
        let start = PROPVARIANT::from(unsafe { MFGetSystemTime() });
        if selected.as_bool() {
            let event = if inner.started { MEUpdatedStream } else { MENewStream };
            let stream: IMFMediaStream2 = self.stream.to_interface();
            queue.send_object(event, &stream)?;
            self.stream.start(&start)?;
            inner.started = true;
        }
        queue.send(MESourceStarted, Some(&start))
    }

    fn Stop(&self) -> windows::core::Result<()> {
        vlog!("source Stop");
        let queue = self.queue()?;
        self.stream.stop()?;
        queue.send(MESourceStopped, None)
    }

    fn Pause(&self) -> windows::core::Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into()) // live source
    }

    fn Shutdown(&self) -> windows::core::Result<()> {
        drop(self.live()?);
        self.shutdown();
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> windows::core::Result<IMFAttributes> {
        Ok(self.live()?.attributes.clone())
    }

    fn GetStreamAttributes(&self, stream_id: u32) -> windows::core::Result<IMFAttributes> {
        drop(self.live()?);
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
