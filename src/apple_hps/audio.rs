use std::{
    collections::BTreeMap,
    net::{SocketAddr, UdpSocket},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::json;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    audio::{self as sound, Decoder, DecoderControl, Pcm, Publisher, Settings, Status},
    audit::{Level, Scope},
    error::Result,
    telemetry::Counters,
};

use super::crypto::{Packet, Srtp};

struct Jitter {
    pending: BTreeMap<u64, Packet>,
    next: Option<u64>,
    waiting: Option<Instant>,
    lost: u64,
}

impl Jitter {
    fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            next: None,
            waiting: None,
            lost: 0,
        }
    }

    fn push(&mut self, mut packet: Packet) {
        let next = *self.next.get_or_insert(packet.index);
        if packet.index < next {
            packet.payload.zeroize();
            return;
        }
        self.pending.insert(packet.index, packet);
        while self.pending.len() > 16 {
            if let Some((index, mut packet)) = self.pending.pop_first() {
                packet.payload.zeroize();
                if index >= next {
                    self.lost = self.lost.saturating_add(index - next + 1);
                    self.next = index.checked_add(1);
                }
            }
        }
    }

    fn ready(&mut self, now: Instant) -> (Vec<Packet>, u64) {
        let mut ready = Vec::new();
        while ready.len() < 16 {
            let Some(next) = self.next else { break };
            if let Some(packet) = self.pending.remove(&next) {
                ready.push(packet);
                self.next = next.checked_add(1);
                self.waiting = None;
                continue;
            }
            let Some(&first) = self.pending.first_key_value().map(|(index, _)| index) else {
                break;
            };
            let since = *self.waiting.get_or_insert(now);
            if first > next
                && (now.duration_since(since) >= Duration::from_millis(30)
                    || self.pending.len() >= 4)
            {
                self.lost = self.lost.saturating_add(first - next);
                self.next = Some(first);
                self.waiting = None;
                continue;
            }
            break;
        }
        (ready, std::mem::take(&mut self.lost))
    }
}

impl Drop for Jitter {
    fn drop(&mut self) {
        for packet in self.pending.values_mut() {
            packet.payload.zeroize();
        }
    }
}

#[derive(Default)]
struct Measurements {
    packets: AtomicU64,
    rejected: AtomicU64,
    decoded: AtomicU64,
    lost: AtomicU64,
}

pub(super) struct Service {
    stop: Arc<AtomicBool>,
    source_failed: Arc<AtomicBool>,
    control: DecoderControl,
    worker: Option<thread::JoinHandle<()>>,
    output: sound::Playback,
    statuses: Receiver<Status>,
    status: Sender<Status>,
    metrics: Arc<Measurements>,
    scope: Scope,
    last_summary: Instant,
}

impl Service {
    pub fn start(
        socket: &UdpSocket,
        peer: SocketAddr,
        key: &[u8; 46],
        settings: Settings,
        counters: Arc<Counters>,
        scope: Scope,
        converter: &Path,
    ) -> Result<Self> {
        let (status, statuses) = mpsc::channel();
        let decoder = Decoder::start(converter, scope.clone())?;
        let control = decoder.control();
        let output = sound::Playback::start(settings, scope.clone(), status.clone())?;
        let publisher = output.publisher();
        let socket = socket.try_clone()?;
        let receiver = Srtp::audio(key);
        let stop = Arc::new(AtomicBool::new(false));
        let source_failed = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(Measurements::default());
        let worker_stop = stop.clone();
        let worker_failed = source_failed.clone();
        let worker_metrics = metrics.clone();
        let worker_scope = scope.clone();
        let worker_status = status.clone();
        let worker = thread::Builder::new()
            .name("maccanvas-audio-source".into())
            .spawn(move || {
                if let Err(error) = source(
                    socket,
                    peer,
                    receiver,
                    decoder,
                    publisher,
                    counters,
                    SourceContext {
                        scope: &worker_scope,
                        stop: &worker_stop,
                        metrics: &worker_metrics,
                    },
                ) && !worker_stop.load(Ordering::Acquire)
                {
                    worker_failed.store(true, Ordering::Release);
                    sound::report_failure(&worker_scope, &worker_status, &error);
                }
            })?;
        Ok(Self {
            stop,
            source_failed,
            control,
            worker: Some(worker),
            output,
            statuses,
            status,
            metrics,
            scope,
            last_summary: Instant::now(),
        })
    }

    pub fn configure(&self, settings: Settings) -> Result<()> {
        self.output.configure(settings)
    }

    pub fn receiving(&self) -> bool {
        !self.source_failed.load(Ordering::Acquire)
            && self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
    }

    pub fn status(&mut self) -> Result<Option<Status>> {
        if !self.stop.load(Ordering::Acquire) && !self.source_failed.load(Ordering::Acquire) {
            let result = self
                .control
                .check()
                .and_then(|_| self.output.check())
                .and_then(|_| {
                    if self
                        .worker
                        .as_ref()
                        .is_some_and(thread::JoinHandle::is_finished)
                    {
                        Err(sound::failed("The Mac audio receiver stopped unexpectedly"))
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = result {
                self.source_failed.store(true, Ordering::Release);
                self.stop.store(true, Ordering::Release);
                if let Err(cleanup) = self.control.stop() {
                    self.scope.event(
                        "media.audio.codec.cleanup.failed",
                        Level::Error,
                        crate::audit::error_attributes(&cleanup),
                    )?;
                }
                sound::report_failure(&self.scope, &self.status, &error);
            }
        }
        if self.last_summary.elapsed() >= Duration::from_secs(60) {
            let output = self.output.statistics()?;
            self.scope.event(
                "media.audio.summary",
                Level::Info,
                json!({
                    "audio.packets.authenticated":self.metrics.packets.load(Ordering::Relaxed),
                    "audio.packets.rejected":self.metrics.rejected.load(Ordering::Relaxed),
                    "audio.packets.lost":self.metrics.lost.load(Ordering::Relaxed),
                    "audio.frames.decoded":self.metrics.decoded.load(Ordering::Relaxed),
                    "audio.frames.rendered":output.rendered_frames,
                    "audio.frames.dropped":output.dropped_frames,
                    "audio.frames.underrun":output.underrun_frames,
                    "audio.buffer.frames":output.buffered_frames,
                }),
            )?;
            self.last_summary = Instant::now();
        }
        match self.statuses.try_recv() {
            Ok(status) => Ok(Some(status)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err(sound::failed("Audio status channel closed unexpectedly"))
            }
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Err(error) = self.control.stop() {
            eprintln!("MacCanvas audio process stop failed: {error}");
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("MacCanvas audio receiver stopped unexpectedly");
        }
        if let Err(error) = self.output.close() {
            eprintln!("MacCanvas audio output stop failed: {error}");
        }
    }
}

fn deliver(
    jitter: &mut Jitter,
    decoder: &mut Decoder,
    publisher: &Publisher,
    scope: &Scope,
    metrics: &Measurements,
    now: Instant,
) -> Result<()> {
    let (ready, lost) = jitter.ready(now);
    if lost > 0 {
        metrics.lost.fetch_add(lost, Ordering::Relaxed);
        if lost > 8 {
            publisher.clear()?;
        }
        for _ in 0..lost.min(8) {
            publisher.push(&Pcm {
                samples: Zeroizing::new(vec![0; 960]),
            })?;
        }
    }
    for mut packet in ready {
        let payload = Zeroizing::new(std::mem::take(&mut packet.payload));
        if payload.as_slice() == [0, 0, 0, 0] {
            publisher.push(&Pcm {
                samples: Zeroizing::new(vec![0; 960]),
            })?;
        } else {
            decoder.feed(&payload)?;
        }
    }
    for pcm in decoder.frames()? {
        if metrics.decoded.fetch_add(1, Ordering::Relaxed) == 0 {
            scope.critical(
                "media.audio.first_frame",
                Level::Info,
                json!({
                    "audio.codec":"aac_eld_sbr", "audio.sample_rate":48000, "audio.channels":2,
                    "audio.frames_per_packet":480, "network.encrypted":true,
                }),
            )?;
        }
        publisher.push(&pcm)?;
    }
    Ok(())
}

struct SourceContext<'a> {
    scope: &'a Scope,
    stop: &'a AtomicBool,
    metrics: &'a Measurements,
}

fn source(
    socket: UdpSocket,
    peer: SocketAddr,
    mut receiver: Srtp,
    mut decoder: Decoder,
    publisher: Publisher,
    counters: Arc<Counters>,
    context: SourceContext<'_>,
) -> Result<()> {
    let SourceContext {
        scope,
        stop,
        metrics,
    } = context;
    let mut buffer = [0u8; 65_536];
    let mut jitter = Jitter::new();
    let mut selected = None;
    let mut last_packet = Instant::now();
    let mut last_rejection = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::Acquire) {
        for _ in 0..128 {
            match socket.recv_from(&mut buffer) {
                Ok((length, sender)) if sender == peer => {
                    counters
                        .received
                        .fetch_add(length as u64, Ordering::Relaxed);
                    if length < 22 || buffer[1] & 127 != 101 {
                        continue;
                    }
                    match receiver.receive(&buffer[..length]) {
                        Ok(Some(mut packet)) => {
                            if packet.payload.len() > 4096 {
                                packet.payload.zeroize();
                                return Err(sound::failed(
                                    "The Mac sent an unsupported audio access-unit size",
                                ));
                            }
                            let selected = selected.get_or_insert(packet.ssrc);
                            if *selected != packet.ssrc {
                                packet.payload.zeroize();
                                if last_packet.elapsed() > Duration::from_millis(500) {
                                    return Err(sound::failed(
                                        "The Mac audio source changed; retry audio to reconnect",
                                    ));
                                }
                                continue;
                            }
                            last_packet = Instant::now();
                            metrics.packets.fetch_add(1, Ordering::Relaxed);
                            jitter.push(packet);
                            deliver(
                                &mut jitter,
                                &mut decoder,
                                &publisher,
                                scope,
                                metrics,
                                Instant::now(),
                            )?;
                        }
                        Ok(None) => {
                            metrics.rejected.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => {
                            metrics.rejected.fetch_add(1, Ordering::Relaxed);
                            if last_rejection.elapsed() >= Duration::from_secs(1) {
                                scope.event(
                                    "media.audio.packet.rejected",
                                    Level::Warn,
                                    crate::audit::error_attributes(&error),
                                )?;
                                last_rejection = Instant::now();
                            }
                        }
                    }
                }
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(error) => return Err(error.into()),
            }
        }
        deliver(
            &mut jitter,
            &mut decoder,
            &publisher,
            scope,
            metrics,
            Instant::now(),
        )?;
        thread::sleep(Duration::from_millis(2));
    }
    decoder.close()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(index: u64) -> Packet {
        Packet {
            index,
            ssrc: 1,
            timestamp: index as u32 * 900,
            marker: false,
            payload: vec![1],
        }
    }

    #[test]
    fn encrypted_audio_jitter_reorders_and_bounds_missing_packet_delay() {
        let now = Instant::now();
        let mut jitter = Jitter::new();
        jitter.push(packet(1));
        assert_eq!(jitter.ready(now).0[0].index, 1);
        jitter.push(packet(3));
        assert!(jitter.ready(now).0.is_empty());
        jitter.push(packet(2));
        let (ready, lost) = jitter.ready(now + Duration::from_millis(10));
        assert_eq!(
            ready.iter().map(|packet| packet.index).collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(lost, 0);
        jitter.push(packet(5));
        assert!(jitter.ready(now).0.is_empty());
        let (ready, lost) = jitter.ready(now + Duration::from_millis(31));
        assert_eq!(ready[0].index, 5);
        assert_eq!(lost, 1);
    }

    #[test]
    fn audio_payload_authentication_and_replay_are_separate_from_video() {
        let key = [29; 46];
        let mut sender = super::super::crypto::SrtpSender::new(&key, 7);
        let wire = sender.packet(b"synthetic audio", 900, 101, false).unwrap();
        assert!(Srtp::new(&key).receive(&wire).unwrap().is_none());
        let mut tampered = wire.clone();
        tampered[12] ^= 1;
        let mut receiver = Srtp::audio(&key);
        assert!(receiver.receive(&tampered).unwrap().is_none());
        assert_eq!(
            receiver.receive(&wire).unwrap().unwrap().payload,
            b"synthetic audio"
        );
        assert!(receiver.receive(&wire).unwrap().is_none());
    }
}
