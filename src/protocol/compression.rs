use std::io::Read;

use flate2::{Decompress, FlushDecompress, Status};
use zune_jpeg::{
    JpegDecoder,
    zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions},
};

use crate::{
    error::{Error, Result},
    framebuffer::Framebuffer,
    geometry::{Extent, Rect},
};

use super::{MAX_COMPRESSED, bounded_bytes, byte, pixel, u16_be, u32_be};

pub(super) struct CompressionDecoder {
    zlib: Decompress,
    tight: [Decompress; 4],
}

impl CompressionDecoder {
    pub fn new() -> Self {
        Self {
            zlib: Decompress::new(true),
            tight: std::array::from_fn(|_| Decompress::new(true)),
        }
    }

    pub fn decode(
        &mut self,
        reader: &mut impl Read,
        rect: Rect,
        frame: &mut Framebuffer,
        encoding: i32,
    ) -> Result<()> {
        rect.validate(frame.size)?;
        match encoding {
            2 | 4 => rre(reader, rect, frame, encoding == 4),
            6 => {
                let length = u32_be(reader)? as usize;
                let compressed = bounded_bytes(reader, length, MAX_COMPRESSED)?;
                let data = inflate(
                    &mut self.zlib,
                    &compressed,
                    rect.width as usize * rect.height as usize * 4,
                )?;
                frame.raw_bgra(rect, &data)
            }
            7 => self.tight(reader, rect, frame),
            _ => Err(Error::Protocol("Unsupported compression decoder".into())),
        }
    }

    fn tight(&mut self, reader: &mut impl Read, rect: Rect, frame: &mut Framebuffer) -> Result<()> {
        if rect.width > 2048 {
            return Err(Error::Protocol(
                "Tight rectangles must be no wider than 2048 pixels".into(),
            ));
        }
        let control = byte(reader)?;
        for (index, stream) in self.tight.iter_mut().enumerate() {
            if control & (1 << index) != 0 {
                stream.reset(true);
            }
        }
        let mode = control >> 4;
        if mode == 8 {
            return frame.fill(rect, tight_pixel(reader)?);
        }
        if mode == 9 {
            let length = compact_length(reader)?;
            let data = bounded_bytes(reader, length, 4_194_303)?;
            let options = DecoderOptions::default()
                .jpeg_set_out_colorspace(ColorSpace::RGBA)
                .set_max_width(rect.width as usize)
                .set_max_height(rect.height as usize);
            let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
            decoder
                .decode_headers()
                .map_err(|error| Error::Protocol(format!("Invalid Tight JPEG: {error}")))?;
            if decoder.dimensions() != Some((rect.width as usize, rect.height as usize)) {
                return Err(Error::Protocol(
                    "JPEG dimensions do not match the Tight rectangle".into(),
                ));
            }
            let rgba = decoder
                .decode()
                .map_err(|error| Error::Protocol(format!("Invalid Tight JPEG: {error}")))?;
            return frame.write_rgba(rect, &rgba);
        }
        if mode > 7 {
            return Err(Error::Protocol("Unrequested Tight compression mode".into()));
        }
        let filter = if mode & 4 != 0 { byte(reader)? } else { 0 };
        let palette = if filter == 1 {
            let count = usize::from(byte(reader)?) + 1;
            if count < 2 {
                return Err(Error::Protocol(
                    "Tight palette must have at least two colors".into(),
                ));
            }
            let mut palette = Vec::with_capacity(count);
            for _ in 0..count {
                palette.push(tight_pixel(reader)?);
            }
            palette
        } else {
            Vec::new()
        };
        let pixels = rect.width as usize * rect.height as usize;
        let length = match filter {
            0 | 2 => pixels * 3,
            1 if palette.len() == 2 => rect.width.div_ceil(8) as usize * rect.height as usize,
            1 => pixels,
            _ => return Err(Error::Protocol("Unsupported Tight filter".into())),
        };
        let data = if length < 12 {
            bounded_bytes(reader, length, 12)?
        } else {
            let compressed_length = compact_length(reader)?;
            let compressed = bounded_bytes(reader, compressed_length, 4_194_303)?;
            inflate(&mut self.tight[usize::from(mode & 3)], &compressed, length)?
        };
        let mut rgba = vec![0; pixels * 4];
        match filter {
            0 => {
                for (input, output) in data
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .zip(rgba.as_chunks_mut::<4>().0)
                {
                    output.copy_from_slice(&[input[0], input[1], input[2], 255]);
                }
            }
            1 => {
                for y in 0..rect.height as usize {
                    for x in 0..rect.width as usize {
                        let index = if palette.len() == 2 {
                            usize::from(
                                (data[y * rect.width.div_ceil(8) as usize + x / 8] >> (7 - x % 8))
                                    & 1,
                            )
                        } else {
                            usize::from(data[y * rect.width as usize + x])
                        };
                        let color = palette
                            .get(index)
                            .ok_or_else(|| Error::Protocol("Invalid Tight palette index".into()))?;
                        let start = (y * rect.width as usize + x) * 4;
                        rgba[start..start + 4].copy_from_slice(color);
                    }
                }
            }
            2 => {
                let width = rect.width as usize;
                for y in 0..rect.height as usize {
                    for x in 0..width {
                        for component in 0..3 {
                            let offset = (y * width + x) * 4 + component;
                            let left = if x > 0 {
                                i32::from(rgba[offset - 4])
                            } else {
                                0
                            };
                            let above = if y > 0 {
                                i32::from(rgba[offset - width * 4])
                            } else {
                                0
                            };
                            let corner = if x > 0 && y > 0 {
                                i32::from(rgba[offset - width * 4 - 4])
                            } else {
                                0
                            };
                            rgba[offset] = ((left + above - corner).clamp(0, 255) as u8)
                                .wrapping_add(data[(y * width + x) * 3 + component]);
                        }
                        rgba[(y * width + x) * 4 + 3] = 255;
                    }
                }
            }
            _ => unreachable!(),
        }
        frame.write_rgba(rect, &rgba)
    }
}

fn tight_pixel(reader: &mut impl Read) -> Result<[u8; 4]> {
    let mut color = [0; 3];
    reader.read_exact(&mut color)?;
    Ok([color[0], color[1], color[2], 255])
}

fn compact_length(reader: &mut impl Read) -> Result<usize> {
    let first = byte(reader)?;
    let mut length = usize::from(first & 127);
    if first & 128 != 0 {
        let second = byte(reader)?;
        length |= usize::from(second & 127) << 7;
        if second & 128 != 0 {
            length |= usize::from(byte(reader)?) << 14;
        }
    }
    Ok(length)
}

fn inflate(stream: &mut Decompress, compressed: &[u8], expected: usize) -> Result<Vec<u8>> {
    let mut output = vec![0; expected + 1];
    let input_before = stream.total_in();
    let output_before = stream.total_out();
    let status = stream
        .decompress(compressed, &mut output, FlushDecompress::Sync)
        .map_err(|error| Error::Protocol(format!("Invalid compressed rectangle: {error}")))?;
    if (stream.total_in() - input_before) as usize != compressed.len()
        || (stream.total_out() - output_before) as usize != expected
    {
        return Err(Error::Protocol(
            "Compressed rectangle length does not match its pixels".into(),
        ));
    }
    if status == Status::StreamEnd {
        stream.reset(true);
    }
    output.truncate(expected);
    Ok(output)
}

fn rre(reader: &mut impl Read, rect: Rect, frame: &mut Framebuffer, compact: bool) -> Result<()> {
    let count = u32_be(reader)?;
    if count > 1_000_000 {
        return Err(Error::Protocol("Too many RRE subrectangles".into()));
    }
    frame.fill(rect, pixel(reader, 4)?)?;
    let extent = Extent::new(rect.width, rect.height)?;
    for _ in 0..count {
        let color = pixel(reader, 4)?;
        let mut read_coordinate = || -> Result<u32> {
            if compact {
                Ok(u32::from(byte(reader)?))
            } else {
                Ok(u32::from(u16_be(reader)?))
            }
        };
        let sub = Rect {
            x: read_coordinate()?,
            y: read_coordinate()?,
            width: read_coordinate()?,
            height: read_coordinate()?,
        };
        sub.validate(extent)?;
        frame.fill(
            Rect {
                x: rect.x + sub.x,
                y: rect.y + sub.y,
                ..sub
            },
            color,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::{Cursor, Write};

    fn fixture() -> (CompressionDecoder, Framebuffer, Rect) {
        (
            CompressionDecoder::new(),
            Framebuffer::new(Extent::new(2, 2).unwrap()),
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
        )
    }
    fn compressed(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    #[test]
    fn tight_fill_uses_rgb_not_wire_bgr() {
        let (mut decoder, mut frame, rect) = fixture();
        decoder
            .decode(&mut Cursor::new([0x80, 255, 20, 5]), rect, &mut frame, 7)
            .unwrap();
        assert_eq!(frame.rgba, [255, 20, 5, 255].repeat(4));
    }
    #[test]
    fn tight_copy_and_gradient_filters_are_lossless() {
        let (mut decoder, mut frame, rect) = fixture();
        for (control, filter, pixels) in [
            (0x01, None, [10, 20, 30].repeat(4)),
            (
                0x41,
                Some(2),
                [10, 20, 30, 0, 0, 0, 0, 0, 0, 0, 0, 0].to_vec(),
            ),
        ] {
            let data = compressed(&pixels);
            let mut payload = vec![control];
            if let Some(filter) = filter {
                payload.push(filter);
            }
            payload.push(data.len() as u8);
            payload.extend(data);
            decoder
                .decode(&mut Cursor::new(payload), rect, &mut frame, 7)
                .unwrap();
            assert_eq!(frame.rgba, [10, 20, 30, 255].repeat(4));
        }
    }
    #[test]
    fn tight_palette_packs_each_row_independently() {
        let (mut decoder, mut frame, rect) = fixture();
        let data = [0x40, 1, 1, 255, 0, 0, 0, 255, 0, 0b0100_0000, 0b1000_0000];
        decoder
            .decode(&mut Cursor::new(data), rect, &mut frame, 7)
            .unwrap();
        assert_eq!(&frame.rgba[..8], &[255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(&frame.rgba[8..], &[0, 255, 0, 255, 255, 0, 0, 255]);
    }
    #[test]
    fn zlib_exact_length_is_checked() {
        let (mut decoder, mut frame, rect) = fixture();
        let data = compressed(&[1, 2, 3, 0].repeat(4));
        let mut payload = (data.len() as u32).to_be_bytes().to_vec();
        payload.extend(data);
        decoder
            .decode(&mut Cursor::new(payload), rect, &mut frame, 6)
            .unwrap();
        assert_eq!(frame.rgba, [3, 2, 1, 255].repeat(4));
        assert!(inflate(&mut Decompress::new(true), &compressed(&[0; 50]), 4).is_err());
    }
    #[test]
    fn rre_and_compact_rre_validate_subrectangles() {
        for compact in [false, true] {
            let (mut decoder, mut frame, rect) = fixture();
            let mut payload = vec![0, 0, 0, 1, 0, 0, 255, 0, 255, 0, 0, 0];
            if compact {
                payload.extend([1, 1, 1, 1]);
            } else {
                payload.extend([0, 1, 0, 1, 0, 1, 0, 1]);
            }
            decoder
                .decode(
                    &mut Cursor::new(payload),
                    rect,
                    &mut frame,
                    if compact { 4 } else { 2 },
                )
                .unwrap();
            assert_eq!(&frame.rgba[..4], &[255, 0, 0, 255]);
            assert_eq!(&frame.rgba[12..], &[0, 0, 255, 255]);
        }
    }
    #[test]
    fn rejects_bad_filters_palettes_and_jpeg_dimensions() {
        let (mut decoder, mut frame, rect) = fixture();
        for data in [vec![0x40, 5], vec![0x40, 1, 0], vec![0x90, 2, 0, 0]] {
            assert!(
                decoder
                    .decode(&mut Cursor::new(data), rect, &mut frame, 7)
                    .is_err()
            );
        }
    }
    #[test]
    fn tight_jpeg_decodes_rgb_and_refuses_mismatched_headers() {
        let mut jpeg = Vec::new();
        jpeg_encoder::Encoder::new(&mut jpeg, 100)
            .encode(&[240, 20, 10].repeat(4), 2, 2, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        let mut payload = vec![0x90];
        let length = jpeg.len();
        payload.push((length as u8 & 127) | 128);
        payload.push((length >> 7) as u8);
        payload.extend(jpeg);
        let (mut decoder, mut frame, rect) = fixture();
        decoder
            .decode(&mut Cursor::new(payload.clone()), rect, &mut frame, 7)
            .unwrap();
        assert!(frame.rgba[0] > 230);
        assert!(frame.rgba[1] < 30);
        assert_eq!(frame.rgba[3], 255);
        let rect = Rect {
            width: 1,
            height: 2,
            ..rect
        };
        assert!(
            decoder
                .decode(&mut Cursor::new(payload), rect, &mut frame, 7)
                .is_err()
        );
    }

    #[test]
    fn tight_compression_streams_persist_and_reset_independently() {
        let (mut decoder, mut frame, rect) = fixture();
        let mut streams: [flate2::Compress; 2] =
            std::array::from_fn(|_| flate2::Compress::new(Compression::fast(), true));
        for (stream, reset, color) in [
            (0, false, [220, 10, 30]),
            (1, false, [10, 220, 30]),
            (0, false, [40, 50, 220]),
            (0, true, [200, 150, 100]),
            (1, false, [70, 120, 180]),
        ] {
            if reset {
                streams[stream] = flate2::Compress::new(Compression::fast(), true);
            }
            let mut compressed = Vec::with_capacity(256);
            streams[stream]
                .compress_vec(
                    &color.repeat(4),
                    &mut compressed,
                    flate2::FlushCompress::Sync,
                )
                .unwrap();
            assert!(compressed.len() < 128);
            let mut payload = vec![
                (stream as u8) << 4 | u8::from(reset),
                compressed.len() as u8,
            ];
            payload.extend(compressed);
            decoder
                .decode(&mut Cursor::new(payload), rect, &mut frame, 7)
                .unwrap();
            assert_eq!(frame.rgba, [color[0], color[1], color[2], 255].repeat(4));
        }
    }
}
