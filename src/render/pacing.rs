use std::time::{Duration, Instant};

pub struct Pacer<T> {
    pending: Option<T>,
    interval: Duration,
    next: Option<Instant>,
}

impl<T> Pacer<T> {
    pub fn new(fps: u16) -> Self {
        Self {
            pending: None,
            interval: Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 240))),
            next: None,
        }
    }

    pub fn set_fps(&mut self, fps: u16) {
        self.interval = Duration::from_secs_f64(1.0 / f64::from(fps.clamp(1, 240)));
        self.next = None;
    }

    pub fn submit(&mut self, frame: T) {
        self.pending = Some(frame);
    }

    pub fn clear(&mut self) {
        self.pending = None;
        self.next = None;
    }

    pub fn take(&mut self, now: Instant) -> Option<T> {
        if self.next.is_some_and(|next| now < next) {
            return None;
        }
        let frame = self.pending.take()?;
        let remainder = self.next.map_or(Duration::ZERO, |next| {
            Duration::from_nanos(
                (now.duration_since(next).as_nanos() % self.interval.as_nanos()) as u64,
            )
        });
        self.next = Some(now + self.interval - remainder);
        Some(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn early_frame_is_presented_even_when_no_later_frame_arrives() {
        let start = Instant::now();
        let mut pacer = Pacer::new(30);
        pacer.submit(1);
        assert_eq!(pacer.take(start), Some(1));
        pacer.submit(2);
        assert_eq!(pacer.take(start + Duration::from_millis(20)), None);
        assert_eq!(pacer.take(start + Duration::from_millis(34)), Some(2));
    }

    #[test]
    fn pending_frames_are_bounded_and_newest_wins() {
        let start = Instant::now();
        let mut pacer = Pacer::new(60);
        pacer.submit(0);
        assert_eq!(pacer.take(start), Some(0));
        for frame in 1..100 {
            pacer.submit(frame);
        }
        assert_eq!(pacer.take(start + Duration::from_millis(17)), Some(99));
        assert_eq!(pacer.take(start + Duration::from_secs(1)), None);
    }

    #[test]
    fn presentation_deadlines_do_not_drift_with_late_polls() {
        let start = Instant::now();
        let mut pacer = Pacer::new(50);
        pacer.submit(0);
        assert_eq!(pacer.take(start), Some(0));
        pacer.submit(1);
        assert_eq!(pacer.take(start + Duration::from_millis(25)), Some(1));
        pacer.submit(2);
        assert_eq!(pacer.take(start + Duration::from_millis(40)), Some(2));
        pacer.submit(3);
        assert_eq!(pacer.take(start + Duration::from_millis(401)), Some(3));
        pacer.submit(4);
        assert_eq!(pacer.take(start + Duration::from_millis(419)), None);
        assert_eq!(pacer.take(start + Duration::from_millis(420)), Some(4));
    }

    #[test]
    fn recovery_and_rate_changes_do_not_replay_stale_frames() {
        let start = Instant::now();
        let mut pacer = Pacer::new(30);
        pacer.submit(1);
        pacer.clear();
        assert_eq!(pacer.take(start), None);
        pacer.submit(2);
        pacer.set_fps(60);
        assert_eq!(pacer.take(start), Some(2));
        pacer.submit(3);
        assert_eq!(pacer.take(start + Duration::from_millis(17)), Some(3));
    }
}
