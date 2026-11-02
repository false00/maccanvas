use std::arch::x86_64::{
    __m128i, _mm_add_epi32, _mm_loadl_epi64, _mm_madd_epi16, _mm_packs_epi32, _mm_packus_epi16,
    _mm_set_epi16, _mm_set1_epi8, _mm_set1_epi16, _mm_set1_epi32, _mm_setzero_si128,
    _mm_slli_epi32, _mm_srai_epi32, _mm_storeu_si128, _mm_sub_epi16, _mm_unpackhi_epi16,
    _mm_unpacklo_epi8, _mm_unpacklo_epi16,
};

use super::Matrix;

pub(super) fn full_range_444(y: &[u8], cb: &[u8], cr: &[u8], rgba: &mut [u8]) {
    assert_eq!(y.len(), cb.len());
    assert_eq!(y.len(), cr.len());
    assert_eq!(y.len() * 4, rgba.len());
    let vector_width = y.len() / 8 * 8;

    // SSE2 is mandatory on the only supported target, Windows x64. Q14 keeps all
    // coefficients in signed 16 bits for eight pixels per PMADDWD; coefficient
    // error is <0.01 RGB levels, so rounding differs by at most one level.
    // Slice lengths above bound every unaligned 8-byte load and 32-byte store;
    // neither native padding nor bytes beyond a visible row are accessed.
    unsafe {
        let zero = _mm_setzero_si128();
        let center = _mm_set1_epi16(128);
        let round = _mm_set1_epi32(1 << 13);
        let red = _mm_set1_epi32((25802 << 16) | 16384);
        let blue = _mm_set1_epi32((30402 << 16) | 16384);
        let green = _mm_set_epi16(-7670, -3069, -7670, -3069, -7670, -3069, -7670, -3069);
        let alpha = _mm_set1_epi8(-1);
        for column in (0..vector_width).step_by(8) {
            let y16 = _mm_unpacklo_epi8(
                _mm_loadl_epi64(y.as_ptr().add(column).cast::<__m128i>()),
                zero,
            );
            let cb16 = _mm_sub_epi16(
                _mm_unpacklo_epi8(
                    _mm_loadl_epi64(cb.as_ptr().add(column).cast::<__m128i>()),
                    zero,
                ),
                center,
            );
            let cr16 = _mm_sub_epi16(
                _mm_unpacklo_epi8(
                    _mm_loadl_epi64(cr.as_ptr().add(column).cast::<__m128i>()),
                    zero,
                ),
                center,
            );
            let r_lo = _mm_madd_epi16(_mm_unpacklo_epi16(y16, cr16), red);
            let r_hi = _mm_madd_epi16(_mm_unpackhi_epi16(y16, cr16), red);
            let b_lo = _mm_madd_epi16(_mm_unpacklo_epi16(y16, cb16), blue);
            let b_hi = _mm_madd_epi16(_mm_unpackhi_epi16(y16, cb16), blue);
            let g_lo = _mm_add_epi32(
                _mm_slli_epi32::<14>(_mm_unpacklo_epi16(y16, zero)),
                _mm_madd_epi16(_mm_unpacklo_epi16(cb16, cr16), green),
            );
            let g_hi = _mm_add_epi32(
                _mm_slli_epi32::<14>(_mm_unpackhi_epi16(y16, zero)),
                _mm_madd_epi16(_mm_unpackhi_epi16(cb16, cr16), green),
            );
            let r = _mm_packus_epi16(
                _mm_packs_epi32(
                    _mm_srai_epi32::<14>(_mm_add_epi32(r_lo, round)),
                    _mm_srai_epi32::<14>(_mm_add_epi32(r_hi, round)),
                ),
                zero,
            );
            let g = _mm_packus_epi16(
                _mm_packs_epi32(
                    _mm_srai_epi32::<14>(_mm_add_epi32(g_lo, round)),
                    _mm_srai_epi32::<14>(_mm_add_epi32(g_hi, round)),
                ),
                zero,
            );
            let b = _mm_packus_epi16(
                _mm_packs_epi32(
                    _mm_srai_epi32::<14>(_mm_add_epi32(b_lo, round)),
                    _mm_srai_epi32::<14>(_mm_add_epi32(b_hi, round)),
                ),
                zero,
            );
            let rg = _mm_unpacklo_epi8(r, g);
            let ba = _mm_unpacklo_epi8(b, alpha);
            let output = rgba.as_mut_ptr().add(column * 4).cast::<__m128i>();
            _mm_storeu_si128(output, _mm_unpacklo_epi16(rg, ba));
            _mm_storeu_si128(output.add(1), _mm_unpackhi_epi16(rg, ba));
        }
    }
    let scalar = Matrix::new(true);
    for column in vector_width..y.len() {
        rgba[column * 4..column * 4 + 4]
            .copy_from_slice(&scalar.pixel(y[column], cb[column], cr[column]));
    }
}
