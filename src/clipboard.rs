pub mod native;

use std::io::{Read, Write};

use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use zeroize::Zeroizing;

use crate::error::{Error, Result};

pub const ENCODING: i32 = i32::from_be_bytes([0xc0, 0xa1, 0xe5, 0xce]);
pub const MAX_TEXT: usize = 1024 * 1024;
const TEXT: u32 = 1;
const CAPS: u32 = 1 << 24;
const REQUEST: u32 = 1 << 25;
const PEEK: u32 = 1 << 26;
const NOTIFY: u32 = 1 << 27;
const PROVIDE: u32 = 1 << 28;
const ACTIONS: u32 = 0xff00_0000;
const FORMATS: u32 = 0xffff;

pub enum Packet {
    Text {
        text: Zeroizing<String>,
        extended: bool,
        wire_bytes: usize,
    },
    Caps {
        flags: u32,
        text_limit: u32,
    },
    Request(u32),
    Peek,
    Notify(u32),
    Ignored {
        wire_bytes: usize,
        reason: &'static str,
    },
}

pub enum Notice {
    Text(Zeroizing<String>),
    Mode(bool),
    Received {
        bytes: usize,
        encoding: &'static str,
        accepted: bool,
    },
    Sent {
        bytes: usize,
        encoding: &'static str,
        announced: bool,
    },
    Ignored {
        bytes: usize,
        reason: &'static str,
    },
}

pub fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

pub fn validate(text: &str, utf8: bool) -> Result<()> {
    if text.len() > MAX_TEXT || text.contains('\0') {
        return Err(Error::Clipboard(
            "Plain text must be at most 1 MiB and cannot contain NUL characters".into(),
        ));
    }
    if !utf8 && text.chars().any(|character| u32::from(character) > 255) {
        return Err(Error::Clipboard("This server has not negotiated UTF-8 clipboard support. Standard VNC supports Latin-1 text only; this text was not sent or replaced.".into()));
    }
    Ok(())
}

pub fn validate_vnc(text: &str, utf8: bool) -> Result<()> {
    validate(text, utf8)?;
    if utf8 {
        let normalized = normalize(text);
        if normalized
            .len()
            .saturating_add(normalized.bytes().filter(|&byte| byte == b'\n').count())
            .saturating_add(1)
            > MAX_TEXT
        {
            return Err(Error::Clipboard(
                "UTF-8 VNC text exceeds 1 MiB after line-ending conversion".into(),
            ));
        }
    }
    Ok(())
}

fn protocol(message: &str) -> Error {
    Error::Protocol(format!("Clipboard: {message}"))
}

pub fn read_packet(reader: &mut impl Read, length: i32) -> Result<Packet> {
    read_packet_with_sharing(reader, length, true)
}

pub fn read_packet_with_sharing(
    reader: &mut impl Read,
    length: i32,
    shared: bool,
) -> Result<Packet> {
    let size = length.unsigned_abs() as usize;
    if size > MAX_TEXT {
        return Err(protocol("message exceeds the 1 MiB limit"));
    }
    let mut bytes = Zeroizing::new(vec![0_u8; size]);
    reader.read_exact(&mut bytes)?;
    if length >= 0 {
        if !shared {
            return Ok(Packet::Ignored {
                wire_bytes: size,
                reason: "sharing_disabled",
            });
        }
        let text: String = bytes.iter().map(|&byte| char::from(byte)).collect();
        if text.contains('\0') {
            return Ok(Packet::Ignored {
                wire_bytes: size,
                reason: "embedded_nul_not_supported",
            });
        }
        return Ok(Packet::Text {
            text: Zeroizing::new(normalize(&text)),
            extended: false,
            wire_bytes: size,
        });
    }
    if size < 4 {
        if !shared {
            return Ok(Packet::Ignored {
                wire_bytes: size,
                reason: "sharing_disabled",
            });
        }
        return Err(protocol("extended header is truncated"));
    }
    let flags = u32::from_be_bytes(bytes[..4].try_into().expect("checked header length"));
    if !shared
        && (flags & CAPS == 0
            || flags & 0x00ff_0000 != 0
            || size != 4 + (flags & FORMATS).count_ones() as usize * 4)
    {
        return Ok(Packet::Ignored {
            wire_bytes: size,
            reason: "sharing_disabled",
        });
    }
    if flags & 0x00ff_0000 != 0 {
        return Err(protocol("reserved flag bits are set"));
    }
    let formats = flags & FORMATS;
    if flags & CAPS != 0 {
        if size != 4 + formats.count_ones() as usize * 4 {
            if !shared {
                return Ok(Packet::Ignored {
                    wire_bytes: size,
                    reason: "invalid_capabilities_ignored",
                });
            }
            return Err(protocol("capability sizes are invalid"));
        }
        let text_limit = if formats & TEXT != 0 {
            u32::from_be_bytes(bytes[4..8].try_into().expect("checked capability sizes"))
        } else {
            0
        };
        return Ok(Packet::Caps { flags, text_limit });
    }
    if (flags & ACTIONS).count_ones() != 1 {
        return Err(protocol("exactly one action is required"));
    }
    match flags & ACTIONS {
        REQUEST | PEEK | NOTIFY if size != 4 => {
            Err(protocol("control message contains unexpected data"))
        }
        REQUEST => Ok(Packet::Request(formats)),
        PEEK => Ok(Packet::Peek),
        NOTIFY => Ok(Packet::Notify(formats)),
        PROVIDE => {
            let mut decompressed = Zeroizing::new(vec![0_u8; MAX_TEXT + 16 * 4]);
            let mut inflater = Decompress::new(true);
            let status = inflater
                .decompress(&bytes[4..], &mut decompressed, FlushDecompress::Finish)
                .map_err(|_| protocol("compressed data is invalid"))?;
            if status != Status::StreamEnd || inflater.total_in() as usize != size - 4 {
                return Err(protocol(
                    "compressed data is truncated, excessive, or has trailing data",
                ));
            }
            decompressed.truncate(inflater.total_out() as usize);
            let mut position = 0;
            let mut text = None;
            for bit in 0..16 {
                if formats & (1 << bit) == 0 {
                    continue;
                }
                let end = position + 4;
                let header = decompressed
                    .get(position..end)
                    .ok_or_else(|| protocol("format length is truncated"))?;
                let length =
                    u32::from_be_bytes(header.try_into().expect("four-byte header")) as usize;
                position = end;
                let end = position
                    .checked_add(length)
                    .filter(|&end| end <= decompressed.len())
                    .ok_or_else(|| protocol("format payload exceeds its bounds"))?;
                if bit == 0 {
                    let value = decompressed[position..end]
                        .strip_suffix(&[0])
                        .ok_or_else(|| protocol("UTF-8 text has no terminator"))?;
                    let value = std::str::from_utf8(value)
                        .map_err(|_| protocol("text is not valid UTF-8"))?;
                    if value.contains('\0') {
                        return Err(protocol("UTF-8 text contains an embedded terminator"));
                    }
                    text = Some(Zeroizing::new(normalize(value)));
                }
                position = end;
            }
            if position != decompressed.len() {
                return Err(protocol("format payload has trailing data"));
            }
            Ok(match text {
                Some(text) => Packet::Text {
                    text,
                    extended: true,
                    wire_bytes: size,
                },
                None => Packet::Ignored {
                    wire_bytes: size,
                    reason: "unsupported_format",
                },
            })
        }
        _ => Err(protocol("unsupported extended action")),
    }
}

fn send(writer: &mut impl Write, payload: &[u8], extended: bool) -> Result<()> {
    if payload.len() > MAX_TEXT {
        return Err(Error::Clipboard(
            "Encoded clipboard text exceeds 1 MiB".into(),
        ));
    }
    let length = i32::try_from(payload.len()).expect("bounded clipboard payload");
    writer.write_all(&[6, 0, 0, 0])?;
    writer.write_all(&(if extended { -length } else { length }).to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn control(writer: &mut impl Write, flags: u32) -> Result<()> {
    send(writer, &flags.to_be_bytes(), true)
}

fn provide(writer: &mut impl Write, text: &str) -> Result<()> {
    let text = Zeroizing::new(normalize(text).replace('\n', "\r\n"));
    validate(&text, true)?;
    let length = text
        .len()
        .checked_add(1)
        .filter(|&size| size <= MAX_TEXT)
        .ok_or_else(|| Error::Clipboard("Encoded clipboard text exceeds 1 MiB".into()))?;
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(&(length as u32).to_be_bytes())?;
    encoder.write_all(text.as_bytes())?;
    encoder.write_all(&[0])?;
    let compressed = Zeroizing::new(encoder.finish()?);
    let mut payload = Zeroizing::new((PROVIDE | TEXT).to_be_bytes().to_vec());
    payload.extend_from_slice(&compressed);
    send(writer, &payload, true)
}

pub struct Transport {
    enabled: bool,
    caps: Option<(u32, u32)>,
    local: Option<Zeroizing<String>>,
}

impl Transport {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            caps: None,
            local: None,
        }
    }

    pub fn utf8(&self) -> bool {
        self.caps
            .is_some_and(|(flags, _)| flags & (TEXT | PROVIDE) == TEXT | PROVIDE)
    }

    pub fn enable(&mut self, writer: &mut impl Write, enabled: bool) -> Result<()> {
        self.enabled = enabled;
        if !enabled {
            self.local = None;
            if self.caps.is_some_and(|(flags, _)| flags & NOTIFY != 0) {
                control(writer, NOTIFY)?;
            }
        }
        Ok(())
    }

    pub fn local(
        &mut self,
        writer: &mut impl Write,
        text: Zeroizing<String>,
    ) -> Result<Vec<Notice>> {
        if !self.enabled {
            return Err(Error::Clipboard(
                "Text clipboard sharing is disabled".into(),
            ));
        }
        validate_vnc(&text, self.utf8())?;
        let bytes = text.len();
        let mut announced = false;
        if self.utf8() {
            let (flags, limit) = self.caps.expect("UTF-8 is negotiated");
            if bytes.saturating_mul(2).saturating_add(1) <= limit as usize {
                provide(writer, &text)?;
            } else if flags & NOTIFY != 0 {
                control(writer, NOTIFY | TEXT)?;
                announced = true;
            } else {
                return Err(Error::Clipboard(
                    "The server cannot accept this clipboard size using its negotiated actions"
                        .into(),
                ));
            }
            self.local = Some(text);
        } else {
            let bytes = Zeroizing::new(
                normalize(&text)
                    .chars()
                    .map(|character| u8::try_from(u32::from(character)).expect("validated Latin-1"))
                    .collect::<Vec<_>>(),
            );
            send(writer, &bytes, false)?;
        }
        Ok(vec![Notice::Sent {
            bytes,
            encoding: if self.utf8() { "utf8" } else { "latin1" },
            announced,
        }])
    }

    pub fn server(&mut self, writer: &mut impl Write, packet: Packet) -> Result<Vec<Notice>> {
        let mut notices = Vec::new();
        match packet {
            Packet::Caps { flags, text_limit } => {
                self.caps = Some((flags, text_limit.min(MAX_TEXT as u32)));
                let mut caps = (TEXT | CAPS | REQUEST | PEEK | NOTIFY | PROVIDE)
                    .to_be_bytes()
                    .to_vec();
                caps.extend_from_slice(&0_u32.to_be_bytes());
                send(writer, &caps, true)?;
                notices.push(Notice::Mode(self.utf8()));
            }
            Packet::Text {
                text,
                extended,
                wire_bytes,
            } => {
                let accepted = self.enabled
                    && text.len() <= MAX_TEXT
                    && (!extended || self.caps.is_some_and(|(flags, _)| flags & TEXT != 0));
                notices.push(Notice::Received {
                    bytes: wire_bytes,
                    encoding: if extended { "utf8" } else { "latin1" },
                    accepted,
                });
                if accepted {
                    notices.push(Notice::Text(text));
                } else if self.enabled && text.len() > MAX_TEXT {
                    notices.push(Notice::Ignored {
                        bytes: wire_bytes,
                        reason: "decoded_size_limit",
                    });
                }
            }
            Packet::Notify(formats) => {
                if self.enabled
                    && formats & TEXT != 0
                    && self.caps.is_some_and(|(flags, _)| flags & REQUEST != 0)
                {
                    control(writer, REQUEST | TEXT)?;
                } else {
                    notices.push(Notice::Ignored {
                        bytes: 4,
                        reason: "disabled_or_no_plain_text",
                    });
                }
            }
            Packet::Request(formats) => {
                if self.enabled
                    && formats & TEXT != 0
                    && self.utf8()
                    && let Some(text) = &self.local
                {
                    provide(writer, text)?;
                    notices.push(Notice::Sent {
                        bytes: text.len(),
                        encoding: "utf8",
                        announced: false,
                    });
                } else if self.caps.is_some_and(|(flags, _)| flags & NOTIFY != 0) {
                    control(writer, NOTIFY)?;
                }
            }
            Packet::Peek => {
                if self.caps.is_some_and(|(flags, _)| flags & NOTIFY != 0) {
                    control(
                        writer,
                        NOTIFY
                            | if self.enabled && self.local.is_some() {
                                TEXT
                            } else {
                                0
                            },
                    )?;
                }
            }
            Packet::Ignored { wire_bytes, reason } => notices.push(Notice::Ignored {
                bytes: wire_bytes,
                reason,
            }),
        }
        Ok(notices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_sharing_preserves_viewing_when_vendor_clipboard_data_is_opaque() {
        let data = [0xff_u8; 19];
        assert!(matches!(
            read_packet_with_sharing(&mut data.as_slice(), -19, false).unwrap(),
            Packet::Ignored { .. }
        ));
        assert!(matches!(
            read_packet_with_sharing(&mut b"raw\0text".as_slice(), 8, false).unwrap(),
            Packet::Ignored { .. }
        ));
        assert!(read_packet(&mut data.as_slice(), -19).is_err());
        assert!(validate_vnc(&"\n".repeat(MAX_TEXT), true).is_err());
    }

    #[test]
    fn sharing_is_read_after_the_blocking_server_header_not_before_it() {
        use std::{
            io::Cursor,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
        };
        struct Reader {
            bytes: Cursor<Vec<u8>>,
            enabled: Arc<AtomicBool>,
        }
        impl Read for Reader {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                self.enabled.store(true, Ordering::Release);
                self.bytes.read(bytes)
            }
        }
        let enabled = Arc::new(AtomicBool::new(false));
        let mut bytes = vec![3, 0, 0, 0, 0, 0, 0, 4];
        bytes.extend_from_slice(b"text");
        let mut reader = Reader {
            bytes: Cursor::new(bytes),
            enabled: enabled.clone(),
        };
        let mut decoder =
            crate::protocol::Decoder::new(crate::geometry::Extent::new(1, 1).unwrap());
        assert!(matches!(
            decoder
                .read_message_with_clipboard(&mut reader, || enabled.load(Ordering::Acquire))
                .unwrap(),
            crate::protocol::Message::Clipboard(Packet::Text { .. })
        ));
    }

    fn from_wire(bytes: &[u8]) -> Packet {
        assert_eq!(&bytes[..4], &[6, 0, 0, 0]);
        let length = i32::from_be_bytes(bytes[4..8].try_into().unwrap());
        read_packet(&mut &bytes[8..], length).unwrap()
    }

    #[test]
    fn latin1_text_roundtrips_without_lossy_unicode_replacement() {
        let mut transport = Transport::new(true);
        let mut bytes = Vec::new();
        transport
            .local(&mut bytes, Zeroizing::new("Caf\u{e9}\r\nMac".into()))
            .unwrap();
        match from_wire(&bytes) {
            Packet::Text {
                text,
                extended: false,
                ..
            } => assert_eq!(&*text, "Caf\u{e9}\nMac"),
            _ => panic!("expected Latin-1"),
        }
        assert!(
            transport
                .local(&mut Vec::new(), Zeroizing::new("\u{1f34e}".into()))
                .is_err()
        );
    }

    #[test]
    fn negotiated_utf8_is_requested_provided_and_terminated_correctly() {
        let mut transport = Transport::new(true);
        let flags = TEXT | CAPS | REQUEST | PEEK | NOTIFY | PROVIDE;
        let mut caps = Vec::new();
        transport
            .server(
                &mut caps,
                Packet::Caps {
                    flags,
                    text_limit: 0,
                },
            )
            .unwrap();
        assert!(transport.utf8());
        let mut announcement = Vec::new();
        transport
            .local(
                &mut announcement,
                Zeroizing::new("Mac \u{1f34e}\nhello".into()),
            )
            .unwrap();
        assert!(matches!(from_wire(&announcement), Packet::Notify(TEXT)));
        let mut provided = Vec::new();
        transport
            .server(&mut provided, Packet::Request(TEXT))
            .unwrap();
        match from_wire(&provided) {
            Packet::Text {
                text,
                extended: true,
                ..
            } => assert_eq!(&*text, "Mac \u{1f34e}\nhello"),
            _ => panic!("expected UTF-8"),
        }
    }

    #[test]
    fn disabled_sharing_never_delivers_text_or_retains_a_local_copy() {
        let mut transport = Transport::new(false);
        let notices = transport
            .server(
                &mut Vec::new(),
                Packet::Text {
                    text: Zeroizing::new("private".into()),
                    extended: false,
                    wire_bytes: 7,
                },
            )
            .unwrap();
        assert!(
            !notices
                .iter()
                .any(|notice| matches!(notice, Notice::Text(_)))
        );
        assert!(
            transport
                .local(&mut Vec::new(), Zeroizing::new("private".into()))
                .is_err()
        );
    }

    #[test]
    fn malformed_lengths_actions_utf8_and_compression_bombs_are_rejected() {
        assert!(read_packet(&mut &[][..], i32::MIN).is_err());
        assert!(read_packet(&mut &[0; 3][..], -3).is_err());
        assert!(read_packet(&mut &(REQUEST | NOTIFY).to_be_bytes()[..], -4).is_err());
        let mut compressed = ZlibEncoder::new(Vec::new(), Compression::best());
        compressed.write_all(&vec![0; MAX_TEXT + 1024]).unwrap();
        let mut payload = (TEXT | PROVIDE).to_be_bytes().to_vec();
        payload.extend(compressed.finish().unwrap());
        assert!(read_packet(&mut payload.as_slice(), -(payload.len() as i32)).is_err());
        let mut wire = Vec::new();
        provide(&mut wire, "hello").unwrap();
        wire.pop();
        assert!(read_packet(&mut &wire[8..], -((wire.len() - 8) as i32)).is_err());
    }
}
