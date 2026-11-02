use std::io::Write;

use crate::{error::Result, geometry::Extent, streaming::StreamSettings};

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Pointer { x: u16, y: u16, buttons: u8 },
    Key { keysym: u32, down: bool },
    Refresh { size: Extent, incremental: bool },
    Configure(StreamSettings),
}

impl Command {
    pub fn write(self, writer: &mut impl Write) -> Result<()> {
        match self {
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
