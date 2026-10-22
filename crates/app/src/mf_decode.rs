//! H.264 decoding with the decoder built into Windows (Media Foundation transform).

use std::mem::ManuallyDrop;

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

use crate::decode::VideoDecoder;
use crate::hub::Yuv420;

/// Start Media Foundation once for the whole process and leave it running.
///
/// Pairing MFStartup/MFShutdown per object is fragile: the shutdown in `Drop` runs
/// before the object's COM fields are released, and releasing them after the last
/// MFShutdown crashed (access violation) when a session ended.
pub(crate) fn start_media_foundation() -> Result<()> {
    static STARTED: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
    let started =
        STARTED.get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) }.map_err(|e| e.to_string()));
    started.clone().map_err(|e| anyhow::anyhow!("MFStartup: {e}"))
}

pub struct MfH264Decoder {
    transform: IMFTransform,
    /// Allocated picture size (height may be padded, e.g. 1088) and the visible size.
    allocated: (u32, u32),
    visible: (u32, u32),
    timestamp: i64,
}

// The transform is created in the multithreaded apartment and only used from the
// decoder thread.
unsafe impl Send for MfH264Decoder {}

impl MfH264Decoder {
    pub fn new() -> Result<Self> {
        unsafe {
            // Fine if this thread already joined the MTA.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            start_media_foundation()?;
            let transform: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER).context("H.264 decoder")?;
            if let Ok(attributes) = transform.GetAttributes() {
                // Output each frame as soon as it's decoded instead of buffering.
                let _ = attributes.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
            }
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            transform.SetInputType(0, &input, 0).context("H.264 input type")?;
            let mut decoder = Self { transform, allocated: (0, 0), visible: (0, 0), timestamp: 0 };
            decoder.choose_nv12_output()?;
            decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            decoder.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(decoder)
        }
    }

    /// Select NV12 output (again after a format change) and remember the picture size.
    fn choose_nv12_output(&mut self) -> Result<()> {
        unsafe {
            for index in 0.. {
                let Ok(candidate) = self.transform.GetOutputAvailableType(0, index) else { break };
                if candidate.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12 {
                    continue;
                }
                self.transform.SetOutputType(0, &candidate, 0)?;
                let size = candidate.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                self.allocated = ((size >> 32) as u32, size as u32);
                self.visible = self.allocated;
                let mut blob = [0u8; std::mem::size_of::<MFVideoArea>()];
                if candidate.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, &mut blob, None).is_ok() {
                    let area: MFVideoArea = std::ptr::read_unaligned(blob.as_ptr() as *const _);
                    self.visible = (area.Area.cx as u32, area.Area.cy as u32);
                }
                return Ok(());
            }
        }
        anyhow::bail!("decoder offers no NV12 output")
    }

    /// Pull every decoded picture out of the transform.
    fn drain(&mut self, out: &mut dyn FnMut(&Yuv420)) -> Result<()> {
        loop {
            let sample = unsafe { self.allocate_output_sample()? };
            let (result, sample) = unsafe { self.process_output(sample) };
            match result {
                Ok(()) => {
                    let Some(sample) = sample else { continue };
                    unsafe { self.emit(&sample, out)? };
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.choose_nv12_output()?,
                Err(e) => return Err(e).context("ProcessOutput"),
            }
        }
    }

    /// A sample for the transform to decode into, unless it provides its own.
    unsafe fn allocate_output_sample(&self) -> Result<Option<IMFSample>> {
        let info = self.transform.GetOutputStreamInfo(0)?;
        if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
            return Ok(None);
        }
        let sample = MFCreateSample()?;
        sample.AddBuffer(&MFCreateMemoryBuffer(info.cbSize)?)?;
        Ok(Some(sample))
    }

    /// One `ProcessOutput` call: its result and the sample holding the picture, if any.
    unsafe fn process_output(&self, sample: Option<IMFSample>) -> (windows::core::Result<()>, Option<IMFSample>) {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
        let sample = ManuallyDrop::take(&mut buffers[0].pSample);
        drop(ManuallyDrop::take(&mut buffers[0].pEvents));
        (result, sample)
    }

    unsafe fn emit(&self, sample: &IMFSample, out: &mut dyn FnMut(&Yuv420)) -> Result<()> {
        let buffer = sample.GetBufferByIndex(0)?;
        // Prefer the 2D view, which knows the real row pitch, and ask for the buffer's
        // length too: every read below is checked against it.
        if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer2>() {
            let mut scanline0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            let mut start = std::ptr::null_mut();
            let mut length = 0u32;
            buffer2d.Lock2DSize(MF2DBuffer_LockFlags_Read, &mut scanline0, &mut pitch, &mut start, &mut length)?;
            let offset = scanline0.offset_from(start).max(0) as usize;
            let available = (length as usize).saturating_sub(offset);
            if pitch > 0 {
                let data = std::slice::from_raw_parts(scanline0, available);
                self.emit_nv12(data, pitch as usize, out);
            }
            buffer2d.Unlock2D()?;
        } else {
            let mut pointer = std::ptr::null_mut();
            let mut length = 0u32;
            buffer.Lock(&mut pointer, None, Some(&mut length))?;
            let data = std::slice::from_raw_parts(pointer, length as usize);
            self.emit_nv12(data, self.allocated.0 as usize, out);
            buffer.Unlock()?;
        }
        Ok(())
    }

    /// Hand on an NV12 picture, unless `data` is too small for the size we expect (e.g.
    /// a picture from before a resolution change we haven't seen yet).
    fn emit_nv12(&self, data: &[u8], pitch: usize, out: &mut dyn FnMut(&Yuv420)) {
        let (width, height) = self.visible;
        let allocated_height = self.allocated.1 as usize;
        let luma_bytes = pitch * allocated_height;
        let fits = data.len() >= luma_bytes * 3 / 2 && width as usize <= pitch && height as usize <= allocated_height;
        if !fits {
            tracing::warn!("skipping a decoded picture that doesn't match {width}x{height} (pitch {pitch})");
            return;
        }
        let (y, uv) = data.split_at(luma_bytes);
        out(&Yuv420 { width, height, y, y_stride: pitch, u: uv, v: &[], uv_stride: pitch, nv12: true });
    }
}

impl VideoDecoder for MfH264Decoder {
    fn decode(&mut self, annex_b: &[u8], out: &mut dyn FnMut(&Yuv420)) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(annex_b.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), ptr, annex_b.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(annex_b.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.timestamp)?;
            sample.SetSampleDuration(333_333)?;
            self.timestamp += 333_333;
            match self.transform.ProcessInput(0, &sample, 0) {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // Output is backed up: take it, then feed the frame again.
                    self.drain(out)?;
                    self.transform.ProcessInput(0, &sample, 0).context("ProcessInput")?;
                }
                Err(e) => return Err(e).context("ProcessInput"),
            }
        }
        self.drain(out)
    }
}

impl Drop for MfH264Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
        }
    }
}
