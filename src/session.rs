use std::{
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    audit::{self, Level, Scope},
    auth::Credentials,
    credentials::{self, AuditedConsent, AuditedVault, CredentialSource, CredentialStore},
    error::{Error, Result},
    framebuffer::Framebuffer,
    input::Command,
    profiles::Profile,
    protocol::{self, Decoder, Message},
    streaming::StreamSettings,
    telemetry::{Counters, Metered},
};
use serde_json::json;
use zeroize::Zeroizing;

mod hevc;

const STARTUP_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

pub type Frames = Arc<Mutex<Option<Arc<Framebuffer>>>>;

pub enum Event {
    Retrying {
        attempt: usize,
        delay: Duration,
    },
    Connected {
        profile: Profile,
        server_name: String,
        credentials_saved: bool,
    },
    Warning(String),
    Audio(crate::audio::Status),
    Cursor {
        update: crate::apple_hps::cursor::Update,
        point_scale: f32,
    },
    Error(String),
    Disconnected {
        reconnect: bool,
    },
}

pub struct Request {
    pub profile: Profile,
    pub credentials: CredentialSource,
    pub streaming: StreamSettings,
    pub audio: crate::audio::Settings,
    pub remember: bool,
    pub window: isize,
    pub generation: u64,
    pub audit: Scope,
    pub clipboard: bool,
}

struct Control {
    cancelled: AtomicBool,
    socket: Mutex<Option<TcpStream>>,
    gpu_frame: Mutex<Option<Arc<crate::render::gpu::nvdec::Frame>>>,
    frame_notifier: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    clipboard_enabled: AtomicBool,
    clipboard_utf8: AtomicBool,
    clipboard_pending: AtomicUsize,
    clipboard_text: Mutex<Option<Zeroizing<String>>>,
}

impl Control {
    fn new(clipboard: bool) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            socket: Mutex::new(None),
            gpu_frame: Mutex::new(None),
            frame_notifier: Mutex::new(None),
            clipboard_enabled: AtomicBool::new(clipboard),
            clipboard_utf8: AtomicBool::new(false),
            clipboard_pending: AtomicUsize::new(0),
            clipboard_text: Mutex::new(None),
        }
    }

    fn close(&self) -> Result<()> {
        self.cancelled.store(true, Ordering::Release);
        self.shutdown_socket()
    }

    fn shutdown_socket(&self) -> Result<()> {
        let socket = self
            .socket
            .lock()
            .map_err(|_| Error::Protocol("Connection control is unavailable".into()))?;
        if let Some(socket) = socket.as_ref() {
            match socket.shutdown(Shutdown::Both) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotConnected => (),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}

pub struct Session {
    control: Arc<Control>,
    sender: SyncSender<WriterCommand>,
    pub counters: Arc<Counters>,
    audit: Scope,
    worker: Option<thread::JoinHandle<()>>,
    streaming: StreamSettings,
}

enum WriterCommand {
    Input(Command),
    ClipboardServer(crate::clipboard::Packet),
    ClipboardLocal(Zeroizing<String>),
    ClipboardEnabled(bool),
    Complete {
        size: crate::geometry::Extent,
        bytes: u64,
    },
}

impl Session {
    pub fn set_frame_notifier(&self, notifier: Arc<dyn Fn() + Send + Sync>) -> Result<()> {
        *self
            .control
            .frame_notifier
            .lock()
            .map_err(|_| Error::Display("Frame notification is unavailable".into()))? =
            Some(notifier);
        Ok(())
    }

    pub fn take_gpu_frame(&self) -> Result<Option<Arc<crate::render::gpu::nvdec::Frame>>> {
        Ok(self
            .control
            .gpu_frame
            .lock()
            .map_err(|_| Error::Display("GPU frame queue is unavailable".into()))?
            .take())
    }

    pub fn start(request: Request, events: Sender<(u64, Event)>, frames: Frames) -> Result<Self> {
        request
            .streaming
            .validate_authentication(request.profile.authentication)?;
        request.audio.validate()?;
        let streaming = request.streaming;
        let encoding = streaming.encoding;
        request.audit.critical("session.connection.requested", Level::Info, json!({
            "server.address": request.profile.host, "server.port": request.profile.port,
            "network.transport": "tcp", "network.protocol.name": "rfb",
            "rfb.authentication": request.profile.authentication,
            "credential.source": if matches!(request.credentials, CredentialSource::Saved) { "saved" } else { "manual" },
            "rfb.streaming": request.streaming,
        }))?;
        let audit = request.audit.clone();
        let control = Arc::new(Control::new(request.clipboard && !encoding.is_hevc()));
        let (sender, receiver) = mpsc::sync_channel(256);
        sender
            .try_send(WriterCommand::ClipboardEnabled(
                request.clipboard && !encoding.is_hevc(),
            ))
            .map_err(|_| Error::Protocol("Cannot initialize the clipboard writer".into()))?;
        let counters = Counters::new();
        let traffic = counters.clone();
        let worker_control = control.clone();
        let refresh = sender.clone();
        let worker_audit = audit.clone();
        let summary = counters.clone();
        let worker = thread::Builder::new()
            .name("maccanvas-rfb".into())
            .spawn(move || {
                let started = Instant::now();
                let generation = request.generation;
                let outcome = run(
                    request,
                    receiver,
                    refresh,
                    &events,
                    &frames,
                    &worker_control,
                    traffic,
                );
                let reconnect = encoding.is_hevc()
                    && !worker_control.cancelled.load(Ordering::Acquire)
                    && outcome
                        .as_ref()
                        .err()
                        .is_some_and(retryable_connection_error);
                if let Err(error) = &outcome
                    && !worker_control.cancelled.load(Ordering::Acquire)
                    && events
                        .send((generation, Event::Error(error.to_string())))
                        .is_err()
                {
                    eprintln!("MacCanvas connection failed: {error}");
                }
                if let Err(error) = worker_audit.critical(
                    "session.connection.closed",
                    if outcome.is_ok() {
                        Level::Info
                    } else {
                        Level::Error
                    },
                    json!({
                        "event.outcome": if outcome.is_ok() { "closed" } else { "failure" },
                        "error.type": outcome.as_ref().err().map(audit::error_type),
                        "network.io.received": summary.received.load(Ordering::Relaxed),
                        "network.io.sent": summary.sent.load(Ordering::Relaxed),
                        "session.duration_ms": started.elapsed().as_millis() as u64,
                    }),
                ) {
                    eprintln!("MacCanvas connection summary could not be persisted: {error}");
                }
                if let Err(error) = worker_control.close() {
                    eprintln!("MacCanvas shutdown failed: {error}");
                }
                if events
                    .send((generation, Event::Disconnected { reconnect }))
                    .is_err()
                {
                    eprintln!("MacCanvas connection window has closed");
                }
            })?;
        Ok(Self {
            control,
            sender,
            counters,
            audit,
            worker: Some(worker),
            streaming,
        })
    }

    pub fn send(&self, command: Command) -> Result<()> {
        if matches!(command, Command::Audio(_)) && !self.uses_hevc() {
            return Err(Error::Audio {
                code: None,
                message: "Audio requires an HEVC session".into(),
            });
        }
        if let Command::Configure(settings) = command
            && self.requires_reconnect(settings)
        {
            return Err(Error::Configuration(
                "Changing transports, HEVC presets or the HEVC bitrate ceiling requires restarting the stream".into(),
            ));
        }
        self.post(WriterCommand::Input(command))
    }

    pub fn uses_hevc(&self) -> bool {
        self.streaming.encoding.is_hevc()
    }

    pub fn requires_reconnect(&self, settings: StreamSettings) -> bool {
        self.streaming.requires_reconnect(settings)
    }

    fn post(&self, command: WriterCommand) -> Result<()> {
        match self.sender.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.control.close()?;
                self.audit.critical(
                    "session.input_queue.exhausted",
                    Level::Error,
                    json!({"queue.capacity":256, "event.outcome":"disconnected"}),
                )?;
                Err(Error::Protocol(
                    "Input queue filled; the connection was closed to avoid stuck keys".into(),
                ))
            }
            Err(TrySendError::Disconnected(_)) => {
                Err(Error::Protocol("The connection has closed".into()))
            }
        }
    }

    pub fn close(&self) -> Result<()> {
        self.control.close()
    }

    pub fn is_closed(&self) -> bool {
        self.control.cancelled.load(Ordering::Acquire)
    }

    pub fn share_clipboard(&self, enabled: bool) -> Result<()> {
        if enabled && self.uses_hevc() {
            return Err(Error::Clipboard(
                "Use the encrypted SSH controls in Clipboard while using HEVC".into(),
            ));
        }
        self.audit.critical(
            "clipboard.sharing.changed",
            Level::Info,
            json!({"clipboard.enabled":enabled}),
        )?;
        self.control
            .clipboard_enabled
            .store(enabled, Ordering::Release);
        if !enabled {
            self.control
                .clipboard_text
                .lock()
                .map_err(|_| Error::Clipboard("Clipboard queue is unavailable".into()))?
                .take();
        }
        self.post(WriterCommand::ClipboardEnabled(enabled))
    }

    pub fn clipboard_utf8(&self) -> bool {
        self.control.clipboard_utf8.load(Ordering::Acquire)
    }

    pub fn send_clipboard(&self, text: Zeroizing<String>) -> Result<()> {
        if !self.control.clipboard_enabled.load(Ordering::Acquire) {
            return Err(Error::Clipboard(
                "Enable plain-text clipboard sharing first".into(),
            ));
        }
        crate::clipboard::validate_vnc(&text, self.clipboard_utf8())?;
        self.post(WriterCommand::ClipboardLocal(text))
    }

    pub fn take_clipboard(&self) -> Result<Option<Zeroizing<String>>> {
        Ok(self
            .control
            .clipboard_text
            .lock()
            .map_err(|_| Error::Clipboard("Clipboard queue is unavailable".into()))?
            .take())
    }

    pub fn scope(&self) -> &Scope {
        &self.audit
    }

    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }

    pub fn finish(mut self) -> Result<()> {
        self.control.close()?;
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| Error::Protocol("Connection worker stopped unexpectedly".into()))?;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.control.close() {
            eprintln!("MacCanvas connection cleanup failed: {error}");
        }
    }
}

fn retryable_connection_error(error: &Error) -> bool {
    matches!(error, Error::Io(_) | Error::ConnectionLost(_))
}

fn connect_with_retry<T>(
    scope: &Scope,
    events: &Sender<(u64, Event)>,
    generation: u64,
    control: &Control,
    resolve: impl FnOnce() -> Result<Credentials>,
    mut connect: impl FnMut(&Credentials) -> Result<T>,
) -> Result<Option<(T, Credentials)>> {
    if control.cancelled.load(Ordering::Acquire) {
        return Ok(None);
    }
    let credentials = resolve()?;
    let mut attempt = 1;
    loop {
        if control.cancelled.load(Ordering::Acquire) {
            return Ok(None);
        }
        scope.critical(
            "session.connection.attempt.started",
            Level::Info,
            json!({"session.connection.attempt":attempt}),
        )?;
        if attempt > 1 {
            events
                .send((
                    generation,
                    Event::Retrying {
                        attempt,
                        delay: Duration::ZERO,
                    },
                ))
                .map_err(|_| Error::Protocol("Connection window closed".into()))?;
        }
        let result = connect(&credentials);
        if control.cancelled.load(Ordering::Acquire) {
            control.shutdown_socket()?;
            return Ok(None);
        }
        let error = match result {
            Ok(connection) => return Ok(Some((connection, credentials))),
            Err(error) => error,
        };
        if !retryable_connection_error(&error) {
            return Err(error);
        }
        let Some(&delay) = STARTUP_RETRY_DELAYS.get(attempt - 1) else {
            return Err(error);
        };
        control.shutdown_socket()?;
        control
            .socket
            .lock()
            .map_err(|_| Error::Protocol("Connection control is unavailable".into()))?
            .take();
        let mut attributes = audit::error_attributes(&error);
        attributes["session.connection.attempt"] = json!(attempt + 1);
        attributes["session.connection.max_attempts"] = json!(STARTUP_RETRY_DELAYS.len() + 1);
        attributes["session.connection.delay_ms"] = json!(delay.as_millis() as u64);
        scope.critical(
            "session.connection.retry.scheduled",
            Level::Warn,
            attributes,
        )?;
        events
            .send((
                generation,
                Event::Retrying {
                    attempt: attempt + 1,
                    delay,
                },
            ))
            .map_err(|_| Error::Protocol("Connection window closed".into()))?;
        let retry_at = Instant::now() + delay;
        while !control.cancelled.load(Ordering::Acquire) {
            let remaining = retry_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            thread::sleep(remaining.min(Duration::from_millis(20)));
        }
        attempt += 1;
    }
}

fn run(
    request: Request,
    incoming: Receiver<WriterCommand>,
    refresh: SyncSender<WriterCommand>,
    events: &Sender<(u64, Event)>,
    frames: &Frames,
    control: &Arc<Control>,
    counters: Arc<Counters>,
) -> Result<()> {
    if request.streaming.encoding.is_hevc() {
        return hevc::run(request, incoming, events, frames, control, counters);
    }
    let generation = request.generation;
    let Some(((mut stream, info), credentials)) = connect_with_retry(
        &request.audit,
        events,
        generation,
        control,
        || {
            credentials::resolve(
                request.profile.id,
                request.credentials,
                &AuditedConsent {
                    scope: &request.audit,
                    window: request.window,
                },
                &AuditedVault {
                    scope: &request.audit,
                },
            )
        },
        |credentials| {
            let stream = protocol::connect(&request.profile)?;
            request.audit.critical("network.connection.established", Level::Info, json!({
                "server.socket.address": stream.peer_addr()?.ip().to_string(), "server.socket.port": stream.peer_addr()?.port(),
                "client.socket.address": stream.local_addr()?.ip().to_string(), "client.socket.port": stream.local_addr()?.port(),
            }))?;
            *control
                .socket
                .lock()
                .map_err(|_| Error::Protocol("Connection control is unavailable".into()))? =
                Some(stream.try_clone()?);
            if control.cancelled.load(Ordering::Acquire) {
                return Err(Error::Cancelled);
            }
            let mut stream = Metered {
                inner: stream,
                counters: counters.clone(),
            };
            request.audit.critical(
                "security.authentication.started",
                Level::Info,
                json!({"network.protocol.name":"rfb"}),
            )?;
            let info = protocol::handshake_with_settings(
                &mut stream,
                request.profile.authentication,
                credentials,
                request.streaming,
            )?;
            request.audit.critical("security.authentication.completed", Level::Info, json!({
                "event.outcome":"success", "rfb.security.type": if request.profile.authentication == crate::profiles::AuthMode::MacLogin { 30 } else { 2 },
                "rfb.framebuffer.width": info.size.width, "rfb.framebuffer.height": info.size.height,
                "rfb.desktop.name": info.name, "network.encrypted": false,
            }))?;
            stream.inner.set_read_timeout(None)?;
            Ok((stream, info))
        },
    )?
    else {
        return Ok(());
    };
    let mut profile = request.profile;
    let mut credentials_saved = false;
    if request.remember {
        match (AuditedVault {
            scope: &request.audit,
        })
        .save(profile.id, &credentials)
        {
            Ok(()) => {
                profile.has_saved_credentials = true;
                credentials_saved = true;
            }
            Err(error) => {
                if events
                    .send((
                        generation,
                        Event::Warning(format!(
                            "Connected, but the password was not saved: {error}"
                        )),
                    ))
                    .is_err()
                {
                    return Err(Error::Protocol("Connection window closed".into()));
                }
            }
        }
    }
    drop(credentials);
    let writer = stream.inner.try_clone()?;
    writer.set_write_timeout(Some(Duration::from_secs(3)))?;
    let mut writer = Metered {
        inner: writer,
        counters: counters.clone(),
    };
    let writer_control = control.clone();
    let writer_events = events.clone();
    let writer_audit = request.audit.clone();
    let writer_thread = thread::spawn(move || {
        let mut noticed = |notice| -> Result<()> {
            use crate::clipboard::Notice;
            match notice {
                Notice::Text(text) => {
                    if writer_control.clipboard_enabled.load(Ordering::Acquire) {
                        *writer_control.clipboard_text.lock().map_err(|_| Error::Clipboard("Clipboard queue is unavailable".into()))? = Some(text);
                    }
                }
                Notice::Mode(utf8) => { writer_control.clipboard_utf8.store(utf8, Ordering::Release); }
                Notice::Received { bytes, encoding, accepted } => writer_audit.event("clipboard.received", Level::Info, json!({
                    "clipboard.direction":"mac_to_windows", "clipboard.encoding":encoding, "clipboard.wire_bytes":bytes, "clipboard.accepted":accepted,
                }))?,
                Notice::Sent { bytes, encoding, announced } => writer_audit.event(if announced { "clipboard.announced" } else { "clipboard.sent" }, Level::Info, json!({
                    "clipboard.direction":"windows_to_mac", "clipboard.encoding":encoding, "clipboard.text_bytes":bytes,
                }))?,
                Notice::Ignored { bytes, reason } => writer_audit.event("clipboard.ignored", Level::Info, json!({
                    "clipboard.wire_bytes":bytes, "clipboard.reason":reason,
                }))?,
            }
            Ok(())
        };
        if let Err(error) = write_loop(
            &mut writer,
            incoming,
            request.streaming,
            &writer_control,
            &mut noticed,
        ) {
            if let Err(audit_error) = writer_audit.critical(
                "session.writer.failed",
                Level::Error,
                json!({"error.type":audit::error_type(&error)}),
            ) {
                eprintln!("MacCanvas could not persist the writer failure: {audit_error}");
            }
            if !writer_control.cancelled.load(Ordering::Acquire)
                && writer_events
                    .send((generation, Event::Error(error.to_string())))
                    .is_err()
            {
                eprintln!("MacCanvas input failed: {error}");
            }
            if let Err(error) = writer_control.close() {
                eprintln!("MacCanvas input shutdown failed: {error}");
            }
        }
    });
    events
        .send((
            generation,
            Event::Connected {
                profile,
                server_name: info.name,
                credentials_saved,
            },
        ))
        .map_err(|_| Error::Protocol("Connection window closed".into()))?;
    let mut decoder = Decoder::new(info.size);
    let result = loop {
        if control.cancelled.load(Ordering::Acquire) {
            break Ok(());
        }
        let bytes_before = counters.received.load(Ordering::Relaxed);
        match decoder.read_message_with_clipboard(&mut stream, || {
            control.clipboard_enabled.load(Ordering::Acquire)
        }) {
            Ok(Message::Update { changed }) => {
                if let Some(encoding) = decoder.last_encoding {
                    counters.encoding.store(encoding, Ordering::Relaxed);
                }
                if changed {
                    let mut latest = match frames.lock() {
                        Ok(latest) => latest,
                        Err(_) => break Err(Error::Display("Frame queue is unavailable".into())),
                    };
                    *latest = Some(Arc::new(decoder.frame.clone()));
                }
                if refresh
                    .send(WriterCommand::Complete {
                        size: decoder.frame.size,
                        bytes: counters
                            .received
                            .load(Ordering::Relaxed)
                            .saturating_sub(bytes_before),
                    })
                    .is_err()
                {
                    break if control.cancelled.load(Ordering::Acquire) {
                        Ok(())
                    } else {
                        Err(Error::Protocol("Input worker closed".into()))
                    };
                }
            }
            Ok(Message::Clipboard(packet)) => {
                if control.clipboard_pending.fetch_add(1, Ordering::AcqRel) >= 4 {
                    control.clipboard_pending.fetch_sub(1, Ordering::AcqRel);
                    break Err(Error::Protocol(
                        "Too many pending clipboard messages".into(),
                    ));
                }
                if refresh
                    .send(WriterCommand::ClipboardServer(packet))
                    .is_err()
                {
                    break Err(Error::Protocol("Clipboard writer closed".into()));
                }
            }
            Ok(Message::Other) => (),
            Err(_) if control.cancelled.load(Ordering::Acquire) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    control.close()?;
    writer_thread
        .join()
        .map_err(|_| Error::Protocol("Input worker stopped unexpectedly".into()))?;
    result
}

fn write_loop(
    writer: &mut impl std::io::Write,
    incoming: Receiver<WriterCommand>,
    mut settings: StreamSettings,
    control: &Control,
    mut noticed: impl FnMut(crate::clipboard::Notice) -> Result<()>,
) -> Result<()> {
    let mut clipboard = crate::clipboard::Transport::new(false);
    let mut last_request = Instant::now();
    let mut pending = None;
    let mut ignore_in_flight_budget = false;
    while !control.cancelled.load(Ordering::Acquire) {
        let delay = pending.map_or(Duration::from_millis(100), |(_, bytes)| {
            (last_request + settings.request_interval(bytes))
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100))
        });
        if let Some((size, bytes)) = pending
            && Instant::now() >= last_request + settings.request_interval(bytes)
        {
            Command::Refresh {
                size,
                incremental: true,
            }
            .write(writer)?;
            last_request = Instant::now();
            pending = None;
            continue;
        }
        match incoming.recv_timeout(delay) {
            Ok(WriterCommand::ClipboardEnabled(enabled)) => clipboard.enable(writer, enabled)?,
            Ok(WriterCommand::ClipboardLocal(text)) => {
                for notice in clipboard.local(writer, text)? {
                    noticed(notice)?;
                }
            }
            Ok(WriterCommand::ClipboardServer(packet)) => {
                control.clipboard_pending.fetch_sub(1, Ordering::AcqRel);
                for notice in clipboard.server(writer, packet)? {
                    noticed(notice)?;
                }
            }
            Ok(WriterCommand::Input(Command::Configure(updated))) => {
                updated.validate()?;
                settings = updated;
                Command::Configure(updated).write(writer)?;
                // A new target must not charge a frame requested under the old target.
                if let Some((size, _)) = pending {
                    pending = Some((size, 0));
                    ignore_in_flight_budget = false;
                } else {
                    ignore_in_flight_budget = true;
                }
                last_request = Instant::now();
            }
            Ok(WriterCommand::Input(command)) => {
                command.write(writer)?;
                if matches!(command, Command::Refresh { .. }) {
                    last_request = Instant::now();
                    pending = None;
                }
            }
            Ok(WriterCommand::Complete { size, bytes }) => {
                let bytes = if std::mem::take(&mut ignore_in_flight_budget) {
                    0
                } else {
                    bytes
                };
                pending = Some((size, bytes));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct SavedLogin {
        calls: RefCell<Vec<&'static str>>,
        approved: bool,
    }

    impl credentials::ConsentVerifier for SavedLogin {
        fn verify(&self) -> Result<()> {
            self.calls.borrow_mut().push("hello");
            if self.approved {
                Ok(())
            } else {
                Err(Error::Consent("fixture denied".into()))
            }
        }
    }

    impl CredentialStore for SavedLogin {
        fn load(&self, _: uuid::Uuid) -> Result<Credentials> {
            self.calls.borrow_mut().push("read");
            Ok(Credentials::new("fixture".into(), "FixtureOnly".into()))
        }

        fn save(&self, _: uuid::Uuid, _: &Credentials) -> Result<()> {
            unreachable!("Startup retries must not save credentials")
        }

        fn delete(&self, _: uuid::Uuid) -> Result<()> {
            unreachable!("Startup retries must not delete credentials")
        }
    }

    #[test]
    fn startup_retries_share_one_hello_approval_and_vault_read() {
        let directory = tempfile::tempdir().unwrap();
        let mut audit = audit::Audit::start(directory.path().join("audit")).unwrap();
        let scope = audit.audit().scope(json!({}));
        let login = SavedLogin {
            calls: RefCell::new(Vec::new()),
            approved: true,
        };
        let control = Control::new(false);
        let (events, receiver) = mpsc::channel();
        let mut attempts = 0;
        let connection = connect_with_retry(
            &scope,
            &events,
            7,
            &control,
            || {
                credentials::resolve(
                    uuid::Uuid::new_v4(),
                    CredentialSource::Saved,
                    &login,
                    &login,
                )
            },
            |credentials| {
                assert_eq!(credentials.username(), "fixture");
                assert_eq!(credentials.password(), "FixtureOnly");
                attempts += 1;
                match attempts {
                    1 => Err(std::io::Error::from(std::io::ErrorKind::ConnectionReset).into()),
                    2 => Err(Error::ConnectionLost("No first picture yet".into())),
                    _ => Ok("connected"),
                }
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(connection.0, "connected");
        assert_eq!(attempts, 3);
        assert_eq!(*login.calls.borrow(), ["hello", "read"]);
        let progress = receiver
            .try_iter()
            .map(|(generation, event)| {
                assert_eq!(generation, 7);
                match event {
                    Event::Retrying { attempt, delay } => (attempt, delay.as_secs()),
                    _ => panic!("Startup failures must report progress, not an error"),
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(progress, [(2, 1), (2, 0), (3, 2), (3, 0)]);
        audit.shutdown().unwrap();
    }

    #[test]
    fn startup_retries_are_bounded_and_preserve_the_final_error() {
        let directory = tempfile::tempdir().unwrap();
        let mut audit = audit::Audit::start(directory.path().join("audit")).unwrap();
        let scope = audit.audit().scope(json!({}));
        let control = Control::new(false);
        let (events, receiver) = mpsc::channel();
        let mut attempts = 0;
        let result = connect_with_retry::<()>(
            &scope,
            &events,
            1,
            &control,
            || Ok(Credentials::new("fixture".into(), "FixtureOnly".into())),
            |_| {
                attempts += 1;
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    format!("fixture attempt {attempts}"),
                )
                .into())
            },
        );
        match result {
            Err(Error::Io(error)) => assert_eq!(error.to_string(), "fixture attempt 4"),
            _ => panic!("The final startup failure must be returned"),
        }
        assert_eq!(attempts, 4);
        let delays = receiver
            .try_iter()
            .filter_map(|(_, event)| match event {
                Event::Retrying { attempt, delay } if !delay.is_zero() => {
                    Some((attempt, delay.as_secs()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(delays, [(2, 1), (3, 2), (4, 4)]);
        audit.shutdown().unwrap();
    }

    #[test]
    fn startup_never_retries_denied_consent_or_non_transport_failures() {
        let directory = tempfile::tempdir().unwrap();
        let mut audit = audit::Audit::start(directory.path().join("audit")).unwrap();
        let scope = audit.audit().scope(json!({}));
        let control = Control::new(false);
        let (events, receiver) = mpsc::channel();
        let login = SavedLogin {
            calls: RefCell::new(Vec::new()),
            approved: false,
        };
        let denied = connect_with_retry::<()>(
            &scope,
            &events,
            1,
            &control,
            || {
                credentials::resolve(
                    uuid::Uuid::new_v4(),
                    CredentialSource::Saved,
                    &login,
                    &login,
                )
            },
            |_| panic!("Denied consent must not start a connection"),
        );
        assert!(matches!(denied, Err(Error::Consent(_))));
        assert_eq!(*login.calls.borrow(), ["hello"]);
        for error in [
            Error::Authentication("fixture rejected".into()),
            Error::Credential("fixture missing".into()),
            Error::Audit("fixture audit failed".into()),
            Error::Protocol("fixture unsupported".into()),
            Error::Configuration("fixture invalid".into()),
        ] {
            let mut failure = Some(error);
            let mut attempts = 0;
            let result = connect_with_retry::<()>(
                &scope,
                &events,
                1,
                &control,
                || Ok(Credentials::new("fixture".into(), "FixtureOnly".into())),
                |_| {
                    attempts += 1;
                    Err(failure.take().expect("Fatal errors must not be retried"))
                },
            );
            assert!(result.is_err());
            assert_eq!(attempts, 1);
        }
        assert_eq!(receiver.try_iter().count(), 0);
        audit.shutdown().unwrap();
    }

    #[test]
    fn cancelling_startup_backoff_prevents_another_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let mut audit = audit::Audit::start(directory.path().join("audit")).unwrap();
        let scope = audit.audit().scope(json!({}));
        let control = Arc::new(Control::new(false));
        let worker_control = control.clone();
        let (events, receiver) = mpsc::channel();
        let (completed, result) = mpsc::channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let worker_attempts = attempts.clone();
        let worker = thread::spawn(move || {
            let outcome = connect_with_retry::<()>(
                &scope,
                &events,
                1,
                &worker_control,
                || Ok(Credentials::new("fixture".into(), "FixtureOnly".into())),
                |_| {
                    worker_attempts.fetch_add(1, Ordering::Relaxed);
                    Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused).into())
                },
            );
            completed.send(outcome).unwrap();
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap().1,
            Event::Retrying { attempt: 2, delay } if delay == Duration::from_secs(1)
        ));
        control.close().unwrap();
        assert!(
            result
                .recv_timeout(Duration::from_millis(500))
                .unwrap()
                .unwrap()
                .is_none()
        );
        worker.join().unwrap();
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        audit.shutdown().unwrap();
    }

    struct Sink {
        sent: Sender<Vec<u8>>,
    }
    impl std::io::Write for Sink {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.sent.send(data.to_vec()).unwrap();
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn bandwidth_pacing_never_blocks_pointer_or_key_release() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let control = Arc::new(Control::new(false));
        let (sent, received) = mpsc::channel();
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            write_loop(
                &mut Sink { sent },
                receiver,
                StreamSettings {
                    encoding: crate::streaming::Encoding::Auto,
                    target_kbps: 100,
                    ..Default::default()
                },
                &worker_control,
                |_| Ok(()),
            )
            .unwrap()
        });
        sender
            .send(WriterCommand::Complete {
                size: crate::geometry::Extent::new(4, 4).unwrap(),
                bytes: 1_000_000,
            })
            .unwrap();
        sender
            .send(WriterCommand::Input(Command::Key {
                keysym: 0xffe3,
                down: false,
            }))
            .unwrap();
        assert_eq!(
            received.recv_timeout(Duration::from_millis(250)).unwrap(),
            [4, 0, 0, 0]
        );
        control.cancelled.store(true, Ordering::Release);
        drop(sender);
        worker.join().unwrap();
    }

    #[test]
    fn lowering_a_target_does_not_freeze_updates_for_the_previous_frame() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let control = Arc::new(Control::new(false));
        let (sent, received) = mpsc::channel();
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            write_loop(
                &mut Sink { sent },
                receiver,
                StreamSettings {
                    encoding: crate::streaming::Encoding::Auto,
                    ..Default::default()
                },
                &worker_control,
                |_| Ok(()),
            )
            .unwrap()
        });
        sender
            .send(WriterCommand::Complete {
                size: crate::geometry::Extent::new(4, 4).unwrap(),
                bytes: 33_000_000,
            })
            .unwrap();
        sender
            .send(WriterCommand::Input(Command::Configure(StreamSettings {
                encoding: crate::streaming::Encoding::Auto,
                target_kbps: 100,
                ..Default::default()
            })))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let fragment = received
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if fragment.first() == Some(&3) {
                break;
            }
        }
        control.cancelled.store(true, Ordering::Release);
        drop(sender);
        worker.join().unwrap();
    }
}
