//! Solid-colour test pictures and colour comparisons.

/// An RGB colour, one byte per channel.
pub type Rgb = [u8; 3];

const JPEG_ENCODE_QUALITY: u8 = 95;

/// A `width` x `height` JPEG filled with `color`.
pub fn solid_jpeg(width: u32, height: u32, color: Rgb) -> Vec<u8> {
    let pixels = color.repeat((width * height) as usize);
    let mut jpeg = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut jpeg, JPEG_ENCODE_QUALITY);
    encoder.encode(&pixels, width as u16, height as u16, jpeg_encoder::ColorType::Rgb).unwrap();
    jpeg
}

/// Average colour of packed RGB pixels.
pub fn average_rgb(pixels: &[u8]) -> [f64; 3] {
    let mut sum = [0f64; 3];
    for pixel in pixels.chunks_exact(3) {
        for (channel_sum, value) in sum.iter_mut().zip(pixel) {
            *channel_sum += *value as f64;
        }
    }
    let pixel_count = (pixels.len() / 3) as f64;
    sum.map(|channel_sum| channel_sum / pixel_count)
}

/// Whether every channel of `average` is within `tolerance` of `expected`.
pub fn is_close(average: [f64; 3], expected: Rgb, tolerance: f64) -> bool {
    average.iter().zip(expected).all(|(got, want)| (got - want as f64).abs() <= tolerance)
}
