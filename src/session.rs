use std::{
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    credentials::{self, CredentialSource, CredentialStore, WindowsHello, WindowsVault},
    error::{Error, Result},
    framebuffer::Framebuffer,
    input::Command,
    profiles::Profile,
    protocol::{self, Decoder, Message},
    streaming::StreamSettings,
    telemetry::{Counters, Metered},
};

pub type Frames = Arc<Mutex<Option<Arc<Framebuffer>>>>;

pub enum Event {
    Connected {
        profile: Profile,
        server_name: String,
    },
    Warning(String),
    Error(String),
    Disconnected,
}

pub struct Request {
    pub profile: Profile,
    pub credentials: CredentialSource,
    pub streaming: StreamSettings,
    pub remember: bool,
    pub window: isize,
    pub generation: u64,
}

struct Control {
    cancelled: AtomicBool,
    socket: Mutex<Option<TcpStream>>,
}

impl Control {
    fn close(&self) -> Result<()> {
        self.cancelled.store(true, Ordering::Release);
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
}

enum WriterCommand {
    Input(Command),
    Complete {
        size: crate::geometry::Extent,
        bytes: u64,
    },
}

impl Session {
    pub fn start(request: Request, events: Sender<(u64, Event)>, frames: Frames) -> Self {
        let control = Arc::new(Control {
            cancelled: AtomicBool::new(false),
            socket: Mutex::new(None),
        });
        let (sender, receiver) = mpsc::sync_channel(256);
        let counters = Counters::new();
        let traffic = counters.clone();
        let worker_control = control.clone();
        let refresh = sender.clone();
        thread::spawn(move || {
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
            if let Err(error) = outcome
                && !worker_control.cancelled.load(Ordering::Acquire)
                && events
                    .send((generation, Event::Error(error.to_string())))
                    .is_err()
            {
                eprintln!("MacCanvas connection failed: {error}");
            }
            if let Err(error) = worker_control.close() {
                eprintln!("MacCanvas shutdown failed: {error}");
            }
            if events.send((generation, Event::Disconnected)).is_err() {
                eprintln!("MacCanvas connection window has closed");
            }
        });
        Self {
            control,
            sender,
            counters,
        }
    }

    pub fn send(&self, command: Command) -> Result<()> {
        match self.sender.try_send(WriterCommand::Input(command)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.control.close()?;
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
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.control.close() {
            eprintln!("MacCanvas connection cleanup failed: {error}");
        }
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
    let generation = request.generation;
    let credentials = credentials::resolve(
        request.profile.id,
        request.credentials,
        &WindowsHello {
            window: request.window,
        },
        &WindowsVault,
    )?;
    if control.cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let stream = protocol::connect(&request.profile)?;
    *control
        .socket
        .lock()
        .map_err(|_| Error::Protocol("Connection control is unavailable".into()))? =
        Some(stream.try_clone()?);
    if control.cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut stream = Metered {
        inner: stream,
        counters: counters.clone(),
    };
    let info = protocol::handshake_with_settings(
        &mut stream,
        request.profile.authentication,
        &credentials,
        request.streaming,
    )?;
    stream.inner.set_read_timeout(None)?;
    let mut profile = request.profile;
    if request.remember {
        match WindowsVault.save(profile.id, &credentials) {
            Ok(()) => profile.has_saved_credentials = true,
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
    let writer_thread = thread::spawn(move || {
        if let Err(error) = write_loop(&mut writer, incoming, request.streaming, &writer_control) {
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
            },
        ))
        .map_err(|_| Error::Protocol("Connection window closed".into()))?;
    let mut decoder = Decoder::new(info.size);
    let result = loop {
        if control.cancelled.load(Ordering::Acquire) {
            break Ok(());
        }
        let bytes_before = counters.received.load(Ordering::Relaxed);
        match decoder.read_message(&mut stream) {
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
) -> Result<()> {
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
        let control = Arc::new(Control {
            cancelled: AtomicBool::new(false),
            socket: Mutex::new(None),
        });
        let (sent, received) = mpsc::channel();
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            write_loop(
                &mut Sink { sent },
                receiver,
                StreamSettings {
                    target_kbps: 100,
                    ..Default::default()
                },
                &worker_control,
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
        let control = Arc::new(Control {
            cancelled: AtomicBool::new(false),
            socket: Mutex::new(None),
        });
        let (sent, received) = mpsc::channel();
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            write_loop(
                &mut Sink { sent },
                receiver,
                StreamSettings::default(),
                &worker_control,
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
