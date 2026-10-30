//! H.264 decoding for the WebRTC stream, with Cisco's OpenH264 (built from source).

use openh264::formats::YUVSource;

use crate::hub::Yuv420;

pub struct H264Decoder(openh264::decoder::Decoder);

impl H264Decoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self(openh264::decoder::Decoder::new()?))
    }

    /// Decode one Annex B access unit and hand the picture to `on_picture`, if it
    /// completed one.
    pub fn decode(&mut self, annex_b: &[u8], on_picture: impl FnOnce(&Yuv420)) -> anyhow::Result<()> {
        let Some(yuv) = self.0.decode(annex_b)? else { return Ok(()) };
        let (width, height) = yuv.dimensions();
        let (y_stride, uv_stride, _) = yuv.strides();
        on_picture(&Yuv420 {
            width: width as u32,
            height: height as u32,
            y: yuv.y(),
            u: yuv.u(),
            v: yuv.v(),
            y_stride,
            uv_stride,
        });
        Ok(())
    }
}
