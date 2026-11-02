use crate::{DecodedFrame, Error, Result, ffi, validate_dimensions};

mod sse2;

struct Plane<'a> {
    data: &'a [u8],
    width: usize,
    height: usize,
    stride: usize,
}

fn plane(image: &ffi::Image, channel: i32) -> Result<Plane<'_>> {
    let width = unsafe { ffi::de265_get_image_width(image, channel) };
    let height = unsafe { ffi::de265_get_image_height(image, channel) };
    if width <= 0 || height <= 0 {
        return Err(Error::InvalidInput("native plane has invalid dimensions"));
    }
    validate_dimensions(width as u32, height as u32)?;
    let mut stride = 0;
    let data = unsafe { ffi::de265_get_image_plane(image, channel, &mut stride) };
    if data.is_null() || stride < width {
        return Err(Error::InvalidInput(
            "native plane is missing or has an invalid stride",
        ));
    }
    let (width, height, stride) = (width as usize, height as usize, stride as usize);
    let length = (height - 1)
        .checked_mul(stride)
        .and_then(|bytes| bytes.checked_add(width))
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or(Error::InvalidInput("native plane byte length overflows"))?;
    // The native allocator owns each validated visible row, including when a
    // conformance window offsets the first pixel within the allocation.
    let data = unsafe { std::slice::from_raw_parts(data, length) };
    Ok(Plane {
        data,
        width,
        height,
        stride,
    })
}

pub fn copy_frame(image: &ffi::Image) -> Result<DecodedFrame> {
    let chroma = unsafe { ffi::de265_get_chroma_format(image) };
    let shift = match chroma {
        1 => 1,
        3 => 0,
        _ => return Err(Error::UnsupportedChroma(chroma as u32)),
    };
    let luma_depth = unsafe { ffi::de265_get_bits_per_pixel(image, 0) } as u32;
    let cb_depth = unsafe { ffi::de265_get_bits_per_pixel(image, 1) } as u32;
    let cr_depth = unsafe { ffi::de265_get_bits_per_pixel(image, 2) } as u32;
    if luma_depth != 8 || cb_depth != 8 || cr_depth != 8 {
        return Err(Error::UnsupportedBitDepth {
            luma: luma_depth,
            chroma: cb_depth.max(cr_depth),
        });
    }
    let y = plane(image, 0)?;
    let cb = plane(image, 1)?;
    let cr = plane(image, 2)?;
    let chroma_width = y.width.div_ceil(1 << shift);
    let chroma_height = y.height.div_ceil(1 << shift);
    if cb.width != chroma_width
        || cr.width != chroma_width
        || cb.height != chroma_height
        || cr.height != chroma_height
    {
        return Err(Error::InvalidInput(
            "native chroma dimensions do not match luma",
        ));
    }
    let full_range = unsafe { ffi::de265_get_image_full_range_flag(image) } != 0;
    let matrix = Matrix::new(full_range);
    let bytes = y.width * y.height * 4;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(bytes)
        .map_err(|_| Error::AllocationFailed)?;
    rgba.resize(bytes, 0);
    for (row, output) in rgba.chunks_exact_mut(y.width * 4).enumerate() {
        let y_start = row * y.stride;
        let cb_start = (row >> shift) * cb.stride;
        let cr_start = (row >> shift) * cr.stride;
        convert_row(
            &y.data[y_start..y_start + y.width],
            &cb.data[cb_start..cb_start + cb.width],
            &cr.data[cr_start..cr_start + cr.width],
            output,
            shift,
            &matrix,
        );
    }
    Ok(DecodedFrame {
        pts: unsafe { ffi::de265_get_image_PTS(image) },
        width: y.width as u32,
        height: y.height as u32,
        rgba,
    })
}

fn convert_row(y: &[u8], cb: &[u8], cr: &[u8], rgba: &mut [u8], shift: usize, matrix: &Matrix) {
    if shift == 0 && matrix.y_offset == 0 {
        sse2::full_range_444(y, cb, cr, rgba);
    } else {
        for (column, pixel) in rgba.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&matrix.pixel(
                y[column],
                cb[column >> shift],
                cr[column >> shift],
            ));
        }
    }
}

struct Matrix {
    y_offset: i32,
    y_scale: i32,
    r_cr: i32,
    g_cb: i32,
    g_cr: i32,
    b_cb: i32,
}

impl Matrix {
    fn new(full_range: bool) -> Self {
        let (offset, y_scale, c_scale) = if full_range {
            (0, 1.0, 1.0)
        } else {
            (16, 255.0 / 219.0, 255.0 / 224.0)
        };
        let fixed = |v: f64| (v * 65536.0 + 0.5) as i32;
        Self {
            y_offset: offset,
            y_scale: fixed(y_scale),
            r_cr: fixed(1.5748 * c_scale),
            g_cb: fixed(0.187_324_272_930_648_8 * c_scale),
            g_cr: fixed(0.468_124_272_930_648_8 * c_scale),
            b_cb: fixed(1.8556 * c_scale),
        }
    }

    fn pixel(&self, y: u8, cb: u8, cr: u8) -> [u8; 4] {
        let y = (i32::from(y) - self.y_offset) * self.y_scale;
        let cb = i32::from(cb) - 128;
        let cr = i32::from(cr) - 128;
        let round = |value: i32| ((value + 32768) >> 16).clamp(0, 255) as u8;
        [
            round(y + self.r_cr * cr),
            round(y - self.g_cb * cb - self.g_cr * cr),
            round(y + self.b_cb * cb),
            255,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(y: u8, cb: u8, cr: u8, full_range: bool) -> [u8; 4] {
        let (y, cb, cr) = if full_range {
            (f64::from(y), f64::from(cb) - 128.0, f64::from(cr) - 128.0)
        } else {
            (
                (f64::from(y) - 16.0) * 255.0 / 219.0,
                (f64::from(cb) - 128.0) * 255.0 / 224.0,
                (f64::from(cr) - 128.0) * 255.0 / 224.0,
            )
        };
        let byte = |value: f64| value.round().clamp(0.0, 255.0) as u8;
        [
            byte(y + 1.5748 * cr),
            byte(y - 0.187_324_272_930_648_8 * cb - 0.468_124_272_930_648_8 * cr),
            byte(y + 1.8556 * cb),
            255,
        ]
    }

    fn assert_color(actual: &[u8], expected: [u8; 4]) {
        for (channel, &value) in expected.iter().enumerate() {
            assert!(
                actual[channel].abs_diff(value) <= if channel == 3 { 0 } else { 1 },
                "channel {channel}: got {}, expected {value}",
                actual[channel]
            );
        }
    }

    #[test]
    fn sse2_matches_scalar_and_reference_for_all_chroma_pairs() {
        let cr: Vec<u8> = (0..=255).collect();
        let matrix = Matrix::new(true);
        let mut output = [0; 256 * 4];
        for luma in [0, 16, 54, 127, 128, 182, 235, 255] {
            let y = [luma; 256];
            for chroma in 0..=255 {
                let cb = [chroma; 256];
                sse2::full_range_444(&y, &cb, &cr, &mut output);
                for (column, pixel) in output.chunks_exact(4).enumerate() {
                    assert_color(pixel, reference(luma, chroma, cr[column], true));
                    assert_color(pixel, matrix.pixel(luma, chroma, cr[column]));
                }
            }
        }
    }

    #[test]
    fn sse2_handles_unaligned_rows_scalar_tails_and_exact_output_bounds() {
        for width in [
            1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 1919, 1920, 8191, 8192,
        ] {
            let y: Vec<u8> = (0..width + 1).map(|i| (i * 37) as u8).collect();
            let cb: Vec<u8> = (0..width + 3).map(|i| (i * 71) as u8).collect();
            let cr: Vec<u8> = (0..width + 5).map(|i| (i * 97) as u8).collect();
            let (y, cb, cr) = (&y[1..], &cb[3..], &cr[5..]);
            let mut output = vec![0x5a; width * 4 + 16];
            sse2::full_range_444(y, cb, cr, &mut output[9..9 + width * 4]);
            assert!(output[..9].iter().all(|&byte| byte == 0x5a));
            assert!(output[9 + width * 4..].iter().all(|&byte| byte == 0x5a));
            for (column, pixel) in output[9..9 + width * 4].chunks_exact(4).enumerate() {
                assert_color(pixel, reference(y[column], cb[column], cr[column], true));
            }
        }
    }

    #[test]
    fn limited_444_and_full_or_limited_420_keep_the_scalar_fallback() {
        let y: Vec<u8> = (0..34).map(|i| (i * 43) as u8).collect();
        let cb: Vec<u8> = (0..34).map(|i| (i * 73) as u8).collect();
        let cr: Vec<u8> = (0..34).map(|i| (i * 103) as u8).collect();
        for (shift, full_range) in [(0, false), (1, false), (1, true)] {
            let matrix = Matrix::new(full_range);
            let mut output = vec![0; y.len() * 4];
            convert_row(&y, &cb, &cr, &mut output, shift, &matrix);
            for (column, pixel) in output.chunks_exact(4).enumerate() {
                assert_color(
                    pixel,
                    reference(
                        y[column],
                        cb[column >> shift],
                        cr[column >> shift],
                        full_range,
                    ),
                );
                assert_eq!(
                    pixel,
                    matrix.pixel(y[column], cb[column >> shift], cr[column >> shift])
                );
            }
        }
    }
}
