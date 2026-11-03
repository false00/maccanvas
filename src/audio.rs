mod codec;
mod playback;

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::Sender,
    },
    thread,
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    audit::{self, Level, Scope},
    error::{Error, Result},
};

pub(crate) use codec::{Decoder, DecoderControl};
pub use codec::{codec_available, install_codec};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
const CAPACITY: usize = SAMPLE_RATE as usize * CHANNELS * 120 / 1000;
const MAX_RENDER_FRAMES: usize = SAMPLE_RATE as usize / 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub muted: bool,
    pub volume: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            muted: false,
            volume: 100,
        }
    }
}

impl Settings {
    pub fn validate(self) -> Result<()> {
        if self.volume > 100 {
            return Err(Error::Configuration(
                "Audio volume must be between 0 and 100".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub enum Status {
    Waiting,
    CodecMissing(String),
    Playing,
    Unavailable(String),
}

pub struct Pcm {
    pub samples: Zeroizing<Vec<i16>>,
}

pub(crate) fn failed(message: impl std::fmt::Display) -> Error {
    Error::Audio {
        code: None,
        message: message.to_string(),
    }
}

fn device_error(error: wasapi::WasapiError) -> Error {
    let code = match &error {
        wasapi::WasapiError::Windows(error) | wasapi::WasapiError::RegisterNotifications(error) => {
            Some(error.code().0)
        }
        _ => None,
    };
    Error::Audio {
        code,
        message: format!("Windows playback is unavailable: {error}"),
    }
}

struct Block {
    bytes: Zeroizing<Vec<u8>>,
    source_frames: u32,
}

struct Buffer {
    samples: Zeroizing<Vec<f32>>,
    read: usize,
    len: usize,
    received: bool,
    settings: Settings,
    gain: f32,
    dropped: u64,
    underrun: u64,
}

impl Buffer {
    fn new(settings: Settings) -> Self {
        Self {
            samples: Zeroizing::new(vec![0.0; CAPACITY]),
            read: 0,
            len: 0,
            received: false,
            settings,
            gain: 0.0,
            dropped: 0,
            underrun: 0,
        }
    }

    fn push(&mut self, pcm: &Pcm) -> Result<()> {
        if pcm.samples.is_empty()
            || pcm.samples.len() > 4096
            || !pcm.samples.len().is_multiple_of(CHANNELS)
        {
            return Err(failed("The decoder returned an invalid stereo PCM block"));
        }
        for frame in pcm.samples.chunks_exact(CHANNELS) {
            if self.len + CHANNELS > CAPACITY {
                for _ in 0..CHANNELS {
                    self.samples[self.read] = 0.0;
                    self.read = (self.read + 1) % CAPACITY;
                    self.len -= 1;
                }
                self.dropped = self.dropped.saturating_add(1);
            }
            for &sample in frame {
                let position = (self.read + self.len) % CAPACITY;
                self.samples[position] = f32::from(sample) / 32768.0;
                self.len += 1;
            }
        }
        self.received = true;
        Ok(())
    }

    fn clear(&mut self) {
        self.samples.as_mut_slice().zeroize();
        self.read = 0;
        self.len = 0;
    }

    fn render(&mut self, frames: usize) -> Result<Block> {
        if frames > MAX_RENDER_FRAMES {
            return Err(failed("Windows requested an oversized audio buffer"));
        }
        let mut bytes = Zeroizing::new(vec![0u8; frames * CHANNELS * 4]);
        let mut source_frames = 0;
        for frame in bytes.chunks_exact_mut(CHANNELS * 4) {
            let available = self.len >= CHANNELS;
            if available {
                source_frames += 1;
            } else {
                self.underrun = self.underrun.saturating_add(1);
            }
            let target = f32::from(self.settings.volume) / 100.0;
            if self.settings.muted {
                self.gain = 0.0;
            } else {
                self.gain += (target - self.gain).clamp(-1.0 / 480.0, 1.0 / 480.0);
            }
            for output in frame.chunks_exact_mut(4) {
                let sample = if available {
                    let value = self.samples[self.read];
                    self.samples[self.read] = 0.0;
                    self.read = (self.read + 1) % CAPACITY;
                    self.len -= 1;
                    value
                } else {
                    0.0
                };
                output.copy_from_slice(&(sample * self.gain).to_le_bytes());
            }
        }
        Ok(Block {
            bytes,
            source_frames,
        })
    }
}

struct Shared {
    buffer: Mutex<Buffer>,
    changed: Condvar,
    stopping: AtomicBool,
    epoch: AtomicU64,
    rendered: AtomicU64,
    failed: AtomicBool,
}

#[derive(Clone)]
pub(crate) struct Publisher(Arc<Shared>);

impl Publisher {
    pub fn push(&self, pcm: &Pcm) -> Result<()> {
        self.0
            .buffer
            .lock()
            .map_err(|_| failed("Audio buffer is unavailable"))?
            .push(pcm)?;
        self.0.changed.notify_all();
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        self.0
            .buffer
            .lock()
            .map_err(|_| failed("Audio buffer is unavailable"))?
            .clear();
        Ok(())
    }
}

pub(crate) struct Playback {
    shared: Arc<Shared>,
    worker: Option<thread::JoinHandle<()>>,
    scope: Scope,
}

pub(crate) struct Statistics {
    pub buffered_frames: u64,
    pub dropped_frames: u64,
    pub underrun_frames: u64,
    pub rendered_frames: u64,
}

impl Playback {
    pub fn start(settings: Settings, scope: Scope, status: Sender<Status>) -> Result<Self> {
        settings.validate()?;
        let shared = Arc::new(Shared {
            buffer: Mutex::new(Buffer::new(settings)),
            changed: Condvar::new(),
            stopping: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            rendered: AtomicU64::new(0),
            failed: AtomicBool::new(false),
        });
        let worker_shared = shared.clone();
        let worker_scope = scope.clone();
        let worker = thread::Builder::new()
            .name("maccanvas-audio-output".into())
            .spawn(move || {
                if let Err(error) = playback::run(&worker_shared, &worker_scope, &status) {
                    worker_shared.failed.store(true, Ordering::Release);
                    report_failure(&worker_scope, &status, &error);
                }
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
            scope,
        })
    }

    pub fn publisher(&self) -> Publisher {
        Publisher(self.shared.clone())
    }

    pub fn configure(&self, settings: Settings) -> Result<()> {
        settings.validate()?;
        self.scope.event(
            "media.audio.output.configured",
            Level::Info,
            json!({
                "audio.muted":settings.muted, "audio.volume.percent":settings.volume,
            }),
        )?;
        self.shared
            .buffer
            .lock()
            .map_err(|_| failed("Audio buffer is unavailable"))?
            .settings = settings;
        self.shared.changed.notify_all();
        Ok(())
    }

    pub fn statistics(&self) -> Result<Statistics> {
        let buffer = self
            .shared
            .buffer
            .lock()
            .map_err(|_| failed("Audio buffer is unavailable"))?;
        Ok(Statistics {
            buffered_frames: (buffer.len / CHANNELS) as u64,
            dropped_frames: buffer.dropped,
            underrun_frames: buffer.underrun,
            rendered_frames: self.shared.rendered.load(Ordering::Relaxed),
        })
    }

    pub fn check(&self) -> Result<()> {
        if self
            .worker
            .as_ref()
            .is_some_and(thread::JoinHandle::is_finished)
            && !self.shared.stopping.load(Ordering::Acquire)
            && !self.shared.failed.load(Ordering::Acquire)
        {
            return Err(failed(
                "The Windows audio output worker stopped unexpectedly",
            ));
        }
        Ok(())
    }

    pub fn close(&mut self) -> Result<()> {
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.changed.notify_all();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| failed("The audio output worker stopped unexpectedly"))?;
        }
        self.shared
            .buffer
            .lock()
            .map_err(|_| failed("Audio buffer is unavailable"))?
            .clear();
        Ok(())
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("MacCanvas audio shutdown failed: {error}");
        }
    }
}

pub(crate) fn report_failure(scope: &Scope, status: &Sender<Status>, error: &Error) {
    if let Err(log_error) = scope.critical(
        "media.audio.failed",
        Level::Error,
        audit::error_attributes(error),
    ) {
        eprintln!("MacCanvas could not record an audio failure: {log_error}");
    }
    if status.send(Status::Unavailable(error.to_string())).is_err() {
        eprintln!("MacCanvas audio status window closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(frames: usize, value: i16) -> Pcm {
        Pcm {
            samples: Zeroizing::new(vec![value; frames * CHANNELS]),
        }
    }

    #[test]
    fn volume_and_mute_are_local_and_do_not_replay_stale_pcm() {
        let mut buffer = Buffer::new(Settings::default());
        buffer.push(&pcm(960, 16384)).unwrap();
        let block = buffer.render(960).unwrap();
        let last = f32::from_le_bytes(block.bytes[block.bytes.len() - 4..].try_into().unwrap());
        assert!((last - 0.5).abs() < 0.00001);
        buffer.settings.muted = true;
        buffer.push(&pcm(480, 16384)).unwrap();
        let block = buffer.render(480).unwrap();
        assert_eq!(block.source_frames, 480);
        assert!(block.bytes.iter().all(|&byte| byte == 0));
        assert_eq!(buffer.len, 0);
        buffer.settings = Settings {
            muted: false,
            volume: 25,
        };
        buffer.push(&pcm(960, 16384)).unwrap();
        let block = buffer.render(960).unwrap();
        let last = f32::from_le_bytes(block.bytes[block.bytes.len() - 4..].try_into().unwrap());
        assert!((last - 0.125).abs() < 0.00001);
    }

    #[test]
    fn audio_backlog_is_bounded_and_consumed_samples_are_cleared() {
        let mut buffer = Buffer::new(Settings::default());
        for _ in 0..4 {
            buffer.push(&pcm(2048, 1000)).unwrap();
        }
        assert_eq!(buffer.len, CAPACITY);
        assert_eq!(buffer.dropped, (4 * 2048 - CAPACITY / CHANNELS) as u64);
        let block = buffer.render(CAPACITY / CHANNELS).unwrap();
        assert_eq!(block.source_frames as usize, CAPACITY / CHANNELS);
        assert!(buffer.samples.iter().all(|&sample| sample == 0.0));
        assert_eq!(buffer.len, 0);
        buffer.push(&pcm(480, 1000)).unwrap();
        buffer.clear();
        assert!(buffer.samples.iter().all(|&sample| sample == 0.0));
        assert_eq!(buffer.len, 0);
        assert!(buffer.render(MAX_RENDER_FRAMES + 1).is_err());
    }

    #[test]
    fn audio_settings_are_bounded_and_defaults_are_explicit() {
        assert_eq!(
            Settings::default(),
            Settings {
                muted: false,
                volume: 100
            }
        );
        assert!(
            Settings {
                muted: false,
                volume: 101
            }
            .validate()
            .is_err()
        );
        assert!(serde_json::from_str::<Settings>(r#"{"volume":255,"password":"never"}"#).is_err());
    }

    #[test]
    #[ignore = "Plays only an original very quiet test tone on the user's default output; never captures audio"]
    fn quiet_synthetic_audio_reaches_the_real_windows_output() {
        let directory = tempfile::tempdir().unwrap();
        let mut guard = audit::Audit::start(directory.path().join("audit")).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut output = Playback::start(
            Settings::default(),
            guard.audit().scope(json!({"test.fixture":"quiet_output"})),
            sender,
        )
        .unwrap();
        let publisher = output.publisher();
        for block in 0..50 {
            let samples = (0..480)
                .flat_map(|frame| {
                    let time = f64::from(block * 480 + frame) / f64::from(SAMPLE_RATE);
                    let value = (160.0 * (std::f64::consts::TAU * 440.0 * time).sin()) as i16;
                    [value, value]
                })
                .collect();
            publisher
                .push(&Pcm {
                    samples: Zeroizing::new(samples),
                })
                .unwrap();
            thread::sleep(std::time::Duration::from_millis(10));
        }
        match receiver
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
        {
            Status::Playing => (),
            status => panic!("Audio output did not start: {status:?}"),
        }
        assert!(output.statistics().unwrap().rendered_frames > 0);
        output.close().unwrap();
        guard.shutdown().unwrap();
        audit::verify(&[guard.audit().path()]).unwrap();
    }
}
