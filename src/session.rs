use std::{
    net::{Shutdown, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender, SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};

use crate::{
    credentials::{self, CredentialSource, CredentialStore, WindowsHello, WindowsVault},
    error::{Error, Result},
    framebuffer::Framebuffer,
    input::Command,
    profiles::Profile,
    protocol::{self, Decoder, Message},
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
    pub hello_for_all: bool,
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
    sender: SyncSender<Command>,
}

impl Session {
    pub fn start(request: Request, events: Sender<(u64, Event)>, frames: Frames) -> Self {
        let control = Arc::new(Control {
            cancelled: AtomicBool::new(false),
            socket: Mutex::new(None),
        });
        let (sender, receiver) = mpsc::sync_channel(256);
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
        Self { control, sender }
    }

    pub fn send(&self, command: Command) -> Result<()> {
        match self.sender.try_send(command) {
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
    incoming: Receiver<Command>,
    refresh: SyncSender<Command>,
    events: &Sender<(u64, Event)>,
    frames: &Frames,
    control: &Arc<Control>,
) -> Result<()> {
    let generation = request.generation;
    let credentials = credentials::resolve(
        request.profile.id,
        request.credentials,
        request.hello_for_all,
        &WindowsHello {
            window: request.window,
        },
        &WindowsVault,
    )?;
    if control.cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut stream = protocol::connect(&request.profile)?;
    *control
        .socket
        .lock()
        .map_err(|_| Error::Protocol("Connection control is unavailable".into()))? =
        Some(stream.try_clone()?);
    if control.cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let info = protocol::handshake(&mut stream, request.profile.authentication, &credentials)?;
    stream.set_read_timeout(None)?;
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
    let mut writer = stream.try_clone()?;
    writer.set_write_timeout(Some(Duration::from_secs(3)))?;
    let writer_control = control.clone();
    let writer_events = events.clone();
    let writer_thread = thread::spawn(move || {
        while !writer_control.cancelled.load(Ordering::Acquire) {
            match incoming.recv_timeout(Duration::from_millis(100)) {
                Ok(command) => {
                    if let Err(error) = command.write(&mut writer) {
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
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
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
        match decoder.read_message(&mut stream) {
            Ok(Message::Update { changed }) => {
                if changed {
                    let mut latest = match frames.lock() {
                        Ok(latest) => latest,
                        Err(_) => break Err(Error::Display("Frame queue is unavailable".into())),
                    };
                    *latest = Some(Arc::new(decoder.frame.clone()));
                }
                if refresh
                    .send(Command::Refresh {
                        size: decoder.frame.size,
                        incremental: true,
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
