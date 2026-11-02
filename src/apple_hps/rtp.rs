use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crate::error::{Error, Result};

use super::crypto::Packet;

const MAX_UNIT: usize = 16 * 1024 * 1024;
const MAX_PACKETS: usize = 16_384;

fn failed(message: &str) -> Error {
    Error::Protocol(format!("HEVC RTP: {message}"))
}

struct Group {
    created: Instant,
    packets: BTreeMap<u64, Packet>,
    end: Option<u64>,
    bytes: usize,
}

pub(super) struct Unit {
    pub annex_b: Vec<u8>,
    pub timestamp: i64,
    pub random_access: bool,
}

pub(super) struct Assembler {
    groups: BTreeMap<u32, Group>,
    parameters: BTreeMap<u8, Vec<u8>>,
    previous: Option<u64>,
    lost: Option<String>,
    donl: Option<bool>,
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            groups: BTreeMap::new(),
            parameters: BTreeMap::new(),
            previous: None,
            lost: None,
            donl: None,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn pending(&self) -> usize {
        self.groups.len()
    }

    pub fn donl(&self) -> Option<bool> {
        self.donl
    }

    pub fn push(&mut self, packet: Packet, now: Instant) -> Result<()> {
        if packet.payload.len() < 2 {
            return Err(failed("truncated authenticated payload"));
        }
        let nal_type = (packet.payload[0] >> 1) & 63;
        if matches!(nal_type, 32..=34) && self.donl.is_some() {
            let prefix = if self.donl == Some(true) { 4 } else { 2 };
            if packet.payload.len() < prefix {
                return Err(failed("truncated parameter header"));
            }
            let mut nal = packet.payload[..2].to_vec();
            nal.extend_from_slice(&packet.payload[prefix..]);
            if nal.len() > 65_536 {
                return Err(failed("oversized parameter set"));
            }
            self.parameters.insert(nal_type, nal);
        }
        if !self.groups.contains_key(&packet.timestamp) && self.groups.len() >= 8 {
            self.groups.clear();
            self.previous = None;
            self.lost = Some("HEVC packet window exceeded its limit".into());
        }
        let group = self
            .groups
            .entry(packet.timestamp)
            .or_insert_with(|| Group {
                created: now,
                packets: BTreeMap::new(),
                end: None,
                bytes: 0,
            });
        if group.bytes + packet.payload.len() > MAX_UNIT || group.packets.len() >= MAX_PACKETS {
            return Err(failed("access unit exceeds its resource limit"));
        }
        if packet.marker {
            group.end = Some(packet.index);
        }
        group.bytes += packet.payload.len();
        group.packets.insert(packet.index, packet);
        Ok(())
    }

    pub fn missing(&self, now: Instant) -> Vec<u16> {
        let mut missing = Vec::new();
        for group in self.groups.values() {
            if now.duration_since(group.created) < Duration::from_millis(8) {
                continue;
            }
            let Some((&first, _)) = group.packets.first_key_value() else {
                continue;
            };
            let start = self.previous.map_or(first, |last| last.saturating_add(1));
            let end = group
                .end
                .unwrap_or_else(|| *group.packets.last_key_value().expect("nonempty group").0);
            if end.saturating_sub(start) > MAX_PACKETS as u64 {
                continue;
            }
            for index in start..=end {
                if !group.packets.contains_key(&index) {
                    missing.push(index as u16);
                    if missing.len() >= 32 {
                        return missing;
                    }
                }
            }
        }
        missing
    }

    pub fn take_loss(&mut self) -> Option<String> {
        self.lost.take()
    }

    pub fn ready(&mut self, now: Instant) -> Result<Vec<Unit>> {
        let mut output = Vec::new();
        let mut ready = Vec::new();
        for (&timestamp, group) in &self.groups {
            if now.duration_since(group.created) > Duration::from_millis(150) {
                ready.push((timestamp, false));
                continue;
            }
            let Some(end) = group.end else {
                continue;
            };
            if let Some(packet) = group.packets.values().next()
                && (packet.payload[0] >> 1) & 63 == 49
                && packet
                    .payload
                    .get(2)
                    .is_some_and(|header| header & 128 == 0)
            {
                continue;
            }
            let Some((&first, _)) = group.packets.first_key_value() else {
                continue;
            };
            let start = self.previous.map_or(first, |last| last.saturating_add(1));
            let count = end
                .checked_sub(start)
                .and_then(|count| count.checked_add(1));
            if count == Some(group.packets.len() as u64)
                && first == start
                && now.duration_since(group.created) >= Duration::from_millis(4)
            {
                ready.push((timestamp, true));
            }
        }
        for (timestamp, complete) in ready {
            let group = self.groups.remove(&timestamp).expect("known packet group");
            if !complete {
                self.previous = group
                    .end
                    .or_else(|| group.packets.last_key_value().map(|(index, _)| *index));
                self.lost = Some("missing HEVC RTP marker or packets".into());
                continue;
            }
            self.previous = group.end;
            let packets: Vec<_> = group
                .packets
                .values()
                .map(|packet| packet.payload.as_slice())
                .collect();
            if let Some(aggregate) = packets.iter().find(|packet| (packet[0] >> 1) & 63 == 48) {
                self.donl = Some(detect_donl(aggregate)?);
            }
            let nalus = match depacketize(packets.into_iter(), self.donl.unwrap_or(true)) {
                Ok(nalus) => nalus,
                Err(error) => {
                    self.lost = Some(error.to_string());
                    continue;
                }
            };
            let mut random_access = false;
            let mut picture = false;
            for nal in &nalus {
                if nal.len() < 2 {
                    return Err(failed("invalid reconstructed NAL"));
                }
                let kind = (nal[0] >> 1) & 63;
                if matches!(kind, 32..=34) {
                    if nal.len() > 65_536 {
                        return Err(failed("oversized parameter set"));
                    }
                    self.parameters.insert(kind, nal.clone());
                }
                random_access |= matches!(kind, 16..=23);
                picture |= kind <= 31;
            }
            if !picture {
                continue;
            }
            let mut annex_b = Vec::new();
            if random_access {
                for parameter in self.parameters.values() {
                    annex_b.extend_from_slice(&[0, 0, 0, 1]);
                    annex_b.extend_from_slice(parameter);
                }
            }
            for nal in nalus {
                if annex_b.len() + nal.len() + 4 > MAX_UNIT {
                    return Err(failed("reconstructed unit is too large"));
                }
                annex_b.extend_from_slice(&[0, 0, 0, 1]);
                annex_b.extend_from_slice(&nal);
            }
            output.push(Unit {
                annex_b,
                timestamp: i64::from(timestamp),
                random_access,
            });
        }
        Ok(output)
    }
}

fn aggregation(data: &[u8], mut offset: usize) -> Result<Vec<Vec<u8>>> {
    let mut nalus = Vec::new();
    while offset < data.len() {
        let length = data
            .get(offset..offset + 2)
            .ok_or_else(|| failed("truncated aggregation length"))?;
        let length = usize::from(u16::from_be_bytes([length[0], length[1]]));
        offset += 2;
        if length < 2 {
            return Err(failed("empty aggregated NAL"));
        }
        let nal = data
            .get(offset..offset + length)
            .ok_or_else(|| failed("truncated aggregated NAL"))?;
        if nal[0] & 128 != 0 || nal[1] & 7 == 0 || (nal[0] >> 1) & 63 >= 48 {
            return Err(failed("invalid aggregated NAL header"));
        }
        nalus.push(nal.to_vec());
        if nalus.len() > 1024 {
            return Err(failed("too many aggregated NAL units"));
        }
        offset += length;
    }
    if nalus.is_empty() {
        return Err(failed("empty aggregation packet"));
    }
    Ok(nalus)
}

fn detect_donl(data: &[u8]) -> Result<bool> {
    match (aggregation(data, 4), aggregation(data, 2)) {
        (Ok(_), Err(_)) => Ok(true),
        (Err(_), Ok(_)) => Ok(false),
        (Ok(_), Ok(_)) => Err(failed("ambiguous aggregation layout")),
        (Err(error), Err(_)) => Err(error),
    }
}

fn depacketize<'a>(packets: impl Iterator<Item = &'a [u8]>, donl: bool) -> Result<Vec<Vec<u8>>> {
    let mut nalus = Vec::new();
    let mut fragment: Option<Vec<u8>> = None;
    for data in packets {
        if data.len() < if donl { 4 } else { 2 } {
            return Err(failed("invalid HEVC payload header"));
        }
        match (data[0] >> 1) & 63 {
            48 => {
                if fragment.is_some() {
                    return Err(failed("interrupted fragmentation unit"));
                }
                nalus.extend(aggregation(data, if donl { 4 } else { 2 })?);
            }
            49 => {
                let offset = if donl { 5 } else { 3 };
                if data.len() <= offset {
                    return Err(failed("truncated fragmentation header"));
                }
                let first = data[2] & 128 != 0;
                let last = data[2] & 64 != 0;
                if first {
                    if fragment.is_some() {
                        return Err(failed("nested fragmentation unit"));
                    }
                    fragment = Some(vec![(data[0] & 0x81) | ((data[2] & 63) << 1), data[1]]);
                }
                let nal = fragment
                    .as_mut()
                    .ok_or_else(|| failed("fragment without its beginning"))?;
                if nal.len() + data.len() - offset > MAX_UNIT {
                    return Err(failed("inconsistent fragmentation unit"));
                }
                nal.extend_from_slice(&data[offset..]);
                if last {
                    nalus.push(fragment.take().expect("checked fragment"));
                }
            }
            0..=47 => {
                if fragment.is_some() {
                    return Err(failed("interrupted fragmentation unit"));
                }
                let mut nal = data[..2].to_vec();
                nal.extend_from_slice(&data[if donl { 4 } else { 2 }..]);
                nalus.push(nal);
            }
            _ => return Err(failed("unsupported HEVC payload unit")),
        }
        if nalus.len() > 1024 {
            return Err(failed("too many HEVC NAL units"));
        }
    }
    if fragment.is_some() {
        return Err(failed("incomplete fragmentation unit"));
    }
    Ok(nalus)
}

pub(super) fn fir(ssrc: u32) -> Vec<u8> {
    let mut out = vec![0x80, 192, 0, 1];
    out.extend_from_slice(&ssrc.to_be_bytes());
    out
}

pub(super) fn report(own: u32, target: Option<(u32, u64)>) -> Vec<u8> {
    let mut out = vec![
        if target.is_some() { 0x81 } else { 0x80 },
        201,
        0,
        if target.is_some() { 7 } else { 1 },
    ];
    out.extend_from_slice(&own.to_be_bytes());
    if let Some((ssrc, highest)) = target {
        out.extend_from_slice(&ssrc.to_be_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(highest as u32).to_be_bytes());
        out.extend_from_slice(&[0; 12]);
    }
    out
}

pub(super) fn nack(own: u32, target: u32, missing: &[u16]) -> Vec<u8> {
    let mut out = vec![0x81, 205, 0, 0];
    out.extend_from_slice(&own.to_be_bytes());
    out.extend_from_slice(&target.to_be_bytes());
    for sequence in missing.iter().take(32) {
        out.extend_from_slice(&sequence.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
    }
    let words = out.len() / 4 - 1;
    out[2..4].copy_from_slice(&(words as u16).to_be_bytes());
    out
}

pub(super) fn sender_report(own: u32) -> Result<Vec<u8>> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| failed("system clock precedes the RTCP epoch"))?;
    let seconds = (time.as_secs() + 2_208_988_800) as u32;
    let fraction = ((u64::from(time.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut out = vec![0x80, 200, 0, 6];
    out.extend_from_slice(&own.to_be_bytes());
    out.extend_from_slice(&seconds.to_be_bytes());
    out.extend_from_slice(&fraction.to_be_bytes());
    out.extend_from_slice(&(time.as_millis() as u32).wrapping_mul(90).to_be_bytes());
    out.extend_from_slice(&[0; 8]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_donl_is_removed_from_every_fragment() {
        let parts = [
            vec![98, 1, 0x80 | 19, 0, 9, 1, 2],
            vec![98, 1, 0x40 | 19, 0, 9, 3, 4],
        ];
        assert_eq!(
            depacketize(parts.iter().map(Vec::as_slice), true).unwrap(),
            vec![vec![38, 1, 1, 2, 3, 4]]
        );
        assert!(depacketize(parts[1..].iter().map(Vec::as_slice), true).is_err());
        let single = [vec![64, 1, 0, 7, 42]];
        assert_eq!(
            depacketize(single.iter().map(Vec::as_slice), true).unwrap(),
            vec![vec![64, 1, 42]]
        );
    }

    #[test]
    fn aggregation_uses_one_donl_and_bounded_unit_lengths() {
        let packets = [vec![96, 1, 0, 3, 0, 3, 64, 1, 2, 0, 3, 66, 1, 4]];
        assert_eq!(
            depacketize(packets.iter().map(Vec::as_slice), true)
                .unwrap()
                .len(),
            2
        );
        assert!(depacketize([&[96, 1, 0, 3, 0, 30][..]].into_iter(), true).is_err());
    }

    #[test]
    fn single_picture_hosts_can_omit_optional_donl() {
        let aggregate = [96, 1, 0, 3, 64, 1, 42, 0, 3, 66, 1, 43];
        assert!(!detect_donl(&aggregate).unwrap());
        let fragmented = [vec![98, 1, 0x80 | 19, 5, 6], vec![98, 1, 0x40 | 19, 7, 8]];
        assert_eq!(
            depacketize(fragmented.iter().map(Vec::as_slice), false).unwrap(),
            vec![vec![38, 1, 5, 6, 7, 8]]
        );
        assert_eq!(
            aggregation(&aggregate, 2).unwrap(),
            vec![vec![64, 1, 42], vec![66, 1, 43]]
        );
    }
}
