use std::{
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{Receiver, Sender, TryRecvError},
    },
    thread,
    time::Duration,
};

use serde_json::json;

use crate::{
    apple_hps,
    audit::Level,
    credentials::{self, AuditedConsent, AuditedVault, CredentialStore},
    error::{Error, Result},
    protocol,
    telemetry::{Counters, Metered},
};

use super::{Control, Event, Frames, Request, WriterCommand};

pub(super) fn run(
    request: Request,
    incoming: Receiver<WriterCommand>,
    events: &Sender<(u64, Event)>,
    frames: &Frames,
    control: &Arc<Control>,
    counters: Arc<Counters>,
) -> Result<()> {
    let credentials = credentials::resolve(
        request.profile.id,
        request.credentials,
        &AuditedConsent {
            scope: &request.audit,
            window: request.window,
        },
        &AuditedVault {
            scope: &request.audit,
        },
    )?;
    if control.cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    let stream = protocol::connect(&request.profile)?;
    request.audit.critical(
        "network.connection.established",
        Level::Info,
        json!({
            "server.socket.address":stream.peer_addr()?.ip().to_string(),
            "server.socket.port":stream.peer_addr()?.port(),
            "client.socket.address":stream.local_addr()?.ip().to_string(),
            "client.socket.port":stream.local_addr()?.port(),
            "network.protocol.name":"apple_screen_sharing","media.codec.requested":"hevc",
        }),
    )?;
    *control
        .socket
        .lock()
        .map_err(|_| Error::Protocol("Connection control is unavailable".into()))? =
        Some(stream.try_clone()?);
    if control.cancelled.load(Ordering::Acquire) {
        return Ok(());
    }
    request.audit.critical(
        "security.authentication.started",
        Level::Info,
        json!({"network.protocol.name":"apple_screen_sharing","rfb.security.type":33}),
    )?;
    let stream = Metered {
        inner: stream,
        counters,
    };
    let mut client = apple_hps::Client::open(
        stream,
        &credentials,
        request.streaming,
        request.audit.clone(),
        request.audio,
        request.window != 0,
    )?;
    let mut profile = Some(request.profile);
    let mut credentials = Some(credentials);
    while !control.cancelled.load(Ordering::Acquire) {
        while let Some(status) = client.audio_status()? {
            events
                .send((request.generation, Event::Audio(status)))
                .map_err(|_| Error::Protocol("Connection window closed".into()))?;
        }
        for _ in 0..128 {
            match incoming.try_recv() {
                Ok(WriterCommand::Input(command)) => client.input(command)?,
                Ok(WriterCommand::ClipboardEnabled(false)) | Ok(WriterCommand::Complete { .. }) => {
                }
                Ok(
                    WriterCommand::ClipboardEnabled(true)
                    | WriterCommand::ClipboardLocal(_)
                    | WriterCommand::ClipboardServer(_),
                ) => {
                    return Err(Error::Clipboard(
                        "HEVC uses the encrypted SSH controls in Clipboard".into(),
                    ));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        let frame = match client.poll() {
            Ok(frame) => frame,
            Err(_) if control.cancelled.load(Ordering::Acquire) => return Ok(()),
            Err(error) => return Err(error),
        };
        if let Some((update, point_scale)) = client.cursor_status() {
            events
                .send((
                    request.generation,
                    Event::Cursor {
                        update,
                        point_scale,
                    },
                ))
                .map_err(|_| Error::Protocol("Connection window closed".into()))?;
        }
        if let Some(frame) = frame {
            if let Some(mut connected) = profile.take() {
                let mut credentials_saved = false;
                if request.remember
                    && let Some(credentials) = &credentials
                {
                    match (AuditedVault {
                        scope: &request.audit,
                    })
                    .save(connected.id, credentials)
                    {
                        Ok(()) => {
                            connected.has_saved_credentials = true;
                            credentials_saved = true;
                        }
                        Err(error) => events
                            .send((
                                request.generation,
                                Event::Warning(format!(
                                    "Connected, but the password was not saved: {error}"
                                )),
                            ))
                            .map_err(|_| Error::Protocol("Connection window closed".into()))?,
                    }
                }
                credentials.take();
                request.audit.critical(
                    "media.hevc.first_frame",
                    Level::Info,
                    json!({
                        "media.codec.name":"hevc","media.width":frame.size().width,
                        "media.height":frame.size().height,"network.encrypted":true,
                    }),
                )?;
                events
                    .send((
                        request.generation,
                        Event::Connected {
                            profile: connected,
                            server_name: client.info.name.clone(),
                            credentials_saved,
                        },
                    ))
                    .map_err(|_| Error::Protocol("Connection window closed".into()))?;
            }
            match frame {
                apple_hps::Frame::Software(frame) => {
                    control
                        .gpu_frame
                        .lock()
                        .map_err(|_| Error::Display("GPU frame queue is unavailable".into()))?
                        .take();
                    *frames
                        .lock()
                        .map_err(|_| Error::Display("Frame queue is unavailable".into()))? =
                        Some(Arc::new(frame));
                }
                apple_hps::Frame::Gpu(frame) => {
                    frames
                        .lock()
                        .map_err(|_| Error::Display("Frame queue is unavailable".into()))?
                        .take();
                    *control
                        .gpu_frame
                        .lock()
                        .map_err(|_| Error::Display("GPU frame queue is unavailable".into()))? =
                        Some(frame);
                }
            }
            let notifier = control
                .frame_notifier
                .lock()
                .map_err(|_| Error::Display("Frame notification is unavailable".into()))?
                .clone();
            if let Some(notifier) = notifier {
                notifier();
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}
