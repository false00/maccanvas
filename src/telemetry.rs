use std::{
    collections::VecDeque,
    io::{Read, Write},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicI32, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Counters {
    pub received: AtomicU64,
    pub sent: AtomicU64,
    pub encoding: AtomicI32,
    pub decoder: AtomicI32,
}

impl Counters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            encoding: AtomicI32::new(i32::MIN),
            ..Self::default()
        })
    }
}

pub struct Metered<T> {
    pub inner: T,
    pub counters: Arc<Counters>,
}

impl<T: Read> Read for Metered<T> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.counters
            .received
            .fetch_add(count as u64, Ordering::Relaxed);
        Ok(count)
    }
}

impl<T: Write> Write for Metered<T> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let count = self.inner.write(buffer)?;
        self.counters
            .sent
            .fetch_add(count as u64, Ordering::Relaxed);
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<T: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Metered<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(context, buffer);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.counters
                .received
                .fetch_add((buffer.filled().len() - before) as u64, Ordering::Relaxed);
        }
        result
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Metered<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let result = Pin::new(&mut self.inner).poll_write(context, bytes);
        if let Poll::Ready(Ok(size)) = result {
            self.counters.sent.fetch_add(size as u64, Ordering::Relaxed);
        }
        result
    }
    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub download_bps: f64,
    pub upload_bps: f64,
    pub fps: f64,
}

pub const HISTORY: usize = 40;
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

pub struct Monitor {
    previous: (u64, u64),
    last: Instant,
    displayed: u64,
    pub history: VecDeque<Sample>,
}

impl Monitor {
    pub fn new(now: Instant) -> Self {
        Self {
            previous: (0, 0),
            last: now,
            displayed: 0,
            history: VecDeque::from(vec![Sample::default(); HISTORY]),
        }
    }
    pub fn displayed(&mut self) {
        self.displayed += 1;
    }
    pub fn sample(&mut self, counters: &Counters, now: Instant) -> Option<Sample> {
        let elapsed = now.saturating_duration_since(self.last);
        if elapsed < SAMPLE_INTERVAL {
            return None;
        }
        let current = (
            counters.received.load(Ordering::Relaxed),
            counters.sent.load(Ordering::Relaxed),
        );
        let seconds = elapsed.as_secs_f64();
        let sample = Sample {
            download_bps: current.0.saturating_sub(self.previous.0) as f64 * 8.0 / seconds,
            upload_bps: current.1.saturating_sub(self.previous.1) as f64 * 8.0 / seconds,
            fps: self.displayed as f64 / seconds,
        };
        self.previous = current;
        self.last = now;
        self.displayed = 0;
        self.history.pop_front();
        self.history.push_back(sample);
        Some(sample)
    }
    pub fn bars(&self) -> Vec<(f32, f32)> {
        let maximum = self
            .history
            .iter()
            .map(|sample| sample.download_bps.max(sample.upload_bps))
            .fold(1000.0, f64::max);
        self.history
            .iter()
            .map(|sample| {
                (
                    (sample.download_bps / maximum) as f32,
                    (sample.upload_bps / maximum) as f32,
                )
            })
            .collect()
    }
}

pub fn rate(bits: f64) -> String {
    if bits >= 1_000_000_000.0 {
        format!("{:.2} Gbps", bits / 1_000_000_000.0)
    } else if bits >= 1_000_000.0 {
        format!("{:.2} Mbps", bits / 1_000_000.0)
    } else {
        format!("{:.1} kbps", bits.max(0.0) / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn meters_actual_bytes_not_buffer_capacity() {
        let counters = Counters::new();
        let mut input = Metered {
            inner: std::io::Cursor::new(vec![1, 2, 3]),
            counters: counters.clone(),
        };
        let mut buffer = [0; 100];
        assert_eq!(input.read(&mut buffer).unwrap(), 3);
        let mut output = Metered {
            inner: Vec::new(),
            counters: counters.clone(),
        };
        output.write_all(&[0; 7]).unwrap();
        assert_eq!(counters.received.load(Ordering::Relaxed), 3);
        assert_eq!(counters.sent.load(Ordering::Relaxed), 7);
    }
    #[test]
    fn rates_and_fps_use_actual_elapsed_time_and_go_idle() {
        let start = Instant::now();
        let mut monitor = Monitor::new(start);
        let counters = Counters::new();
        counters.received.store(1_000_000, Ordering::Relaxed);
        counters.sent.store(125, Ordering::Relaxed);
        for _ in 0..15 {
            monitor.displayed();
        }
        assert!(
            monitor
                .sample(&counters, start + Duration::from_millis(100))
                .is_none()
        );
        let sample = monitor
            .sample(&counters, start + Duration::from_millis(500))
            .unwrap();
        assert_eq!(sample.download_bps, 16_000_000.0);
        assert_eq!(sample.upload_bps, 2000.0);
        assert_eq!(sample.fps, 30.0);
        let idle = monitor
            .sample(&counters, start + Duration::from_secs(1))
            .unwrap();
        assert_eq!(idle.download_bps, 0.0);
        assert_eq!(idle.fps, 0.0);
        assert_eq!(monitor.history.len(), HISTORY);
    }
    #[test]
    fn formats_decimal_bits_not_bytes() {
        assert_eq!(rate(999_999.0), "1000.0 kbps");
        assert_eq!(rate(1_000_000.0), "1.00 Mbps");
        assert_eq!(rate(0.0), "0.0 kbps");
        assert_eq!(rate(8_000_000_000.0), "8.00 Gbps");
    }
    #[test]
    fn history_is_bounded_and_normalized() {
        let start = Instant::now();
        let mut monitor = Monitor::new(start);
        let counters = Counters::new();
        for index in 1..=200 {
            counters.received.fetch_add(100, Ordering::Relaxed);
            monitor.sample(&counters, start + Duration::from_millis(index * 500));
        }
        assert_eq!(monitor.bars().len(), HISTORY);
        assert!(
            monitor
                .bars()
                .iter()
                .all(|&(rx, tx)| (0.0..=1.0).contains(&rx) && tx == 0.0)
        );
    }
}
