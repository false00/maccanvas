// Adapted from iShareScreen's enc1103 and SRTP implementations (AGPL-3.0-or-later).
use std::collections::HashMap;

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, KeyIvInit, StreamCipher};
use hmac::{Hmac, Mac};
use sha1::{Digest, Sha1};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::error::{Error, Result};

fn failed(message: &str) -> Error {
    Error::Protocol(format!("Apple HEVC: {message}"))
}

pub(super) struct Records {
    key: Zeroizing<[u8; 16]>,
    send_iv: Zeroizing<[u8; 16]>,
    receive_iv: Zeroizing<[u8; 16]>,
    sent: u32,
    received: u32,
}

impl Records {
    pub fn pointer(&self, x: u16, y: u16, buttons: u8) -> Zeroizing<Vec<u8>> {
        let mut block = Zeroizing::new([0; 16]);
        block[10] = 0xff;
        block[11] = buttons;
        block[12..14].copy_from_slice(&x.to_be_bytes());
        block[14..16].copy_from_slice(&y.to_be_bytes());
        aes::Aes128::new((&*self.key).into()).encrypt_block((&mut *block).into());
        let mut out = Zeroizing::new(vec![0x10, 0]);
        out.extend_from_slice(&*block);
        out
    }

    pub fn new(wrap: &[u8; 16], rekey: &[u8]) -> Result<Self> {
        if rekey.len() != 36 {
            return Err(failed("invalid control rekey length"));
        }
        let cipher = aes::Aes128::new(wrap.into());
        let mut key = Zeroizing::new([0; 16]);
        let mut iv = Zeroizing::new([0; 16]);
        key.copy_from_slice(&rekey[4..20]);
        iv.copy_from_slice(&rekey[20..36]);
        cipher.decrypt_block((&mut *key).into());
        cipher.decrypt_block((&mut *iv).into());
        Ok(Self {
            key,
            send_iv: iv.clone(),
            receive_iv: iv,
            sent: 0,
            received: 0,
        })
    }

    pub fn rekey(&mut self, data: &[u8]) -> Result<()> {
        let mut next = Self::new(&self.key, data)?;
        next.sent = self.sent;
        next.received = self.received;
        *self = next;
        Ok(())
    }

    pub fn encrypt(&mut self, body: &[u8]) -> Result<Vec<u8>> {
        if body.len() > 65_498 {
            return Err(failed("control message exceeds its record limit"));
        }
        let size = (body.len() + 22).next_multiple_of(16);
        let mut plain = Zeroizing::new(vec![0; size]);
        plain[..2].copy_from_slice(&(body.len() as u16).to_be_bytes());
        plain[2..2 + body.len()].copy_from_slice(body);
        let mut hash = Sha1::new();
        hash.update(self.sent.to_be_bytes());
        hash.update(&plain[..size - 20]);
        plain[size - 20..].copy_from_slice(&hash.finalize());
        self.sent = self
            .sent
            .checked_add(1)
            .ok_or_else(|| failed("control sequence exhausted"))?;
        let cipher = aes::Aes128::new((&*self.key).into());
        for block in plain.as_chunks_mut::<16>().0 {
            for (byte, iv) in block.iter_mut().zip(self.send_iv.iter()) {
                *byte ^= iv;
            }
            cipher.encrypt_block((&mut *block).into());
            self.send_iv.copy_from_slice(block);
        }
        let mut wire = Vec::with_capacity(size + 2);
        wire.extend_from_slice(&(size as u16).to_be_bytes());
        wire.extend_from_slice(&plain);
        Ok(wire)
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        if ciphertext.len() < 32
            || ciphertext.len() > 65_520
            || !ciphertext.len().is_multiple_of(16)
        {
            return Err(failed("invalid encrypted control record size"));
        }
        let cipher = aes::Aes128::new((&*self.key).into());
        let mut plain = Zeroizing::new(ciphertext.to_vec());
        for block in plain.as_chunks_mut::<16>().0 {
            let previous = *block;
            cipher.decrypt_block((&mut *block).into());
            for (byte, iv) in block.iter_mut().zip(self.receive_iv.iter()) {
                *byte ^= iv;
            }
            self.receive_iv.copy_from_slice(&previous);
        }
        let end = plain.len() - 20;
        let mut matched = None;
        for counter in self.received.saturating_sub(1)..=self.received.saturating_add(5) {
            let mut hash = Sha1::new();
            hash.update(counter.to_be_bytes());
            hash.update(&plain[..end]);
            if bool::from(hash.finalize().as_slice().ct_eq(&plain[end..])) {
                matched = Some(counter);
                break;
            }
        }
        self.received = matched
            .and_then(|counter| counter.checked_add(1))
            .ok_or_else(|| failed("control record integrity verification failed"))?;
        let length = usize::from(u16::from_be_bytes([plain[0], plain[1]]));
        if length > end - 2 {
            return Err(failed("invalid authenticated control body length"));
        }
        Ok(Zeroizing::new(plain[2..2 + length].to_vec()))
    }
}

fn derive(blob: &[u8; 46], label: u8, size: usize) -> Zeroizing<Vec<u8>> {
    let cipher = aes::Aes256::new_from_slice(&blob[..32]).expect("fixed master key");
    let mut iv = [0; 16];
    iv[..14].copy_from_slice(&blob[32..]);
    iv[7] ^= label;
    let start = u128::from_be_bytes(iv);
    let mut result = Zeroizing::new(Vec::with_capacity(size));
    for index in 0..size.div_ceil(16) {
        let mut block = start.wrapping_add(index as u128).to_be_bytes();
        cipher.encrypt_block((&mut block).into());
        result.extend_from_slice(&block);
    }
    result.truncate(size);
    result
}

struct Keys {
    cipher: Zeroizing<Vec<u8>>,
    auth: Zeroizing<Vec<u8>>,
    salt: Zeroizing<Vec<u8>>,
}

impl Keys {
    fn new(blob: &[u8; 46], label: u8) -> Self {
        Self {
            cipher: derive(blob, label, 32),
            auth: derive(blob, label + 1, 20),
            salt: derive(blob, label + 2, 14),
        }
    }

    fn crypt(&self, bytes: &mut [u8], ssrc: u32, index: u64) {
        let mut iv = [0; 16];
        iv[..14].copy_from_slice(&self.salt);
        let iv = (u128::from_be_bytes(iv) ^ (u128::from(ssrc) << 64) ^ (u128::from(index) << 16))
            .to_be_bytes();
        ctr::Ctr128BE::<aes::Aes256>::new_from_slices(&self.cipher, &iv)
            .expect("fixed SRTP key and IV")
            .apply_keystream(bytes);
    }

    fn tag(&self, body: &[u8], roc: Option<u32>) -> [u8; 20] {
        let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(&self.auth).expect("fixed SRTP auth key");
        mac.update(body);
        if let Some(roc) = roc {
            mac.update(&roc.to_be_bytes());
        }
        mac.finalize().into_bytes().into()
    }
}

#[derive(Default)]
struct Replay {
    highest: Option<u64>,
    seen: u128,
}

impl Replay {
    fn accept(&mut self, index: u64) -> bool {
        match self.highest {
            None => {
                self.highest = Some(index);
                self.seen = 1;
                true
            }
            Some(highest) if index > highest => {
                let delta = index - highest;
                self.seen = if delta >= 128 {
                    1
                } else {
                    (self.seen << delta) | 1
                };
                self.highest = Some(index);
                true
            }
            Some(highest) if highest - index < 128 => {
                let bit = 1_u128 << (highest - index);
                if self.seen & bit != 0 {
                    false
                } else {
                    self.seen |= bit;
                    true
                }
            }
            Some(_) => false,
        }
    }
}

pub(super) struct Packet {
    pub index: u64,
    pub ssrc: u32,
    pub timestamp: u32,
    pub marker: bool,
    pub payload: Vec<u8>,
}

pub(super) struct Srtp {
    keys: Keys,
    streams: HashMap<u32, Replay>,
}

impl Srtp {
    pub fn new(blob: &[u8; 46]) -> Self {
        Self {
            keys: Keys::new(blob, 0),
            streams: HashMap::new(),
        }
    }

    pub fn receive(&mut self, data: &[u8]) -> Result<Option<Packet>> {
        if data.len() < 22 || data[0] >> 6 != 2 || data[1] & 127 != 100 {
            return Ok(None);
        }
        let end = data.len() - 10;
        let sequence = u16::from_be_bytes([data[2], data[3]]);
        let ssrc = u32::from_be_bytes(data[8..12].try_into().expect("checked RTP header"));
        let highest = self.streams.get(&ssrc).and_then(|state| state.highest);
        let current = highest.map_or(0, |index| (index >> 16) as u32);
        let max_sequence = highest.map_or(sequence, |index| index as u16);
        let guess = if i32::from(sequence) - i32::from(max_sequence) < -32768 {
            current.saturating_add(1)
        } else if i32::from(sequence) - i32::from(max_sequence) > 32768 {
            current.saturating_sub(1)
        } else {
            current
        };
        let candidates = [
            guess,
            current,
            guess.saturating_add(1),
            guess.saturating_sub(1),
        ];
        let roc = candidates.into_iter().find(|roc| {
            bool::from(self.keys.tag(&data[..end], Some(*roc))[..10].ct_eq(&data[end..]))
        });
        let Some(roc) = roc else {
            return Ok(None);
        };
        let index = (u64::from(roc) << 16) | u64::from(sequence);
        let mut offset = 12 + usize::from(data[0] & 15) * 4;
        if offset > end {
            return Err(failed("invalid authenticated RTP header"));
        }
        if data[0] & 16 != 0 {
            if offset + 4 > end {
                return Err(failed("truncated authenticated RTP extension"));
            }
            let words = usize::from(u16::from_be_bytes([data[offset + 2], data[offset + 3]]));
            offset += 4 + words * 4;
            if offset > end {
                return Err(failed("oversized authenticated RTP extension"));
            }
        }
        if !self.streams.contains_key(&ssrc) && self.streams.len() >= 16 {
            return Err(failed("too many HEVC media streams"));
        }
        if !self.streams.entry(ssrc).or_default().accept(index) {
            return Ok(None);
        }
        let mut payload = data[offset..end].to_vec();
        self.keys.crypt(&mut payload, ssrc, index);
        if data[0] & 32 != 0 {
            let count = payload.last().copied().unwrap_or(0) as usize;
            if count == 0 || count > payload.len() {
                return Err(failed("invalid authenticated RTP padding"));
            }
            payload.truncate(payload.len() - count);
        }
        Ok(Some(Packet {
            index,
            ssrc,
            timestamp: u32::from_be_bytes(data[4..8].try_into().expect("checked RTP header")),
            marker: data[1] & 128 != 0,
            payload,
        }))
    }

    pub fn reset(&mut self) {
        self.streams.clear();
    }
}

pub(super) struct Srtcp {
    keys: Keys,
    index: u32,
}

pub(super) struct SrtpSender {
    keys: Keys,
    ssrc: u32,
    index: u64,
}

impl SrtpSender {
    pub fn new(blob: &[u8; 46], ssrc: u32) -> Self {
        Self {
            keys: Keys::new(blob, 0),
            ssrc,
            index: 0,
        }
    }

    pub fn heartbeat(&mut self, timestamp: u32) -> Result<Vec<u8>> {
        self.packet(&[0x00, 0x68, 0x34, 0x00], timestamp, 101, false)
    }

    pub fn packet(
        &mut self,
        data: &[u8],
        timestamp: u32,
        kind: u8,
        marker: bool,
    ) -> Result<Vec<u8>> {
        if self.index >= (1_u64 << 48) {
            return Err(failed("SRTP sequence exhausted"));
        }
        if data.len() > 65_000 || kind > 127 {
            return Err(failed("invalid outgoing RTP packet"));
        }
        let mut out = vec![0x80, kind | if marker { 128 } else { 0 }];
        out.extend_from_slice(&(self.index as u16).to_be_bytes());
        out.extend_from_slice(&timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        let mut payload = data.to_vec();
        self.keys.crypt(&mut payload, self.ssrc, self.index);
        out.extend_from_slice(&payload);
        let tag = self.keys.tag(&out, Some((self.index >> 16) as u32));
        out.extend_from_slice(&tag[..10]);
        self.index += 1;
        Ok(out)
    }
}

impl Srtcp {
    pub fn new(blob: &[u8; 46]) -> Self {
        Self {
            keys: Keys::new(blob, 3),
            index: 0,
        }
    }

    pub fn protect(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        if packet.len() < 8 || packet.len() > 4096 || !packet.len().is_multiple_of(4) {
            return Err(failed("invalid outgoing RTCP packet"));
        }
        if self.index >= 0x7fff_ffff {
            return Err(failed("RTCP sequence exhausted"));
        }
        let ssrc = u32::from_be_bytes(packet[4..8].try_into().expect("checked RTCP header"));
        let mut out = packet.to_vec();
        self.keys.crypt(&mut out[8..], ssrc, u64::from(self.index));
        out.extend_from_slice(&(self.index | 0x8000_0000).to_be_bytes());
        self.index += 1;
        let tag = self.keys.tag(&out, None);
        out.extend_from_slice(&tag[..10]);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rekey() -> ([u8; 16], [u8; 36]) {
        let wrap = [7; 16];
        let cipher = aes::Aes128::new((&wrap).into());
        let mut packet = [0; 36];
        packet[3] = 1;
        packet[4..20].fill(11);
        packet[20..36].fill(13);
        for block in packet[4..].as_chunks_mut::<16>().0 {
            cipher.encrypt_block(block.into());
        }
        (wrap, packet)
    }

    fn hex(data: &[u8]) -> String {
        data.iter().map(|byte| format!("{byte:02x}")).collect()
    }
    fn unhex(data: &str) -> Vec<u8> {
        data.as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    #[test]
    fn matches_independent_isharescreen_wire_vectors() {
        let (key, blob) = rekey();
        let mut records = Records::new(&key, &blob).unwrap();
        assert_eq!(
            hex(&records.pointer(32, 24, 2)),
            "1000cf50dd97960fe07327051644c743ea4e"
        );
        assert_eq!(
            hex(&records.encrypt(b"first").unwrap()),
            "002059e92f16bd23e13fd68461c3ffc81be10065f982b42f7b028e136a5d01baec01"
        );
        assert_eq!(
            hex(&records.encrypt(b"second record").unwrap()),
            "00303e638c0b36c5c5c406ba9343ee5bd28696d208edd362a13548eb7be73eec72baecfdb554bdef4831d839adf99659aca5"
        );
        let blob = [17; 46];
        assert_eq!(
            hex(&derive(&blob, 0, 32)),
            "494c11ea796f09aa97fe4ccbe45617a3930f1b7f8f739ff2388cfde3bc3d5c37"
        );
        let wire = unhex(
            "80e4ffff0000007b00000009816d5ea38aba09c2fa61cd4baa0f790874bd629471a8777e707c469247ea",
        );
        assert_eq!(
            Srtp::new(&blob).receive(&wire).unwrap().unwrap().payload,
            b"private HEVC payload"
        );
        let mut report = vec![0x81, 201, 0, 7];
        for value in [9_u32, 0x01020304, 0, 65537, 0, 0, 0] {
            report.extend_from_slice(&value.to_be_bytes());
        }
        let mut feedback = Srtcp::new(&blob);
        assert_eq!(
            hex(&feedback.protect(&report).unwrap()),
            "81c90007000000097be70f45aaf3e99d678710634668ab4caacbd5ebd74f89b180000000e941f8bec2ad94141187"
        );
        assert_eq!(
            hex(&feedback.protect(&report).unwrap()),
            "81c9000700000009a5b04caf4da30d7f9680c2c2374def3687b0ca148af1f82780000001ff1e794c6b17fc263066"
        );
    }

    #[test]
    fn control_records_keep_cbc_state_and_reject_tampering() {
        let (key, blob) = rekey();
        let mut send = Records::new(&key, &blob).unwrap();
        let mut receive = Records::new(&key, &blob).unwrap();
        for text in [b"first".as_slice(), b"second record"] {
            let wire = send.encrypt(text).unwrap();
            assert_eq!(&*receive.decrypt(&wire[2..]).unwrap(), text);
        }
        let mut wire = send.encrypt(b"not authentic").unwrap();
        wire[4] ^= 128;
        assert!(receive.decrypt(&wire[2..]).is_err());
        assert!(send.encrypt(&vec![0; 65_499]).is_err());
    }

    #[test]
    fn srtp_checks_tags_rollover_extensions_and_replay() {
        let blob = [17; 46];
        let keys = Keys::new(&blob, 0);
        let mut receiver = Srtp::new(&blob);
        for index in [65535_u64, 65536, 65537] {
            let mut wire = vec![0x80, 0xe4];
            wire.extend_from_slice(&(index as u16).to_be_bytes());
            wire.extend_from_slice(&123_u32.to_be_bytes());
            wire.extend_from_slice(&9_u32.to_be_bytes());
            let mut body = b"private HEVC payload".to_vec();
            keys.crypt(&mut body, 9, index);
            wire.extend_from_slice(&body);
            let tag = keys.tag(&wire, Some((index >> 16) as u32));
            wire.extend_from_slice(&tag[..10]);
            assert_eq!(
                receiver.receive(&wire).unwrap().unwrap().payload,
                b"private HEVC payload"
            );
            assert!(receiver.receive(&wire).unwrap().is_none());
            wire[12] ^= 1;
            assert!(receiver.receive(&wire).unwrap().is_none());
        }
    }
}
