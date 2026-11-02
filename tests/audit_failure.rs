use std::{
    net::TcpListener,
    sync::{Arc, Mutex, mpsc},
};

use maccanvas::{
    audit::{Audit, Level},
    credentials::CredentialSource,
    error::Error,
    files,
    profiles::{AuthMode, Profile},
    session::{Request, Session},
    streaming::StreamSettings,
};
use serde_json::json;
use uuid::Uuid;

#[test]
fn runtime_log_failure_is_sticky_and_blocks_vnc_and_ssh_before_credentials_or_network() {
    let directory = tempfile::tempdir().unwrap();
    let mut guard = Audit::start(directory.path().join("audit")).unwrap();
    let scope = guard.audit().scope(json!({}));
    scope
        .critical("test.started", Level::Info, json!({}))
        .unwrap();
    let path = guard.audit().path();
    std::fs::rename(&path, path.with_file_name("preserved.jsonl")).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(
        scope
            .critical("test.disk_failure", Level::Info, json!({}))
            .is_err()
    );
    assert!(guard.audit().failure().is_some());
    assert!(
        scope
            .critical("test.must_not_resume", Level::Info, json!({}))
            .is_err()
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "Audit fixture".into(),
        host: "127.0.0.1".into(),
        port,
        authentication: AuthMode::MacLogin,
        has_saved_credentials: true,
    };
    let ssh = files::Client::start(files::Request {
        profile: profile.clone(),
        port,
        credentials: CredentialSource::Saved,
        pinned: None,
        window: 0,
        audit: scope.clone(),
    });
    assert!(matches!(ssh, Err(Error::Audit(_))));
    let (events, _receiver) = mpsc::channel();
    let vnc = Session::start(
        Request {
            profile,
            credentials: CredentialSource::Saved,
            streaming: StreamSettings::default(),
            remember: false,
            window: 0,
            generation: 1,
            audit: scope,
            clipboard: false,
        },
        events,
        Arc::new(Mutex::new(None)),
    );
    assert!(matches!(vnc, Err(Error::Audit(_))));
    assert!(
        matches!(listener.accept(),Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(guard.shutdown().is_err());
}
