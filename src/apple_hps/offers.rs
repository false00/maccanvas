// Apple media negotiation adapted from iShareScreen (AGPL-3.0-or-later).
use std::{
    io::{Read, Write},
    time::{SystemTime, UNIX_EPOCH},
};

use flate2::{Compression, read::ZlibDecoder, write::ZlibEncoder};
use plist::{Dictionary, Value};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    error::{Error, Result},
    geometry::Extent,
};

const BLOB: &str = "avcMediaStreamNegotiatorMediaBlob";

fn failed(message: &str) -> Error {
    Error::Protocol(format!("Apple HEVC negotiation: {message}"))
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value > 127 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}
fn number(out: &mut Vec<u8>, field: u32, value: u64) {
    varint(out, u64::from(field) << 3);
    varint(out, value);
}
fn bytes(out: &mut Vec<u8>, field: u32, value: &[u8]) {
    varint(out, (u64::from(field) << 3) | 2);
    varint(out, value.len() as u64);
    out.extend_from_slice(value);
}
fn random_u32() -> Result<u32> {
    let mut value = [0; 4];
    getrandom::fill(&mut value).map_err(|_| failed("secure randomness failed"))?;
    Ok(u32::from_be_bytes(value))
}

pub(super) struct Offers {
    pub audio: Vec<u8>,
    pub video: Vec<u8>,
    pub audio_ssrc: u32,
    pub video_ssrc: u32,
    pub audio_send: Zeroizing<[u8; 46]>,
    pub audio_receive: Zeroizing<[u8; 46]>,
    pub video_send: Zeroizing<[u8; 46]>,
    pub video_receive: Zeroizing<[u8; 46]>,
}

impl Offers {
    pub fn new(audio: bool, video_bitrate_bps: u32) -> Result<Self> {
        Self::with_bank(false, audio, video_bitrate_bps)
    }

    pub fn with_bank(alternate: bool, audio: bool, video_bitrate_bps: u32) -> Result<Self> {
        let audio_ssrc = random_u32()?;
        let video_ssrc = random_u32()?;
        let key = || -> Result<Zeroizing<[u8; 46]>> {
            let mut value = Zeroizing::new([0; 46]);
            getrandom::fill(&mut *value)
                .map_err(|_| failed("secure media-key randomness failed"))?;
            Ok(value)
        };
        Ok(Self {
            audio: offer(false, audio_ssrc, alternate, audio, 0)?,
            video: offer(true, video_ssrc, alternate, audio, video_bitrate_bps)?,
            audio_ssrc,
            video_ssrc,
            audio_send: key()?,
            audio_receive: key()?,
            video_send: key()?,
            video_receive: key()?,
        })
    }

    pub fn configuration(&self, sixty_fps: bool) -> Result<Zeroizing<Vec<u8>>> {
        let size = self.audio.len() + self.video.len() + 0xd8;
        if size > 60_000 {
            return Err(failed("media offers exceed their wire limit"));
        }
        let mut out = Zeroizing::new(vec![0; size + 4]);
        out[0] = 0x1c;
        out[2..4].copy_from_slice(&(size as u16).to_be_bytes());
        out[4..6].copy_from_slice(&3_u16.to_be_bytes());
        let flags: u32 = if sixty_fps { 5 } else { 4 };
        // The Agent's IPC argument is swapped; the interoperable wire word is BE.
        out[6..10].copy_from_slice(&flags.to_be_bytes());
        out[10..12].copy_from_slice(&(self.audio.len() as u16).to_be_bytes());
        out[12..14].copy_from_slice(&(self.video.len() as u16).to_be_bytes());
        out[0x14..0x24].copy_from_slice(Uuid::new_v4().as_bytes());
        out[0x24..0x52].copy_from_slice(&*self.audio_send);
        out[0x52..0x80].copy_from_slice(&*self.audio_receive);
        out[0x80..0x80 + self.audio.len()].copy_from_slice(&self.audio);
        let offset = 0x80 + self.audio.len();
        out[offset..offset + 46].copy_from_slice(&*self.video_send);
        out[offset + 46..offset + 92].copy_from_slice(&*self.video_receive);
        out[offset + 92..].copy_from_slice(&self.video);
        Ok(out)
    }
}

fn offer(
    video: bool,
    session: u32,
    alternate: bool,
    audio: bool,
    video_bitrate_bps: u32,
) -> Result<Vec<u8>> {
    let mut description = Vec::new();
    number(&mut description, 1, u64::from(session));
    if video {
        number(&mut description, 2, 1);
        let mut bank = Vec::new();
        // Host releases disagree on bank IDs; output NALs are verified before use.
        number(&mut bank, 1, if alternate { 123 } else { 100 });
        let variants: &[u64] = if alternate { &[1, 2, 1, 2] } else { &[1, 2] };
        for &variant in variants {
            let mut resolution = Vec::new();
            for (field, value) in [(1, 1), (2, variant), (3, 50115), (4, 0)] {
                number(&mut resolution, field, value);
            }
            bytes(&mut bank, 2, &resolution);
        }
        bytes(
            &mut bank,
            3,
            if alternate {
                b"FLS;MS:-1;LF:-1;CABAC;POS:0;EOD:1;HTS:2;RR:3;AR:16/9,5/8;XR:16/9,5/8;"
            } else {
                b"FLS;LF:-1;POS:5;EOD:1;HTS:2;RR:3;POSE:4;AR:16/9,5/8;XR:16/9,5/8;"
            },
        );
        number(&mut bank, 4, if alternate { 1 } else { 14 });
        bytes(&mut description, 3, &bank);
        for (field, value) in [(6, 1), (7, 0), (8, 63), (9, 1), (12, 1)] {
            number(&mut description, field, value);
        }
    } else {
        for (field, value) in [
            (2, 0),
            (3, 0),
            (4, if audio { 24191 } else { 1000 }),
            (5, 0),
            (6, 0),
        ] {
            number(&mut description, field, value);
        }
    }
    let mut media = Vec::new();
    number(&mut media, 1, 1);
    number(&mut media, 2, 1);
    bytes(&mut media, if video { 5 } else { 3 }, &description);
    bytes(
        &mut media,
        6,
        format!("MacCanvas {}", crate::VERSION).as_bytes(),
    );
    number(&mut media, 8, 0);
    for (kind, rate, cap) in [
        (0, 40_000_000, Some(12288)),
        (0, 6_000_000, Some(131072)),
        (4074, 0, Some(16384)),
        (16, 4100, None),
        (0, 75_000_000, Some(524288)),
        (0, 20_000_000, Some(98304)),
        (4, 6500, None),
        (0, 60_000_000, Some(262144)),
        (1, 299, None),
        (0, 100_000_000, Some(1048576)),
    ] {
        let mut tier = Vec::new();
        number(&mut tier, 1, kind);
        // Kind-zero entries cap bandwidth for the different network profiles.
        let rate = if video && kind == 0 && video_bitrate_bps != 0 {
            u64::from(video_bitrate_bps)
        } else {
            rate
        };
        number(&mut tier, 2, rate);
        if let Some(cap) = cap {
            number(&mut tier, 3, cap);
        }
        bytes(&mut media, 9, &tier);
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| failed("system clock precedes Unix epoch"))?
        .as_nanos();
    number(
        &mut media,
        13,
        u64::try_from(timestamp).map_err(|_| failed("system timestamp overflow"))?,
    );
    for (field, value) in [(14, 2), (16, 0), (18, 1)] {
        number(&mut media, field, value);
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&media)?;
    let compressed = encoder.finish()?;
    let mut endpoint = Vec::new();
    number(&mut endpoint, 1, 0);
    number(&mut endpoint, 2, 1);
    bytes(&mut endpoint, 3, b"Windows-x86_64");
    bytes(&mut endpoint, 4, b"1.0.0");
    bytes(&mut endpoint, 5, b"Windows");
    let mut plist = Dictionary::new();
    plist.insert(
        "avcMediaStreamOptionRemoteEndpointInfo".into(),
        Value::Data(endpoint),
    );
    plist.insert(
        "avcMediaStreamNegotiatorMode".into(),
        Value::Integer((if video { 7_u64 } else { 8 }).into()),
    );
    plist.insert(BLOB.into(), Value::Data(compressed));
    plist.insert(
        "avcMediaStreamOptionCallID".into(),
        Value::String(Uuid::new_v4().to_string().to_uppercase()),
    );
    let mut out = Vec::new();
    Value::Dictionary(plist)
        .to_writer_binary(&mut out)
        .map_err(|_| failed("could not encode media property list"))?;
    Ok(out)
}

fn read_varint(data: &[u8], offset: &mut usize) -> Result<u64> {
    let mut out = 0;
    for shift in (0..70).step_by(7) {
        let byte = *data
            .get(*offset)
            .ok_or_else(|| failed("truncated protobuf integer"))?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err(failed("protobuf integer overflow"));
        }
        out |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(out);
        }
    }
    Err(failed("oversized protobuf integer"))
}

enum Field<'a> {
    Number(u32, u64),
    Bytes(u32, &'a [u8]),
    Other,
}

fn fields(data: &[u8]) -> Result<Vec<Field<'_>>> {
    let mut offset = 0;
    let mut result = Vec::new();
    while offset < data.len() {
        if result.len() >= 4096 {
            return Err(failed("too many protobuf fields"));
        }
        let tag = read_varint(data, &mut offset)?;
        let field = u32::try_from(tag >> 3).map_err(|_| failed("protobuf field overflow"))?;
        if field == 0 {
            return Err(failed("invalid protobuf field"));
        }
        let item = match tag & 7 {
            0 => Field::Number(field, read_varint(data, &mut offset)?),
            2 => {
                let size = usize::try_from(read_varint(data, &mut offset)?)
                    .map_err(|_| failed("protobuf length overflow"))?;
                let end = offset
                    .checked_add(size)
                    .filter(|end| *end <= data.len())
                    .ok_or_else(|| failed("truncated protobuf bytes"))?;
                let bytes = &data[offset..end];
                offset = end;
                Field::Bytes(field, bytes)
            }
            kind @ (1 | 5) => {
                offset = offset
                    .checked_add(if kind == 1 { 8 } else { 4 })
                    .filter(|end| *end <= data.len())
                    .ok_or_else(|| failed("truncated fixed protobuf field"))?;
                Field::Other
            }
            _ => return Err(failed("unsupported protobuf wire type")),
        };
        result.push(item);
    }
    Ok(result)
}

pub(super) fn canvas(data: &[u8]) -> Result<Option<Extent>> {
    for blob in super::bplist::media_blobs(data)? {
        if let Some(canvas) = canvas_blob(blob)? {
            return Ok(Some(canvas));
        }
    }
    Ok(None)
}

fn canvas_blob(compressed: &[u8]) -> Result<Option<Extent>> {
    let mut decoded = Vec::new();
    ZlibDecoder::new(compressed)
        .take(1_048_577)
        .read_to_end(&mut decoded)?;
    if decoded.len() > 1_048_576 {
        return Err(failed("media-answer expansion exceeds its limit"));
    }
    for field in fields(&decoded)? {
        if let Field::Bytes(5, video) = field {
            let (mut width, mut height, mut tiles) = (0, 0, 0);
            for field in fields(video)? {
                if let Field::Number(field, value) = field {
                    match field {
                        4 => width = value,
                        5 => height = value,
                        6 => tiles = value,
                        _ => (),
                    }
                }
            }
            if width != 0 && height != 0 {
                // Some hosts omit field 6 (protobuf zero); decoded geometry is checked.
                if tiles > 1 {
                    return Err(failed(&format!(
                        "the Mac selected {tiles} HEVC pictures per frame for a {width}x{height} canvas"
                    )));
                }
                return Extent::new(
                    u32::try_from(width).map_err(|_| failed("canvas width overflow"))?,
                    u32::try_from(height).map_err(|_| failed("canvas height overflow"))?,
                )
                .map(Some);
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn offers_request_real_hevc_single_picture_and_suppress_audio() {
        let offers = Offers::new(false, 0).unwrap();
        assert_ne!(&*offers.video_send, &*offers.video_receive);
        let video = Value::from_reader(Cursor::new(&offers.video)).unwrap();
        let dict = video.as_dictionary().unwrap();
        let mut blob = Vec::new();
        ZlibDecoder::new(dict[BLOB].as_data().unwrap())
            .read_to_end(&mut blob)
            .unwrap();
        let description = fields(&blob)
            .unwrap()
            .into_iter()
            .find_map(|field| match field {
                Field::Bytes(5, data) => Some(data),
                _ => None,
            })
            .unwrap();
        assert!(
            fields(description)
                .unwrap()
                .iter()
                .any(|field| matches!(field, Field::Number(6, 1)))
        );
        assert!(
            fields(description)
                .unwrap()
                .iter()
                .any(|field| matches!(field, Field::Number(7, 0)))
        );
        let fast = offers.configuration(true).unwrap();
        let slow = offers.configuration(false).unwrap();
        assert_eq!(&fast[6..10], &5_u32.to_be_bytes());
        assert_eq!(&slow[6..10], &4_u32.to_be_bytes());
        assert_eq!(fast.len(), offers.audio.len() + offers.video.len() + 0xdc);
    }

    fn bandwidth_caps(offer: &[u8]) -> Vec<u64> {
        let value = Value::from_reader(Cursor::new(offer)).unwrap();
        let mut blob = Vec::new();
        ZlibDecoder::new(value.as_dictionary().unwrap()[BLOB].as_data().unwrap())
            .read_to_end(&mut blob)
            .unwrap();
        fields(&blob)
            .unwrap()
            .into_iter()
            .filter_map(|field| {
                let Field::Bytes(9, tier) = field else {
                    return None;
                };
                let tier = fields(tier).unwrap();
                if !tier
                    .iter()
                    .any(|field| matches!(field, Field::Number(1, 0)))
                {
                    return None;
                }
                tier.into_iter().find_map(|field| match field {
                    Field::Number(2, value) => Some(value),
                    _ => None,
                })
            })
            .collect()
    }

    #[test]
    fn custom_video_bitrate_replaces_network_caps_in_both_banks_without_changing_audio() {
        let adaptive = Offers::new(true, 0).unwrap();
        let default_caps = vec![
            40_000_000,
            6_000_000,
            75_000_000,
            20_000_000,
            60_000_000,
            100_000_000,
        ];
        assert_eq!(bandwidth_caps(&adaptive.video), default_caps);
        for alternate in [false, true] {
            for bitrate in [1, 500_000, 125_500_000, u32::MAX] {
                let manual = Offers::with_bank(alternate, true, bitrate).unwrap();
                assert_eq!(
                    bandwidth_caps(&manual.video),
                    vec![u64::from(bitrate); default_caps.len()]
                );
                assert_eq!(bandwidth_caps(&manual.audio), default_caps);
                for sixty_fps in [false, true] {
                    let wire = manual.configuration(sixty_fps).unwrap();
                    let video = &wire[0x80 + manual.audio.len() + 92..];
                    assert_eq!(bandwidth_caps(video), bandwidth_caps(&manual.video));
                }
            }
        }
    }

    #[test]
    fn protobuf_parsing_is_bounded_and_rejects_truncation() {
        assert!(fields(&[0x2a, 0xff]).is_err());
        assert!(fields(&[0]).is_err());
        assert!(fields(&[0x08, 0xff, 0xff]).is_err());
    }

    #[test]
    fn unspecified_picture_count_still_requires_a_valid_canvas() {
        let mut compressed = ZlibEncoder::new(Vec::new(), Compression::default());
        compressed
            .write_all(&[0x2a, 4, 0x20, 64, 0x28, 48])
            .unwrap();
        assert_eq!(
            canvas_blob(&compressed.finish().unwrap()).unwrap(),
            Some(Extent::new(64, 48).unwrap())
        );
    }
}
