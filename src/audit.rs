mod windows;

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::windows::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{Error, Result};

const QUEUE_SIZE: usize = 1024;
const MAX_RECORD: usize = 256 * 1024;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const ARCHIVES: usize = 7;

#[derive(Clone, Copy)]
pub enum Level {
    Info = 9,
    Warn = 13,
    Error = 17,
    Fatal = 21,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }
}

#[derive(Clone)]
pub struct Audit(Arc<Inner>);

struct Inner {
    sender: SyncSender<Work>,
    directory: PathBuf,
    health: Arc<Mutex<Option<String>>>,
    lost: Arc<AtomicU64>,
    started: Instant,
}

#[derive(Clone)]
pub struct Scope {
    audit: Audit,
    trace_id: String,
    span_id: String,
    attributes: Value,
}

struct Record {
    scope: Scope,
    name: &'static str,
    level: Level,
    attributes: Value,
    timestamp: String,
    elapsed_ns: String,
    thread_id: u32,
    thread_name: Option<String>,
}

enum Work {
    Record(
        Box<Record>,
        Option<SyncSender<std::result::Result<(), String>>>,
    ),
    Stop(SyncSender<std::result::Result<(), String>>),
}

pub struct Guard {
    audit: Audit,
    worker: Option<JoinHandle<()>>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

pub fn default_directory() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|path| path.join(".maccanvas"))
        .ok_or_else(|| Error::Audit("Windows did not return a home directory".into()))
}

fn set_failure(health: &Mutex<Option<String>>, error: &str) {
    match health.lock() {
        Ok(mut value) if value.is_none() => *value = Some(error.into()),
        Ok(_) => (),
        Err(_) => eprintln!("MacCanvas audit health state is unavailable: {error}"),
    }
}

impl Audit {
    pub fn start(directory: PathBuf) -> Result<Guard> {
        let instance = Uuid::new_v4().simple().to_string();
        let resource = windows::resource(&instance);
        Self::start_with(directory, instance, resource, MAX_FILE)
    }

    fn start_with(
        directory: PathBuf,
        instance: String,
        resource: Value,
        limit: u64,
    ) -> Result<Guard> {
        if resource["service.instance.id"].as_str() != Some(instance.as_str()) {
            return Err(Error::Audit(
                "Audit resource is missing its process instance identifier".into(),
            ));
        }
        let (sender, receiver) = mpsc::sync_channel(QUEUE_SIZE);
        let health = Arc::new(Mutex::new(None));
        let lost = Arc::new(AtomicU64::new(0));
        let (ready, initialized) = mpsc::sync_channel(1);
        let worker_health = health.clone();
        let worker_lost = lost.clone();
        let worker_directory = directory.clone();
        let worker = thread::Builder::new()
            .name("maccanvas-audit".into())
            .spawn(move || {
                let mut store = match Store::new(worker_directory, resource, limit) {
                    Ok(store) => {
                        if ready.send(Ok(())).is_err() {
                            return;
                        }
                        store
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error.to_string()));
                        return;
                    }
                };
                for work in receiver {
                    match work {
                        Work::Record(record, completed) => {
                            let lost = worker_lost.swap(0, Ordering::AcqRel);
                            let result = store
                                .write(*record, lost)
                                .map_err(|error| error.to_string());
                            if let Err(error) = &result {
                                worker_lost.fetch_add(lost + 1, Ordering::Relaxed);
                                set_failure(&worker_health, error);
                                eprintln!("MacCanvas security audit failed: {error}");
                            }
                            if let Some(completed) = completed {
                                let _ = completed.send(result);
                            }
                        }
                        Work::Stop(completed) => {
                            let _ = completed.send(Ok(()));
                            break;
                        }
                    }
                }
            })?;
        initialized
            .recv_timeout(Duration::from_secs(15))
            .map_err(|error| {
                Error::Audit(format!("Audit initialization did not complete: {error}"))
            })?
            .map_err(Error::Audit)?;
        let audit = Self(Arc::new(Inner {
            sender,
            directory,
            health,
            lost,
            started: Instant::now(),
        }));
        Ok(Guard {
            audit,
            worker: Some(worker),
        })
    }

    pub fn scope(&self, attributes: Value) -> Scope {
        Scope {
            audit: self.clone(),
            trace_id: Uuid::new_v4().simple().to_string(),
            span_id: Uuid::new_v4().simple().to_string()[..16].into(),
            attributes,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.0.directory.join("log.json")
    }

    pub fn failure(&self) -> Option<String> {
        match self.0.health.lock() {
            Ok(value) => value.clone(),
            Err(_) => Some("Audit health state is unavailable".into()),
        }
    }
}

impl Guard {
    pub fn audit(&self) -> Audit {
        self.audit.clone()
    }

    pub fn shutdown(&mut self) -> Result<()> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        let (done, completion) = mpsc::sync_channel(1);
        let mut stop = Work::Stop(done);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.audit.0.sender.try_send(stop) {
                Ok(()) => break,
                Err(TrySendError::Full(work)) => {
                    if Instant::now() >= deadline {
                        return Err(Error::Audit("Audit shutdown queue did not drain".into()));
                    }
                    stop = work;
                    thread::sleep(Duration::from_millis(10));
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(Error::Audit(
                        "Audit writer is unavailable during shutdown".into(),
                    ));
                }
            }
        }
        completion
            .recv_timeout(Duration::from_secs(10))
            .map_err(|error| Error::Audit(format!("Audit shutdown did not complete: {error}")))?
            .map_err(Error::Audit)?;
        worker
            .join()
            .map_err(|_| Error::Audit("Audit writer stopped unexpectedly".into()))?;
        if let Some(error) = self.audit.failure() {
            return Err(Error::Audit(error));
        }
        Ok(())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("MacCanvas audit shutdown: {error}");
        }
    }
}

impl Scope {
    pub fn child(&self, attributes: Value) -> Self {
        let mut inherited = self.attributes.clone();
        if let (Some(inherited), Value::Object(attributes)) =
            (inherited.as_object_mut(), attributes)
        {
            inherited.extend(attributes);
        }
        Self {
            audit: self.audit.clone(),
            trace_id: self.trace_id.clone(),
            span_id: Uuid::new_v4().simple().to_string()[..16].into(),
            attributes: inherited,
        }
    }

    pub fn event(&self, name: &'static str, level: Level, attributes: Value) -> Result<()> {
        self.enqueue(name, level, attributes, false)
    }

    pub fn critical(&self, name: &'static str, level: Level, attributes: Value) -> Result<()> {
        self.enqueue(name, level, attributes, true)
    }

    pub fn audit(&self) -> &Audit {
        &self.audit
    }

    fn enqueue(
        &self,
        name: &'static str,
        level: Level,
        attributes: Value,
        durable: bool,
    ) -> Result<()> {
        if let Some(error) = self.audit.failure() {
            return Err(Error::Audit(error));
        }
        if !attributes.is_object() || !self.attributes.is_object() {
            return Err(Error::Audit("Audit attributes must be an object".into()));
        }
        let record = Box::new(Record {
            scope: self.clone(),
            name,
            level,
            attributes,
            timestamp: now(),
            elapsed_ns: self.audit.0.started.elapsed().as_nanos().to_string(),
            thread_id: unsafe { ::windows::Win32::System::Threading::GetCurrentThreadId() },
            thread_name: thread::current().name().map(str::to_owned),
        });
        let (done, completion) = mpsc::sync_channel(1);
        match self
            .audit
            .0
            .sender
            .try_send(Work::Record(record, durable.then_some(done)))
        {
            Ok(()) => (),
            Err(TrySendError::Full(_)) => {
                self.audit.0.lost.fetch_add(1, Ordering::Relaxed);
                let error =
                    "Audit queue exhausted; restart MacCanvas before making another connection";
                set_failure(&self.audit.0.health, error);
                return Err(Error::Audit(error.into()));
            }
            Err(TrySendError::Disconnected(_)) => {
                let error = "Audit writer is unavailable";
                set_failure(&self.audit.0.health, error);
                return Err(Error::Audit(error.into()));
            }
        }
        if durable {
            match completion.recv_timeout(Duration::from_secs(5)) {
                Ok(result) => result.map_err(Error::Audit)?,
                Err(error) => {
                    let error = format!("Audit persistence did not complete: {error}");
                    set_failure(&self.audit.0.health, &error);
                    return Err(Error::Audit(error));
                }
            }
        }
        Ok(())
    }
}

struct Store {
    directory: PathBuf,
    sid: String,
    lock: windows::LockFile,
    resource: Value,
    sequence: u64,
    previous: String,
    limit: u64,
}

impl Store {
    fn new(directory: PathBuf, resource: Value, limit: u64) -> Result<Self> {
        fs::create_dir_all(&directory)?;
        let sid = windows::current_sid()?;
        windows::protect(&directory, &sid, true)?;
        let lock = windows::LockFile::open(&directory, &sid)?;
        Ok(Self {
            directory,
            sid,
            lock,
            resource,
            sequence: 0,
            previous: String::new(),
            limit,
        })
    }

    fn write(&mut self, record: Record, lost: u64) -> Result<()> {
        let _lock = self.lock.acquire()?;
        let path = self.directory.join("log.json");
        let mut attributes = record.scope.attributes.clone();
        attributes
            .as_object_mut()
            .expect("checked attributes")
            .extend(
                record
                    .attributes
                    .as_object()
                    .expect("checked attributes")
                    .clone(),
            );
        attributes["thread.id"] = json!(record.thread_id);
        attributes["thread.name"] = json!(record.thread_name);
        redact(&mut attributes);
        let mut recovery = None;
        let size = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                windows::reject_reparse(&path)?;
                metadata.len()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        if size != 0 && windows::incomplete_tail(&path)? {
            recovery = Some(archive(&path, "partial")?);
        }
        let mut value = json!({
            "schema_version": 1, "timestamp": record.timestamp, "observed_timestamp": now(),
            "severity_number": record.level as u8, "severity_text": record.level.name(),
            "event_name": record.name, "body": record.name,
            "trace_id": record.scope.trace_id, "span_id": record.scope.span_id, "trace_flags": 0,
            "resource": self.resource, "attributes": attributes,
            "log": {
                "record.uid": Uuid::new_v4().simple().to_string(),
                "record.sequence": self.sequence + 1,
                "audit.elapsed_ns": record.elapsed_ns,
                "records_lost_since_last_write": lost,
                "file.name": "log.json",
            },
            "integrity": { "algorithm": "sha256-chain-v1", "previous_hash": self.previous },
        });
        if let Some(recovered) = &recovery {
            value["log"]["incomplete_file.preserved_as"] = json!(recovered);
            value["log"]["incomplete_file.size"] = json!(size);
        }
        let mut removed = prune(&self.directory, &self.sid)?;
        if !removed.is_empty() {
            value["log"]["retention.removed_files"] = json!(removed);
        }
        value["log"]["file.offset"] = json!(if recovery.is_some() { 0 } else { size });
        let (mut line, mut hash) = encode_record(&value)?;
        if recovery.is_none() && size > 0 && size.saturating_add(line.len() as u64) > self.limit {
            let rotated = archive(&path, "jsonl")?;
            value["log"]["rotation.previous_file"] = json!(rotated);
            value["log"]["rotation.previous_size"] = json!(size);
            value["log"]["file.offset"] = json!(0);
            removed.extend(prune(&self.directory, &self.sid)?);
            if !removed.is_empty() {
                value["log"]["retention.removed_files"] = json!(removed);
            }
            (line, hash) = encode_record(&value)?;
        }
        if line.len() as u64 > self.limit {
            return Err(Error::Audit(
                "A single record exceeds the audit file-size limit".into(),
            ));
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .custom_flags(::windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&path)?;
        windows::protect(&path, &self.sid, false)?;
        file.write_all(&line)?;
        file.sync_all()?;
        self.sequence += 1;
        self.previous = hash;
        Ok(())
    }
}

fn encode_record(value: &Value) -> Result<(Vec<u8>, String)> {
    let mut value = value.clone();
    value.sort_all_objects();
    let canonical = serde_json::to_vec(&value).map_err(|error| Error::Audit(error.to_string()))?;
    let hash = format!("{:x}", Sha256::digest(canonical));
    value["integrity"]["hash"] = json!(hash);
    let mut line = serde_json::to_vec(&value).map_err(|error| Error::Audit(error.to_string()))?;
    line.push(b'\n');
    if line.len() > MAX_RECORD {
        return Err(Error::Audit(
            "Audit record exceeds the 256 KiB safety limit".into(),
        ));
    }
    Ok((line, hash))
}

fn archive(path: &Path, extension: &str) -> Result<String> {
    let name = format!(
        "log-{}-{}.{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
        Uuid::new_v4().simple(),
        extension
    );
    fs::rename(path, path.with_file_name(&name))?;
    Ok(name)
}

fn prune(directory: &Path, sid: &str) -> Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let owned = name
            .strip_prefix("log-")
            .and_then(|name| name.rsplit_once('.'))
            .filter(|(_, extension)| matches!(*extension, "jsonl" | "partial"))
            .and_then(|(stem, _)| stem.rsplit_once('-'))
            .is_some_and(|(stamp, id)| {
                Uuid::parse_str(id).is_ok()
                    && chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%S%.9fZ").is_ok()
            });
        if owned {
            windows::reject_reparse(&entry.path())?;
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                windows::protect(&entry.path(), sid, false)?;
                files.push((metadata.modified()?, name, entry.path()));
            }
        }
    }
    files.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    let remove = files.len().saturating_sub(ARCHIVES);
    let mut removed = Vec::new();
    for (_, name, path) in files.into_iter().take(remove) {
        fs::remove_file(path)?;
        removed.push(name);
    }
    Ok(removed)
}

fn redact(value: &mut Value) {
    redact_at(value, "");
}

fn redact_at(value: &mut Value, prefix: &str) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                let key = key.to_ascii_lowercase().replace(['-', '_'], ".");
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                if matches!(
                    key.as_str(),
                    "password"
                        | "pin"
                        | "secret"
                        | "authorization"
                        | "access.token"
                        | "refresh.token"
                        | "argv"
                        | "args"
                        | "command.line"
                        | "command.args"
                ) || key.ends_with(".password")
                    || key.ends_with(".secret")
                    || key.ends_with(".pin")
                    || path.ends_with(".command.line")
                    || path.ends_with(".command.args")
                    || path.starts_with("process.environment")
                    || (path.starts_with("clipboard.")
                        && (path.contains("text")
                            || path.contains("content")
                            || path.contains("hash")
                            || path.contains("sha256")))
                        && !matches!(
                            path.as_str(),
                            "clipboard.text.bytes" | "clipboard.text.bytes.count"
                        )
                    || path == "file.content"
                {
                    *value = json!("[REDACTED]");
                } else {
                    redact_at(value, &path);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(|value| redact_at(value, prefix)),
        _ => (),
    }
}

pub fn error_type(error: &Error) -> &'static str {
    match error {
        Error::Io(_) => "io",
        Error::Protocol(_) => "rfb_protocol",
        Error::Authentication(_) => "authentication",
        Error::Credential(_) => "credential_vault",
        Error::Consent(_) => "windows_hello",
        Error::Configuration(_) => "configuration",
        Error::Display(_) => "display",
        Error::Update(_) => "update",
        Error::UpdateRecovery(_) => "update_recovery",
        Error::Audit(_) => "audit",
        Error::Clipboard(_) => "clipboard",
        Error::Transfer(_) => "file_transfer",
        Error::Cancelled => "cancelled",
    }
}

pub fn error_attributes(error: &Error) -> Value {
    let mut attributes = json!({"error.type":error_type(error)});
    match error {
        Error::Io(error) => {
            attributes["error.code"] = json!(error.raw_os_error());
            attributes["error.kind"] = json!(format!("{:?}", error.kind()));
        }
        Error::Consent(reason) => attributes["security.consent.reason"] = json!(reason),
        Error::UpdateRecovery(reason) => attributes["update.recovery.detail"] = json!(reason),
        _ => (),
    }
    attributes
}

pub fn verify(paths: &[PathBuf]) -> Result<u64> {
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader, Read},
    };
    let mut runs = HashMap::<String, (u64, String)>::new();
    let mut count = 0;
    for path in paths {
        let mut reader = BufReader::new(std::fs::File::open(path)?);
        let mut line = Vec::new();
        let mut number = 0;
        loop {
            line.clear();
            let bytes = reader
                .by_ref()
                .take(MAX_RECORD as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if bytes == 0 {
                break;
            }
            number += 1;
            let fail = || {
                Error::Audit(format!(
                    "Invalid or altered audit record at {}:{number}",
                    path.display()
                ))
            };
            if line.len() > MAX_RECORD || !line.ends_with(b"\n") {
                return Err(fail());
            }
            let mut value: Value = serde_json::from_slice(&line).map_err(|_| fail())?;
            let hex = |value: &Value, length: usize| {
                value.as_str().is_some_and(|text| {
                    text.len() == length && text.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            };
            if value["schema_version"] != 1
                || value["integrity"]["algorithm"] != "sha256-chain-v1"
                || !hex(&value["trace_id"], 32)
                || !hex(&value["span_id"], 16)
                || !hex(&value["log"]["record.uid"], 32)
                || chrono::DateTime::parse_from_rfc3339(
                    value["timestamp"].as_str().ok_or_else(fail)?,
                )
                .is_err()
                || chrono::DateTime::parse_from_rfc3339(
                    value["observed_timestamp"].as_str().ok_or_else(fail)?,
                )
                .is_err()
            {
                return Err(fail());
            }
            let integrity = value
                .get_mut("integrity")
                .and_then(Value::as_object_mut)
                .ok_or_else(fail)?;
            let hash = integrity
                .remove("hash")
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(fail)?;
            value.sort_all_objects();
            if hash
                != format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&value).map_err(|_| fail())?)
                )
            {
                return Err(fail());
            }
            let instance = value["resource"]["service.instance.id"]
                .as_str()
                .ok_or_else(fail)?;
            let sequence = value["log"]["record.sequence"].as_u64().ok_or_else(fail)?;
            let previous = value["integrity"]["previous_hash"]
                .as_str()
                .ok_or_else(fail)?;
            if sequence == 0
                || (sequence == 1 && !previous.is_empty())
                || (sequence > 1 && !hex(&value["integrity"]["previous_hash"], 64))
            {
                return Err(fail());
            }
            if let Some((old_sequence, old_hash)) = runs.get(instance)
                && (old_sequence.checked_add(1) != Some(sequence) || old_hash != previous)
            {
                return Err(fail());
            }
            runs.insert(instance.into(), (sequence, hash));
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_point_metrics_and_nested_private_fields_roundtrip_safely() {
        let temp = tempfile::tempdir().unwrap();
        let mut guard = fixture(&temp.path().join("audit"), MAX_FILE);
        let scope = guard.audit().scope(json!({}));
        for value in [
            0.0,
            -0.0,
            0.1,
            1.2345678901234567,
            1.0e-20,
            1.0e20,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            scope.critical("test.metric",Level::Info,json!({
                "metric":value,"clipboard":{"text":"PRIVATE_TEXT","hash":"PRIVATE_HASH"},
                "process":{"command_line":"PRIVATE_COMMAND","environment":{"token":"PRIVATE_TOKEN"}},
                "clipboard.text_bytes":12,
            })).unwrap();
        }
        guard.shutdown().unwrap();
        assert_eq!(verify(&[guard.audit.path()]).unwrap(), 8);
        let text = fs::read_to_string(guard.audit.path()).unwrap();
        assert!(!text.contains("PRIVATE_"));
        assert!(text.contains("\"clipboard.text_bytes\":12"));
    }

    fn fixture(directory: &Path, limit: u64) -> Guard {
        let instance = Uuid::new_v4().simple().to_string();
        Audit::start_with(
            directory.into(),
            instance.clone(),
            json!({"service.name":"MacCanvas.Tests", "service.instance.id": instance}),
            limit,
        )
        .unwrap()
    }

    #[test]
    fn jsonl_redacts_secrets_and_verifies_a_per_run_chain() {
        let temp = tempfile::tempdir().unwrap();
        let mut guard = fixture(&temp.path().join("audit"), MAX_FILE);
        let scope = guard.audit().scope(json!({"profile.id": Uuid::nil()}));
        scope.critical("test.start", Level::Info, json!({"password":"DO_NOT_LOG", "nested":{"access_token":"SECRET_TOKEN"}, "clipboard.text":"SECRET_TEXT", "bytes":42})).unwrap();
        scope.critical("test.stop", Level::Info, json!({})).unwrap();
        guard.shutdown().unwrap();
        let path = guard.audit.path();
        assert_eq!(verify(std::slice::from_ref(&path)).unwrap(), 2);
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("DO_NOT_LOG")
                && !text.contains("SECRET_TOKEN")
                && !text.contains("SECRET_TEXT")
        );
        assert_eq!(text.lines().count(), 2);
        let mut records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        records[1]["attributes"]["bytes"] = json!(1000);
        fs::write(
            &path,
            records
                .into_iter()
                .map(|value| format!("{value}\n"))
                .collect::<String>(),
        )
        .unwrap();
        assert!(verify(&[path]).is_err());
    }

    #[test]
    fn concurrent_writers_preserve_complete_lines_and_independent_chains() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("audit");
        let mut first = fixture(&directory, MAX_FILE);
        let mut second = fixture(&directory, MAX_FILE);
        let scopes = [
            first.audit().scope(json!({})),
            second.audit().scope(json!({})),
        ];
        let workers: Vec<_> = scopes
            .into_iter()
            .map(|scope| {
                std::thread::spawn(move || {
                    for n in 0..20 {
                        scope
                            .critical("test.concurrent", Level::Info, json!({"index":n}))
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        first.shutdown().unwrap();
        second.shutdown().unwrap();
        assert_eq!(verify(&[directory.join("log.json")]).unwrap(), 40);
    }

    #[test]
    fn rotation_is_bounded_and_incomplete_tails_are_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("audit");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("log.json"), b"{\"incomplete\":").unwrap();
        let mut guard = fixture(&directory, 4096);
        let scope = guard.audit().scope(json!({}));
        for n in 0..40 {
            scope
                .critical(
                    "test.rotation",
                    Level::Info,
                    json!({"index":n,"padding":"x".repeat(200)}),
                )
                .unwrap();
        }
        guard.shutdown().unwrap();
        let files: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(
            files
                .iter()
                .filter(|path| path
                    .extension()
                    .is_some_and(|ext| ext == "jsonl" || ext == "partial"))
                .count(),
            ARCHIVES
        );
        for path in files.iter().filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "jsonl" || ext == "json")
        }) {
            assert!(fs::metadata(path).unwrap().len() <= 4096);
        }
        assert!(verify(&[directory.join("log.json")]).unwrap() >= 1);
    }

    #[test]
    fn an_unwritable_audit_path_fails_instead_of_disabling_audit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("not-a-directory");
        fs::write(&path, b"file").unwrap();
        let instance = Uuid::new_v4().simple().to_string();
        assert!(
            Audit::start_with(
                path,
                instance.clone(),
                json!({"service.instance.id":instance}),
                MAX_FILE
            )
            .is_err()
        );
    }

    #[test]
    fn own_process_evidence_has_parent_and_identity_without_command_lines() {
        let resource = windows::resource("test-instance");
        assert_eq!(resource["process.pid"], std::process::id());
        assert!(resource["process.parent_pid"].as_u64().is_some());
        assert!(
            resource["user.id"]
                .as_str()
                .is_some_and(|sid| sid.starts_with("S-1-"))
        );
        assert!(
            resource["process.executable.hash.sha256"]
                .as_str()
                .is_some_and(|hash| hash.len() == 64)
        );
        assert!(!resource.to_string().contains("command_line"));
    }
}
