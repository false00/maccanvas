use std::{
    collections::HashSet,
    io::{Cursor, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

use flate2::{Decompress, FlushDecompress, Status};

mod compression;

use crate::{
    auth::{self, Credentials},
    error::{Error, Result},
    framebuffer::Framebuffer,
    geometry::{Extent, Rect},
    input::Command,
    profiles::{AuthMode, Profile},
    streaming::StreamSettings,
};

pub const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
const MAX_COMPRESSED: usize = 64 * 1024 * 1024;

pub struct ServerInfo {
    pub size: Extent,
    pub name: String,
}

fn byte(reader: &mut impl Read) -> Result<u8> {
    let mut bytes = [0; 1];
    reader.read_exact(&mut bytes)?;
    Ok(bytes[0])
}

fn u16_be(reader: &mut impl Read) -> Result<u16> {
    let mut bytes = [0; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}

fn u32_be(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}

fn skip(reader: &mut impl Read, mut length: usize) -> Result<()> {
    let mut buffer = [0; 4096];
    while length > 0 {
        let count = length.min(buffer.len());
        reader.read_exact(&mut buffer[..count])?;
        length -= count;
    }
    Ok(())
}

fn bounded_bytes(reader: &mut impl Read, length: usize, maximum: usize) -> Result<Vec<u8>> {
    if length > maximum {
        return Err(Error::Protocol(
            "Server message exceeds its size limit".into(),
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn connect(profile: &Profile) -> Result<TcpStream> {
    profile.validate()?;
    let addresses: Vec<_> = (profile.host.as_str(), profile.port)
        .to_socket_addrs()?
        .take(8)
        .collect();
    if addresses.is_empty() {
        return Err(Error::Protocol(
            "The host did not resolve to an address".into(),
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = None;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, remaining.min(Duration::from_secs(5))) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                stream.set_read_timeout(Some(Duration::from_secs(10)))?;
                stream.set_write_timeout(Some(Duration::from_secs(3)))?;
                return Ok(stream);
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, "Connection timed out")
        })
        .into())
}

pub fn handshake(
    stream: &mut (impl Read + Write),
    mode: AuthMode,
    credentials: &Credentials,
) -> Result<ServerInfo> {
    handshake_with_settings(stream, mode, credentials, StreamSettings::default())
}

pub fn handshake_with_settings(
    stream: &mut (impl Read + Write),
    mode: AuthMode,
    credentials: &Credentials,
    settings: StreamSettings,
) -> Result<ServerInfo> {
    settings.validate()?;
    if settings.encoding.is_hevc() {
        return Err(Error::Configuration(
            "HEVC requires the native Apple High Performance transport".into(),
        ));
    }
    credentials.validate(mode)?;
    let mut version = [0; 12];
    stream.read_exact(&mut version)?;
    if &version[..4] != b"RFB " || version[7] != b'.' || version[11] != b'\n' {
        return Err(Error::Protocol("The endpoint is not a VNC server".into()));
    }
    let major = std::str::from_utf8(&version[4..7])
        .ok()
        .and_then(|value| value.parse::<u16>().ok());
    let minor = std::str::from_utf8(&version[8..11])
        .ok()
        .and_then(|value| value.parse::<u16>().ok());
    if major != Some(3) || minor.is_none_or(|value| value < 3) {
        return Err(Error::Protocol("Unsupported RFB version".into()));
    }
    let negotiated = if minor.unwrap() >= 8 {
        8
    } else if minor.unwrap() >= 7 {
        7
    } else {
        3
    };
    stream.write_all(match negotiated {
        8 => b"RFB 003.008\n",
        7 => b"RFB 003.007\n",
        _ => b"RFB 003.003\n",
    })?;
    stream.flush()?;
    let wanted = match mode {
        AuthMode::MacLogin => 30,
        AuthMode::VncPassword => 2,
    };
    let supported = if negotiated == 3 {
        u32_be(stream)? == u32::from(wanted)
    } else {
        let count = usize::from(byte(stream)?);
        if count == 0 {
            let length = u32_be(stream)? as usize;
            bounded_bytes(stream, length, 4096)?;
            return Err(Error::Authentication(
                "The Mac refused this connection".into(),
            ));
        }
        let types = bounded_bytes(stream, count, 255)?;
        if types.contains(&wanted) {
            stream.write_all(&[wanted])?;
            stream.flush()?;
            true
        } else {
            false
        }
    };
    if !supported {
        return Err(Error::Authentication(match mode {
            AuthMode::MacLogin => "The server does not offer macOS username/password login. Enable Screen Sharing, or explicitly choose VNC password mode if configured.",
            AuthMode::VncPassword => "The server does not offer classic VNC password authentication.",
        }.into()));
    }
    match mode {
        AuthMode::MacLogin => auth::mac_login(stream, credentials)?,
        AuthMode::VncPassword => auth::vnc_password(stream, credentials)?,
    }
    let result = u32_be(stream)?;
    if result != 0 {
        if negotiated >= 8 {
            let length = u32_be(stream)? as usize;
            bounded_bytes(stream, length, 4096)?;
        }
        return Err(Error::Authentication(
            "Login was rejected. Check the Mac account, password, and Screen Sharing permissions."
                .into(),
        ));
    }
    stream.write_all(&[1])?;
    stream.flush()?;
    let size = Extent::new(u32::from(u16_be(stream)?), u32::from(u16_be(stream)?))?;
    skip(stream, 16)?;
    let name_length = u32_be(stream)? as usize;
    let name = String::from_utf8_lossy(&bounded_bytes(stream, name_length, 4096)?).into_owned();
    stream.write_all(&[0, 0, 0, 0])?;
    stream.write_all(&PIXEL_FORMAT)?;
    settings.write_encodings(stream)?;
    Command::Refresh {
        size,
        incremental: false,
    }
    .write(stream)?;
    Ok(ServerInfo { size, name })
}

pub enum Message {
    Update { changed: bool },
    Clipboard(crate::clipboard::Packet),
    Other,
}

pub struct Decoder {
    pub frame: Framebuffer,
    zrle: Decompress,
    compression: compression::CompressionDecoder,
    pub last_encoding: Option<i32>,
}

impl Decoder {
    pub fn new(size: Extent) -> Self {
        Self {
            frame: Framebuffer::new(size),
            zrle: Decompress::new(true),
            compression: compression::CompressionDecoder::new(),
            last_encoding: None,
        }
    }

    pub fn read_message(&mut self, reader: &mut impl Read) -> Result<Message> {
        self.read_message_with_clipboard(reader, || false)
    }

    pub fn read_message_with_clipboard(
        &mut self,
        reader: &mut impl Read,
        shared: impl Fn() -> bool,
    ) -> Result<Message> {
        match byte(reader)? {
            0 => self.update(reader),
            1 => {
                skip(reader, 1)?;
                u16_be(reader)?;
                let colors = usize::from(u16_be(reader)?);
                skip(reader, colors * 6)?;
                Ok(Message::Other)
            }
            2 => Ok(Message::Other),
            3 => {
                skip(reader, 3)?;
                let length = u32_be(reader)? as i32;
                Ok(Message::Clipboard(
                    crate::clipboard::read_packet_with_sharing(reader, length, shared())?,
                ))
            }
            _ => Err(Error::Protocol("Unsupported server message".into())),
        }
    }

    fn update(&mut self, reader: &mut impl Read) -> Result<Message> {
        skip(reader, 1)?;
        let count = usize::from(u16_be(reader)?);
        let mut changed = false;
        for index in 0..count {
            if index >= 4096 {
                return Err(Error::Protocol("Too many rectangles in one update".into()));
            }
            let rect = Rect {
                x: u32::from(u16_be(reader)?),
                y: u32::from(u16_be(reader)?),
                width: u32::from(u16_be(reader)?),
                height: u32::from(u16_be(reader)?),
            };
            let encoding = u32_be(reader)? as i32;
            if matches!(encoding, 0 | 1 | 2 | 4 | 5 | 6 | 7 | 16) {
                self.last_encoding = Some(encoding);
            }
            match encoding {
                -224 => break,
                -223 => {
                    self.frame.resize(Extent::new(rect.width, rect.height)?);
                    changed = true;
                }
                -308 => {
                    let screens = usize::from(byte(reader)?);
                    skip(reader, 3)?;
                    let size = Extent::new(rect.width, rect.height)?;
                    let mut identifiers = HashSet::new();
                    for _ in 0..screens {
                        let id = u32_be(reader)?;
                        let screen = Rect {
                            x: u32::from(u16_be(reader)?),
                            y: u32::from(u16_be(reader)?),
                            width: u32::from(u16_be(reader)?),
                            height: u32::from(u16_be(reader)?),
                        };
                        u32_be(reader)?;
                        screen.validate(size)?;
                        if !identifiers.insert(id) {
                            return Err(Error::Protocol(
                                "Duplicate remote display identifier".into(),
                            ));
                        }
                    }
                    if screens == 0 || (rect.x == 1 && rect.y != 0) {
                        return Err(Error::Protocol("Invalid remote display layout".into()));
                    }
                    if self.frame.size != size {
                        self.frame.resize(size);
                        changed = true;
                    }
                }
                -239 | -240 => {
                    if rect.width > 512 || rect.height > 512 {
                        return Err(Error::Protocol("Remote cursor is too large".into()));
                    }
                    let mask = rect.width.div_ceil(8) as usize * rect.height as usize;
                    let length = if encoding == -239 {
                        rect.width as usize * rect.height as usize * 4 + mask
                    } else if rect.width > 0 && rect.height > 0 {
                        6 + mask * 2
                    } else {
                        0
                    };
                    // Native Windows cursors stay visible and are never scaled with the desktop.
                    skip(reader, length)?;
                }
                0 => {
                    rect.validate(self.frame.size)?;
                    let bytes = bounded_bytes(
                        reader,
                        rect.width as usize * rect.height as usize * 4,
                        self.frame.size.bytes(),
                    )?;
                    self.frame.raw_bgra(rect, &bytes)?;
                    changed = true;
                }
                1 => {
                    let x = u32::from(u16_be(reader)?);
                    let y = u32::from(u16_be(reader)?);
                    self.frame.copy(rect, x, y)?;
                    changed = true;
                }
                5 => {
                    rect.validate(self.frame.size)?;
                    self.hextile(reader, rect)?;
                    changed = true;
                }
                16 => {
                    rect.validate(self.frame.size)?;
                    self.zrle(reader, rect)?;
                    changed = true;
                }
                2 | 4 | 6 | 7 => {
                    self.compression
                        .decode(reader, rect, &mut self.frame, encoding)?;
                    changed = true;
                }
                _ => {
                    return Err(Error::Protocol(format!(
                        "Server sent unrequested encoding {encoding}"
                    )));
                }
            }
        }
        Ok(Message::Update { changed })
    }

    fn hextile(&mut self, reader: &mut impl Read, rect: Rect) -> Result<()> {
        let mut background = None;
        let mut foreground = None;
        for y in (0..rect.height).step_by(16) {
            for x in (0..rect.width).step_by(16) {
                let tile = Rect {
                    x: rect.x + x,
                    y: rect.y + y,
                    width: 16.min(rect.width - x),
                    height: 16.min(rect.height - y),
                };
                let flags = byte(reader)?;
                if flags & 0xe0 != 0 {
                    return Err(Error::Protocol("Invalid Hextile flags".into()));
                }
                if flags & 1 != 0 {
                    let data = bounded_bytes(
                        reader,
                        tile.width as usize * tile.height as usize * 4,
                        1024,
                    )?;
                    self.frame.raw_bgra(tile, &data)?;
                    continue;
                }
                if flags & 2 != 0 {
                    background = Some(pixel(reader, 4)?);
                }
                if flags & 4 != 0 {
                    foreground = Some(pixel(reader, 4)?);
                }
                self.frame.fill(
                    tile,
                    background
                        .ok_or_else(|| Error::Protocol("Hextile background is missing".into()))?,
                )?;
                if flags & 8 != 0 {
                    let count = byte(reader)?;
                    for _ in 0..count {
                        let color = if flags & 16 != 0 {
                            pixel(reader, 4)?
                        } else {
                            foreground.ok_or_else(|| {
                                Error::Protocol("Hextile foreground is missing".into())
                            })?
                        };
                        let position = byte(reader)?;
                        let dimensions = byte(reader)?;
                        let sub = Rect {
                            x: u32::from(position >> 4),
                            y: u32::from(position & 15),
                            width: u32::from(dimensions >> 4) + 1,
                            height: u32::from(dimensions & 15) + 1,
                        };
                        sub.validate(Extent::new(tile.width, tile.height)?)?;
                        self.frame.fill(
                            Rect {
                                x: tile.x + sub.x,
                                y: tile.y + sub.y,
                                ..sub
                            },
                            color,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    fn zrle(&mut self, reader: &mut impl Read, rect: Rect) -> Result<()> {
        let compressed_length = u32_be(reader)? as usize;
        let compressed = bounded_bytes(reader, compressed_length, MAX_COMPRESSED)?;
        let tile_count = rect.width.div_ceil(64) as usize * rect.height.div_ceil(64) as usize;
        let limit = rect.width as usize * rect.height as usize * 5 + tile_count * 384;
        let mut decoded = vec![0; limit + 1];
        let before_in = self.zrle.total_in();
        let before_out = self.zrle.total_out();
        let status = self
            .zrle
            .decompress(&compressed, &mut decoded, FlushDecompress::Sync)
            .map_err(|error| Error::Protocol(format!("Invalid ZRLE stream: {error}")))?;
        let consumed = (self.zrle.total_in() - before_in) as usize;
        let produced = (self.zrle.total_out() - before_out) as usize;
        if consumed != compressed.len() || produced > limit {
            return Err(Error::Protocol(
                "ZRLE output exceeds its rectangle limit".into(),
            ));
        }
        decoded.truncate(produced);
        if status == Status::StreamEnd {
            self.zrle.reset(true);
        }
        let mut input = Cursor::new(decoded);
        for y in (0..rect.height).step_by(64) {
            for x in (0..rect.width).step_by(64) {
                let tile = Rect {
                    x: rect.x + x,
                    y: rect.y + y,
                    width: 64.min(rect.width - x),
                    height: 64.min(rect.height - y),
                };
                let mode = byte(&mut input)?;
                let count = tile.width as usize * tile.height as usize;
                let mut pixels = Vec::<[u8; 4]>::with_capacity(count);
                match mode {
                    0 => {
                        for _ in 0..count {
                            pixels.push(pixel(&mut input, 3)?);
                        }
                    }
                    1 => pixels.resize(count, pixel(&mut input, 3)?),
                    2..=16 => {
                        let palette = palette(&mut input, usize::from(mode))?;
                        let bits = if mode == 2 {
                            1
                        } else if mode <= 4 {
                            2
                        } else {
                            4
                        };
                        for _ in 0..tile.height {
                            let row = bounded_bytes(
                                &mut input,
                                (tile.width as usize * bits).div_ceil(8),
                                32,
                            )?;
                            for column in 0..tile.width as usize {
                                let index = (row[column * bits / 8]
                                    >> (8 - bits - column * bits % 8))
                                    & ((1 << bits) - 1);
                                pixels.push(*palette.get(usize::from(index)).ok_or_else(|| {
                                    Error::Protocol("Invalid ZRLE palette index".into())
                                })?);
                            }
                        }
                    }
                    128 => {
                        while pixels.len() < count {
                            let color = pixel(&mut input, 3)?;
                            let length = run_length(&mut input, count - pixels.len())?;
                            pixels.resize(pixels.len() + length, color);
                        }
                    }
                    129..=255 => {
                        let palette = palette(&mut input, usize::from(mode & 127))?;
                        while pixels.len() < count {
                            let index = byte(&mut input)?;
                            let color =
                                *palette.get(usize::from(index & 127)).ok_or_else(|| {
                                    Error::Protocol("Invalid ZRLE palette index".into())
                                })?;
                            let length = if index & 128 != 0 {
                                run_length(&mut input, count - pixels.len())?
                            } else {
                                1
                            };
                            pixels.resize(pixels.len() + length, color);
                        }
                    }
                    _ => return Err(Error::Protocol("Unsupported ZRLE tile mode".into())),
                }
                let bytes: Vec<u8> = pixels.into_iter().flatten().collect();
                self.frame.write_rgba(tile, &bytes)?;
            }
        }
        if input.position() as usize != input.get_ref().len() {
            return Err(Error::Protocol("Trailing data in ZRLE rectangle".into()));
        }
        Ok(())
    }
}

fn pixel(reader: &mut impl Read, size: usize) -> Result<[u8; 4]> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes[..size])?;
    Ok([bytes[2], bytes[1], bytes[0], 255])
}

fn palette(reader: &mut impl Read, count: usize) -> Result<Vec<[u8; 4]>> {
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        result.push(pixel(reader, 3)?);
    }
    Ok(result)
}

fn run_length(reader: &mut impl Read, remaining: usize) -> Result<usize> {
    let mut length = 1usize;
    loop {
        let value = byte(reader)?;
        length = length
            .checked_add(usize::from(value))
            .ok_or_else(|| Error::Protocol("ZRLE run length overflow".into()))?;
        if length > remaining {
            return Err(Error::Protocol("ZRLE run exceeds the tile".into()));
        }
        if value != 255 {
            return Ok(length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};

    fn rectangle(encoding: i32, payload: &[u8]) -> Cursor<Vec<u8>> {
        let mut bytes = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 2];
        bytes.extend_from_slice(&encoding.to_be_bytes());
        bytes.extend_from_slice(payload);
        Cursor::new(bytes)
    }

    #[test]
    fn raw_frames_convert_channels_correctly() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        let mut message = rectangle(0, &[1, 2, 3, 0].repeat(4));
        assert!(matches!(
            decoder.read_message(&mut message).unwrap(),
            Message::Update { changed: true }
        ));
        assert_eq!(&decoder.frame.rgba[..4], &[3, 2, 1, 255]);
    }

    #[test]
    fn hextile_solid_and_subrectangles() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        let payload = [2 | 4 | 8, 0, 0, 255, 0, 255, 0, 0, 0, 1, 0x11, 0];
        decoder.read_message(&mut rectangle(5, &payload)).unwrap();
        assert_eq!(&decoder.frame.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&decoder.frame.rgba[12..16], &[0, 0, 255, 255]);
    }

    #[test]
    fn zrle_solid_tile_is_bounded_and_opaque() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&[1, 3, 2, 1]).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut payload = (compressed.len() as u32).to_be_bytes().to_vec();
        payload.extend(compressed);
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        decoder.read_message(&mut rectangle(16, &payload)).unwrap();
        assert_eq!(decoder.frame.rgba, [1, 2, 3, 255].repeat(4));
    }

    fn zrle_message(decoded: &[u8]) -> Cursor<Vec<u8>> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(decoded).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut payload = (compressed.len() as u32).to_be_bytes().to_vec();
        payload.extend(compressed);
        rectangle(16, &payload)
    }

    #[test]
    fn zrle_plain_and_palette_runs_decode_exactly() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        decoder
            .read_message(&mut zrle_message(&[128, 3, 2, 1, 3]))
            .unwrap();
        assert_eq!(decoder.frame.rgba, [1, 2, 3, 255].repeat(4));
        decoder
            .read_message(&mut zrle_message(&[
                130, 0, 0, 255, 0, 255, 0, 128, 1, 129, 1,
            ]))
            .unwrap();
        assert_eq!(&decoder.frame.rgba[..8], &[255, 0, 0, 255].repeat(2));
        assert_eq!(&decoder.frame.rgba[8..], &[0, 255, 0, 255].repeat(2));
    }

    #[test]
    fn zrle_packed_palette_is_aligned_per_row() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        decoder
            .read_message(&mut zrle_message(&[
                2,
                0,
                0,
                255,
                0,
                255,
                0,
                0b0100_0000,
                0b1000_0000,
            ]))
            .unwrap();
        assert_eq!(
            decoder.frame.rgba,
            [
                255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255, 255, 0, 0, 255,
            ]
        );
    }

    #[test]
    fn zrle_rejects_runs_that_exceed_the_destination() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        assert!(
            decoder
                .read_message(&mut zrle_message(&[128, 3, 2, 1, 255]))
                .is_err()
        );
    }

    #[test]
    fn hextile_rejects_out_of_tile_subrectangles() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        let payload = [2 | 4 | 8, 0, 0, 255, 0, 255, 0, 0, 0, 1, 0xff, 0xff];
        assert!(decoder.read_message(&mut rectangle(5, &payload)).is_err());
    }

    #[test]
    fn remote_resize_replaces_frame_dimensions_not_the_local_window() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        let mut message = vec![0, 0, 0, 1, 0, 0, 0, 0, 0x0a, 0, 0x06, 0x40];
        message.extend_from_slice(&(-223i32).to_be_bytes());
        decoder.read_message(&mut Cursor::new(message)).unwrap();
        assert_eq!(decoder.frame.size, Extent::new(2560, 1600).unwrap());
    }

    #[test]
    fn oversized_cursor_is_rejected_without_allocation() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        let mut message = vec![0, 0, 0, 1, 0, 0, 0, 0, 0x04, 0, 0x04, 0];
        message.extend_from_slice(&(-239i32).to_be_bytes());
        assert!(decoder.read_message(&mut Cursor::new(message)).is_err());
    }

    #[test]
    fn cursor_updates_do_not_draw_or_shrink_the_local_pointer() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        let before = decoder.frame.rgba.clone();
        decoder
            .read_message(&mut rectangle(-239, &[0; 18]))
            .unwrap();
        assert_eq!(decoder.frame.rgba, before);
    }

    #[test]
    fn unknown_unrequested_encodings_are_explicit_errors() {
        let mut decoder = Decoder::new(Extent::new(2, 2).unwrap());
        assert!(decoder.read_message(&mut rectangle(123456, &[])).is_err());
    }
}
