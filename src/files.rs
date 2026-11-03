use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use russh::{
    ChannelMsg, client,
    keys::{HashAlg, PublicKeyOrCertificate},
};
use russh_sftp::{
    client::{Config as SftpConfig, RawSftpSession, error::Error as SftpError},
    protocol::{FileAttributes, OpenFlags, StatusCode},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc as async_mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    audit::{self, Level, Scope},
    clipboard,
    credentials::{self, AuditedConsent, AuditedVault, CredentialSource},
    error::{Error, Result},
    profiles::{AuthMode, Profile, SshHostKey},
    telemetry::{Counters, Metered},
};

pub mod dialog;

const CHUNK: usize = 32 * 1024;
const MAX_FILE: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 2048;
const MAX_PATH: usize = 4096;

const READ_CLIPBOARD: &str = r#"/usr/bin/osascript -l JavaScript -e '
var stage = "load_foundation", reason = null, desktop = null, sameAccount = null;
try {
    ObjC.import("Foundation");
    try {
        var attributes = $.NSFileManager.defaultManager.attributesOfItemAtPathError("/dev/console", null);
        var owner = ObjC.unwrap(attributes.objectForKey($.NSFileOwnerAccountName));
        desktop = !!owner && owner !== "root" && owner !== "loginwindow" && owner !== "_mbsetupuser";
        sameAccount = desktop ? owner === ObjC.unwrap($.NSUserName()) : null;
    } catch (ignored) {}
    stage = "load_appkit"; ObjC.import("AppKit");
    stage = "open_pasteboard"; var pasteboard = $.NSPasteboard.generalPasteboard;
    stage = "read_plain_text"; var text = pasteboard.stringForType($.NSPasteboardTypeString);
    stage = "check_plain_text";
    if (text.isNil()) { reason = "no_plain_text"; throw Error("No plain text is available"); }
    stage = "encode_plain_text"; var data = text.dataUsingEncoding($.NSUTF8StringEncoding);
    stage = "write_output"; $.NSFileHandle.fileHandleWithStandardOutput.writeData(data);
} catch (error) {
    if (!reason) {
        reason = error.name === "TypeError" ? "type_error" : error.name === "ReferenceError" ? "reference_error" : error.name === "SyntaxError" ? "syntax_error" : "helper_exception";
    }
    var code = typeof error.errorNumber === "number" ? error.errorNumber : typeof error.number === "number" ? error.number : null;
    throw Error("MACCANVAS_CLIPBOARD_DIAGNOSTIC=" + JSON.stringify({version:1, stage:stage, reason:reason, error_code:code, desktop_session:desktop, same_desktop_account:sameAccount}));
}'"#;
const WRITE_CLIPBOARD: &str = r#"/usr/bin/osascript -l JavaScript -e '
var stage = "load_foundation", reason = null, desktop = null, sameAccount = null;
try {
    ObjC.import("Foundation");
    try {
        var attributes = $.NSFileManager.defaultManager.attributesOfItemAtPathError("/dev/console", null);
        var owner = ObjC.unwrap(attributes.objectForKey($.NSFileOwnerAccountName));
        desktop = !!owner && owner !== "root" && owner !== "loginwindow" && owner !== "_mbsetupuser";
        sameAccount = desktop ? owner === ObjC.unwrap($.NSUserName()) : null;
    } catch (ignored) {}
    stage = "load_appkit"; ObjC.import("AppKit");
    stage = "read_input"; var data = $.NSFileHandle.fileHandleWithStandardInput.readDataToEndOfFile;
    stage = "decode_input"; var text = $.NSString.alloc.initWithDataEncoding(data, $.NSUTF8StringEncoding);
    if (text.isNil()) { reason = "invalid_utf8"; throw Error("Invalid UTF-8 input"); }
    stage = "open_pasteboard"; var pasteboard = $.NSPasteboard.generalPasteboard;
    stage = "clear_pasteboard"; pasteboard.clearContents;
    stage = "write_plain_text";
    if (!pasteboard.setStringForType(text, $.NSPasteboardTypeString)) { reason = "write_rejected"; throw Error("Cannot set plain text"); }
} catch (error) {
    if (!reason) {
        reason = error.name === "TypeError" ? "type_error" : error.name === "ReferenceError" ? "reference_error" : error.name === "SyntaxError" ? "syntax_error" : "helper_exception";
    }
    var code = typeof error.errorNumber === "number" ? error.errorNumber : typeof error.number === "number" ? error.number : null;
    throw Error("MACCANVAS_CLIPBOARD_DIAGNOSTIC=" + JSON.stringify({version:1, stage:stage, reason:reason, error_code:code, desktop_session:desktop, same_desktop_account:sameAccount}));
}'"#;

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ClipboardStage {
    LoadFoundation,
    LoadAppkit,
    OpenPasteboard,
    ReadPlainText,
    CheckPlainText,
    EncodePlainText,
    WriteOutput,
    ReadInput,
    DecodeInput,
    ClearPasteboard,
    WritePlainText,
}

#[derive(Clone, Copy, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ClipboardReason {
    NoPlainText,
    InvalidUtf8,
    WriteRejected,
    TypeError,
    ReferenceError,
    SyntaxError,
    HelperException,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipboardDiagnostic {
    version: u8,
    stage: ClipboardStage,
    reason: ClipboardReason,
    error_code: Option<i32>,
    desktop_session: Option<bool>,
    same_desktop_account: Option<bool>,
}

impl ClipboardDiagnostic {
    fn parse(stderr: &[u8]) -> Option<Self> {
        std::str::from_utf8(stderr).ok()?.lines().find_map(|line| {
            let marker = "MACCANVAS_CLIPBOARD_DIAGNOSTIC=";
            let payload = &line[line.find(marker)? + marker.len()..];
            let diagnostic = serde_json::Deserializer::from_str(payload)
                .into_iter::<Self>()
                .next()?
                .ok()?;
            (diagnostic.version == 1).then_some(diagnostic)
        })
    }

    fn message(&self) -> String {
        let detail = match self.reason {
            ClipboardReason::NoPlainText => {
                "The Mac clipboard has no plain text. Copy some text on the Mac and retry."
            }
            ClipboardReason::InvalidUtf8 => "The Mac clipboard helper rejected its UTF-8 input.",
            ClipboardReason::WriteRejected => "The Mac pasteboard rejected the text write.",
            ClipboardReason::TypeError => {
                "The Mac clipboard helper encountered a JavaScript type error."
            }
            ClipboardReason::ReferenceError => {
                "The Mac clipboard helper could not resolve a JavaScript or AppKit symbol."
            }
            ClipboardReason::SyntaxError => {
                "The Mac clipboard helper encountered a JavaScript syntax error."
            }
            ClipboardReason::HelperException => "The Mac clipboard helper raised an exception.",
        };
        let account = if self.desktop_session == Some(false) {
            "No logged-in console desktop was detected."
        } else {
            match self.same_desktop_account {
                Some(true) => "SSH matches the logged-in Mac desktop account.",
                Some(false) => "SSH does not match the logged-in Mac desktop account.",
                None => "The desktop account match could not be determined.",
            }
        };
        let code = self
            .error_code
            .map_or_else(|| "not reported".into(), |value| value.to_string());
        format!(
            "{detail} Stage: {:?}; macOS error: {code}. {account}",
            self.stage
        )
    }
}

impl From<russh::Error> for Error {
    fn from(error: russh::Error) -> Self {
        Error::Transfer(format!("SSH: {error}"))
    }
}

impl From<SftpError> for Error {
    fn from(error: SftpError) -> Self {
        match error {
            SftpError::Status(status) => {
                let message = if status.status_code == StatusCode::PermissionDenied {
                    "The Mac denied access. Choose a folder this SSH account can access. For protected Desktop, Documents or Downloads folders, check 'Allow full disk access for remote users' in the Mac's Remote Login settings, then reconnect SSH. Also check the account's folder permissions.".into()
                } else {
                    format!("SFTP request rejected: {}", status.status_code)
                };
                Error::Sftp {
                    code: status.status_code as u32,
                    message,
                }
            }
            error => Error::Transfer(format!("SFTP: {error}")),
        }
    }
}

#[derive(Clone)]
pub struct Entry {
    pub name: String,
    pub directory: bool,
    pub regular: bool,
    pub size: Option<u64>,
}

#[derive(Clone, Default)]
pub struct Progress {
    pub phase: &'static str,
    pub bytes: u64,
    pub total: u64,
    pub bits_per_second: f64,
}

pub enum Event {
    HostKey {
        key: SshHostKey,
        reply: oneshot::Sender<bool>,
    },
    Connected,
    Directory {
        path: String,
        entries: Vec<Entry>,
    },
    Completed {
        description: String,
    },
    Clipboard(Zeroizing<String>),
    ClipboardSent(u32),
    Failed(String),
    DirectoryFailed {
        path: String,
        error: String,
    },
    Closed,
}

pub enum Command {
    List(String),
    Upload {
        local: PathBuf,
        directory: String,
        name: String,
    },
    Download {
        remote: String,
        local: PathBuf,
    },
    CopyClipboard,
    SendClipboard {
        text: Zeroizing<String>,
        sequence: u32,
    },
}

impl Command {
    fn attributes(&self) -> Result<serde_json::Value> {
        Ok(match self {
            Self::List(path) => {
                valid_path(path)?;
                json!({"operation.name":"directory_list","file.path":path})
            }
            Self::Upload {
                local,
                directory,
                name,
            } => {
                valid_path(directory)?;
                valid_name(name)?;
                json!({"operation.name":"file_upload","file.source.path":local,"file.destination.directory":directory,"file.destination.name":name})
            }
            Self::Download { remote, local } => {
                valid_path(remote)?;
                json!({"operation.name":"file_download","file.source.path":remote,"file.destination.path":local})
            }
            Self::CopyClipboard => {
                json!({"operation.name":"clipboard_copy","clipboard.direction":"mac_to_windows"})
            }
            Self::SendClipboard { text, .. } => {
                clipboard::validate(text, true)?;
                json!({"operation.name":"clipboard_send","clipboard.direction":"windows_to_mac","clipboard.text_bytes":text.len()})
            }
        })
    }
}

pub struct Request {
    pub profile: Profile,
    pub port: u16,
    pub credentials: CredentialSource,
    pub pinned: Option<SshHostKey>,
    pub window: isize,
    pub audit: Scope,
}

struct Control {
    stop: CancellationToken,
    cancel: AtomicBool,
    progress: Mutex<Progress>,
}

impl Control {
    fn check(&self) -> Result<()> {
        if self.stop.is_cancelled() || self.cancel.load(Ordering::Acquire) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn progress(
        &self,
        phase: &'static str,
        bytes: u64,
        total: u64,
        started: Instant,
    ) -> Result<()> {
        *self
            .progress
            .lock()
            .map_err(|_| Error::Transfer("Progress state is unavailable".into()))? = Progress {
            phase,
            bytes,
            total,
            bits_per_second: bytes as f64 * 8.0 / started.elapsed().as_secs_f64().max(0.001),
        };
        Ok(())
    }
}

pub struct Client {
    pub counters: Arc<Counters>,
    scope: Scope,
    commands: async_mpsc::Sender<Command>,
    events: Receiver<Event>,
    control: Arc<Control>,
    worker: Option<JoinHandle<()>>,
}

fn emit(events: &SyncSender<Event>, event: Event) -> Result<()> {
    events
        .try_send(event)
        .map_err(|_| Error::Transfer("File window is closed or its event queue is full".into()))
}

impl Client {
    pub fn start(request: Request) -> Result<Self> {
        request.profile.validate()?;
        if request.port == 0 {
            return Err(Error::Transfer("SSH port must be 1-65535".into()));
        }
        if matches!(request.credentials, CredentialSource::Saved)
            && (request.profile.authentication != AuthMode::MacLogin
                || !request.profile.has_saved_credentials)
        {
            return Err(Error::Transfer(
                "Classic VNC passwords cannot be used for SSH. Enter the Mac account login.".into(),
            ));
        }
        if request
            .pinned
            .as_ref()
            .is_some_and(|key| !key.endpoint(&request.profile.host, request.port))
        {
            return Err(Error::Transfer(
                "The pinned SSH key belongs to a different endpoint".into(),
            ));
        }
        request.audit.critical("ssh.connection.requested", Level::Info, json!({
            "server.address": request.profile.host, "server.port":request.port, "network.protocol.name":"ssh",
            "credential.source": if matches!(request.credentials, CredentialSource::Saved) { "saved" } else { "manual" },
        }))?;
        let (commands, incoming) = async_mpsc::channel(16);
        let (events, receiver) = mpsc::sync_channel(64);
        let control = Arc::new(Control {
            stop: CancellationToken::new(),
            cancel: AtomicBool::new(false),
            progress: Mutex::new(Progress::default()),
        });
        let worker_control = control.clone();
        let scope = request.audit.clone();
        let client_scope = scope.clone();
        let counters = Counters::new();
        let worker_counters = counters.clone();
        let worker = thread::Builder::new().name("maccanvas-ssh".into()).spawn(move || {
            let started = Instant::now();
            let result = tokio::runtime::Builder::new_current_thread().enable_all().build()
                .map_err(Error::from).and_then(|runtime| runtime.block_on(run(request, incoming, &events, &worker_control,worker_counters.clone())));
            if let Err(error) = &result
                && !matches!(error, Error::Cancelled)
                && let Err(event_error) = emit(&events, Event::Failed(error.to_string()))
            { eprintln!("MacCanvas SSH notification failed: {event_error}"); }
            if let Err(error) = scope.critical("ssh.connection.closed", if result.is_ok() || matches!(result, Err(Error::Cancelled)) { Level::Info } else { Level::Error }, json!({
                "event.outcome":if result.is_ok() { "closed" } else { "failure_or_cancelled" },
                "error.type":result.as_ref().err().map(audit::error_type), "session.duration_ms":started.elapsed().as_millis() as u64,
                "network.io.received":worker_counters.received.load(Ordering::Relaxed),"network.io.sent":worker_counters.sent.load(Ordering::Relaxed),
            })) { eprintln!("MacCanvas SSH summary audit failed: {error}"); }
            if let Err(error) = emit(&events, Event::Closed) { eprintln!("MacCanvas SSH close notification: {error}"); }
        })?;
        Ok(Self {
            commands,
            events: receiver,
            control,
            worker: Some(worker),
            counters,
            scope: client_scope,
        })
    }

    pub fn command(&self, command: Command) -> Result<()> {
        if let Err(error) = command.attributes() {
            self.scope.event(
                "ssh.operation.validation_failed",
                Level::Warn,
                audit::error_attributes(&error),
            )?;
            return Err(error);
        }
        if self.control.stop.is_cancelled() {
            return Err(Error::Transfer("SSH is disconnected".into()));
        }
        self.control.cancel.store(false, Ordering::Release);
        *self
            .control
            .progress
            .lock()
            .map_err(|_| Error::Transfer("Progress state is unavailable".into()))? =
            Progress::default();
        self.commands
            .try_send(command)
            .map_err(|_| Error::Transfer("The SSH connection is closed or busy".into()))
    }

    pub fn event(&self) -> Result<Option<Event>> {
        match self.events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) if self.is_finished() => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(Error::Transfer(
                "The SSH event channel closed unexpectedly".into(),
            )),
        }
    }

    pub fn progress(&self) -> Result<Progress> {
        self.control
            .progress
            .lock()
            .map(|value| value.clone())
            .map_err(|_| Error::Transfer("Progress state is unavailable".into()))
    }

    pub fn cancel(&self) {
        self.control.cancel.store(true, Ordering::Release);
    }
    pub fn close(&self) {
        self.control.stop.cancel();
        self.cancel();
    }
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    pub fn finish(mut self) -> Result<()> {
        self.close();
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| Error::Transfer("The SSH worker stopped unexpectedly".into()))?;
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

struct HostVerifier {
    host: String,
    port: u16,
    trusted: Option<SshHostKey>,
    events: SyncSender<Event>,
    stop: CancellationToken,
    scope: Scope,
}

impl client::Handler for HostVerifier {
    type Error = Error;

    async fn check_server_key(&mut self, public: &PublicKeyOrCertificate) -> Result<bool> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = public else {
            return Err(Error::Transfer(
                "SSH certificates are not supported; use the Mac's regular host key".into(),
            ));
        };
        let key = SshHostKey {
            host: self.host.clone(),
            port: self.port,
            algorithm: key.algorithm().to_string(),
            sha256: key.fingerprint(HashAlg::Sha256).to_string(),
        };
        key.validate()?;
        self.scope.critical("ssh.host_key.observed", Level::Info, json!({"ssh.host_key.algorithm":key.algorithm,"ssh.host_key.fingerprint.sha256":key.sha256}))?;
        if let Some(trusted) = &self.trusted {
            if !trusted.endpoint(&key.host, key.port)
                || trusted.algorithm != key.algorithm
                || trusted.sha256 != key.sha256
            {
                self.scope.critical("ssh.host_key.changed", Level::Error, json!({"ssh.host_key.expected.sha256":trusted.sha256,"ssh.host_key.observed.sha256":key.sha256,"event.outcome":"blocked"}))?;
                return Err(Error::Transfer("The SSH host key has changed. Connection blocked before credentials were read or sent. Verify the Mac's key outside MacCanvas.".into()));
            }
            return Ok(true);
        }
        let (reply, approval) = oneshot::channel();
        self.scope
            .critical("ssh.host_key.approval_requested", Level::Info, json!({}))?;
        emit(
            &self.events,
            Event::HostKey {
                key: key.clone(),
                reply,
            },
        )?;
        let approved = tokio::select! {
            _ = self.stop.cancelled() => return Err(Error::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(120), approval) => match result {
                Ok(Ok(value)) => value,
                Ok(Err(_)) => return Err(Error::Cancelled),
                Err(_) => return Err(Error::Transfer("SSH fingerprint approval timed out".into())),
            },
        };
        self.scope.critical(
            "ssh.host_key.approval_completed",
            if approved { Level::Info } else { Level::Warn },
            json!({"event.outcome":if approved { "accepted" } else { "rejected" }}),
        )?;
        if approved {
            self.trusted = Some(key);
        }
        Ok(approved)
    }
}

async fn run(
    request: Request,
    mut commands: async_mpsc::Receiver<Command>,
    events: &SyncSender<Event>,
    control: &Control,
    counters: Arc<Counters>,
) -> Result<()> {
    let tcp = tokio::select! {
        _ = control.stop.cancelled() => return Err(Error::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(10), tokio::net::TcpStream::connect((request.profile.host.as_str(),request.port))) =>
            result.map_err(|_| Error::Transfer("SSH connection timed out; enable macOS Remote Login".into()))??,
    };
    tcp.set_nodelay(true)?;
    request.audit.critical("ssh.transport.connected", Level::Info, json!({
        "server.socket.address":tcp.peer_addr()?.ip().to_string(),"server.socket.port":tcp.peer_addr()?.port(),
        "client.socket.address":tcp.local_addr()?.ip().to_string(),"client.socket.port":tcp.local_addr()?.port(),
    }))?;
    let config = client::Config {
        keepalive_interval: Some(Duration::from_secs(20)),
        keepalive_max: 3,
        maximum_packet_size: 32 * 1024,
        channel_buffer_size: 8,
        ..Default::default()
    };
    let verifier = HostVerifier {
        host: request.profile.host.clone(),
        port: request.port,
        trusted: request.pinned,
        events: events.clone(),
        stop: control.stop.clone(),
        scope: request.audit.clone(),
    };
    let tcp = Metered {
        inner: tcp,
        counters,
    };
    let mut ssh = tokio::select! {
        _ = control.stop.cancelled() => return Err(Error::Cancelled),
        result = tokio::time::timeout(Duration::from_secs(150), client::connect_stream(Arc::new(config),tcp,verifier)) =>
            result.map_err(|_| Error::Transfer("SSH negotiation timed out".into()))??,
    };
    control.check()?;
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
    credentials.validate(AuthMode::MacLogin)?;
    control.check()?;
    request.audit.critical(
        "ssh.authentication.started",
        Level::Info,
        json!({"network.encrypted":true}),
    )?;
    let authenticated = tokio::time::timeout(
        Duration::from_secs(30),
        ssh.authenticate_password(credentials.username(), credentials.password()),
    )
    .await
    .map_err(|_| Error::Transfer("SSH authentication timed out".into()))?
    .map_err(|_| {
        Error::Transfer(
            "SSH authentication failed; no server-supplied credential details are displayed".into(),
        )
    })?
    .success();
    drop(credentials);
    if !authenticated {
        return Err(Error::Transfer(
            "The Mac rejected this account login or requires an unsupported authentication method"
                .into(),
        ));
    }
    request.audit.critical(
        "ssh.authentication.completed",
        Level::Info,
        json!({"event.outcome":"success","network.encrypted":true}),
    )?;
    control.check()?;
    let channel = ssh.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    let mut sftp = RawSftpSession::new_with_config(
        channel.into_stream(),
        SftpConfig {
            max_packet_len: 256 * 1024,
            max_concurrent_reads: 4,
            max_concurrent_writes: 4,
            request_timeout_secs: 10,
            ..Default::default()
        },
    );
    let version = sftp.init().await?;
    if version.version != 3 {
        return Err(Error::Transfer(
            "The server did not negotiate SFTP version 3".into(),
        ));
    }
    let fsync = version
        .extensions
        .get("fsync@openssh.com")
        .is_some_and(|version| version == "1");
    if version
        .extensions
        .get("limits@openssh.com")
        .is_some_and(|version| version == "1")
    {
        let limits = sftp.limits().await?;
        sftp.set_limits(limits.into());
    }
    emit(events, Event::Connected)?;
    let mut initial = Some(Command::List(".".into()));
    loop {
        let command = match initial.take() {
            Some(command) => command,
            None => tokio::select! {
                _ = control.stop.cancelled() => break,
                command = commands.recv() => match command { Some(command) => command, None => break },
            },
        };
        let browse_path = match &command {
            Command::List(path) => Some(path.clone()),
            _ => None,
        };
        let mut attributes = command.attributes()?;
        attributes["operation.id"] = json!(Uuid::new_v4());
        let operation = request.audit.child(attributes);
        operation.critical("ssh.operation.requested", Level::Info, json!({}))?;
        let result = match command {
            Command::List(path) => list(&sftp, &path, events, control, &operation).await,
            Command::Upload {
                local,
                directory,
                name,
            } => upload(&sftp, fsync, &local, &directory, &name, control, &operation)
                .await
                .and_then(|_| {
                    emit(
                        events,
                        Event::Completed {
                            description: "File sent and verified by reading it back from the Mac."
                                .into(),
                        },
                    )
                }),
            Command::Download { remote, local } => {
                download(&sftp, &remote, &local, control, &operation)
                    .await
                    .and_then(|_| {
                        emit(
                            events,
                            Event::Completed {
                                description:
                                    "File received. A SHA-256 receipt is in the local audit log."
                                        .into(),
                            },
                        )
                    })
            }
            Command::CopyClipboard => mac_clipboard(&ssh, None, control, &operation)
                .await
                .and_then(|text| emit(events, Event::Clipboard(text))),
            Command::SendClipboard { text, sequence } => {
                mac_clipboard(&ssh, Some(text), control, &operation)
                    .await
                    .and_then(|_| emit(events, Event::ClipboardSent(sequence)))
            }
        };
        if let Err(error) = result {
            let recorded = operation.critical(
                if matches!(error, Error::Cancelled) {
                    "ssh.operation.cancelled"
                } else {
                    "ssh.operation.failed"
                },
                if matches!(error, Error::Cancelled) {
                    Level::Info
                } else {
                    Level::Error
                },
                audit::error_attributes(&error),
            );
            emit(
                events,
                match browse_path {
                    Some(path) => Event::DirectoryFailed {
                        path,
                        error: error.to_string(),
                    },
                    None => Event::Failed(error.to_string()),
                },
            )?;
            recorded?;
        }
    }
    sftp.close_session()?;
    tokio::time::timeout(
        Duration::from_secs(3),
        ssh.disconnect(
            russh::Disconnect::ByApplication,
            "MacCanvas disconnected",
            "en",
        ),
    )
    .await
    .map_err(|_| Error::Transfer("SSH close timed out".into()))??;
    Ok(())
}

pub fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|character| character.is_control() || "/\\".contains(character))
    {
        return Err(Error::Transfer("Use a single file name without slashes or control characters (at most 255 UTF-8 bytes)".into()));
    }
    Ok(())
}

fn valid_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > MAX_PATH || path.chars().any(char::is_control) {
        return Err(Error::Transfer(
            "The remote path is empty, too long, or contains control characters".into(),
        ));
    }
    Ok(())
}

pub fn join(directory: &str, name: &str) -> Result<String> {
    valid_path(directory)?;
    valid_name(name)?;
    let path = format!("{}/{}", directory.trim_end_matches('/'), name);
    valid_path(&path)?;
    Ok(path)
}

pub fn parent(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return "/".into();
    }
    path.rsplit_once('/').map_or_else(
        || ".".into(),
        |(directory, _)| {
            if directory.is_empty() {
                "/".into()
            } else {
                directory.into()
            }
        },
    )
}

async fn canonical(sftp: &RawSftpSession, path: &str) -> Result<String> {
    valid_path(path)?;
    let result = sftp.realpath(path).await?;
    if result.files.len() != 1 {
        return Err(Error::Transfer(
            "The server returned an ambiguous canonical path".into(),
        ));
    }
    let path = result
        .files
        .into_iter()
        .next()
        .expect("checked path count")
        .filename;
    valid_path(&path)?;
    if !path.starts_with('/') {
        return Err(Error::Transfer(
            "The server returned a non-absolute canonical path".into(),
        ));
    }
    Ok(path)
}

async fn list(
    sftp: &RawSftpSession,
    path: &str,
    events: &SyncSender<Event>,
    control: &Control,
    scope: &Scope,
) -> Result<()> {
    control.check()?;
    let path = canonical(sftp, path).await?;
    scope.event(
        "sftp.directory.requested",
        Level::Info,
        json!({"file.path":path}),
    )?;
    let handle = sftp.opendir(&path).await?.handle;
    let result = async {
        let mut entries = Vec::new();
        let mut total_names = 0;
        loop {
            control.check()?;
            let names = match sftp.readdir(&handle).await {
                Ok(names) => names,
                Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break,
                Err(error) => return Err(error.into()),
            };
            if names.files.is_empty() { return Err(Error::Transfer("The server returned an empty non-final directory page".into())); }
            for entry in names.files {
                if matches!(entry.filename.as_str(),"."|"..") { continue; }
                valid_name(&entry.filename)?;
                total_names += entry.filename.len();
                if entries.len() >= MAX_ENTRIES || total_names > 1024*1024 {
                    return Err(Error::Transfer("This folder exceeds the 2,048-entry browser limit. Enter a smaller subfolder directly.".into()));
                }
                entries.push(Entry { name:entry.filename,
                    directory:entry.attrs.permissions.is_some_and(|mode| mode & 0o170000 == 0o040000),
                    regular:entry.attrs.permissions.is_some_and(|mode| mode & 0o170000 == 0o100000),
                    size:entry.attrs.size });
            }
        }
        entries.sort_by(|left,right| right.directory.cmp(&left.directory).then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase())));
        scope.event("sftp.directory.listed",Level::Info,json!({"file.path":path,"file.directory.entries":entries.len()}))?;
        emit(events,Event::Directory { path,entries })
    }.await;
    let closed = sftp.close(handle).await.map(|_| ()).map_err(Error::from);
    result.and(closed)
}

fn regular_size(attributes: &FileAttributes) -> Result<u64> {
    if attributes
        .permissions
        .is_none_or(|mode| mode & 0o170000 != 0o100000)
    {
        return Err(Error::Transfer("Only regular files can be transferred; folders, links, and special files are not followed".into()));
    }
    attributes
        .size
        .filter(|&size| size <= MAX_FILE)
        .ok_or_else(|| {
            Error::Transfer(
                "The file has no reported size or exceeds the 1 TiB safety limit".into(),
            )
        })
}

async fn missing(sftp: &RawSftpSession, path: &str) -> Result<()> {
    match sftp.lstat(path).await {
        Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(()),
        Ok(_) => Err(Error::Transfer(
            "The destination already exists. Choose a new name; MacCanvas never overwrites files."
                .into(),
        )),
        Err(error) => Err(error.into()),
    }
}

async fn read_chunk(sftp: &RawSftpSession, handle: &str, offset: u64) -> Result<Option<Vec<u8>>> {
    match sftp.read(handle, offset, CHUNK as u32).await {
        Ok(data) if !data.data.is_empty() && data.data.len() <= CHUNK => Ok(Some(data.data)),
        Ok(_) => Err(Error::Transfer(
            "The server returned an invalid file-data chunk".into(),
        )),
        Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => Ok(None),
        Err(error) => Err(error.into()),
    }
}

async fn download(
    sftp: &RawSftpSession,
    remote: &str,
    local: &Path,
    control: &Control,
    scope: &Scope,
) -> Result<()> {
    valid_path(remote)?;
    control.check()?;
    if local.exists() {
        return Err(Error::Transfer(
            "The Windows destination already exists. Choose a new file name.".into(),
        ));
    }
    let before = sftp.lstat(remote).await?.attrs;
    let size = regular_size(&before)?;
    scope.critical("file.transfer.requested",Level::Info,json!({"file.transfer.direction":"download","file.source.path":remote,"file.destination.path":local,"file.size":size,"file.source.mtime_unix":before.mtime,"network.encrypted":true}))?;
    let directory = local
        .parent()
        .ok_or_else(|| Error::Transfer("Choose a local destination folder".into()))?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".maccanvas-download-")
        .suffix(".part")
        .tempfile_in(directory)?;
    scope.critical(
        "file.temporary.created",
        Level::Info,
        json!({"file.path":temporary.path(),"file.transfer.direction":"download"}),
    )?;
    let handle = sftp
        .open(remote, OpenFlags::READ, FileAttributes::empty())
        .await?
        .handle;
    let result = async {
        let actual = sftp.fstat(&handle).await?.attrs;
        if regular_size(&actual)? != size || actual.mtime != before.mtime {
            return Err(Error::Transfer("The Mac file changed before download began".into()));
        }
        let started = Instant::now();
        let mut offset = 0_u64;
        let mut digest = Sha256::new();
        control.progress("Receiving",0,size,started)?;
        while let Some(bytes) = read_chunk(sftp,&handle,offset).await? {
            control.check()?;
            if offset.saturating_add(bytes.len() as u64) > size { return Err(Error::Transfer("The Mac file grew during download".into())); }
            temporary.write_all(&bytes)?;
            digest.update(&bytes);
            offset += bytes.len() as u64;
            control.progress("Receiving",offset,size,started)?;
        }
        let after = sftp.fstat(&handle).await?.attrs;
        if offset != size || after.size != before.size || after.mtime != before.mtime {
            return Err(Error::Transfer("The Mac file changed or was truncated during download".into()));
        }
        control.check()?;
        temporary.as_file().sync_all()?;
        let hash = format!("{:x}",digest.finalize());
        scope.critical("file.transfer.commit_requested",Level::Info,json!({"file.destination.path":local,"file.size":offset,"file.hash.sha256":hash,"file.verification":"ssh_stream_size_and_sha256"}))?;
        Ok((offset,hash))
    }.await;
    let closed = sftp.close(handle).await.map(|_| ()).map_err(Error::from);
    let (size, hash) = match after_close(result, closed, scope) {
        Ok(value) => value,
        Err(error) => {
            cleanup_local(temporary, scope)?;
            return Err(error);
        }
    };
    if let Err(error) = control.check() {
        cleanup_local(temporary, scope)?;
        return Err(error);
    }
    if let Err(error) = temporary.persist_noclobber(local) {
        let message = format!(
            "Cannot save the new file without overwriting an existing file: {}",
            error.error
        );
        cleanup_local(error.file, scope)?;
        return Err(Error::Transfer(message));
    }
    scope.critical("file.transfer.completed",Level::Info,json!({"file.transfer.direction":"download","file.source.path":remote,"file.destination.path":local,"file.size":size,"file.hash.sha256":hash,"event.outcome":"success"}))
        .map_err(|error| Error::Transfer(format!("The file was saved, but its final audit receipt failed: {error}. Check the destination before retrying.")))
}

async fn upload(
    sftp: &RawSftpSession,
    fsync: bool,
    local: &Path,
    directory: &str,
    name: &str,
    control: &Control,
    scope: &Scope,
) -> Result<()> {
    control.check()?;
    let directory = canonical(sftp, directory).await?;
    let destination = join(&directory, name)?;
    missing(sftp, &destination).await?;
    if std::fs::symlink_metadata(local)?.file_type().is_symlink() {
        return Err(Error::Transfer(
            "Choose a regular Windows file, not a symbolic link".into(),
        ));
    }
    let mut file = File::open(local)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() > MAX_FILE {
        return Err(Error::Transfer(
            "Choose a regular file no larger than 1 TiB".into(),
        ));
    }
    let size = before.len();
    scope.critical("file.transfer.requested",Level::Info,json!({"file.transfer.direction":"upload","file.source.path":local,"file.destination.path":destination,"file.size":size,"network.encrypted":true}))?;
    let temporary = join(
        &directory,
        &format!(".maccanvas-{}.part", Uuid::new_v4().simple()),
    )?;
    let handle = sftp
        .open(
            &temporary,
            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
            FileAttributes {
                permissions: Some(0o600),
                ..FileAttributes::empty()
            },
        )
        .await?
        .handle;
    let result = async {
        scope.critical(
            "file.temporary.created",
            Level::Info,
            json!({"file.path":temporary,"file.transfer.direction":"upload"}),
        )?;
        let started = Instant::now();
        let mut offset = 0_u64;
        let mut digest = Sha256::new();
        let mut bytes = vec![0_u8; CHUNK];
        control.progress("Sending", 0, size, started)?;
        loop {
            control.check()?;
            let length = file.read(&mut bytes)?;
            if length == 0 {
                break;
            }
            if offset.saturating_add(length as u64) > size {
                return Err(Error::Transfer(
                    "The Windows file grew during upload".into(),
                ));
            }
            sftp.write(&handle, offset, bytes[..length].to_vec())
                .await?;
            digest.update(&bytes[..length]);
            offset += length as u64;
            control.progress("Sending", offset, size, started)?;
        }
        let after = file.metadata()?;
        if offset != size || after.len() != size || after.modified()? != before.modified()? {
            return Err(Error::Transfer(
                "The Windows file changed during upload".into(),
            ));
        }
        if fsync {
            sftp.fsync(&handle).await?;
        }
        let hash = format!("{:x}", digest.finalize());
        Ok(hash)
    }
    .await;
    let closed = sftp.close(handle).await.map(|_| ()).map_err(Error::from);
    let result = async {
        let expected = after_close(result,closed,scope)?;
        let verified = remote_hash(sftp,&temporary,size,control,scope).await?;
        if verified != expected { return Err(Error::Transfer("Upload SHA-256 read-back verification failed; the final file was not created".into())); }
        control.check()?;
        missing(sftp,&destination).await?;
        scope.critical("file.transfer.commit_requested",Level::Info,json!({"file.destination.path":destination,"file.size":size,"file.hash.sha256":expected,"file.verification":"remote_readback_sha256","file.remote.fsync":fsync}))?;
        sftp.rename(&temporary,&destination).await?;
        scope.critical("file.transfer.completed",Level::Info,json!({"file.transfer.direction":"upload","file.source.path":local,"file.destination.path":destination,"file.size":size,"file.hash.sha256":expected,"event.outcome":"success"}))
            .map_err(|error| Error::Transfer(format!("The file was sent, but its final audit receipt failed: {error}. Check the Mac before retrying.")))
    }.await;
    if result.is_err() {
        match sftp.remove(&temporary).await {
            Ok(_) => scope.critical(
                "file.temporary.cleaned",
                Level::Info,
                json!({"file.path":temporary}),
            )?,
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => (),
            Err(error) => {
                if let Err(audit_error) = scope.critical(
                    "file.temporary.cleanup_failed",
                    Level::Error,
                    json!({"file.path":temporary,"error.type":"sftp"}),
                ) {
                    eprintln!("MacCanvas could not audit temporary-file cleanup: {audit_error}");
                }
                return Err(Error::Transfer(format!(
                    "Transfer failed and the temporary Mac file could not be removed: {temporary}. SFTP: {error}"
                )));
            }
        }
    }
    result
}

fn after_close<T>(result: Result<T>, closed: Result<()>, scope: &Scope) -> Result<T> {
    if let Err(error) = &closed {
        scope.event(
            "sftp.handle.close_failed",
            Level::Warn,
            audit::error_attributes(error),
        )?;
    }
    result.and_then(|value| closed.map(|_| value))
}

fn cleanup_local(temporary: tempfile::NamedTempFile, scope: &Scope) -> Result<()> {
    let path = temporary.path().to_path_buf();
    match temporary.close() {
        Ok(()) => scope.critical(
            "file.temporary.cleaned",
            Level::Info,
            json!({"file.path":path}),
        ),
        Err(error) => {
            if let Err(audit_error) = scope.critical(
                "file.temporary.cleanup_failed",
                Level::Error,
                json!({"file.path":path,"error.code":error.raw_os_error()}),
            ) {
                eprintln!("MacCanvas could not persist local cleanup evidence: {audit_error}");
            }
            Err(Error::Transfer(format!(
                "A partial download could not be removed: {}. {error}",
                path.display()
            )))
        }
    }
}

async fn remote_hash(
    sftp: &RawSftpSession,
    path: &str,
    size: u64,
    control: &Control,
    scope: &Scope,
) -> Result<String> {
    let handle = sftp
        .open(path, OpenFlags::READ, FileAttributes::empty())
        .await?
        .handle;
    let result = async {
        let started = Instant::now();
        let mut offset = 0_u64;
        let mut digest = Sha256::new();
        control.progress("Verifying", 0, size, started)?;
        while let Some(bytes) = read_chunk(sftp, &handle, offset).await? {
            control.check()?;
            offset += bytes.len() as u64;
            if offset > size {
                return Err(Error::Transfer(
                    "Uploaded verification data exceeds its expected size".into(),
                ));
            }
            digest.update(&bytes);
            control.progress("Verifying", offset, size, started)?;
        }
        if offset != size {
            return Err(Error::Transfer(
                "Uploaded verification data is truncated".into(),
            ));
        }
        Ok(format!("{:x}", digest.finalize()))
    }
    .await;
    let closed = sftp.close(handle).await.map(|_| ()).map_err(Error::from);
    after_close(result, closed, scope)
}

async fn mac_clipboard(
    ssh: &client::Handle<HostVerifier>,
    input: Option<Zeroizing<String>>,
    control: &Control,
    scope: &Scope,
) -> Result<Zeroizing<String>> {
    control.check()?;
    let sending = input.is_some();
    if let Some(text) = &input {
        clipboard::validate(text, true)?;
    }
    scope.critical("ssh.clipboard.requested",Level::Info,json!({"clipboard.direction":if sending { "windows_to_mac" } else { "mac_to_windows" },"clipboard.encoding":"utf8","network.encrypted":true}))?;
    let mut channel = ssh.channel_open_session().await?;
    let result = tokio::time::timeout(Duration::from_secs(20),async {
        channel.exec(true,if sending { WRITE_CLIPBOARD } else { READ_CLIPBOARD }).await?;
        if let Some(text) = &input { channel.data(text.as_bytes()).await?; }
        channel.eof().await?;
        let mut output = Zeroizing::new(Vec::new());
        let mut status = None;
        let mut stderr = Zeroizing::new(Vec::new());
        while let Some(message) = channel.wait().await {
            control.check()?;
            match message {
                ChannelMsg::Data { data } => {
                    if output.len().saturating_add(data.len()) > clipboard::MAX_TEXT { return Err(Error::Transfer("Mac clipboard output exceeds 1 MiB".into())); }
                    output.extend_from_slice(&data);
                }
                ChannelMsg::ExtendedData { data,.. } => {
                    if stderr.len().saturating_add(data.len()) > 4096 { return Err(Error::Transfer("The Mac clipboard helper returned excessive diagnostic output".into())); }
                    stderr.extend_from_slice(&data);
                }
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::ExitSignal { .. } | ChannelMsg::Failure => return Err(Error::Transfer("The Mac denied or stopped its clipboard helper".into())),
                ChannelMsg::Close => break,
                _ => (),
            }
        }
        if status != Some(0) {
            let diagnostic = ClipboardDiagnostic::parse(&stderr);
            scope.critical("ssh.clipboard.failed", Level::Error, json!({
                "clipboard.direction":if sending { "windows_to_mac" } else { "mac_to_windows" },
                "clipboard.helper.exit_code":status,
                "clipboard.helper.diagnostic_bytes":stderr.len(),
                "clipboard.helper.stage":diagnostic.as_ref().map(|value|value.stage),
                "clipboard.helper.reason":diagnostic.as_ref().map(|value|value.reason),
                "clipboard.helper.macos_error":diagnostic.as_ref().and_then(|value|value.error_code),
                "clipboard.helper.desktop_session":diagnostic.as_ref().and_then(|value|value.desktop_session),
                "clipboard.helper.same_desktop_account":diagnostic.as_ref().and_then(|value|value.same_desktop_account),
                "event.outcome":"failure",
            }))?;
            let detail = diagnostic.map_or_else(
                || "No structured diagnostic was returned. Raw output is withheld to protect clipboard contents; this does not establish a macOS permission restriction.".to_owned(),
                |value|value.message(),
            );
            let exit = status.map_or_else(|| "not reported".into(), |value| value.to_string());
            return Err(Error::Clipboard(format!("Mac clipboard helper failed (exit {exit}). {detail}")));
        }
        let output = Zeroizing::new(std::str::from_utf8(&output).map_err(|_| Error::Transfer("The Mac clipboard did not return valid UTF-8".into()))?.to_owned());
        clipboard::validate(&output,true)?;
        scope.critical("ssh.clipboard.completed",Level::Info,json!({
            "clipboard.direction":if sending { "windows_to_mac" } else { "mac_to_windows" },
            "clipboard.text_bytes":input.as_ref().map_or(output.len(),|text| text.len()),"clipboard.encoding":"utf8",
            "clipboard.helper.diagnostic_bytes":stderr.len(),"event.outcome":"success",
        }))?;
        Ok(output)
    }).await.map_err(|_| Error::Transfer("The Mac clipboard helper timed out".into()))?;
    let closed = channel.close().await.map_err(Error::from);
    match result {
        Ok(output) => {
            closed?;
            Ok(output)
        }
        Err(error) => {
            if let Err(close_error) = closed {
                eprintln!("MacCanvas SSH clipboard channel cleanup: {close_error}");
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_names_cannot_escape_the_selected_folder() {
        for name in ["", "..", ".", "../secret", "a/b", "a\\b", "nul\0name", "\n"] {
            assert!(valid_name(name).is_err());
        }
        assert_eq!(
            join("/Users/test", "photo \u{1f34e}.png").unwrap(),
            "/Users/test/photo \u{1f34e}.png"
        );
        assert_eq!(join("/", "hello.txt").unwrap(), "/hello.txt");
        assert_eq!(parent("/Users/test/"), "/Users");
        assert_eq!(parent("/"), "/");
    }

    #[test]
    fn host_keys_are_bound_to_the_endpoint_and_have_real_sha256_shape() {
        let mut key = SshHostKey {
            host: "mac.local".into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            sha256: format!("SHA256:{}", "A".repeat(43)),
        };
        key.validate().unwrap();
        assert!(key.endpoint("MAC.local", 22));
        assert!(!key.endpoint("mac.local", 2222));
        key.sha256 = "anything".into();
        assert!(key.validate().is_err());
    }

    #[test]
    fn clipboard_commands_are_fixed_and_use_only_plain_text_pasteboard_apis() {
        assert!(READ_CLIPBOARD.contains("NSPasteboardTypeString"));
        assert!(WRITE_CLIPBOARD.contains("setStringForType"));
        assert!(!WRITE_CLIPBOARD.contains("eval("));
        assert!(!READ_CLIPBOARD.contains("pbpaste"));
    }

    #[test]
    fn clipboard_diagnostics_distinguish_empty_text_from_helper_errors() {
        let no_text = ClipboardDiagnostic::parse(br#"MACCANVAS_CLIPBOARD_DIAGNOSTIC={"version":1,"stage":"check_plain_text","reason":"no_plain_text","error_code":null,"desktop_session":true,"same_desktop_account":true}"#).unwrap();
        assert!(no_text.message().contains("has no plain text"));
        assert!(no_text.message().contains("SSH matches"));
        let type_error = ClipboardDiagnostic::parse(br#"MACCANVAS_CLIPBOARD_DIAGNOSTIC={"version":1,"stage":"check_plain_text","reason":"type_error","error_code":-2700,"desktop_session":true,"same_desktop_account":false}"#).unwrap();
        assert!(type_error.message().contains("JavaScript type error"));
        assert!(type_error.message().contains("CheckPlainText"));
        assert!(type_error.message().contains("-2700"));
        assert!(type_error.message().contains("SSH does not match"));
    }

    #[test]
    fn clipboard_diagnostics_reject_arbitrary_remote_text() {
        for stderr in [
            &b"PRIVATE_CLIPBOARD_TEXT"[..],
            &br#"MACCANVAS_CLIPBOARD_DIAGNOSTIC={"version":1,"stage":"PRIVATE_CLIPBOARD_TEXT","reason":"type_error"}"#[..],
            &br#"MACCANVAS_CLIPBOARD_DIAGNOSTIC={"version":1,"stage":"check_plain_text","reason":"PRIVATE_CLIPBOARD_TEXT"}"#[..],
            &br#"MACCANVAS_CLIPBOARD_DIAGNOSTIC={"version":1,"stage":"check_plain_text","reason":"type_error","message":"PRIVATE_CLIPBOARD_TEXT"}"#[..],
            &br#"MACCANVAS_CLIPBOARD_DIAGNOSTIC={"version":2,"stage":"check_plain_text","reason":"type_error"}"#[..],
        ] {
            assert!(ClipboardDiagnostic::parse(stderr).is_none());
        }
    }
}
