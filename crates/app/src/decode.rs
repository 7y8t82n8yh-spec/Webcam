//! H.264 decoding for the WebRTC stream.
//!
//! On Windows the decoder built into Media Foundation is used: it's hardware
//! accelerated, ships with Windows, and Microsoft holds the patent license. The
//! `openh264` feature adds Cisco's OpenH264 (built from source) for development and
//! tests on other systems.

use crate::hub::Yuv420;

pub trait VideoDecoder: Send {
    /// Decode one Annex B access unit; `out` is called for every picture produced.
    fn decode(&mut self, annex_b: &[u8], out: &mut dyn FnMut(&Yuv420)) -> anyhow::Result<()>;
}

/// The best decoder available on this system, if any.
pub fn new_decoder() -> anyhow::Result<Box<dyn VideoDecoder>> {
    #[cfg(windows)]
    {
        return Ok(Box::new(crate::mf_decode::MfH264Decoder::new()?));
    }
    #[cfg(all(not(windows), feature = "openh264"))]
    {
        return Ok(Box::new(OpenH264::new()?));
    }
    #[allow(unreachable_code)]
    Err(anyhow::anyhow!("no H.264 decoder in this build (build with --features openh264)"))
}

/// Whether [`new_decoder`] can succeed, so WebRTC isn't offered when it can't.
pub fn available() -> bool {
    cfg!(any(windows, feature = "openh264"))
}

#[cfg(feature = "openh264")]
pub struct OpenH264(openh264::decoder::Decoder);

#[cfg(feature = "openh264")]
impl OpenH264 {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(openh264::decoder::Decoder::new()?))
    }
}

#[cfg(feature = "openh264")]
impl VideoDecoder for OpenH264 {
    fn decode(&mut self, annex_b: &[u8], out: &mut dyn FnMut(&Yuv420)) -> anyhow::Result<()> {
        use openh264::formats::YUVSource;
        if let Some(yuv) = self.0.decode(annex_b)? {
            let (w, h) = yuv.dimensions();
            let (ys, us, _) = yuv.strides();
            out(&Yuv420 {
                width: w as u32,
                height: h as u32,
                y: yuv.y(),
                y_stride: ys,
                u: yuv.u(),
                v: yuv.v(),
                uv_stride: us,
                nv12: false,
            });
        }
        Ok(())
    }
}
