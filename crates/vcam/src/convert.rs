//! RGB24 frames from the app -> the pixel formats the camera offers.

/// Nearest-neighbour scale `rgb` (`sw x sh`, tightly packed) into `dst` (`w x h`) as
/// NV12 (BT.601 limited range, matching the media type's attributes) or RGB32 (BGRX,
/// top-down). A picture of another shape is letterboxed: the app's frames turn
/// portrait when the picture is rotated, while the video app keeps the size it chose.
pub fn convert(rgb: &[u8], sw: u32, sh: u32, dst: &mut [u8], w: u32, h: u32, nv12: bool) {
    let (ox, oy, tw, th) = fit(sw, sh, w, h);
    if (tw, th) != (w as usize, h as usize) {
        fill_black(dst, w, h, nv12);
    }
    let xmap: Vec<usize> = (0..tw).map(|x| (x as u64 * sw as u64 / tw as u64) as usize * 3).collect();
    let row_of = |y: usize| {
        let sy = (y as u64 * sh as u64 / th as u64) as usize;
        &rgb[sy * sw as usize * 3..(sy + 1) * sw as usize * 3]
    };
    let (w, h) = (w as usize, h as usize);
    if !nv12 {
        for (y, out) in dst.chunks_exact_mut(w * 4).take(h).skip(oy).take(th).enumerate() {
            let row = row_of(y);
            for (px, &sx) in out[ox * 4..].chunks_exact_mut(4).zip(&xmap) {
                px.copy_from_slice(&[row[sx + 2], row[sx + 1], row[sx], 255]);
            }
        }
        return;
    }
    let (luma, chroma) = dst.split_at_mut(w * h);
    for y in 0..th {
        let row = row_of(y);
        let out_y = oy + y;
        let out = &mut luma[out_y * w + ox..out_y * w + ox + tw];
        for (o, &sx) in out.iter_mut().zip(&xmap) {
            let (r, g, b) = (row[sx] as i32, row[sx + 1] as i32, row[sx + 2] as i32);
            *o = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
        if out_y % 2 == 0 {
            let uv = &mut chroma[(out_y / 2) * w + ox..(out_y / 2) * w + ox + tw];
            for x in (0..tw.saturating_sub(1)).step_by(2) {
                let sx = xmap[x];
                let (r, g, b) = (row[sx] as i32, row[sx + 1] as i32, row[sx + 2] as i32);
                uv[x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                uv[x + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
    }
}

/// Where a `sw x sh` picture goes in a `w x h` frame, keeping its shape: (x, y, width,
/// height), all even so NV12's half-size colour planes line up.
fn fit(sw: u32, sh: u32, w: u32, h: u32) -> (usize, usize, usize, usize) {
    let scale = (w as f64 / sw as f64).min(h as f64 / sh as f64);
    let even = |size: f64, max: u32| ((size.round() as u32 & !1).clamp(2, max)) as usize;
    let (tw, th) = (even(sw as f64 * scale, w), even(sh as f64 * scale, h));
    (((w as usize - tw) / 2) & !1, ((h as usize - th) / 2) & !1, tw, th)
}

pub fn fill_black(dst: &mut [u8], w: u32, h: u32, nv12: bool) {
    let pixels = (w * h) as usize;
    if nv12 {
        dst[..pixels].fill(16);
        dst[pixels..pixels * 3 / 2].fill(128);
    } else {
        for px in dst.chunks_exact_mut(4).take(pixels) {
            px.copy_from_slice(&[0, 0, 0, 255]);
        }
    }
}

pub fn frame_bytes(w: u32, h: u32, nv12: bool) -> usize {
    if nv12 {
        (w * h * 3 / 2) as usize
    } else {
        (w * h * 4) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_of_a_solid_colour() {
        let rgb = [220u8, 120, 40].repeat(8 * 4);
        let mut out = vec![0u8; frame_bytes(16, 8, true)];
        convert(&rgb, 8, 4, &mut out, 16, 8, true);
        assert!(out[..128].iter().all(|&y| y == 137), "{:?}", &out[..4]);
        assert_eq!(&out[128..130], &[78, 177]);
    }

    #[test]
    fn rgb32_is_bgrx_and_scaled() {
        let mut rgb = [10u8, 20, 30].repeat(2 * 2);
        rgb[3..6].copy_from_slice(&[200, 100, 50]); // top-right pixel
        let mut out = vec![0u8; frame_bytes(4, 4, false)];
        convert(&rgb, 2, 2, &mut out, 4, 4, false);
        assert_eq!(&out[0..4], &[30, 20, 10, 255]);
        assert_eq!(&out[12..16], &[50, 100, 200, 255], "top-right maps to top-right");
    }

    #[test]
    fn other_shapes_are_letterboxed() {
        // A white portrait picture in a landscape frame: black bars left and right.
        let rgb = [255u8, 255, 255].repeat(4 * 8);
        let mut out = vec![0u8; frame_bytes(16, 8, true)];
        convert(&rgb, 4, 8, &mut out, 16, 8, true);
        assert_eq!(&out[0..6], &[16, 16, 16, 16, 16, 16]);
        assert_eq!(&out[6..10], &[235, 235, 235, 235]);
        assert_eq!(out[10], 16);
        let mut out = vec![0u8; frame_bytes(16, 8, false)];
        convert(&rgb, 4, 8, &mut out, 16, 8, false);
        assert_eq!(&out[0..4], &[0, 0, 0, 255]);
        assert_eq!(&out[24..28], &[255, 255, 255, 255]);
    }

    #[test]
    fn black_frames() {
        let mut out = vec![1u8; frame_bytes(4, 2, true)];
        fill_black(&mut out, 4, 2, true);
        assert_eq!(out, [16, 16, 16, 16, 16, 16, 16, 16, 128, 128, 128, 128]);
    }
}
