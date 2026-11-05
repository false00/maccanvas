use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::json;
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Threading::CREATE_NO_WINDOW,
    },
};
use zeroize::Zeroizing;

use crate::{
    audit::{self, Level, Scope},
    error::Result,
    updates,
};

use super::{CHANNELS, Pcm, SAMPLE_RATE, failed};

pub(crate) const CONFIG: &[u8] = &[0xf8, 0xe6, 0x51, 0x32, 0xe0, 0x00];
const MAX_PACKET: usize = 4096;

fn cache_path() -> Result<PathBuf> {
    let root = crate::profiles::default_directory()?;
    Ok(root
        .join("components")
        .join("audio")
        .join(audio_codec_tool::SHA256)
        .join(audio_codec_tool::ASSET))
}

fn verified(path: &Path) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    audit::windows::reject_reparse(path)?;
    if !metadata.is_file() || metadata.len() != audio_codec_tool::size() {
        return Err(failed(
            "The audio component has an unexpected size; remove it and enable audio again",
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(audio_codec_tool::size() + 1)
        .read_to_end(&mut bytes)?;
    updates::verify_binary(&bytes, audio_codec_tool::size(), audio_codec_tool::SHA256)
        .map_err(|error| failed(format!("Audio component verification failed: {error}")))?;
    Ok(true)
}

pub fn codec_available() -> Result<Option<PathBuf>> {
    let executable = std::env::current_exe()?;
    let sibling = executable
        .parent()
        .ok_or_else(|| failed("The viewer has no application folder"))?
        .join(audio_codec_tool::ASSET);
    for path in [cache_path()?, sibling] {
        if verified(&path)? {
            return Ok(Some(path));
        }
    }
    #[cfg(debug_assertions)]
    {
        let path = PathBuf::from(audio_codec_tool::BUILD_PATH);
        if verified(&path)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

pub fn install_codec(scope: &Scope) -> Result<PathBuf> {
    let path = cache_path()?;
    if verified(&path)? {
        return Ok(path);
    }
    let directory = path
        .parent()
        .ok_or_else(|| failed("The audio component has no cache folder"))?;
    let root = crate::profiles::default_directory()?;
    fs::create_dir_all(&root)?;
    audit::windows::reject_reparse(&root)?;
    let sid = audit::windows::current_sid()?;
    let mut current = root;
    for component in ["components", "audio", audio_codec_tool::SHA256] {
        current.push(component);
        if current.exists() {
            audit::windows::reject_reparse(&current)?;
        }
        fs::create_dir_all(&current)?;
        audit::windows::protect(&current, &sid, true)?;
    }
    let mut staged = tempfile::Builder::new()
        .prefix(".audio-")
        .suffix(".exe")
        .tempfile_in(directory)?;
    audit::windows::protect(staged.path(), &sid, false)?;
    let url = format!(
        "https://github.com/false00/maccanvas/releases/download/v{}/{}",
        crate::VERSION,
        audio_codec_tool::ASSET
    );
    scope.critical(
        "component.audio.download.requested",
        Level::Info,
        json!({
            "url.full":url, "artifact.hash.sha256":audio_codec_tool::SHA256,
            "artifact.size":audio_codec_tool::size(),
        }),
    )?;
    updates::http::download(
        &url,
        audio_codec_tool::size(),
        Duration::from_secs(60),
        &mut staged,
        &mut |_| {},
    )?;
    staged.flush()?;
    staged.as_file().sync_all()?;
    verified(staged.path())?;
    match staged.persist_noclobber(&path) {
        Ok(_) => (),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !verified(&path)? {
                return Err(failed(
                    "The audio component cache changed during installation",
                ));
            }
        }
        Err(error) => return Err(error.error.into()),
    }
    scope.critical(
        "component.audio.installed",
        Level::Info,
        json!({
            "artifact.path":path, "artifact.hash.sha256":audio_codec_tool::SHA256,
        }),
    )?;
    Ok(path)
}

struct Bits {
    bytes: Zeroizing<Vec<u8>>,
    used: u8,
}

impl Bits {
    fn new() -> Self {
        Self {
            bytes: Zeroizing::new(Vec::new()),
            used: 0,
        }
    }
    fn put(&mut self, value: u32, width: u8) {
        for bit in (0..width).rev() {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let last = self.bytes.len() - 1;
            self.bytes[last] |= (((value >> bit) & 1) as u8) << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
    }
    fn value(&mut self, value: u8) {
        self.put(0, 2);
        self.put(u32::from(value), 8);
    }
}

pub(crate) fn loas_frame(payload: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if payload.is_empty() || payload.len() > MAX_PACKET {
        return Err(failed("AAC access unit exceeds its supported size"));
    }
    let mut bits = Bits::new();
    bits.put(0, 1); // Each independent LOAS packet includes its standard StreamMuxConfig.
    bits.put(1, 1);
    bits.put(0, 1);
    bits.value(0);
    bits.put(1, 1);
    bits.put(0, 6);
    bits.put(0, 4);
    bits.put(0, 3);
    bits.value((CONFIG.len() * 8) as u8);
    for &byte in CONFIG {
        bits.put(u32::from(byte), 8);
    }
    bits.put(0, 3);
    bits.put(255, 8);
    bits.put(0, 1);
    bits.put(0, 1);
    let mut size = payload.len();
    while size >= 255 {
        bits.put(255, 8);
        size -= 255;
    }
    bits.put(size as u32, 8);
    for &byte in payload {
        bits.put(u32::from(byte), 8);
    }
    if bits.bytes.len() > 8191 {
        return Err(failed("LOAS frame exceeds its format limit"));
    }
    let mut framed = Zeroizing::new(Vec::with_capacity(bits.bytes.len() + 3));
    framed.extend_from_slice(&[
        0x56,
        0xe0 | (bits.bytes.len() >> 8) as u8,
        bits.bytes.len() as u8,
    ]);
    framed.extend_from_slice(&bits.bytes);
    Ok(framed)
}

struct Health {
    last_output: Instant,
    writing: Option<Instant>,
    submitted: u64,
    decoded: u64,
    dropped: u64,
    error: Option<String>,
}

struct Inner {
    child: Mutex<Child>,
    frames: Mutex<VecDeque<Pcm>>,
    health: Mutex<Health>,
    stopping: AtomicBool,
    _job: OwnedHandle,
}

#[derive(Clone)]
pub(crate) struct DecoderControl(Arc<Inner>);

impl DecoderControl {
    pub fn check(&self) -> Result<()> {
        if self.0.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        let health = self
            .0
            .health
            .lock()
            .map_err(|_| failed("Audio decoder health is unavailable"))?;
        if let Some(error) = &health.error {
            return Err(failed(error));
        }
        if health
            .writing
            .is_some_and(|started| started.elapsed() > Duration::from_secs(3))
            || (health.submitted > health.decoded.saturating_add(8)
                && health.last_output.elapsed() > Duration::from_secs(3))
        {
            return Err(failed(
                "The audio converter stopped responding; retry audio",
            ));
        }
        drop(health);
        if let Some(status) = self
            .0
            .child
            .lock()
            .map_err(|_| failed("Audio process control is unavailable"))?
            .try_wait()?
        {
            return Err(failed(format!(
                "The audio converter stopped (exit {:?}); retry audio",
                status.code()
            )));
        }
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.0.stopping.store(true, Ordering::Release);
        let mut child = self
            .0
            .child
            .lock()
            .map_err(|_| failed("Audio process control is unavailable"))?;
        if child.try_wait()?.is_none() {
            child.kill()?;
        }
        Ok(())
    }
}

pub(crate) struct Decoder {
    control: DecoderControl,
    input: Option<ChildStdin>,
    reader: Option<thread::JoinHandle<()>>,
    errors: Option<thread::JoinHandle<()>>,
    scope: Scope,
}

impl Decoder {
    pub fn start(path: &Path, scope: Scope) -> Result<Self> {
        if !verified(path)? {
            return Err(failed("The audio converter is not installed"));
        }
        let job = unsafe { CreateJobObjectW(None, None) }.map_err(failed)?;
        let job = unsafe { OwnedHandle::from_raw_handle(job.0) };
        let handle = HANDLE(job.as_raw_handle());
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        limits.BasicLimitInformation.ActiveProcessLimit = 1;
        limits.ProcessMemoryLimit = 128 * 1024 * 1024;
        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        }
        .map_err(failed)?;
        scope.critical("media.audio.codec.process.requested", Level::Info, json!({
            "process.executable.path":path, "process.executable.hash.sha256":audio_codec_tool::SHA256,
            "audio.codec.interface":"loas_to_wave_stdio",
        }))?;
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()?;
        if let Err(error) =
            unsafe { AssignProcessToJobObject(handle, HANDLE(child.as_raw_handle())) }
        {
            let killed = child.kill();
            let waited = child.wait();
            if let Err(cleanup) = killed.and(waited.map(|_| ())) {
                eprintln!("MacCanvas audio child cleanup failed: {cleanup}");
            }
            return Err(failed(format!("Cannot contain the audio process: {error}")));
        }
        let pid = child.id();
        let input = child
            .stdin
            .take()
            .ok_or_else(|| failed("Audio input pipe is unavailable"))?;
        let output = child
            .stdout
            .take()
            .ok_or_else(|| failed("Audio output pipe is unavailable"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| failed("Audio diagnostic pipe is unavailable"))?;
        let control = DecoderControl(Arc::new(Inner {
            child: Mutex::new(child),
            frames: Mutex::new(VecDeque::new()),
            health: Mutex::new(Health {
                last_output: Instant::now(),
                writing: None,
                submitted: 0,
                decoded: 0,
                dropped: 0,
                error: None,
            }),
            stopping: AtomicBool::new(false),
            _job: job,
        }));
        scope.critical(
            "media.audio.codec.process.started",
            Level::Info,
            json!({
                "process.pid":pid, "process.parent_pid":std::process::id(),
                "process.executable.hash.sha256":audio_codec_tool::SHA256,
            }),
        )?;
        let reader_control = control.clone();
        let reader = thread::Builder::new()
            .name("maccanvas-audio-pcm".into())
            .spawn(move || {
                if let Err(error) = read_pcm(output, &reader_control)
                    && !reader_control.0.stopping.load(Ordering::Acquire)
                {
                    match reader_control.0.health.lock() {
                        Ok(mut health) => health.error = Some(error.to_string()),
                        Err(_) => eprintln!("MacCanvas audio decoder health is unavailable"),
                    }
                }
            })?;
        let errors = thread::Builder::new()
            .name("maccanvas-audio-diagnostics".into())
            .spawn(move || {
                let mut buffer = [0u8; 512];
                loop {
                    match stderr.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(_) => (),
                        Err(error) => {
                            eprintln!("MacCanvas audio diagnostics pipe failed: {error}");
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            control,
            input: Some(input),
            reader: Some(reader),
            errors: Some(errors),
            scope,
        })
    }

    pub fn control(&self) -> DecoderControl {
        self.control.clone()
    }

    pub fn feed(&mut self, payload: &[u8]) -> Result<()> {
        self.control.check()?;
        let frame = loas_frame(payload)?;
        self.control
            .0
            .health
            .lock()
            .map_err(|_| failed("Audio decoder health is unavailable"))?
            .writing = Some(Instant::now());
        self.input
            .as_mut()
            .ok_or_else(|| failed("Audio converter is closed"))?
            .write_all(&frame)
            .map_err(|error| failed(format!("Audio input pipe failed: {error}")))?;
        let mut health = self
            .control
            .0
            .health
            .lock()
            .map_err(|_| failed("Audio decoder health is unavailable"))?;
        health.writing = None;
        health.submitted = health.submitted.saturating_add(1);
        Ok(())
    }

    pub fn frames(&self) -> Result<Vec<Pcm>> {
        Ok(self
            .control
            .0
            .frames
            .lock()
            .map_err(|_| failed("Audio PCM queue is unavailable"))?
            .drain(..)
            .collect())
    }

    pub fn close(&mut self) -> Result<()> {
        if self.input.is_none() && self.reader.is_none() && self.errors.is_none() {
            return Ok(());
        }
        self.control.0.stopping.store(true, Ordering::Release);
        self.input.take();
        let deadline = Instant::now() + Duration::from_millis(750);
        loop {
            if self
                .control
                .0
                .child
                .lock()
                .map_err(|_| failed("Audio process control is unavailable"))?
                .try_wait()?
                .is_some()
            {
                break;
            }
            if Instant::now() >= deadline {
                self.control.stop()?;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        if let Some(reader) = self.reader.take() {
            reader
                .join()
                .map_err(|_| failed("The audio PCM reader stopped unexpectedly"))?;
        }
        if let Some(errors) = self.errors.take() {
            errors
                .join()
                .map_err(|_| failed("The audio diagnostics reader stopped unexpectedly"))?;
        }
        self.control
            .0
            .child
            .lock()
            .map_err(|_| failed("Audio process control is unavailable"))?
            .wait()?;
        self.scope
            .event("media.audio.codec.process.closed", Level::Info, json!({}))?;
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!("MacCanvas audio converter cleanup failed: {error}");
        }
    }
}

fn wave_format(reader: &mut impl Read) -> Result<()> {
    let mut header = [0; 12];
    reader.read_exact(&mut header)?;
    if &header[..4] != b"RIFF" || &header[8..] != b"WAVE" {
        return Err(failed("Audio converter returned an invalid WAVE stream"));
    }
    let mut format_seen = false;
    for _ in 0..16 {
        let mut chunk = [0; 8];
        reader.read_exact(&mut chunk)?;
        let size = u32::from_le_bytes(chunk[4..].try_into().expect("fixed chunk header"));
        if &chunk[..4] == b"data" {
            return if format_seen {
                Ok(())
            } else {
                Err(failed("Audio format is missing"))
            };
        }
        if size > 4096 {
            return Err(failed("Audio converter returned an oversized WAVE header"));
        }
        let mut data = vec![0; size as usize];
        reader.read_exact(&mut data)?;
        if size % 2 != 0 {
            reader.read_exact(&mut [0])?;
        }
        if &chunk[..4] == b"fmt " {
            if data.len() < 16
                || data[..2] != 1u16.to_le_bytes()
                || data[2..4] != (CHANNELS as u16).to_le_bytes()
                || data[4..8] != SAMPLE_RATE.to_le_bytes()
                || data[12..14] != 4u16.to_le_bytes()
                || data[14..16] != 16u16.to_le_bytes()
            {
                return Err(failed("Audio converter returned an unsupported PCM format"));
            }
            format_seen = true;
        }
    }
    Err(failed("Audio converter did not provide PCM data"))
}

fn read_pcm(mut reader: impl Read, control: &DecoderControl) -> Result<()> {
    wave_format(&mut reader)?;
    loop {
        let mut bytes = Zeroizing::new(vec![0u8; 480 * CHANNELS * 2]);
        let first = reader.read(&mut bytes[..1])?;
        if first == 0 {
            return Ok(());
        }
        reader.read_exact(&mut bytes[1..])?;
        let samples = Zeroizing::new(
            bytes
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        );
        let mut frames = control
            .0
            .frames
            .lock()
            .map_err(|_| failed("Audio PCM queue is unavailable"))?;
        let mut dropped = false;
        if frames.len() >= 8 {
            frames.pop_front();
            dropped = true;
        }
        frames.push_back(Pcm { samples });
        drop(frames);
        let mut health = control
            .0
            .health
            .lock()
            .map_err(|_| failed("Audio decoder health is unavailable"))?;
        health.last_output = Instant::now();
        health.decoded = health.decoded.saturating_add(1);
        if dropped {
            health.dropped = health.dropped.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_cache_identity_does_not_include_the_viewer_version() {
        let expected = crate::profiles::default_directory()
            .unwrap()
            .join("components")
            .join("audio")
            .join(audio_codec_tool::SHA256)
            .join(audio_codec_tool::ASSET);
        assert_eq!(cache_path().unwrap(), expected);
    }

    #[test]
    fn component_verification_rejects_modified_and_nonregular_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(audio_codec_tool::ASSET);
        assert!(!verified(&path).unwrap());
        assert!(verified(directory.path()).is_err());
        fs::copy(audio_codec_tool::BUILD_PATH, &path).unwrap();
        assert!(verified(&path).unwrap());
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&path, &bytes).unwrap();
        assert!(verified(&path).is_err());
        fs::write(&path, &bytes[..last]).unwrap();
        assert!(verified(&path).is_err());
    }

    #[test]
    fn standard_media_pipes_decode_native_apple_stereo_without_linking_the_codec() {
        let directory = tempfile::tempdir().unwrap();
        let mut guard = audit::Audit::start(directory.path().join("audit")).unwrap();
        let scope = guard
            .audit()
            .scope(json!({"test.fixture":"native_apple_audio"}));
        let mut decoder = Decoder::start(Path::new(audio_codec_tool::BUILD_PATH), scope).unwrap();
        let mut wire = &include_bytes!("../../tests/data/apple-aac-eld-stereo.aaceld")[..];
        let mut samples = Vec::new();
        let mut packets = 0;
        while !wire.is_empty() {
            let size = usize::from(u16::from_be_bytes([wire[0], wire[1]]));
            wire = &wire[2..];
            decoder.feed(&wire[..size]).unwrap();
            wire = &wire[size..];
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let frames = decoder.frames().unwrap();
                if !frames.is_empty() {
                    for frame in frames {
                        samples.extend_from_slice(&frame.samples);
                    }
                    break;
                }
                decoder.control.check().unwrap();
                assert!(Instant::now() < deadline, "Native AAC output timed out");
                thread::sleep(Duration::from_millis(1));
            }
            packets += 1;
        }
        decoder.close().unwrap();
        assert_eq!(packets, 100);
        assert_eq!(samples.len(), 100 * 480 * CHANNELS);
        let amplitude = |channel: usize, frequency: f64| {
            let signal: Vec<_> = samples
                .chunks_exact(CHANNELS)
                .skip(1920)
                .take(100 * 480 - 2880)
                .map(|frame| f64::from(frame[channel]) / 32768.0)
                .collect();
            let (mut real, mut imaginary) = (0.0, 0.0);
            for (index, &sample) in signal.iter().enumerate() {
                let phase =
                    std::f64::consts::TAU * frequency * index as f64 / f64::from(SAMPLE_RATE);
                real += sample * phase.cos();
                imaginary += sample * phase.sin();
            }
            2.0 * real.hypot(imaginary) / signal.len() as f64
        };
        assert!((0.06..0.14).contains(&amplitude(0, 440.0)));
        assert!((0.06..0.14).contains(&amplitude(1, 880.0)));
        assert!(amplitude(0, 880.0) < 0.015);
        assert!(amplitude(1, 440.0) < 0.015);
        guard.shutdown().unwrap();
        audit::verify(&[guard.audit().path()]).unwrap();
        let log = fs::read_to_string(guard.audit().path()).unwrap();
        assert!(log.contains("media.audio.codec.process.started"));
        assert!(log.contains("media.audio.codec.process.closed"));
    }

    #[test]
    fn malformed_wave_formats_and_unbounded_access_units_are_rejected() {
        assert!(loas_frame(&[]).is_err());
        assert!(loas_frame(&vec![0; MAX_PACKET + 1]).is_err());
        assert!(wave_format(&mut &b"not a wave!!"[..]).is_err());
    }
}
