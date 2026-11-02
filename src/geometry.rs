use crate::error::{Error, Result};

pub const MAX_DIMENSION: u32 = 16_384;
pub const MAX_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub width: u32,
    pub height: u32,
}

impl Extent {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        if width == 0
            || height == 0
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || u64::from(width) * u64::from(height) > MAX_PIXELS
        {
            return Err(Error::Protocol("Unsupported framebuffer dimensions".into()));
        }
        Ok(Self { width, height })
    }

    pub fn bytes(self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn validate(self, extent: Extent) -> Result<()> {
        if self.width == 0
            || self.height == 0
            || self
                .x
                .checked_add(self.width)
                .is_none_or(|x| x > extent.width)
            || self
                .y
                .checked_add(self.height)
                .is_none_or(|y| y > extent.height)
        {
            return Err(Error::Protocol("Rectangle exceeds the framebuffer".into()));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub source: Extent,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub pixels: Extent,
    dpi: f64,
}

impl Viewport {
    pub fn fit(source: Extent, width: f32, height: f32, dpi: f32) -> Result<Self> {
        if !width.is_finite()
            || !height.is_finite()
            || !dpi.is_finite()
            || width <= 0.0
            || height <= 0.0
            || dpi <= 0.0
        {
            return Err(Error::Display("Invalid canvas dimensions or DPI".into()));
        }
        let dpi = f64::from(dpi);
        let factor = (f64::from(width) * dpi / f64::from(source.width))
            .min(f64::from(height) * dpi / f64::from(source.height))
            .min(1.0);
        let pixels = Extent::new(
            (f64::from(source.width) * factor).floor().max(1.0) as u32,
            (f64::from(source.height) * factor).floor().max(1.0) as u32,
        )?;
        let draw_width = (f64::from(pixels.width) / dpi) as f32;
        let draw_height = (f64::from(pixels.height) / dpi) as f32;
        Ok(Self {
            source,
            left: (width - draw_width) / 2.0,
            top: (height - draw_height) / 2.0,
            width: draw_width,
            height: draw_height,
            pixels,
            dpi,
        })
    }

    pub fn to_remote(self, x: f32, y: f32, dragging: bool) -> Option<(u16, u16)> {
        if !x.is_finite() || !y.is_finite() {
            return None;
        }
        if !dragging
            && (x < self.left
                || y < self.top
                || x >= self.left + self.width
                || y >= self.top + self.height)
        {
            return None;
        }
        let map = |position: f32, origin: f32, pixels: u32, source: u32| -> u16 {
            let pixel = (f64::from(position) - f64::from(origin)) * self.dpi;
            if pixel <= 0.0 {
                return 0;
            }
            if pixel >= f64::from(pixels - 1) {
                return (source - 1) as u16;
            }
            (((pixel + 0.5) * f64::from(source) / f64::from(pixels)).floor() as u32).min(source - 1)
                as u16
        };
        Some((
            map(x, self.left, self.pixels.width, self.source.width),
            map(y, self.top, self.pixels.height, self.source.height),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_four_k_on_1440p() {
        let view = Viewport::fit(Extent::new(3840, 2160).unwrap(), 2560.0, 1440.0, 1.0).unwrap();
        assert_eq!(view.pixels, Extent::new(2560, 1440).unwrap());
        assert_eq!(view.to_remote(1280.0, 720.0, false), Some((1920, 1080)));
        assert_eq!(view.to_remote(2559.0, 1439.0, false), Some((3839, 2159)));
        assert_eq!(view.to_remote(0.0, 0.0, false), Some((0, 0)));
    }

    #[test]
    fn maps_real_windows_dpi_coordinates() {
        let view = Viewport::fit(Extent::new(3840, 2160).unwrap(), 1280.0, 720.0, 1.5).unwrap();
        assert_eq!(view.pixels, Extent::new(1920, 1080).unwrap());
        assert_eq!(view.to_remote(640.0, 360.0, false), Some((1921, 1081)));
        assert_eq!(
            view.to_remote(1279.3334, 719.3334, false),
            Some((3839, 2159))
        );
    }

    #[test]
    fn padding_does_not_click_the_mac() {
        let view = Viewport::fit(Extent::new(3840, 2160).unwrap(), 900.0, 800.0, 1.0).unwrap();
        assert_eq!(view.pixels, Extent::new(900, 506).unwrap());
        assert_eq!(view.to_remote(50.0, 10.0, false), None);
        assert_eq!(view.to_remote(899.0, 652.0, false), Some((3839, 2159)));
        assert_eq!(view.to_remote(-50.0, 1000.0, true), Some((0, 2159)));
    }

    #[test]
    fn smaller_desktops_stay_at_native_size() {
        let view = Viewport::fit(Extent::new(1280, 720).unwrap(), 2560.0, 1440.0, 1.0).unwrap();
        assert_eq!(view.pixels, Extent::new(1280, 720).unwrap());
        assert_eq!(view.left, 640.0);
    }

    #[test]
    fn rejects_unbounded_allocations_and_invalid_dpi() {
        assert!(Extent::new(0, 2160).is_err());
        assert!(Extent::new(65_535, 65_535).is_err());
        assert!(Extent::new(16_384, 16_384).is_err());
        assert!(Viewport::fit(Extent::new(4, 4).unwrap(), 100.0, 100.0, 0.0).is_err());
    }
}
