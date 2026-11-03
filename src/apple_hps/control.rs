// Apple High Performance negotiation adapted from iShareScreen, AGPL-3.0-or-later.
use std::{
    collections::VecDeque,
    io::{Cursor, Read, Write},
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};

use aes::cipher::{BlockEncrypt, KeyInit};
use rsa::{
    Pkcs1v15Encrypt, RsaPublicKey, pkcs1::DecodeRsaPublicKey, pkcs8::DecodePublicKey,
    traits::PublicKeyParts,
};
use zeroize::Zeroizing;

use crate::{
    auth::Credentials,
    error::{Error, Result},
    geometry::Extent,
    input::Command,
    protocol::{Decoder, ServerInfo},
    telemetry::Metered,
};

use super::{crypto::Records, offers};

const ENCODINGS: [i32; 13] = [
    1010, 1011, 1002, 6, 16, 1104, 1100, -223, 1101, 1105, 1107, 1109, 1110,
];

fn failed(message: &str) -> Error {
    Error::Protocol(format!("Apple HEVC: {message}"))
}
fn word(reader: &mut impl Read) -> Result<u16> {
    let mut bytes = [0; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_be_bytes(bytes))
}
fn integer(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_be_bytes(bytes))
}
fn read_bytes(reader: &mut impl Read, size: usize, maximum: usize) -> Result<Vec<u8>> {
    if size > maximum {
        return Err(failed("server message exceeds its limit"));
    }
    let mut data = vec![0; size];
    reader.read_exact(&mut data)?;
    Ok(data)
}

pub(super) fn authenticate(
    stream: &mut (impl Read + Write),
    credentials: &Credentials,
) -> Result<Zeroizing<[u8; 16]>> {
    credentials.validate(crate::profiles::AuthMode::MacLogin)?;
    let mut version = [0; 12];
    stream.read_exact(&mut version)?;
    if &version != b"RFB 003.889\n" {
        return Err(failed(
            "this server does not offer Apple's High Performance protocol",
        ));
    }
    stream.write_all(b"RFB 003.889\n")?;
    let mut count = [0];
    stream.read_exact(&mut count)?;
    if count[0] == 0 {
        return Err(failed("the Mac rejected the protocol connection"));
    }
    let methods = read_bytes(stream, usize::from(count[0]), 255)?;
    if !methods.contains(&33) {
        return Err(failed("Apple RSA account authentication is unavailable"));
    }
    stream.write_all(b"\x21\x00\x00\x00\x0a\x01\x00RSA1\x00\x00\x00\x00")?;
    stream.flush()?;
    let length = integer(stream)? as usize;
    let packet = read_bytes(stream, length, 16_384)?;
    if packet.len() < 6 {
        return Err(failed("truncated RSA authentication challenge"));
    }
    let length = u32::from_be_bytes(packet[2..6].try_into().expect("checked RSA header")) as usize;
    let der = packet
        .get(6..6 + length)
        .ok_or_else(|| failed("invalid RSA public-key length"))?;
    let public = match RsaPublicKey::from_public_key_der(der) {
        Ok(public) => public,
        Err(_) => RsaPublicKey::from_pkcs1_der(der)
            .map_err(|_| failed("invalid RSA authentication key"))?,
    };
    if !(2048..=4096).contains(&public.n().bits()) {
        return Err(failed("unsupported RSA authentication key size"));
    }
    let mut key = Zeroizing::new([0; 16]);
    getrandom::fill(&mut *key).map_err(|_| failed("secure authentication randomness failed"))?;
    let mut credential_data = Zeroizing::new([0; 128]);
    getrandom::fill(&mut *credential_data)
        .map_err(|_| failed("secure credential padding failed"))?;
    let username = credentials.username().as_bytes();
    let password = credentials.password().as_bytes();
    credential_data[..username.len()].copy_from_slice(username);
    credential_data[username.len()] = 0;
    credential_data[64..64 + password.len()].copy_from_slice(password);
    credential_data[64 + password.len()] = 0;
    let cipher = aes::Aes128::new((&*key).into());
    for block in credential_data.as_chunks_mut::<16>().0 {
        cipher.encrypt_block(block.into());
    }
    let wrapped = public
        .encrypt(&mut rand::rngs::OsRng, Pkcs1v15Encrypt, &*key)
        .map_err(|_| failed("could not encrypt Apple authentication key"))?;
    let mut response = Zeroizing::new(Vec::new());
    response.extend_from_slice(b"\x01\x00RSA1\x00\x01");
    response.extend_from_slice(&*credential_data);
    response.extend_from_slice(&[0, 1]);
    response.extend_from_slice(&wrapped);
    stream.write_all(&(response.len() as u32).to_be_bytes())?;
    stream.write_all(&response)?;
    stream.flush()?;
    let _padding = integer(stream)?;
    if integer(stream)? != 0 {
        return Err(Error::Authentication(
            "The Mac rejected this account login for HEVC screen sharing".into(),
        ));
    }
    Ok(key)
}

fn viewer_info() -> Vec<u8> {
    let mut data = vec![0x21, 0, 0, 62, 0, 1];
    for value in [2_u32, 6, 1, 0, 15, 3, 0] {
        data.extend_from_slice(&value.to_be_bytes());
    }
    let mut mask = [0; 32];
    mask[0] = 0xb0;
    mask[2] = 0x0c;
    mask[3] = 3;
    mask[4] = 0x90;
    mask[10] = 0x40;
    data.extend_from_slice(&mask);
    data
}

pub(super) fn set_encodings() -> Vec<u8> {
    let mut data = vec![2, 0, 0, ENCODINGS.len() as u8];
    for encoding in ENCODINGS {
        data.extend_from_slice(&encoding.to_be_bytes());
    }
    data
}

pub(super) fn refresh(_size: Extent) -> Vec<u8> {
    // Cursor metadata is region-independent; full-frame requests disturb UDP video.
    vec![3, 0, 0, 0, 0, 0, 0, 1, 0, 1]
}

pub(super) enum Notice {
    Canvas(Extent),
    Layout { scaled: Extent, backing: Extent },
    Unsupported(i32),
}

pub(super) struct Channel {
    stream: Metered<TcpStream>,
    cipher: Records,
    wire: Vec<u8>,
    output: VecDeque<Vec<u8>>,
    output_offset: usize,
    fallback: Decoder,
}

impl Channel {
    pub fn bootstrap(mut stream: Metered<TcpStream>, key: &[u8; 16]) -> Result<(Self, ServerInfo)> {
        stream.write_all(&[0xc1])?;
        stream.flush()?;
        let width = word(&mut stream)?;
        let height = word(&mut stream)?;
        let size = Extent::new(u32::from(width), u32::from(height))?;
        if size.width * size.height > 16_777_216 {
            return Err(failed("physical display exceeds the HEVC pixel limit"));
        }
        let mut format = [0; 16];
        stream.read_exact(&mut format)?;
        let name_size = integer(&mut stream)? as usize;
        let name =
            crate::protocol::desktop_name(&read_bytes(&mut stream, name_size, 1_048_576)?, true)?;
        stream.write_all(&viewer_info())?;
        stream.write_all(&[0x12, 0, 0, 1, 0, 1, 0, 1, 0, 0, 0, 1])?;
        stream.flush()?;
        thread::sleep(Duration::from_millis(100));
        stream.write_all(&set_encodings())?;
        stream.flush()?;
        let mut cipher = None;
        for _ in 0..32 {
            let mut first = [0];
            stream.read_exact(&mut first)?;
            match first[0] {
                0x14 => {
                    read_bytes(&mut stream, 7, 7)?;
                }
                0 => {
                    let mut padding = [0];
                    stream.read_exact(&mut padding)?;
                    let count = word(&mut stream)?;
                    if count > 256 {
                        return Err(failed("oversized pre-encryption update"));
                    }
                    for _ in 0..count {
                        let header = read_bytes(&mut stream, 12, 12)?;
                        let encoding = i32::from_be_bytes(
                            header[8..12].try_into().expect("fixed rectangle header"),
                        );
                        match encoding {
                            1103 => {
                                let rekey = Zeroizing::new(read_bytes(&mut stream, 36, 36)?);
                                cipher = Some(Records::new(key, &rekey)?);
                            }
                            1010 | 1011 => {
                                let length = usize::from(word(&mut stream)?);
                                read_bytes(&mut stream, length, 65_535)?;
                            }
                            _ => {
                                return Err(failed("unexpected message before control encryption"));
                            }
                        }
                    }
                    if cipher.is_some() {
                        break;
                    }
                }
                _ => {
                    return Err(failed(
                        "the Mac requires a session choice; use the account logged into its desktop",
                    ));
                }
            }
        }
        let cipher =
            cipher.ok_or_else(|| failed("the Mac did not enable encrypted HEVC control"))?;
        stream.write_all(&[0x12, 0, 0, 2, 0, 1, 0, 0])?;
        stream.flush()?;
        thread::sleep(Duration::from_millis(200));
        stream.inner.set_nonblocking(true)?;
        Ok((
            Self {
                stream,
                cipher,
                wire: Vec::new(),
                output: VecDeque::new(),
                output_offset: 0,
                fallback: Decoder::new(size),
            },
            ServerInfo { size, name },
        ))
    }

    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        if self.output.len() >= 256 {
            return Err(failed("encrypted input queue is full"));
        }
        self.output.push_back(self.cipher.encrypt(data)?);
        self.flush()
    }

    fn flush(&mut self) -> Result<()> {
        while let Some(bytes) = self.output.front() {
            match self.stream.write(&bytes[self.output_offset..]) {
                Ok(0) => {
                    return Err(failed(
                        "the encrypted control connection closed while writing",
                    ));
                }
                Ok(count) => {
                    self.output_offset += count;
                    if self.output_offset == bytes.len() {
                        self.output.pop_front();
                        self.output_offset = 0;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn pointer(&mut self, x: u16, y: u16, buttons: u8, backing: Extent) -> Result<()> {
        let mask = (buttons & !6) | ((buttons & 2) << 1) | ((buttons & 4) >> 1);
        let data = self.cipher.pointer(
            x.min((backing.width - 1) as u16),
            y.min((backing.height - 1) as u16),
            mask,
        );
        self.send(&data)
    }

    pub fn input(&mut self, command: Command, _scaled: Extent, backing: Extent) -> Result<()> {
        if let Command::Scroll {
            x,
            y,
            buttons,
            steps,
        } = command
        {
            for buttons in steps.buttons(buttons) {
                self.pointer(x, y, buttons, backing)?;
            }
            return Ok(());
        }
        if let Command::Pointer { x, y, buttons } = command {
            return self.pointer(x, y, buttons, backing);
        }
        let mut data = Zeroizing::new(Vec::new());
        command.write(&mut *data)?;
        self.send(&data)
    }

    pub fn poll(&mut self) -> Result<Vec<Notice>> {
        self.flush()?;
        let mut notices = Vec::new();
        let mut buffer = [0; 16_384];
        for _ in 0..16 {
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(failed("the Mac closed encrypted control")),
                Ok(count) => self.wire.extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
            self.decode(&mut notices)?;
            if self.wire.len() > 131_072 {
                return Err(failed("control buffering exceeded its limit"));
            }
        }
        self.decode(&mut notices)?;
        Ok(notices)
    }

    fn decode(&mut self, notices: &mut Vec<Notice>) -> Result<()> {
        let mut consumed = 0;
        while self.wire.len() - consumed >= 2 {
            let length = usize::from(u16::from_be_bytes([
                self.wire[consumed],
                self.wire[consumed + 1],
            ]));
            if length < 32 || !length.is_multiple_of(16) {
                return Err(failed("invalid encrypted control framing"));
            }
            if self.wire.len() - consumed < length + 2 {
                break;
            }
            let body = self
                .cipher
                .decrypt(&self.wire[consumed + 2..consumed + 2 + length])?;
            consumed += length + 2;
            self.message(&body, notices)?;
        }
        if consumed != 0 {
            self.wire.drain(..consumed);
        }
        Ok(())
    }

    fn message(&mut self, body: &[u8], notices: &mut Vec<Notice>) -> Result<()> {
        let Some(&kind) = body.first() else {
            return Err(failed("empty authenticated control message"));
        };
        match kind {
            0 => {
                let mut reader = Cursor::new(body);
                let _ = word(&mut reader)?;
                let count = word(&mut reader)?;
                if count > 4096 {
                    return Err(failed("too many control rectangles"));
                }
                for _ in 0..count {
                    let header = read_bytes(&mut reader, 12, 12)?;
                    let encoding = i32::from_be_bytes(
                        header[8..12].try_into().expect("fixed rectangle header"),
                    );
                    match encoding {
                        1103 => {
                            if !self.output.is_empty() {
                                return Err(failed("control rekey interrupted pending input"));
                            }
                            let rekey = Zeroizing::new(read_bytes(&mut reader, 36, 36)?);
                            self.cipher.rekey(&rekey)?;
                        }
                        1010 | 1011 | 1002 | 1107 | 1109 | 1110 | 1105 => {
                            let length = usize::from(word(&mut reader)?);
                            let data = Zeroizing::new(read_bytes(&mut reader, length, 65_535)?);
                            if encoding == 1105 && data.len() >= 10 {
                                let scaled = Extent::new(
                                    u32::from(u16::from_be_bytes([data[2], data[3]])),
                                    u32::from(u16::from_be_bytes([data[4], data[5]])),
                                )?;
                                let backing = Extent::new(
                                    u32::from(u16::from_be_bytes([data[6], data[7]])),
                                    u32::from(u16::from_be_bytes([data[8], data[9]])),
                                )?;
                                notices.push(Notice::Layout { scaled, backing });
                            } else if let Some(size) = offers::canvas(&data)? {
                                notices.push(Notice::Canvas(size));
                            }
                        }
                        1104 => {
                            let _id = integer(&mut reader)?;
                            let size = integer(&mut reader)? as usize;
                            read_bytes(&mut reader, size, 1_048_576)?;
                        }
                        -223 => {
                            let size = Extent::new(
                                u32::from(u16::from_be_bytes([header[4], header[5]])),
                                u32::from(u16::from_be_bytes([header[6], header[7]])),
                            )?;
                            notices.push(Notice::Layout {
                                scaled: size,
                                backing: size,
                            });
                        }
                        6 | 16 => {
                            let length = integer(&mut reader)? as usize;
                            let compressed = read_bytes(&mut reader, length, 16 * 1024 * 1024)?;
                            let mut message = vec![0, 0, 0, 1];
                            message.extend_from_slice(&header);
                            message.extend_from_slice(&(length as u32).to_be_bytes());
                            message.extend_from_slice(&compressed);
                            self.fallback.read_message(&mut Cursor::new(message))?;
                        }
                        unknown => {
                            notices.push(Notice::Unsupported(unknown));
                            break;
                        }
                    }
                }
            }
            1 | 2 | 3 | 0x14 | 0x1f => (),
            _ => notices.push(Notice::Unsupported(i32::from(kind))),
        }
        Ok(())
    }
}

pub(super) fn wait_canvas(
    channel: &mut Channel,
    configuration: &[u8],
    mut punch: impl FnMut() -> Result<()>,
) -> Result<(Extent, Option<(Extent, Extent)>)> {
    channel.send(&set_encodings())?;
    channel.send(configuration)?;
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut layout = None;
    let mut last_offer = Instant::now();
    while Instant::now() < deadline {
        punch()?;
        for notice in channel.poll()? {
            match notice {
                Notice::Canvas(size) => return Ok((size, layout)),
                Notice::Layout { scaled, backing } => layout = Some((scaled, backing)),
                Notice::Unsupported(_) => (),
            }
        }
        if last_offer.elapsed() > Duration::from_secs(2) {
            channel.send(configuration)?;
            last_offer = Instant::now();
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(failed(
        "the Mac did not accept HEVC streaming; this requires a compatible Mac with an active desktop",
    ))
}
