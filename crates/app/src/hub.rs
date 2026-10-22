//! The latest frame from the phone, scaled to the virtual camera's size.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ab_glyph::{Font, FontRef, OutlinedGlyph, PxScale, ScaleFont};

/// After this long without a frame from the phone, show the placeholder again.
pub const STALE_AFTER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum FitMode {
    /// Letterbox: keep the whole picture, add black bars.
    Fit,
    /// Crop the picture so it fills the frame.
    Fill,
}

/// A tightly packed RGB24 image.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Frame {
    pub fn black(width: u32, height: u32) -> Self {
        Self { width, height, rgb: vec![0; (width * height * 3) as usize] }
    }
}

/// A decoded I420 picture: a full-size Y plane and half-size U and V planes.
pub struct Yuv420<'a> {
    pub width: u32,
    pub height: u32,
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
    pub y_stride: usize,
    pub uv_stride: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub frames_in: u64,
    pub frames_out: u64,
    pub decode_errors: u64,
    pub source_size: (u32, u32),
    pub transport: &'static str,
}

struct State {
    frame: Option<Arc<Frame>>,
    last_frame: Option<Instant>,
    placeholder: Arc<Frame>,
    stats: Stats,
}

/// Thread-safe holder of the latest frame.
pub struct FrameHub {
    pub width: u32,
    pub height: u32,
    mode: FitMode,
    state: Mutex<State>,
}

impl FrameHub {
    pub fn new(width: u32, height: u32, mode: FitMode) -> Self {
        let placeholder = Arc::new(placeholder(width, height, &["Waiting for iPhone..."]));
        Self {
            width,
            height,
            mode,
            state: Mutex::new(State { frame: None, last_frame: None, placeholder, stats: Stats::default() }),
        }
    }

    pub fn set_placeholder_text(&self, lines: &[&str]) {
        let frame = Arc::new(placeholder(self.width, self.height, lines));
        self.state.lock().unwrap().placeholder = frame;
    }

    fn store(&self, frame: Frame, source: (u32, u32), transport: &'static str) {
        let mut s = self.state.lock().unwrap();
        s.frame = Some(Arc::new(frame));
        s.last_frame = Some(Instant::now());
        s.stats.frames_in += 1;
        s.stats.source_size = source;
        s.stats.transport = transport;
    }

    /// Decode a JPEG from the fallback transports. Returns false if it isn't one.
    pub fn push_jpeg(&self, data: &[u8]) -> bool {
        match decode_jpeg(data, self.width, self.height, self.mode) {
            Ok((frame, source)) => {
                self.store(frame, source, "JPEG");
                true
            }
            Err(err) => {
                tracing::debug!("dropping undecodable frame: {err}");
                self.count_decode_error();
                false
            }
        }
    }

    /// Store a decoded WebRTC picture.
    pub fn push_yuv(&self, picture: &Yuv420) {
        let frame = convert_yuv(picture, self.width, self.height, self.mode);
        self.store(frame, (picture.width, picture.height), "WebRTC");
    }

    pub fn count_decode_error(&self) {
        self.state.lock().unwrap().stats.decode_errors += 1;
    }

    pub fn count_output_frame(&self) {
        self.state.lock().unwrap().stats.frames_out += 1;
    }

    /// The frame to show now: the phone's, or the placeholder if it went quiet.
    pub fn latest(&self) -> Arc<Frame> {
        let s = self.state.lock().unwrap();
        match (&s.frame, s.last_frame) {
            (Some(frame), Some(at)) if at.elapsed() < STALE_AFTER => frame.clone(),
            _ => s.placeholder.clone(),
        }
    }

    pub fn streaming(&self) -> bool {
        self.state.lock().unwrap().last_frame.is_some_and(|at| at.elapsed() < STALE_AFTER)
    }

    pub fn stats(&self) -> Stats {
        self.state.lock().unwrap().stats.clone()
    }
}

/// Size to scale a `sw x sh` picture to so it fits (or fills) `w x h`.
fn scaled_size(sw: u32, sh: u32, w: u32, h: u32, mode: FitMode) -> (u32, u32) {
    let (sx, sy) = (w as f64 / sw as f64, h as f64 / sh as f64);
    let scale = match mode {
        FitMode::Fit => sx.min(sy),
        FitMode::Fill => sx.max(sy),
    };
    (((sw as f64 * scale).round() as u32).max(1), ((sh as f64 * scale).round() as u32).max(1))
}

/// Fill a `w x h` RGB frame by sampling a `sw x sh` source, scaled and centred per
/// `mode` (nearest neighbour). `sample(x, y)` returns the source pixel.
fn resample(sw: u32, sh: u32, w: u32, h: u32, mode: FitMode, sample: impl Fn(u32, u32) -> [u8; 3]) -> Frame {
    let (tw, th) = scaled_size(sw, sh, w, h, mode);
    // Offset of the scaled picture inside the output; negative when cropping.
    let ox = (w as i64 - tw as i64) / 2;
    let oy = (h as i64 - th as i64) / 2;
    let map = |pos: u32, offset: i64, scaled: u32, src: u32| -> Option<u32> {
        let p = pos as i64 - offset;
        (0..scaled as i64).contains(&p).then(|| ((p as u64 * src as u64) / scaled as u64) as u32)
    };
    let xmap: Vec<Option<u32>> = (0..w).map(|x| map(x, ox, tw, sw)).collect();
    let mut frame = Frame::black(w, h);
    for (y, row) in frame.rgb.chunks_exact_mut(w as usize * 3).enumerate() {
        let Some(sy) = map(y as u32, oy, th, sh) else { continue };
        for (px, sx) in row.chunks_exact_mut(3).zip(&xmap) {
            if let Some(sx) = sx {
                px.copy_from_slice(&sample(*sx, sy));
            }
        }
    }
    frame
}

fn decode_jpeg(data: &[u8], w: u32, h: u32, mode: FitMode) -> anyhow::Result<(Frame, (u32, u32))> {
    use zune_jpeg::zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), options);
    let pixels = decoder.decode()?;
    let (sw, sh) = decoder.dimensions().ok_or_else(|| anyhow::anyhow!("no dimensions"))?;
    let (sw, sh) = (sw as u32, sh as u32);
    anyhow::ensure!(pixels.len() == (sw * sh * 3) as usize, "unexpected pixel buffer size");
    let sample = |x: u32, y: u32| {
        let i = ((y * sw + x) * 3) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    let frame = resample(sw, sh, w, h, mode, sample);
    Ok((frame, (sw, sh)))
}

/// BT.601 limited range YUV -> RGB, as produced by the phone's encoder.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let c = (y as i32 - 16) * 298;
    let d = u as i32 - 128;
    let e = v as i32 - 128;
    let clamp = |x: i32| ((x + 128) >> 8).clamp(0, 255) as u8;
    [clamp(c + 409 * e), clamp(c - 100 * d - 208 * e), clamp(c + 516 * d)]
}

fn convert_yuv(p: &Yuv420, w: u32, h: u32, mode: FitMode) -> Frame {
    let sample = |x: u32, y: u32| {
        let luma = p.y[y as usize * p.y_stride + x as usize];
        let chroma = (y / 2) as usize * p.uv_stride + (x / 2) as usize;
        yuv_to_rgb(luma, p.u[chroma], p.v[chroma])
    };
    resample(p.width, p.height, w, h, mode, sample)
}

/// A dark frame with centred status text, shown while no phone is streaming.
pub fn placeholder(w: u32, h: u32, lines: &[&str]) -> Frame {
    let mut frame = Frame { width: w, height: h, rgb: [24u8, 26, 32].repeat((w * h) as usize) };
    let font = FontRef::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font");
    let scale = PxScale::from((h as f32 / 18.0).max(14.0));
    let scaled = font.as_scaled(scale);
    let line_height = scaled.height() * 1.4;
    let total = line_height * lines.len() as f32;
    let mut baseline = (h as f32 - total) / 2.0 + scaled.ascent();
    for line in lines {
        let width: f32 = line.chars().map(|c| scaled.h_advance(font.glyph_id(c))).sum();
        let mut x = (w as f32 - width) / 2.0;
        for c in line.chars() {
            let id = font.glyph_id(c);
            let glyph = id.with_scale_and_position(scale, ab_glyph::point(x, baseline));
            x += scaled.h_advance(id);
            if let Some(outlined) = font.outline_glyph(glyph) {
                draw_glyph(&mut frame, &outlined);
            }
        }
        baseline += line_height;
    }
    frame
}

/// Blend a light glyph into `frame`, clipped to its bounds.
fn draw_glyph(frame: &mut Frame, outlined: &OutlinedGlyph) {
    let bounds = outlined.px_bounds();
    let (w, h) = (frame.width, frame.height);
    let blend = |gx: u32, gy: u32, coverage: f32| {
        let (px, py) = (bounds.min.x as i64 + gx as i64, bounds.min.y as i64 + gy as i64);
        if px < 0 || py < 0 || px >= w as i64 || py >= h as i64 {
            return;
        }
        let i = ((py as u32 * w + px as u32) * 3) as usize;
        for (k, target) in [220u8, 220, 225].iter().enumerate() {
            let bg = frame.rgb[i + k] as f32;
            frame.rgb[i + k] = (bg + (*target as f32 - bg) * coverage) as u8;
        }
    };
    outlined.draw(blend);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_jpeg(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut out = Vec::new();
        let pixels = rgb.repeat((w * h) as usize);
        jpeg_encoder::Encoder::new(&mut out, 95)
            .encode(&pixels, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        out
    }

    fn px(f: &Frame, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * f.width + x) * 3) as usize;
        [f.rgb[i], f.rgb[i + 1], f.rgb[i + 2]]
    }

    #[test]
    fn portrait_is_letterboxed_or_cropped() {
        let jpeg = solid_jpeg(720, 1280, [200, 30, 30]);
        let (fit, src) = decode_jpeg(&jpeg, 1280, 720, FitMode::Fit).unwrap();
        assert_eq!(src, (720, 1280));
        assert_eq!(px(&fit, 0, 360), [0, 0, 0]);
        assert!(px(&fit, 640, 360)[0] > 180);
        let (fill, _) = decode_jpeg(&jpeg, 1280, 720, FitMode::Fill).unwrap();
        assert!(px(&fill, 0, 360)[0] > 180);
        assert_eq!(fill.rgb.len(), 1280 * 720 * 3);
    }

    #[test]
    fn small_frames_are_upscaled_and_garbage_rejected() {
        let hub = FrameHub::new(1280, 720, FitMode::Fit);
        let placeholder = hub.latest();
        assert!(!hub.push_jpeg(b"not a jpeg"));
        assert_eq!(hub.stats().decode_errors, 1);
        assert!(Arc::ptr_eq(&hub.latest(), &placeholder));
        assert!(hub.push_jpeg(&solid_jpeg(640, 360, [10, 200, 10])));
        assert!(hub.streaming());
        let frame = hub.latest();
        assert_eq!((frame.width, frame.height), (1280, 720));
        assert!(px(&frame, 1279, 719)[1] > 180);
    }

    #[test]
    fn yuv_conversion_matches_bt601() {
        // Y/U/V for RGB (220, 120, 40) in BT.601 limited range.
        let (w, h) = (4u32, 2u32);
        let y = vec![137u8; 8];
        let u = vec![78u8; 2];
        let v = vec![177u8; 2];
        let p = Yuv420 { width: w, height: h, y: &y, u: &u, v: &v, y_stride: 4, uv_stride: 2 };
        let f = convert_yuv(&p, 4, 2, FitMode::Fit);
        let got = px(&f, 1, 1);
        for (g, want) in got.iter().zip([220u8, 120, 40]) {
            assert!((*g as i32 - want as i32).abs() <= 3, "{got:?}");
        }
    }

    #[test]
    fn placeholder_draws_text() {
        let f = placeholder(640, 360, &["Waiting for iPhone..."]);
        assert!(f.rgb.chunks(3).any(|p| p[0] > 150), "some text pixels");
    }
}
