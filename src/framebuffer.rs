use crate::{
    error::{Error, Result},
    geometry::{Extent, Rect},
};

#[derive(Clone)]
pub struct Framebuffer {
    pub size: Extent,
    pub rgba: Vec<u8>,
}

impl Framebuffer {
    pub fn new(size: Extent) -> Self {
        let mut rgba = vec![0; size.bytes()];
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        Self { size, rgba }
    }

    pub fn resize(&mut self, size: Extent) {
        let mut replacement = Self::new(size);
        let width = size.width.min(self.size.width) as usize * 4;
        for y in 0..size.height.min(self.size.height) as usize {
            let old = y * self.size.width as usize * 4;
            let new = y * size.width as usize * 4;
            replacement.rgba[new..new + width].copy_from_slice(&self.rgba[old..old + width]);
        }
        *self = replacement;
    }

    pub fn fill(&mut self, rect: Rect, color: [u8; 4]) -> Result<()> {
        rect.validate(self.size)?;
        for y in rect.y..rect.y + rect.height {
            let start = (y as usize * self.size.width as usize + rect.x as usize) * 4;
            for pixel in self.rgba[start..start + rect.width as usize * 4]
                .as_chunks_mut::<4>()
                .0
            {
                pixel.copy_from_slice(&color);
            }
        }
        Ok(())
    }

    pub fn raw_bgra(&mut self, rect: Rect, bytes: &[u8]) -> Result<()> {
        rect.validate(self.size)?;
        if bytes.len() != rect.width as usize * rect.height as usize * 4 {
            return Err(Error::Protocol("Raw rectangle length is incorrect".into()));
        }
        for (y, row) in bytes.chunks_exact(rect.width as usize * 4).enumerate() {
            let destination =
                ((rect.y as usize + y) * self.size.width as usize + rect.x as usize) * 4;
            for (input, output) in row
                .as_chunks::<4>()
                .0
                .iter()
                .zip(self.rgba[destination..].as_chunks_mut::<4>().0)
            {
                output.copy_from_slice(&[input[2], input[1], input[0], 255]);
            }
        }
        Ok(())
    }

    pub fn write_rgba(&mut self, rect: Rect, bytes: &[u8]) -> Result<()> {
        rect.validate(self.size)?;
        if bytes.len() != rect.width as usize * rect.height as usize * 4 {
            return Err(Error::Protocol(
                "Decoded rectangle length is incorrect".into(),
            ));
        }
        for (y, row) in bytes.chunks_exact(rect.width as usize * 4).enumerate() {
            let destination =
                ((rect.y as usize + y) * self.size.width as usize + rect.x as usize) * 4;
            self.rgba[destination..destination + row.len()].copy_from_slice(row);
        }
        Ok(())
    }

    pub fn copy(&mut self, destination: Rect, x: u32, y: u32) -> Result<()> {
        destination.validate(self.size)?;
        Rect {
            x,
            y,
            width: destination.width,
            height: destination.height,
        }
        .validate(self.size)?;
        let stride = self.size.width as usize * 4;
        let length = destination.width as usize * 4;
        let copy_row = |data: &mut Vec<u8>, row: usize| {
            let source = (y as usize + row) * stride + x as usize * 4;
            let target = (destination.y as usize + row) * stride + destination.x as usize * 4;
            data.copy_within(source..source + length, target);
        };
        if destination.y > y {
            for row in (0..destination.height as usize).rev() {
                copy_row(&mut self.rgba, row);
            }
        } else {
            for row in 0..destination.height as usize {
                copy_row(&mut self.rgba, row);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn converts_wire_pixels_to_opaque_rgba() {
        let mut frame = Framebuffer::new(Extent::new(2, 1).unwrap());
        frame
            .raw_bgra(rect(0, 0, 2, 1), &[3, 2, 1, 0, 6, 5, 4, 0])
            .unwrap();
        assert_eq!(frame.rgba, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn copy_rect_handles_vertical_and_horizontal_overlap() {
        let mut frame = Framebuffer::new(Extent::new(3, 3).unwrap());
        for y in 0..3 {
            for x in 0..3 {
                frame
                    .fill(rect(x, y, 1, 1), [(y * 3 + x) as u8, 0, 0, 255])
                    .unwrap();
            }
        }
        frame.copy(rect(0, 1, 3, 2), 0, 0).unwrap();
        assert_eq!(frame.rgba[12], 0);
        assert_eq!(frame.rgba[24], 3);
        frame.copy(rect(1, 0, 2, 1), 0, 0).unwrap();
        assert_eq!(frame.rgba[4], 0);
        assert_eq!(frame.rgba[8], 1);
    }

    #[test]
    fn resizing_retains_pixels_without_uninitialized_alpha() {
        let mut frame = Framebuffer::new(Extent::new(2, 2).unwrap());
        frame.fill(rect(0, 0, 2, 2), [1, 2, 3, 255]).unwrap();
        frame.resize(Extent::new(3, 3).unwrap());
        assert_eq!(&frame.rgba[..4], &[1, 2, 3, 255]);
        assert_eq!(&frame.rgba[32..36], &[0, 0, 0, 255]);
    }

    #[test]
    fn rejects_bad_rectangles_and_payloads() {
        let mut frame = Framebuffer::new(Extent::new(2, 2).unwrap());
        assert!(frame.fill(rect(u32::MAX, 0, 2, 1), [0; 4]).is_err());
        assert!(frame.raw_bgra(rect(0, 0, 1, 1), &[0; 3]).is_err());
        assert!(frame.copy(rect(0, 0, 2, 2), 1, 1).is_err());
    }
}
