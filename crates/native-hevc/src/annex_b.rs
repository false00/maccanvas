use crate::{Error, MAX_INPUT_BYTES, Result, validate_dimensions};

pub const MAX_NALS: usize = 4096;
const MAX_PARAMETER_SET_BYTES: usize = 64 * 1024;

pub struct Batch<'a> {
    pub nals: Vec<&'a [u8]>,
    pub pictures: u64,
    pub has_sps: bool,
}

pub fn validate(input: &[u8], has_sps: bool) -> Result<Batch<'_>> {
    if input.len() > MAX_INPUT_BYTES {
        return Err(Error::InputTooLarge {
            actual: input.len(),
            maximum: MAX_INPUT_BYTES,
        });
    }
    let first = start_code(input).ok_or(Error::InvalidInput("missing start code"))?;
    if input[..first].iter().any(|&byte| byte != 0) {
        return Err(Error::InvalidInput("data before the first start code"));
    }

    let mut batch = Batch {
        nals: Vec::new(),
        pictures: 0,
        has_sps,
    };
    let mut cursor = first + 3;
    loop {
        let next = start_code(&input[cursor..]).map(|offset| cursor + offset);
        let mut end = next.unwrap_or(input.len());
        while end > cursor && input[end - 1] == 0 {
            end -= 1;
        }
        let nal = &input[cursor..end];
        if nal.len() < 3 {
            return Err(Error::InvalidInput("truncated or empty NAL unit"));
        }
        if nal[0] & 0x80 != 0 || nal[1] & 7 == 0 {
            return Err(Error::InvalidInput("invalid NAL header"));
        }
        if nal[0] & 1 != 0 || nal[1] & 0xf8 != 0 {
            return Err(Error::InvalidInput("multi-layer HEVC is unsupported"));
        }
        let kind = nal[0] >> 1;
        match kind {
            0..=9 | 16..=21 => {
                if !batch.has_sps {
                    return Err(Error::InvalidInput("picture precedes an SPS"));
                }
                if nal[2] & 0x80 != 0 {
                    batch.pictures += 1;
                } else if batch.pictures == 0 {
                    return Err(Error::InvalidInput(
                        "push must contain complete access units, not partial pictures",
                    ));
                }
            }
            32..=34 => {
                if nal.len() > MAX_PARAMETER_SET_BYTES {
                    return Err(Error::ResourceLimit("parameter set exceeds 64 KiB"));
                }
                if kind == 33 {
                    validate_sps(&nal[2..])?;
                    batch.has_sps = true;
                }
            }
            35..=40 => {}
            _ => return Err(Error::InvalidInput("reserved or unsupported NAL type")),
        }
        if batch.nals.len() == MAX_NALS {
            return Err(Error::ResourceLimit("more than 4096 NAL units in one push"));
        }
        batch
            .nals
            .try_reserve(1)
            .map_err(|_| Error::AllocationFailed)?;
        batch.nals.push(nal);
        match next {
            Some(offset) => cursor = offset + 3,
            None => return Ok(batch),
        }
    }
}

fn start_code(bytes: &[u8]) -> Option<usize> {
    bytes.windows(3).position(|window| window == [0, 0, 1])
}

// Inspect the SPS before it reaches native code, including before native CTB/DPB
// allocations. The native image allocator independently enforces the same bounds.
fn validate_sps(ebsp: &[u8]) -> Result<()> {
    let mut bits = Bits::new(ebsp);
    bits.skip(4)?;
    let sublayers = bits.read(3)? as usize;
    if sublayers > 6 {
        return Err(Error::InvalidInput("invalid SPS sublayer count"));
    }
    bits.skip(1)?;
    bits.skip(96)?;
    let mut profiles = [false; 6];
    let mut levels = [false; 6];
    for i in 0..sublayers {
        profiles[i] = bits.read(1)? != 0;
        levels[i] = bits.read(1)? != 0;
    }
    if sublayers != 0 {
        bits.skip((8 - sublayers) * 2)?;
    }
    for i in 0..sublayers {
        if profiles[i] {
            bits.skip(88)?;
        }
        if levels[i] {
            bits.skip(8)?;
        }
    }
    if bits.ue()? > 15 {
        return Err(Error::InvalidInput("invalid SPS identifier"));
    }
    let chroma = bits.ue()?;
    if !matches!(chroma, 1 | 3) {
        return Err(Error::UnsupportedChroma(chroma));
    }
    if chroma == 3 && bits.read(1)? != 0 {
        return Err(Error::InvalidInput(
            "separate colour planes are unsupported",
        ));
    }
    let width = bits.ue()?;
    let height = bits.ue()?;
    validate_dimensions(width, height)?;
    if bits.read(1)? != 0 {
        let left = bits.ue()?;
        let right = bits.ue()?;
        let top = bits.ue()?;
        let bottom = bits.ue()?;
        let unit = if chroma == 1 { 2 } else { 1 };
        let horizontal = left.checked_add(right).and_then(|v| v.checked_mul(unit));
        let vertical = top.checked_add(bottom).and_then(|v| v.checked_mul(unit));
        match (horizontal, vertical) {
            (Some(x), Some(y)) if x < width && y < height => {}
            _ => return Err(Error::InvalidInput("invalid SPS conformance window")),
        }
    }
    let luma = bits.ue()?.checked_add(8);
    let chroma = bits.ue()?.checked_add(8);
    match (luma, chroma) {
        (Some(8), Some(8)) => Ok(()),
        (Some(luma), Some(chroma)) => Err(Error::UnsupportedBitDepth { luma, chroma }),
        _ => Err(Error::InvalidInput("invalid SPS bit depth")),
    }
}

struct Bits<'a> {
    ebsp: &'a [u8],
    offset: usize,
    zeros: u8,
    byte: u8,
    left: u8,
}

impl<'a> Bits<'a> {
    fn new(ebsp: &'a [u8]) -> Self {
        Self {
            ebsp,
            offset: 0,
            zeros: 0,
            byte: 0,
            left: 0,
        }
    }

    fn read(&mut self, count: usize) -> Result<u32> {
        let mut value = 0;
        for _ in 0..count {
            if self.left == 0 {
                let mut byte = *self
                    .ebsp
                    .get(self.offset)
                    .ok_or(Error::InvalidInput("truncated SPS"))?;
                self.offset += 1;
                if self.zeros == 2 && byte == 3 {
                    byte = *self
                        .ebsp
                        .get(self.offset)
                        .filter(|&&byte| byte <= 3)
                        .ok_or(Error::InvalidInput("invalid SPS emulation prevention"))?;
                    self.offset += 1;
                    self.zeros = 0;
                }
                self.zeros = if byte == 0 { self.zeros + 1 } else { 0 };
                self.zeros = self.zeros.min(2);
                self.byte = byte;
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | u32::from((self.byte >> self.left) & 1);
        }
        Ok(value)
    }

    fn skip(&mut self, count: usize) -> Result<()> {
        for _ in 0..count {
            self.read(1)?;
        }
        Ok(())
    }

    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::InvalidInput("SPS Exp-Golomb value overflows"));
            }
        }
        Ok(((1_u32 << zeros) - 1) + self.read(zeros)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Writer(Vec<bool>);

    impl Writer {
        fn bits(&mut self, value: u32, count: usize) {
            for bit in (0..count).rev() {
                self.0.push((value >> bit) & 1 != 0);
            }
        }

        fn zeroes(&mut self, count: usize) {
            self.0.extend(std::iter::repeat_n(false, count));
        }

        fn ue(&mut self, value: u32) {
            let code = value + 1;
            let size = (32 - code.leading_zeros()) as usize;
            self.zeroes(size - 1);
            self.bits(code, size);
        }

        fn finish(mut self) -> Vec<u8> {
            self.0.push(true);
            while !self.0.len().is_multiple_of(8) {
                self.0.push(false);
            }
            let mut output = vec![0, 0, 1, 0x42, 1];
            let mut zeroes = 0;
            for chunk in self.0.chunks_exact(8) {
                let byte = chunk
                    .iter()
                    .fold(0_u8, |byte, &bit| (byte << 1) | u8::from(bit));
                if zeroes == 2 && byte <= 3 {
                    output.push(3);
                    zeroes = 0;
                }
                output.push(byte);
                zeroes = if byte == 0 { zeroes + 1 } else { 0 };
            }
            output
        }
    }

    fn sps(
        width: u32,
        height: u32,
        chroma: u32,
        depth_minus_8: u32,
        crop: Option<[u32; 4]>,
        sublayers: usize,
    ) -> Vec<u8> {
        let mut writer = Writer(Vec::new());
        writer.zeroes(4);
        writer.bits(sublayers as u32, 3);
        writer.bits(1, 1);
        writer.zeroes(96);
        for i in 0..sublayers {
            writer.bits(u32::from(i % 2 == 0), 1);
            writer.bits(u32::from(i % 3 == 0), 1);
        }
        if sublayers != 0 {
            writer.zeroes((8 - sublayers) * 2);
        }
        for i in 0..sublayers {
            if i % 2 == 0 {
                writer.zeroes(88);
            }
            if i % 3 == 0 {
                writer.zeroes(8);
            }
        }
        writer.ue(0);
        writer.ue(chroma);
        if chroma == 3 {
            writer.bits(0, 1);
        }
        writer.ue(width);
        writer.ue(height);
        writer.bits(u32::from(crop.is_some()), 1);
        if let Some(crop) = crop {
            for offset in crop {
                writer.ue(offset);
            }
        }
        writer.ue(depth_minus_8);
        writer.ue(depth_minus_8);
        writer.finish()
    }

    #[test]
    fn rejects_coded_dimensions_before_native_allocation() {
        for (width, height) in [
            (0, 48),
            (64, 0),
            (8193, 48),
            (64, 8193),
            (8192, 2049),
            (4097, 4096),
        ] {
            assert!(matches!(
                validate(&sps(width, height, 3, 0, None, 0), false),
                Err(Error::InvalidDimensions { width: w, height: h }) if w == width && h == height
            ));
        }
        // A conformance window must not be used to hide enormous coded pictures.
        assert!(matches!(
            validate(&sps(16384, 48, 3, 0, Some([0, 16320, 0, 0]), 0), false),
            Err(Error::InvalidDimensions { .. })
        ));
    }

    #[test]
    fn accepts_the_exact_dimension_and_pixel_boundaries() {
        for (width, height) in [(8192, 2048), (2048, 8192), (4096, 4096)] {
            assert!(validate(&sps(width, height, 3, 0, None, 0), false).is_ok());
        }
    }

    #[test]
    fn validates_sublayer_profile_tier_level_skips() {
        for sublayers in 0..=6 {
            assert!(validate(&sps(64, 48, 3, 0, None, sublayers), false).is_ok());
            assert!(matches!(
                validate(&sps(8193, 48, 3, 0, None, sublayers), false),
                Err(Error::InvalidDimensions { .. })
            ));
        }
    }

    #[test]
    fn rejects_invalid_crop_bit_depth_and_chroma() {
        for crop in [[64, 0, 0, 0], [0, 0, 24, 0], [u32::MAX - 1, 2, 0, 0]] {
            assert!(matches!(
                validate(&sps(64, 48, 1, 0, Some(crop), 0), false),
                Err(Error::InvalidInput(_))
            ));
        }
        assert!(matches!(
            validate(&sps(64, 48, 3, 2, None, 0), false),
            Err(Error::UnsupportedBitDepth {
                luma: 10,
                chroma: 10
            })
        ));
        assert!(matches!(
            validate(&sps(64, 48, 2, 0, None, 0), false),
            Err(Error::UnsupportedChroma(2))
        ));
    }

    #[test]
    fn rejects_truncated_sps_and_invalid_emulation_prevention() {
        let valid = sps(64, 48, 3, 0, None, 0);
        for end in 5..valid.len() - 1 {
            assert!(validate(&valid[..end], false).is_err());
        }
        assert!(matches!(
            validate(&[0, 0, 1, 0x42, 1, 0, 0, 3, 4], false),
            Err(Error::InvalidInput(_))
        ));
    }
}
