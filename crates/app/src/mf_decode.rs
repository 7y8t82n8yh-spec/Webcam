//! H.264 decoding with the decoder built into Windows (Media Foundation transform).

use std::mem::ManuallyDrop;

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

use crate::decode::VideoDecoder;
use crate::hub::Yuv420;

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
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).context("MFStartup")?;
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
                if candidate.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
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
        }
        anyhow::bail!("decoder offers no NV12 output")
    }

    /// Pull every decoded picture out of the transform.
    fn drain(&mut self, out: &mut dyn FnMut(&Yuv420)) -> Result<()> {
        loop {
            unsafe {
                let info = self.transform.GetOutputStreamInfo(0)?;
                let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                let sample = if provides {
                    None
                } else {
                    let sample = MFCreateSample()?;
                    sample.AddBuffer(&MFCreateMemoryBuffer(info.cbSize)?)?;
                    Some(sample)
                };
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
                match result {
                    Ok(()) => {
                        if let Some(sample) = sample {
                            self.emit(&sample, out)?;
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.choose_nv12_output()?,
                    Err(e) => return Err(e).context("ProcessOutput"),
                }
            }
        }
    }

    unsafe fn emit(&self, sample: &IMFSample, out: &mut dyn FnMut(&Yuv420)) -> Result<()> {
        let buffer = sample.GetBufferByIndex(0)?;
        let (w, h) = self.visible;
        let (_, alloc_h) = self.allocated;
        // Prefer the 2D view, which knows the real row pitch.
        if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer>() {
            let mut scanline0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            buffer2d.Lock2D(&mut scanline0, &mut pitch)?;
            let pitch = pitch.unsigned_abs() as usize;
            let len = pitch * alloc_h as usize * 3 / 2;
            let data = std::slice::from_raw_parts(scanline0, len);
            let (y, uv) = data.split_at(pitch * alloc_h as usize);
            out(&Yuv420 { width: w, height: h, y, y_stride: pitch, u: uv, v: &[], uv_stride: pitch, nv12: true });
            buffer2d.Unlock2D()?;
        } else {
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize);
            let pitch = self.allocated.0 as usize;
            let luma = pitch * alloc_h as usize;
            if data.len() >= luma * 3 / 2 {
                let (y, uv) = data.split_at(luma);
                out(&Yuv420 { width: w, height: h, y, y_stride: pitch, u: uv, v: &[], uv_stride: pitch, nv12: true });
            }
            buffer.Unlock()?;
        }
        Ok(())
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
            let _ = MFShutdown();
        }
    }
}
