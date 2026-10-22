//! RGB24 frames from the app -> the pixel formats the camera offers.

/// Nearest-neighbour scale `rgb` (`sw x sh`, tightly packed) into `dst` as NV12
/// (BT.601 limited range, matching the media type's attributes) or RGB32 (BGRX,
/// top-down).
pub fn convert(rgb: &[u8], sw: u32, sh: u32, dst: &mut [u8], w: u32, h: u32, nv12: bool) {
    let xmap: Vec<usize> = (0..w).map(|x| (x as u64 * sw as u64 / w as u64) as usize * 3).collect();
    let row_of = |y: u32| {
        let sy = (y as u64 * sh as u64 / h as u64) as usize;
        &rgb[sy * sw as usize * 3..(sy + 1) * sw as usize * 3]
    };
    let (w, h) = (w as usize, h as usize);
    if !nv12 {
        for (y, out) in dst.chunks_exact_mut(w * 4).take(h).enumerate() {
            let row = row_of(y as u32);
            for (px, &sx) in out.chunks_exact_mut(4).zip(&xmap) {
                px.copy_from_slice(&[row[sx + 2], row[sx + 1], row[sx], 255]);
            }
        }
        return;
    }
    let (luma, chroma) = dst.split_at_mut(w * h);
    for y in 0..h {
        let row = row_of(y as u32);
        let out = &mut luma[y * w..(y + 1) * w];
        for (o, &sx) in out.iter_mut().zip(&xmap) {
            let (r, g, b) = (row[sx] as i32, row[sx + 1] as i32, row[sx + 2] as i32);
            *o = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        }
        if y % 2 == 0 {
            let uv = &mut chroma[(y / 2) * w..(y / 2 + 1) * w];
            for x in (0..w.saturating_sub(1)).step_by(2) {
                let sx = xmap[x];
                let (r, g, b) = (row[sx] as i32, row[sx + 1] as i32, row[sx + 2] as i32);
                uv[x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                uv[x + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        }
    }
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
    fn black_frames() {
        let mut out = vec![1u8; frame_bytes(4, 2, true)];
        fill_black(&mut out, 4, 2, true);
        assert_eq!(out, [16, 16, 16, 16, 16, 16, 16, 16, 128, 128, 128, 128]);
    }
}
