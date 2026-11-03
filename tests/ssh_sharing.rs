use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::{SocketAddr, TcpListener},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use maccanvas::{
    audit::{Audit, Guard},
    auth::Credentials,
    credentials::CredentialSource,
    files::{Client, Command, Event, Request},
    profiles::{AuthMode, Profile, SshHostKey},
};
use russh::{
    Channel, ChannelId, ChannelMsg,
    keys::{HashAlg, PrivateKey, ssh_key::private::Ed25519Keypair},
    server::{self, Auth, Msg, Server as _},
};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
};
use serde_json::json;
use uuid::Uuid;
use zeroize::Zeroizing;

const PASSWORD: &str = "FixturePassword-DoNotPutInLogs";
const CLIPBOARD: &str = "PRIVATE_CLIPBOARD_MARKER \u{1f34e} caf\u{e9}\ntext";
const HOME: &str = "/Users/fixture";

#[test]
#[ignore = "Bounded loopback server helper for the owned native GUI smoke test"]
fn serve_owned_gui_fixture() {
    let path =
        std::env::var_os("MACCANVAS_GUI_SSH_FIXTURE").expect("Explicit fixture directory required");
    let path = std::path::PathBuf::from(path);
    let fixture = Fixture::new();
    std::fs::write(
        path.join("ready.json"),
        serde_json::to_vec(&json!({"port":fixture.port,"fingerprint":fixture.key.sha256})).unwrap(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !path.join("stop").exists() {
        assert!(
            Instant::now() < deadline,
            "Bounded GUI fixture did not receive shutdown"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[derive(Default)]
struct Evidence {
    files: Mutex<BTreeMap<String, Vec<u8>>>,
    clipboard: Mutex<Vec<u8>>,
    clipboard_failure: AtomicUsize,
    commands: Mutex<Vec<String>>,
    authentications: AtomicUsize,
    delay_ms: AtomicUsize,
    corrupt: AtomicUsize,
    denied: Mutex<HashSet<String>>,
}

struct Fixture {
    port: u16,
    key: SshHostKey,
    evidence: Arc<Evidence>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let mut seed = Zeroizing::new([0_u8; 32]);
        getrandom::fill(seed.as_mut()).unwrap();
        let private = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
        let key = SshHostKey {
            host: "127.0.0.1".into(),
            port,
            algorithm: "ssh-ed25519".into(),
            sha256: private
                .public_key()
                .fingerprint(HashAlg::Sha256)
                .to_string(),
        };
        let evidence = Arc::new(Evidence::default());
        evidence
            .denied
            .lock()
            .unwrap()
            .insert(format!("{HOME}/Desktop"));
        evidence.files.lock().unwrap().insert(
            format!("{HOME}/source.txt"),
            b"FILE_CONTENTS_MUST_NOT_BE_LOGGED\n".repeat(3000),
        );
        *evidence.clipboard.lock().unwrap() = CLIPBOARD.as_bytes().to_vec();
        let data = evidence.clone();
        let (stop, shutdown) = tokio::sync::oneshot::channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let config = server::Config { keys:vec![private],auth_rejection_time:Duration::ZERO,auth_rejection_time_initial:Some(Duration::ZERO),..Default::default() };
                let mut server = TestServer(data);
                let running = server.run_on_socket(Arc::new(config),&listener);
                let handle = running.handle();
                tokio::pin!(running);
                ready.send(()).unwrap();
                tokio::select! {
                    result = &mut running => result.unwrap(),
                    _ = shutdown => { handle.shutdown("Test complete".into()); running.await.unwrap(); }
                }
            });
        });
        initialized.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            port,
            key,
            evidence,
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    fn client(&self, guard: &Guard, pinned: Option<SshHostKey>, saved: bool) -> Client {
        Client::start(Request {
            profile: Profile {
                id: Uuid::new_v4(),
                name: "SSH fixture".into(),
                host: "127.0.0.1".into(),
                port: 5900,
                authentication: AuthMode::MacLogin,
                has_saved_credentials: saved,
            },
            port: self.port,
            credentials: if saved {
                CredentialSource::Saved
            } else {
                CredentialSource::Manual(Credentials::new("fixture".into(), PASSWORD.into()))
            },
            pinned,
            window: 0,
            audit: guard.audit().scope(json!({"test.fixture":"ssh"})),
        })
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

struct TestServer(Arc<Evidence>);
impl server::Server for TestServer {
    type Handler = Ssh;
    fn new_client(&mut self, _: Option<SocketAddr>) -> Ssh {
        Ssh {
            data: self.0.clone(),
            channels: HashMap::new(),
        }
    }
}

struct Ssh {
    data: Arc<Evidence>,
    channels: HashMap<ChannelId, Channel<Msg>>,
}
impl server::Handler for Ssh {
    type Error = russh::Error;
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        self.data.authentications.fetch_add(1, Ordering::Relaxed);
        if user == "fixture" && password == PASSWORD {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _: &mut server::Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }
    async fn subsystem_request(
        &mut self,
        id: ChannelId,
        name: &str,
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(id)?;
            return Ok(());
        }
        session.channel_success(id)?;
        let channel = self.channels.remove(&id).unwrap();
        let sftp = Sftp {
            data: self.data.clone(),
            handles: HashMap::new(),
            listed: HashSet::new(),
        };
        tokio::spawn(async move {
            russh_sftp::server::run(channel.into_stream(), sftp).await;
        });
        Ok(())
    }
    async fn exec_request(
        &mut self,
        id: ChannelId,
        command: &[u8],
        session: &mut server::Session,
    ) -> Result<(), Self::Error> {
        let command = std::str::from_utf8(command).unwrap().to_owned();
        assert!(command.starts_with("/usr/bin/osascript -l JavaScript -e '"));
        assert!(!command.contains(CLIPBOARD));
        self.data.commands.lock().unwrap().push(command.clone());
        session.channel_success(id)?;
        let mut channel = self.channels.remove(&id).unwrap();
        let data = self.data.clone();
        tokio::spawn(async move {
            let failure = data.clipboard_failure.load(Ordering::Relaxed);
            if failure != 0 {
                let diagnostic = if failure == 3 {
                    format!("Unstructured diagnostic: {CLIPBOARD}\n")
                } else {
                    format!(
                        "{CLIPBOARD}\nexecution error: Error: MACCANVAS_CLIPBOARD_DIAGNOSTIC={} (-2700)\n",
                        json!({
                            "version":1,
                            "stage":"check_plain_text",
                            "reason":if failure == 1 { "no_plain_text" } else { "type_error" },
                            "error_code":if failure == 1 { None } else { Some(-2700) },
                            "desktop_session":true,
                            "same_desktop_account":true,
                        })
                    )
                };
                channel.data(CLIPBOARD.as_bytes()).await.unwrap();
                let middle = diagnostic.len() / 2;
                channel
                    .extended_data(1, &diagnostic.as_bytes()[..middle])
                    .await
                    .unwrap();
                channel
                    .extended_data(1, &diagnostic.as_bytes()[middle..])
                    .await
                    .unwrap();
                channel.exit_status(1).await.unwrap();
                channel.eof().await.unwrap();
                channel.close().await.unwrap();
                return;
            }
            if command.contains("fileHandleWithStandardOutput") {
                let text = data.clipboard.lock().unwrap().clone();
                channel.data(text.as_slice()).await.unwrap();
            } else {
                assert!(command.contains("setStringForType"));
                let mut text = Vec::new();
                while let Some(message) = channel.wait().await {
                    match message {
                        ChannelMsg::Data { data } => text.extend_from_slice(&data),
                        ChannelMsg::Eof => break,
                        _ => (),
                    }
                }
                *data.clipboard.lock().unwrap() = text;
            }
            channel.exit_status(0).await.unwrap();
            channel.eof().await.unwrap();
            channel.close().await.unwrap();
        });
        Ok(())
    }
}

struct Sftp {
    data: Arc<Evidence>,
    handles: HashMap<String, String>,
    listed: HashSet<String>,
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: String::new(),
        language_tag: "en".into(),
    }
}
fn attributes(size: usize) -> FileAttributes {
    FileAttributes {
        size: Some(size as u64),
        permissions: Some(0o100600),
        mtime: Some(100),
        ..FileAttributes::empty()
    }
}

impl russh_sftp::server::Handler for Sftp {
    type Error = StatusCode;
    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }
    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusCode> {
        let path = if path == "." { HOME.into() } else { path };
        Ok(Name {
            id,
            files: vec![File::dummy(path)],
        })
    }
    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusCode> {
        if self.data.denied.lock().unwrap().contains(&path) {
            return Err(StatusCode::PermissionDenied);
        }
        if path != HOME && path != format!("{HOME}/Public") {
            return Err(StatusCode::NoSuchFile);
        }
        self.listed.remove(&path);
        Ok(Handle { id, handle: path })
    }
    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
        if !self.listed.insert(handle.clone()) || handle == format!("{HOME}/Public") {
            return Err(StatusCode::Eof);
        }
        let data = self.data.files.lock().unwrap();
        let mut files: Vec<_> = data
            .iter()
            .map(|(path, bytes)| {
                File::new(path.rsplit('/').next().unwrap(), attributes(bytes.len()))
            })
            .collect();
        for name in ["Desktop", "Public"] {
            files.push(File::new(
                name,
                FileAttributes {
                    permissions: Some(0o040700),
                    ..FileAttributes::empty()
                },
            ));
        }
        Ok(Name { id, files })
    }
    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        self.handles.remove(&handle);
        Ok(ok(id))
    }
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        self.data
            .files
            .lock()
            .unwrap()
            .get(&path)
            .map(|data| Attrs {
                id,
                attrs: attributes(data.len()),
            })
            .ok_or(StatusCode::NoSuchFile)
    }
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        let path = self
            .handles
            .get(&handle)
            .ok_or(StatusCode::NoSuchFile)?
            .clone();
        self.lstat(id, path).await
    }
    async fn open(
        &mut self,
        id: u32,
        path: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        let mut data = self.data.files.lock().unwrap();
        if flags.contains(OpenFlags::CREATE) {
            if flags.contains(OpenFlags::EXCLUDE) && data.contains_key(&path) {
                return Err(StatusCode::Failure);
            }
            data.entry(path.clone()).or_default();
        } else if !data.contains_key(&path) {
            return Err(StatusCode::NoSuchFile);
        }
        let handle = format!("handle-{id}");
        self.handles.insert(handle.clone(), path);
        Ok(Handle { id, handle })
    }
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        length: u32,
    ) -> Result<Data, StatusCode> {
        let delay = self.data.delay_ms.load(Ordering::Relaxed);
        if delay != 0 {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        }
        let path = self.handles.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let files = self.data.files.lock().unwrap();
        let data = files.get(path).ok_or(StatusCode::NoSuchFile)?;
        let offset = usize::try_from(offset).map_err(|_| StatusCode::BadMessage)?;
        if offset >= data.len() {
            return Err(StatusCode::Eof);
        }
        let mut data = data[offset..data.len().min(offset + length as usize)].to_vec();
        if self.data.corrupt.load(Ordering::Relaxed) != 0 && path.contains(".maccanvas-") {
            data[0] ^= 0xff;
        }
        Ok(Data { id, data })
    }
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        let delay = self.data.delay_ms.load(Ordering::Relaxed);
        if delay != 0 {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        }
        let path = self.handles.get(&handle).ok_or(StatusCode::NoSuchFile)?;
        let mut files = self.data.files.lock().unwrap();
        let file = files.get_mut(path).ok_or(StatusCode::NoSuchFile)?;
        let offset = usize::try_from(offset).map_err(|_| StatusCode::BadMessage)?;
        if offset != file.len() {
            return Err(StatusCode::BadMessage);
        }
        file.extend(data);
        Ok(ok(id))
    }
    async fn rename(&mut self, id: u32, old: String, new: String) -> Result<Status, StatusCode> {
        let mut files = self.data.files.lock().unwrap();
        if files.contains_key(&new) {
            return Err(StatusCode::Failure);
        }
        let data = files.remove(&old).ok_or(StatusCode::NoSuchFile)?;
        files.insert(new, data);
        Ok(ok(id))
    }
    async fn remove(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
        if self.data.files.lock().unwrap().remove(&path).is_none() {
            return Err(StatusCode::NoSuchFile);
        }
        Ok(ok(id))
    }
}

fn wait(client: &Client, mut wanted: impl FnMut(Event) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(event) = client.event().unwrap()
            && wanted(event)
        {
            return;
        }
        assert!(Instant::now() < deadline, "SSH fixture operation timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

fn connected(client: &Client, fixture: &Fixture) {
    wait(client, |event| match event {
        Event::HostKey { key, reply } => {
            assert_eq!(
                fixture.evidence.authentications.load(Ordering::Relaxed),
                0,
                "credentials were sent before host-key approval"
            );
            assert_eq!(key, fixture.key);
            reply.send(true).unwrap();
            false
        }
        Event::Directory { .. } => true,
        Event::Failed(error) => panic!("Fixture connect failed: {error}"),
        Event::DirectoryFailed { error, .. } => panic!("Fixture folder failed: {error}"),
        _ => false,
    });
}

#[test]
fn denied_directories_keep_ssh_connected_and_allow_recovery() {
    for deny_home in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut audit = Audit::start(directory.path().join("audit")).unwrap();
        let fixture = Fixture::new();
        if deny_home {
            fixture.evidence.denied.lock().unwrap().insert(HOME.into());
        }
        let client = fixture.client(&audit, Some(fixture.key.clone()), false);
        if !deny_home {
            connected(&client, &fixture);
            client
                .command(Command::List(format!("{HOME}/Desktop")))
                .unwrap();
        }
        wait(&client, |event| match event {
            Event::DirectoryFailed { path, error } => {
                assert_eq!(
                    path,
                    if deny_home {
                        ".".into()
                    } else {
                        format!("{HOME}/Desktop")
                    }
                );
                assert!(error.contains("full disk access for remote users"));
                assert!(error.contains("account"));
                true
            }
            Event::Failed(error) => panic!("A directory denial became a fatal SSH error: {error}"),
            Event::Closed => panic!("SSH closed after a recoverable directory denial"),
            _ => false,
        });
        assert!(!client.is_finished());
        client
            .command(Command::List(format!("{HOME}/Public")))
            .unwrap();
        wait(&client, |event| match event {
            Event::Directory { path, entries } => {
                assert_eq!(path, format!("{HOME}/Public"));
                assert!(entries.is_empty());
                true
            }
            Event::Failed(error) | Event::DirectoryFailed { error, .. } => {
                panic!("An allowed directory could not be opened: {error}")
            }
            _ => false,
        });
        client
            .command(Command::Download {
                remote: format!("{HOME}/source.txt"),
                local: directory.path().join("received.txt"),
            })
            .unwrap();
        wait(&client, |event| match event {
            Event::Completed { .. } => true,
            Event::Failed(error) => panic!("The surviving SSH session could not transfer: {error}"),
            _ => false,
        });
        assert_eq!(fixture.evidence.authentications.load(Ordering::Relaxed), 1);
        client.finish().unwrap();
        audit.shutdown().unwrap();
        let records = std::fs::read_to_string(audit.audit().path()).unwrap();
        assert!(
            records
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .any(|record| {
                    record["event_name"] == "ssh.operation.failed"
                        && record["attributes"]["sftp.status_code"] == 3
                        && record["attributes"]["error.type"] == "file_transfer"
                        && record["attributes"]["operation.name"] == "directory_list"
                })
        );
        maccanvas::audit::verify(&[audit.audit().path()]).unwrap();
    }
}

#[test]
fn verified_ssh_transfers_files_and_unicode_without_logging_contents_or_passwords() {
    let directory = tempfile::tempdir().unwrap();
    let mut audit = Audit::start(directory.path().join("audit")).unwrap();
    let fixture = Fixture::new();
    let client = fixture.client(&audit, None, false);
    connected(&client, &fixture);
    let received = directory.path().join("received.txt");
    client
        .command(Command::Download {
            remote: format!("{HOME}/source.txt"),
            local: received.clone(),
        })
        .unwrap();
    wait(&client, |event| match event {
        Event::Completed { .. } => true,
        Event::Failed(error) => panic!("Download failed: {error}"),
        _ => false,
    });
    assert_eq!(
        std::fs::read(&received).unwrap(),
        fixture.evidence.files.lock().unwrap()[&format!("{HOME}/source.txt")]
    );
    client
        .command(Command::Upload {
            local: received.clone(),
            directory: HOME.into(),
            name: "sent.txt".into(),
        })
        .unwrap();
    wait(&client, |event| match event {
        Event::Completed { .. } => true,
        Event::Failed(error) => panic!("Upload failed: {error}"),
        _ => false,
    });
    assert_eq!(
        fixture.evidence.files.lock().unwrap()[&format!("{HOME}/sent.txt")],
        std::fs::read(&received).unwrap()
    );
    assert!(
        client.counters.received.load(Ordering::Relaxed)
            > std::fs::metadata(&received).unwrap().len()
    );
    assert!(
        client.counters.sent.load(Ordering::Relaxed) > std::fs::metadata(&received).unwrap().len()
    );
    client.command(Command::CopyClipboard).unwrap();
    wait(&client, |event| match event {
        Event::Clipboard(text) => {
            assert_eq!(&*text, CLIPBOARD);
            true
        }
        Event::Failed(error) => panic!("Clipboard failed: {error}"),
        _ => false,
    });
    client
        .command(Command::SendClipboard {
            text: Zeroizing::new("OTHER_PRIVATE_TEXT \u{1f34e}".into()),
            sequence: 42,
        })
        .unwrap();
    wait(&client, |event| match event {
        Event::ClipboardSent(sequence) => {
            assert_eq!(sequence, 42);
            true
        }
        Event::Failed(error) => panic!("Clipboard failed: {error}"),
        _ => false,
    });
    assert_eq!(
        &*fixture.evidence.clipboard.lock().unwrap(),
        "OTHER_PRIVATE_TEXT \u{1f34e}".as_bytes()
    );
    client
        .command(Command::Download {
            remote: format!("{HOME}/source.txt"),
            local: received.clone(),
        })
        .unwrap();
    wait(&client, |event| match event {
        Event::Failed(error) => {
            assert!(error.contains("exists"));
            true
        }
        _ => false,
    });
    client.finish().unwrap();
    audit.shutdown().unwrap();
    let log = std::fs::read_to_string(audit.audit().path()).unwrap();
    for private in [
        PASSWORD,
        CLIPBOARD,
        "OTHER_PRIVATE_TEXT",
        "FILE_CONTENTS_MUST_NOT_BE_LOGGED",
    ] {
        assert!(
            !log.contains(private),
            "sensitive content reached the audit log"
        );
    }
    assert!(log.contains("file.transfer.completed") && log.contains("file.hash.sha256"));
    assert!(maccanvas::audit::verify(&[audit.audit().path()]).unwrap() > 15);
}

#[test]
fn clipboard_failures_report_safe_diagnostics_and_keep_ssh_connected() {
    let directory = tempfile::tempdir().unwrap();
    let mut audit = Audit::start(directory.path().join("audit")).unwrap();
    let fixture = Fixture::new();
    let client = fixture.client(&audit, None, false);
    connected(&client, &fixture);
    for (failure, expected) in [
        (1, "has no plain text"),
        (2, "JavaScript type error"),
        (3, "No structured diagnostic"),
    ] {
        fixture
            .evidence
            .clipboard_failure
            .store(failure, Ordering::Relaxed);
        client.command(Command::CopyClipboard).unwrap();
        wait(&client, |event| match event {
            Event::Failed(error) => {
                assert!(error.contains(expected), "{error}");
                assert!(!error.contains(CLIPBOARD));
                assert!(!error.contains(PASSWORD));
                true
            }
            Event::Clipboard(_) => panic!("A failed helper must not replace the Windows clipboard"),
            Event::Closed => panic!("Clipboard failure must not disconnect SSH"),
            _ => false,
        });
    }
    fixture
        .evidence
        .clipboard_failure
        .store(0, Ordering::Relaxed);
    client.command(Command::CopyClipboard).unwrap();
    wait(&client, |event| match event {
        Event::Clipboard(text) => {
            assert_eq!(&*text, CLIPBOARD);
            true
        }
        Event::Failed(error) => panic!("Clipboard did not recover: {error}"),
        _ => false,
    });
    client.finish().unwrap();
    audit.shutdown().unwrap();
    let log = std::fs::read_to_string(audit.audit().path()).unwrap();
    assert!(!log.contains(CLIPBOARD));
    assert!(!log.contains(PASSWORD));
    let failures = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|record| record["event_name"] == "ssh.clipboard.failed")
        .collect::<Vec<_>>();
    assert_eq!(failures.len(), 3);
    assert_eq!(
        failures[0]["attributes"]["clipboard.helper.reason"],
        "no_plain_text"
    );
    assert_eq!(
        failures[1]["attributes"]["clipboard.helper.reason"],
        "type_error"
    );
    assert_eq!(
        failures[1]["attributes"]["clipboard.helper.macos_error"],
        -2700
    );
    assert_eq!(
        failures[1]["attributes"]["clipboard.helper.same_desktop_account"],
        true
    );
    assert!(failures[2]["attributes"]["clipboard.helper.reason"].is_null());
    maccanvas::audit::verify(&[audit.audit().path()]).unwrap();
}

#[test]
fn a_changed_host_key_is_blocked_before_authentication_and_saved_logins_require_hello() {
    let directory = tempfile::tempdir().unwrap();
    let mut audit = Audit::start(directory.path().join("audit")).unwrap();
    let fixture = Fixture::new();
    let mut wrong = fixture.key.clone();
    wrong.sha256 = format!("SHA256:{}", "A".repeat(43));
    let client = fixture.client(&audit, Some(wrong), true);
    wait(&client, |event| match event {
        Event::Failed(error) => {
            assert!(error.contains("host key has changed"));
            true
        }
        Event::HostKey { .. } => panic!("changed keys must not offer a bypass"),
        _ => false,
    });
    client.finish().unwrap();
    assert_eq!(fixture.evidence.authentications.load(Ordering::Relaxed), 0);
    let client = fixture.client(&audit, Some(fixture.key.clone()), true);
    wait(&client, |event| match event {
        Event::Failed(error) => {
            assert!(error.contains("Windows Hello"));
            true
        }
        _ => false,
    });
    client.finish().unwrap();
    assert_eq!(fixture.evidence.authentications.load(Ordering::Relaxed), 0);
    audit.shutdown().unwrap();
    let log = std::fs::read_to_string(audit.audit().path()).unwrap();
    assert!(log.contains("security.consent.requested"));
    assert!(!log.contains("credential.read.requested"));
}

#[test]
fn cancelled_or_corrupted_uploads_never_create_the_final_file_and_remove_their_temporary_file() {
    let directory = tempfile::tempdir().unwrap();
    let mut audit = Audit::start(directory.path().join("audit")).unwrap();
    let fixture = Fixture::new();
    let client = fixture.client(&audit, None, false);
    connected(&client, &fixture);
    let source = directory.path().join("large.bin");
    std::fs::write(&source, vec![0x51; 2 * 1024 * 1024]).unwrap();
    fixture.evidence.delay_ms.store(5, Ordering::Relaxed);
    client
        .command(Command::Upload {
            local: source.clone(),
            directory: HOME.into(),
            name: "cancelled.bin".into(),
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.progress().unwrap().bytes < 32 * 1024 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    client.cancel();
    wait(&client, |event| matches!(event, Event::Failed(_)));
    assert!(
        !fixture
            .evidence
            .files
            .lock()
            .unwrap()
            .keys()
            .any(|name| name.contains(".maccanvas-") || name.ends_with("cancelled.bin"))
    );
    fixture.evidence.delay_ms.store(0, Ordering::Relaxed);
    fixture.evidence.corrupt.store(1, Ordering::Relaxed);
    client
        .command(Command::Upload {
            local: source,
            directory: HOME.into(),
            name: "corrupt.bin".into(),
        })
        .unwrap();
    wait(&client, |event| match event {
        Event::Failed(error) => {
            assert!(error.contains("SHA-256"));
            true
        }
        _ => false,
    });
    assert!(
        !fixture
            .evidence
            .files
            .lock()
            .unwrap()
            .keys()
            .any(|name| name.contains(".maccanvas-") || name.ends_with("corrupt.bin"))
    );
    client.finish().unwrap();
    audit.shutdown().unwrap();
}
