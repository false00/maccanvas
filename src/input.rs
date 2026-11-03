use std::io::Write;

use crate::{
    error::{Error, Result},
    geometry::Extent,
    streaming::StreamSettings,
};

const SCROLL_PIXELS_PER_NOTCH: f64 = 60.0;
const SCROLL_BATCH: u16 = 16;
const MAX_PENDING_SCROLL: f64 = 4096.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollScale {
    horizontal: u32,
    vertical: u32,
}

impl ScrollScale {
    pub fn system() -> Result<Self> {
        use windows::Win32::UI::WindowsAndMessaging::{
            SPI_GETWHEELSCROLLCHARS, SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
            SystemParametersInfoW,
        };
        let mut horizontal = 0u32;
        let mut vertical = 0u32;
        for (setting, value) in [
            (SPI_GETWHEELSCROLLCHARS, &mut horizontal),
            (SPI_GETWHEELSCROLLLINES, &mut vertical),
        ] {
            unsafe {
                SystemParametersInfoW(
                    setting,
                    0,
                    Some((value as *mut u32).cast()),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                )
            }
            .map_err(|error| {
                Error::Configuration(format!("Cannot read Windows wheel sensitivity: {error}"))
            })?;
        }
        Ok(Self {
            horizontal,
            vertical,
        })
    }

    fn units(setting: u32) -> f64 {
        // RFB has no page unit; use a bounded page-like burst for that Windows preference.
        f64::from(if setting == u32::MAX { 20 } else { setting })
    }
}

#[derive(Default)]
pub struct ScrollAccumulator {
    horizontal: f64,
    vertical: f64,
}

impl ScrollAccumulator {
    pub fn add(&mut self, x: f32, y: f32, scale: ScrollScale) -> Result<()> {
        let horizontal = self.horizontal
            + f64::from(x) / SCROLL_PIXELS_PER_NOTCH * ScrollScale::units(scale.horizontal);
        let vertical = self.vertical
            + f64::from(y) / SCROLL_PIXELS_PER_NOTCH * ScrollScale::units(scale.vertical);
        if !horizontal.is_finite()
            || !vertical.is_finite()
            || horizontal.abs() > MAX_PENDING_SCROLL
            || vertical.abs() > MAX_PENDING_SCROLL
        {
            self.clear();
            return Err(Error::Configuration(
                "Scroll input exceeded its safe buffer; slow the gesture and try again".into(),
            ));
        }
        self.horizontal = horizontal;
        self.vertical = vertical;
        Ok(())
    }

    pub fn drain(&mut self) -> ScrollSteps {
        let both = self.horizontal.abs() >= 1.0 && self.vertical.abs() >= 1.0;
        let vertical_limit = if both { SCROLL_BATCH / 2 } else { SCROLL_BATCH };
        let vertical =
            self.vertical
                .trunc()
                .clamp(-f64::from(vertical_limit), f64::from(vertical_limit)) as i8;
        let horizontal_limit = SCROLL_BATCH - u16::from(vertical.unsigned_abs());
        let horizontal =
            self.horizontal
                .trunc()
                .clamp(-f64::from(horizontal_limit), f64::from(horizontal_limit)) as i8;
        self.vertical -= f64::from(vertical);
        self.horizontal -= f64::from(horizontal);
        ScrollSteps {
            horizontal,
            vertical,
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollSteps {
    horizontal: i8,
    vertical: i8,
}

impl ScrollSteps {
    pub fn new(horizontal: i8, vertical: i8) -> Result<Self> {
        if u16::from(horizontal.unsigned_abs()) + u16::from(vertical.unsigned_abs()) > SCROLL_BATCH
        {
            return Err(Error::Configuration(
                "Scroll batch exceeds its input budget".into(),
            ));
        }
        Ok(Self {
            horizontal,
            vertical,
        })
    }

    pub fn is_empty(self) -> bool {
        self.horizontal == 0 && self.vertical == 0
    }

    pub(crate) fn pointers(self, x: u16, y: u16, buttons: u8) -> impl Iterator<Item = Command> {
        self.buttons(buttons)
            .map(move |buttons| Command::Pointer { x, y, buttons })
    }

    pub(crate) fn buttons(self, buttons: u8) -> impl Iterator<Item = u8> {
        [(self.vertical, 8, 16), (self.horizontal, 32, 64)]
            .into_iter()
            .flat_map(move |(count, positive, negative)| {
                let pressed = buttons | if count > 0 { positive } else { negative };
                (0..count.unsigned_abs()).flat_map(move |_| [pressed, buttons])
            })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Pointer {
        x: u16,
        y: u16,
        buttons: u8,
    },
    Scroll {
        x: u16,
        y: u16,
        buttons: u8,
        steps: ScrollSteps,
    },
    Key {
        keysym: u32,
        down: bool,
    },
    Refresh {
        size: Extent,
        incremental: bool,
    },
    Configure(StreamSettings),
    Audio(crate::audio::Settings),
}

impl Command {
    pub fn write(self, writer: &mut impl Write) -> Result<()> {
        match self {
            Self::Audio(_) => {
                return Err(Error::Configuration(
                    "Standard VNC has no audio control message".into(),
                ));
            }
            Self::Scroll {
                x,
                y,
                buttons,
                steps,
            } => {
                for pointer in steps.pointers(x, y, buttons) {
                    pointer.write(writer)?;
                }
                return Ok(());
            }
            Self::Configure(settings) => return settings.write_encodings(writer),
            Self::Pointer { x, y, buttons } => {
                writer.write_all(&[5, buttons])?;
                writer.write_all(&x.to_be_bytes())?;
                writer.write_all(&y.to_be_bytes())?;
            }
            Self::Key { keysym, down } => {
                writer.write_all(&[4, u8::from(down), 0, 0])?;
                writer.write_all(&keysym.to_be_bytes())?;
            }
            Self::Refresh { size, incremental } => {
                writer.write_all(&[3, u8::from(incremental), 0, 0, 0, 0])?;
                writer.write_all(&(size.width as u16).to_be_bytes())?;
                writer.write_all(&(size.height as u16).to_be_bytes())?;
            }
        }
        writer.flush()?;
        Ok(())
    }
}

pub const SHIFT: u32 = 0xffe1;
pub const CONTROL: u32 = 0xffe3;
pub const OPTION: u32 = 0xffe9;
pub const COMMAND: u32 = 0xffeb;

pub fn character_keysym(character: char) -> u32 {
    let code = character as u32;
    if code <= 255 {
        code
    } else {
        0x0100_0000 | code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAL_SCROLL: ScrollScale = ScrollScale {
        horizontal: 3,
        vertical: 3,
    };

    #[test]
    fn wheel_magnitude_and_windows_sensitivity_are_preserved() {
        let mut scroll = ScrollAccumulator::default();
        scroll.add(0.0, 60.0, NORMAL_SCROLL).unwrap();
        assert_eq!(scroll.drain(), ScrollSteps::new(0, 3).unwrap());
        scroll.add(0.0, -180.0, NORMAL_SCROLL).unwrap();
        assert_eq!(scroll.drain(), ScrollSteps::new(0, -9).unwrap());
    }

    #[test]
    fn high_resolution_wheel_and_touchpad_fractions_are_not_discarded() {
        let mut scroll = ScrollAccumulator::default();
        let mut total = 0;
        for _ in 0..4 {
            scroll.add(0.0, 15.0, NORMAL_SCROLL).unwrap();
            total += scroll.drain().vertical;
        }
        assert_eq!(total, 3);
        for _ in 0..20 {
            scroll.add(1.0, 0.0, NORMAL_SCROLL).unwrap();
        }
        assert_eq!(scroll.drain(), ScrollSteps::new(1, 0).unwrap());
    }

    #[test]
    fn accelerated_diagonal_scroll_is_drained_fairly_in_bounded_batches() {
        let mut scroll = ScrollAccumulator::default();
        scroll.add(600.0, 600.0, NORMAL_SCROLL).unwrap();
        let mut total = (0, 0);
        loop {
            let steps = scroll.drain();
            if steps.is_empty() {
                break;
            }
            assert!(steps.horizontal > 0 && steps.vertical > 0);
            assert!(
                u16::from(steps.horizontal.unsigned_abs())
                    + u16::from(steps.vertical.unsigned_abs())
                    <= SCROLL_BATCH
            );
            total.0 += i32::from(steps.horizontal);
            total.1 += i32::from(steps.vertical);
        }
        assert_eq!(total, (30, 30));
    }

    #[test]
    fn disabled_and_page_preferences_are_handled_explicitly() {
        let mut scroll = ScrollAccumulator::default();
        scroll
            .add(
                60.0,
                60.0,
                ScrollScale {
                    horizontal: 0,
                    vertical: u32::MAX,
                },
            )
            .unwrap();
        assert_eq!(scroll.drain(), ScrollSteps::new(0, 16).unwrap());
        assert_eq!(scroll.drain(), ScrollSteps::new(0, 4).unwrap());
        assert!(scroll.drain().is_empty());
    }

    #[test]
    fn release_clears_pending_scroll_and_invalid_input_cannot_fill_the_queue() {
        let mut scroll = ScrollAccumulator::default();
        scroll.add(60.0, 60.0, NORMAL_SCROLL).unwrap();
        scroll.clear();
        assert!(scroll.drain().is_empty());
        for delta in [f32::NAN, f32::INFINITY, f32::MAX] {
            assert!(scroll.add(delta, delta, NORMAL_SCROLL).is_err());
            assert!(scroll.drain().is_empty());
        }
        assert!(ScrollSteps::new(-128, -128).is_err());
        assert!(ScrollSteps::new(8, 9).is_err());
    }

    #[test]
    fn scroll_packets_keep_position_and_held_buttons_on_every_release() {
        let mut bytes = Vec::new();
        Command::Scroll {
            x: 400,
            y: 300,
            buttons: 4,
            steps: ScrollSteps::new(-2, 3).unwrap(),
        }
        .write(&mut bytes)
        .unwrap();
        assert_eq!(bytes.len(), 5 * 12);
        for (index, pair) in bytes.chunks_exact(12).enumerate() {
            assert_eq!(
                pair,
                [
                    5,
                    4 | if index < 3 { 8 } else { 64 },
                    1,
                    144,
                    1,
                    44,
                    5,
                    4,
                    1,
                    144,
                    1,
                    44,
                ]
            );
        }
    }

    #[test]
    fn reads_current_windows_scroll_settings_without_changing_them() {
        ScrollScale::system().unwrap();
    }

    #[test]
    fn pointer_and_key_packets_are_network_order() {
        let mut bytes = Vec::new();
        Command::Pointer {
            x: 3839,
            y: 2159,
            buttons: 1,
        }
        .write(&mut bytes)
        .unwrap();
        assert_eq!(bytes, [5, 1, 0x0e, 0xff, 0x08, 0x6f]);
        bytes.clear();
        Command::Key {
            keysym: COMMAND,
            down: true,
        }
        .write(&mut bytes)
        .unwrap();
        assert_eq!(bytes, [4, 1, 0, 0, 0, 0, 0xff, 0xeb]);
    }

    #[test]
    fn refresh_never_changes_remote_resolution() {
        let mut bytes = Vec::new();
        Command::Refresh {
            size: Extent::new(3840, 2160).unwrap(),
            incremental: true,
        }
        .write(&mut bytes)
        .unwrap();
        assert_eq!(bytes, [3, 1, 0, 0, 0, 0, 0x0f, 0, 0x08, 0x70]);
    }

    #[test]
    fn unicode_keys_use_the_rfb_unicode_namespace() {
        assert_eq!(character_keysym('c'), 0x63);
        assert_eq!(character_keysym('λ'), 0x0100_03bb);
    }
}
