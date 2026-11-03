use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crate::error::{Error, Result};

use super::crypto::Packet;

const MAX_UNIT: usize = 16 * 1024 * 1024;
const MAX_PACKETS: usize = 16_384;
const MAX_GROUPS: usize = 64;
const MAX_WINDOW_BYTES: usize = 8 * MAX_UNIT;
const MAX_WINDOW_PACKETS: usize = 8 * MAX_PACKETS;

fn failed(message: &str) -> Error {
    Error::Protocol(format!("HEVC RTP: {message}"))
}

struct Group {
    created: Instant,
    packets: BTreeMap<u64, Packet>,
    end: Option<u64>,
    bytes: usize,
}

#[derive(serde::Serialize)]
struct GroupSnapshot {
    timestamp: u32,
    first: u64,
    last: u64,
    end: Option<u64>,
    packets: usize,
    bytes: usize,
    age_ms: u128,
    nal_type: u8,
    fragment_start: Option<bool>,
}

impl Group {
    fn snapshot(&self, timestamp: u32, now: Instant) -> GroupSnapshot {
        let (&first, packet) = self.packets.first_key_value().expect("nonempty group");
        let nal_type = (packet.payload[0] >> 1) & 63;
        GroupSnapshot {
            timestamp,
            first,
            last: *self.packets.last_key_value().expect("nonempty group").0,
            end: self.end,
            packets: self.packets.len(),
            bytes: self.bytes,
            age_ms: now.duration_since(self.created).as_millis(),
            nal_type,
            fragment_start: packet
                .payload
                .get(2)
                .filter(|_| nal_type == 49)
                .map(|header| header & 128 != 0),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct Loss {
    pub reason: String,
    previous: Option<u64>,
    groups: Vec<GroupSnapshot>,
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
    lost: Option<Loss>,
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
        if self.previous.is_some_and(|last| packet.index <= last) {
            return Ok(());
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
        if !self.groups.contains_key(&packet.timestamp) && self.groups.len() >= MAX_GROUPS {
            self.lost.get_or_insert(Loss {
                reason: "HEVC packet window exceeded its limit".into(),
                previous: self.previous,
                groups: self
                    .groups
                    .iter()
                    .map(|(&timestamp, group)| group.snapshot(timestamp, now))
                    .collect(),
            });
            self.groups.clear();
            self.previous = None;
        }
        let (bytes, packets) = self
            .groups
            .values()
            .fold((packet.payload.len(), 1), |(bytes, packets), group| {
                (bytes + group.bytes, packets + group.packets.len())
            });
        if bytes > MAX_WINDOW_BYTES || packets > MAX_WINDOW_PACKETS {
            return Err(failed("packet window exceeds its resource limit"));
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

    fn ordered_timestamps(&self) -> Vec<u32> {
        let mut timestamps: Vec<_> = self.groups.keys().copied().collect();
        timestamps.sort_unstable_by_key(|timestamp| {
            *self.groups[timestamp]
                .packets
                .first_key_value()
                .expect("nonempty group")
                .0
        });
        timestamps
    }

    pub fn missing(&self, now: Instant) -> Vec<u16> {
        let mut missing = Vec::new();
        let mut previous = self.previous;
        for timestamp in self.ordered_timestamps() {
            let group = &self.groups[&timestamp];
            let Some((&first, _)) = group.packets.first_key_value() else {
                continue;
            };
            let start = previous.map_or(first, |last| last.saturating_add(1));
            let end = group
                .end
                .unwrap_or_else(|| *group.packets.last_key_value().expect("nonempty group").0);
            previous = Some(end);
            if now.duration_since(group.created) < Duration::from_millis(8)
                || end.saturating_sub(start) > MAX_PACKETS as u64
            {
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

    pub fn take_loss(&mut self) -> Option<Loss> {
        self.lost.take()
    }

    pub fn ready(&mut self, now: Instant) -> Result<Vec<Unit>> {
        let mut output = Vec::new();
        let mut ready = Vec::new();
        let mut previous = self.previous;
        let timestamps = self.ordered_timestamps();
        for (offset, &timestamp) in timestamps.iter().enumerate() {
            let group = &self.groups[&timestamp];
            let Some((&first, packet)) = group.packets.first_key_value() else {
                continue;
            };
            let start = previous.map_or(first, |last| last.saturating_add(1));
            let fragment_without_start = (packet.payload[0] >> 1) & 63 == 49
                && packet
                    .payload
                    .get(2)
                    .is_some_and(|header| header & 128 == 0);
            let complete = !fragment_without_start
                && first == start
                && group.end.is_some_and(|end| {
                    end.checked_sub(start)
                        .and_then(|count| count.checked_add(1))
                        == Some(group.packets.len() as u64)
                });
            let elapsed = now.duration_since(group.created);
            if complete && (previous.is_some() || elapsed >= Duration::from_millis(4)) {
                ready.push((timestamp, true, group.end));
                previous = group.end;
            } else if elapsed > Duration::from_millis(150) {
                previous = group
                    .end
                    .or_else(|| {
                        timestamps.get(offset + 1).and_then(|next| {
                            self.groups[next]
                                .packets
                                .first_key_value()
                                .and_then(|(index, _)| index.checked_sub(1))
                        })
                    })
                    .or_else(|| group.packets.last_key_value().map(|(index, _)| *index));
                ready.push((timestamp, false, previous));
            } else {
                break;
            }
        }
        for (timestamp, complete, end) in ready {
            let group = self.groups.remove(&timestamp).expect("known packet group");
            if !complete {
                self.lost.get_or_insert(Loss {
                    reason: "missing HEVC RTP marker or packets".into(),
                    previous: self.previous,
                    groups: vec![group.snapshot(timestamp, now)],
                });
                self.previous = end;
                continue;
            }
            self.previous = end;
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
                    self.lost.get_or_insert(Loss {
                        reason: error.to_string(),
                        previous: self.previous,
                        groups: vec![group.snapshot(timestamp, now)],
                    });
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

    fn picture(index: u64, timestamp: u32) -> Packet {
        Packet {
            index,
            ssrc: 1,
            timestamp,
            marker: true,
            payload: vec![2, 1, 0, 0, 128],
        }
    }

    #[test]
    fn queued_complete_frames_do_not_request_received_packets() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        assembler.push(picture(10, 100), now).unwrap();
        assembler.push(picture(11, 200), now).unwrap();

        assert!(
            assembler
                .missing(now + Duration::from_millis(10))
                .is_empty()
        );
    }

    #[test]
    fn queued_complete_frames_drain_in_one_poll() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        assembler.push(picture(10, 100), now).unwrap();
        assembler.push(picture(11, 200), now).unwrap();

        let units = assembler.ready(now + Duration::from_millis(10)).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].timestamp, 100);
        assert_eq!(units[1].timestamp, 200);
        assert!(assembler.take_loss().is_none());
    }

    #[test]
    fn sequence_anchored_complete_frame_does_not_wait_for_reordering() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        assembler.push(picture(10, 100), now).unwrap();

        let units = assembler.ready(now).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].timestamp, 100);
        assert!(assembler.take_loss().is_none());
    }

    #[test]
    fn receiving_a_burst_can_drain_frames_without_overflowing_the_window() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        let mut units = Vec::new();
        for index in 10..110 {
            assembler
                .push(picture(index, index as u32 * 100), now)
                .unwrap();
            units.extend(assembler.ready(now).unwrap());
        }

        assert_eq!(units.len(), 100);
        assert_eq!(assembler.pending(), 0);
        assert!(assembler.take_loss().is_none());
    }

    #[test]
    fn complete_frame_survives_a_delayed_poll() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        assembler.push(picture(10, 100), now).unwrap();

        let units = assembler.ready(now + Duration::from_millis(160)).unwrap();
        assert_eq!(units.len(), 1);
        assert!(assembler.take_loss().is_none());
    }

    #[test]
    fn missing_packet_is_requested_once_and_blocks_later_frames() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        let mut first = picture(10, 100);
        first.marker = false;
        assembler.push(first, now).unwrap();
        assembler.push(picture(12, 100), now).unwrap();
        assembler.push(picture(13, 200), now).unwrap();

        let poll = now + Duration::from_millis(10);
        assert_eq!(assembler.missing(poll), vec![11]);
        assert!(assembler.ready(poll).unwrap().is_empty());
        assert!(assembler.take_loss().is_none());

        let mut retransmission = picture(11, 100);
        retransmission.marker = false;
        assembler.push(retransmission, poll).unwrap();
        let units = assembler.ready(poll).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].timestamp, 100);
        assert_eq!(units[1].timestamp, 200);
        assert!(assembler.take_loss().is_none());
    }

    #[test]
    fn actual_packet_loss_expires_without_discarding_the_next_complete_frame() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        let mut first = picture(10, 100);
        first.marker = false;
        assembler.push(first, now).unwrap();
        assembler.push(picture(12, 100), now).unwrap();
        assembler.push(picture(13, 200), now).unwrap();

        let units = assembler.ready(now + Duration::from_millis(160)).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].timestamp, 200);
        assert_eq!(
            assembler
                .take_loss()
                .as_ref()
                .map(|loss| loss.reason.as_str()),
            Some("missing HEVC RTP marker or packets")
        );
    }

    #[test]
    fn incomplete_frame_gets_repair_time_during_a_received_burst() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        let mut first = picture(10, 1000);
        first.marker = false;
        assembler.push(first, now).unwrap();
        for index in 12..28 {
            assembler
                .push(picture(index, index as u32 * 100), now)
                .unwrap();
        }

        let poll = now + Duration::from_millis(10);
        assert_eq!(assembler.pending(), 17);
        assert!(assembler.ready(poll).unwrap().is_empty());
        assert_eq!(assembler.missing(poll), vec![11]);
        assert!(assembler.take_loss().is_none());
    }

    #[test]
    fn missing_marker_expiry_preserves_the_next_complete_frame() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        let mut first = picture(10, 100);
        first.marker = false;
        assembler.push(first, now).unwrap();
        assembler.push(picture(12, 200), now).unwrap();

        let units = assembler.ready(now + Duration::from_millis(160)).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].timestamp, 200);
        assert!(assembler.take_loss().is_some());
    }

    #[test]
    fn repair_window_keeps_the_previous_memory_budget() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        for index in 10..18 {
            let timestamp = index as u32 * 100;
            assembler.push(picture(index, timestamp), now).unwrap();
            assembler.groups.get_mut(&timestamp).unwrap().bytes = MAX_UNIT;
        }

        assert!(assembler.push(picture(18, 1800), now).is_err());
        assert_eq!(assembler.pending(), 8);
    }

    #[test]
    fn overflowing_window_preserves_bounded_packet_metadata() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        for index in 10..11 + 2 * MAX_GROUPS as u64 {
            assembler
                .push(picture(index, index as u32 * 100), now)
                .unwrap();
        }

        let loss = assembler.take_loss().unwrap();
        assert_eq!(loss.reason, "HEVC packet window exceeded its limit");
        assert_eq!(loss.previous, Some(9));
        assert_eq!(loss.groups.len(), MAX_GROUPS);
        for (offset, group) in loss.groups.iter().enumerate() {
            let index = 10 + offset as u64;
            assert_eq!(group.first, index);
            assert_eq!(group.last, index);
            assert_eq!(group.end, Some(index));
            assert_eq!(group.packets, 1);
            assert_eq!(group.age_ms, 0);
            assert_eq!(group.nal_type, 1);
            assert_eq!(group.fragment_start, None);
        }
        let value = serde_json::to_value(&loss).unwrap();
        assert_eq!(value["groups"].as_array().unwrap().len(), MAX_GROUPS);
    }

    #[test]
    fn late_packet_for_an_expired_frame_does_not_block_new_frames() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        let mut first = picture(10, 100);
        first.marker = false;
        assembler.push(first, now).unwrap();
        assembler.push(picture(12, 100), now).unwrap();
        let poll = now + Duration::from_millis(160);
        assert!(assembler.ready(poll).unwrap().is_empty());
        assert!(assembler.take_loss().is_some());

        let mut retransmission = picture(11, 100);
        retransmission.marker = false;
        assembler.push(retransmission, poll).unwrap();
        assembler.push(picture(13, 200), poll).unwrap();
        let units = assembler.ready(poll + Duration::from_millis(4)).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].timestamp, 200);
        assert_eq!(assembler.pending(), 0);
        assert!(assembler.take_loss().is_none());
    }

    #[test]
    fn frames_follow_packet_sequence_across_timestamp_rollover() {
        let now = Instant::now();
        let mut assembler = Assembler::new();
        assembler.previous = Some(9);
        assembler.push(picture(10, u32::MAX - 100), now).unwrap();
        assembler.push(picture(11, 50), now).unwrap();

        let poll = now + Duration::from_millis(10);
        assert!(assembler.missing(poll).is_empty());
        let units = assembler.ready(poll).unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].timestamp, i64::from(u32::MAX - 100));
        assert_eq!(units[1].timestamp, 50);
        assert!(assembler.take_loss().is_none());
    }

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
