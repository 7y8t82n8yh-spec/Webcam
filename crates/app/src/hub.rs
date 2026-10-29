//! The latest frame from the phone, scaled to the virtual camera's size.

use std::sync::atomic::{AtomicU8, Ordering};
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

/// How far to turn the phone's picture clockwise, in quarter turns (0 to 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rotation(u8);

impl Rotation {
    pub fn degrees(self) -> u32 {
        self.0 as u32 * 90
    }

    /// A quarter turn further clockwise (`quarters` = 1) or anticlockwise (-1).
    pub fn turned(self, quarters: i8) -> Self {
        Rotation((self.0 as i8 + quarters).rem_euclid(4) as u8)
    }

    /// Size of a `width x height` picture after turning it.
    pub fn size(self, width: u32, height: u32) -> (u32, u32) {
        if self.0 % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// Where pixel (x, y) of the turned picture comes from in the `width x height` original.
    fn source(self, x: u32, y: u32, width: u32, height: u32) -> (u32, u32) {
        match self.0 {
            1 => (y, height - 1 - x),
            2 => (width - 1 - x, height - 1 - y),
            3 => (width - 1 - y, x),
            _ => (x, y),
        }
    }
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
    /// The phone's latest frame and when it arrived.
    latest: Option<(Arc<Frame>, Instant)>,
    stats: Stats,
}

/// Thread-safe holder of the latest frame.
pub struct FrameHub {
    /// The camera's size with the picture upright (rotation 0).
    width: u32,
    height: u32,
    mode: FitMode,
    /// Shown while no phone streams: upright, and for a quarter turn.
    placeholders: [Arc<Frame>; 2],
    /// Quarter turns, changed from the admin page while streaming.
    rotation: AtomicU8,
    state: Mutex<State>,
}

impl FrameHub {
    /// `placeholder_text` is shown while no phone is streaming.
    pub fn new(width: u32, height: u32, mode: FitMode, placeholder_text: &[&str]) -> Self {
        let placeholders =
            [placeholder(width, height, placeholder_text), placeholder(height, width, placeholder_text)].map(Arc::new);
        let state = Mutex::new(State { latest: None, stats: Stats::default() });
        Self { width, height, mode, placeholders, rotation: AtomicU8::new(0), state }
    }

    /// The camera's size now: turned with the picture, so a quarter turn gives a
    /// portrait camera.
    pub fn output_size(&self) -> (u32, u32) {
        self.rotation().size(self.width, self.height)
    }

    pub fn rotation(&self) -> Rotation {
        Rotation(self.rotation.load(Ordering::Relaxed))
    }

    /// Turn the phone's picture from the next frame on.
    pub fn set_rotation(&self, rotation: Rotation) {
        self.rotation.store(rotation.0, Ordering::Relaxed);
    }

    fn output(&self) -> Output {
        let (width, height) = self.output_size();
        Output { width, height, mode: self.mode, rotation: self.rotation() }
    }

    fn store(&self, frame: Frame, source: (u32, u32), transport: &'static str) {
        let mut s = self.state.lock().unwrap();
        s.latest = Some((Arc::new(frame), Instant::now()));
        s.stats.frames_in += 1;
        s.stats.source_size = source;
        s.stats.transport = transport;
    }

    /// Decode a JPEG from the fallback transports. Returns false if it isn't one.
    pub fn push_jpeg(&self, data: &[u8]) -> bool {
        match decode_jpeg(data, self.output()) {
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
        let frame = convert_yuv(picture, self.output());
        self.store(frame, (picture.width, picture.height), "WebRTC");
    }

    pub fn count_decode_error(&self) {
        self.state.lock().unwrap().stats.decode_errors += 1;
    }

    pub fn count_output_frame(&self) {
        self.state.lock().unwrap().stats.frames_out += 1;
    }

    /// The frame to show now: the phone's, or the placeholder if it went quiet. Its
    /// size is normally `output_size()`, but a frame from before a rotation can differ.
    pub fn latest(&self) -> Arc<Frame> {
        let s = self.state.lock().unwrap();
        match &s.latest {
            Some((frame, at)) if at.elapsed() < STALE_AFTER => frame.clone(),
            _ => self.placeholders[self.rotation().0 as usize % 2].clone(),
        }
    }

    pub fn streaming(&self) -> bool {
        self.state.lock().unwrap().latest.as_ref().is_some_and(|(_, at)| at.elapsed() < STALE_AFTER)
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

/// The frame the phone's pictures are turned into.
#[derive(Clone, Copy)]
struct Output {
    width: u32,
    height: u32,
    mode: FitMode,
    rotation: Rotation,
}

/// Make the output frame from a `source_width x source_height` picture: turned, then
/// scaled and centred per the mode (nearest neighbour). `sample(x, y)` returns a
/// pixel of the picture.
fn resample((source_width, source_height): (u32, u32), output: Output, sample: impl Fn(u32, u32) -> [u8; 3]) -> Frame {
    let Output { width: w, height: h, mode, rotation } = output;
    let (sw, sh) = rotation.size(source_width, source_height);
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
                let (x, y) = rotation.source(*sx, sy, source_width, source_height);
                px.copy_from_slice(&sample(x, y));
            }
        }
    }
    frame
}

/// `frame` scaled to fit `width x height`, with black bars: for a camera that can't
/// change its size to the frame's.
pub fn letterbox(frame: &Frame, width: u32, height: u32) -> Frame {
    let output = Output { width, height, mode: FitMode::Fit, rotation: Rotation::default() };
    let sample = |x: u32, y: u32| {
        let i = ((y * frame.width + x) * 3) as usize;
        [frame.rgb[i], frame.rgb[i + 1], frame.rgb[i + 2]]
    };
    resample((frame.width, frame.height), output, sample)
}

fn decode_jpeg(data: &[u8], output: Output) -> anyhow::Result<(Frame, (u32, u32))> {
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
    let frame = resample((sw, sh), output, sample);
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

fn convert_yuv(p: &Yuv420, output: Output) -> Frame {
    let sample = |x: u32, y: u32| {
        let luma = p.y[y as usize * p.y_stride + x as usize];
        let chroma = (y / 2) as usize * p.uv_stride + (x / 2) as usize;
        yuv_to_rgb(luma, p.u[chroma], p.v[chroma])
    };
    resample((p.width, p.height), output, sample)
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

    fn output(width: u32, height: u32, mode: FitMode) -> Output {
        Output { width, height, mode, rotation: Rotation::default() }
    }

    #[test]
    fn portrait_is_letterboxed_or_cropped() {
        let jpeg = solid_jpeg(720, 1280, [200, 30, 30]);
        let (fit, src) = decode_jpeg(&jpeg, output(1280, 720, FitMode::Fit)).unwrap();
        assert_eq!(src, (720, 1280));
        assert_eq!(px(&fit, 0, 360), [0, 0, 0]);
        assert!(px(&fit, 640, 360)[0] > 180);
        let (fill, _) = decode_jpeg(&jpeg, output(1280, 720, FitMode::Fill)).unwrap();
        assert!(px(&fill, 0, 360)[0] > 180);
        assert_eq!(fill.rgb.len(), 1280 * 720 * 3);
    }

    #[test]
    fn small_frames_are_upscaled_and_garbage_rejected() {
        let hub = FrameHub::new(1280, 720, FitMode::Fit, &["Waiting for iPhone..."]);
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
        let f = convert_yuv(&p, output(4, 2, FitMode::Fit));
        let got = px(&f, 1, 1);
        for (g, want) in got.iter().zip([220u8, 120, 40]) {
            assert!((*g as i32 - want as i32).abs() <= 3, "{got:?}");
        }
    }

    #[test]
    fn rotation_turns_the_picture() {
        // A 2x1 picture: red on the left, blue on the right.
        let colors = [[255, 0, 0], [0, 0, 255]];
        let sample = |x: u32, _y: u32| colors[x as usize];
        let turned = |quarters| {
            let output = Output { width: 2, height: 2, mode: FitMode::Fill, rotation: Rotation(quarters) };
            resample((2, 1), output, sample)
        };
        // A quarter turn clockwise puts red at the top.
        assert_eq!(px(&turned(1), 0, 0), colors[0]);
        assert_eq!(px(&turned(1), 0, 1), colors[1]);
        assert_eq!(px(&turned(2), 0, 0), colors[1]);
        assert_eq!(px(&turned(3), 0, 0), colors[1]);
        assert_eq!(Rotation(0).turned(-1), Rotation(3));
        assert_eq!(Rotation(1).size(1280, 720), (720, 1280));
        assert_eq!(Rotation(3).turned(1).degrees(), 0);
    }

    #[test]
    fn a_quarter_turn_makes_the_camera_portrait() {
        let hub = FrameHub::new(1280, 720, FitMode::Fit, &["Waiting"]);
        hub.set_rotation(Rotation(1));
        assert_eq!(hub.output_size(), (720, 1280));
        let placeholder = hub.latest();
        assert_eq!((placeholder.width, placeholder.height), (720, 1280));
        assert!(hub.push_jpeg(&solid_jpeg(1280, 720, [10, 200, 10])));
        let frame = hub.latest();
        assert_eq!((frame.width, frame.height), (720, 1280));
        // A landscape picture turned upright fills the portrait camera.
        assert!(px(&frame, 0, 0)[1] > 180 && px(&frame, 719, 1279)[1] > 180);
    }

    #[test]
    fn letterbox_keeps_the_aspect() {
        let frame = Frame { width: 2, height: 4, rgb: [255u8, 255, 255].repeat(8) };
        let boxed = letterbox(&frame, 8, 4);
        assert_eq!(px(&boxed, 0, 0), [0, 0, 0]);
        assert_eq!(px(&boxed, 4, 2), [255, 255, 255]);
    }

    #[test]
    fn placeholder_draws_text() {
        let f = placeholder(640, 360, &["Waiting for iPhone..."]);
        assert!(f.rgb.chunks(3).any(|p| p[0] > 150), "some text pixels");
    }
}
