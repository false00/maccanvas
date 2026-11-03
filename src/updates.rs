use std::{
    fs,
    io::{Read, Write},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
        System::Threading::{CREATE_NO_WINDOW, CreateMutexW, ReleaseMutex, WaitForSingleObject},
    },
    core::HSTRING,
};

use crate::{
    VERSION,
    audit::{self, Level, Scope},
    error::{Error, Result},
};
use serde_json::json;

pub(crate) mod http;
mod shortcuts;

const REPOSITORY: &str = "false00/maccanvas";
const ASSET: &str = "MacCanvas-windows-x64.exe";
const MAX_EXE: u64 = 100 * 1024 * 1024;
const MAX_METADATA: u64 = 256 * 1024;

fn failed(error: impl std::fmt::Display) -> Error {
    Error::Update(error.to_string())
}

fn recovery(error: impl std::fmt::Display) -> Error {
    Error::UpdateRecovery(format!(
        "{error} Close and reopen MacCanvas before retrying."
    ))
}

#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub size: u64,
    pub sha256: String,
    url: String,
}

#[derive(Deserialize)]
struct Metadata {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
    digest: Option<String>,
}

pub fn parse_release(bytes: &[u8], current: &str) -> Result<Option<Release>> {
    if bytes.len() as u64 > MAX_METADATA {
        return Err(failed("Update metadata is too large"));
    }
    let metadata: Metadata = serde_json::from_slice(bytes).map_err(failed)?;
    let version = Version::parse(
        metadata
            .tag_name
            .strip_prefix('v')
            .ok_or_else(|| failed("Invalid release tag"))?,
    )
    .map_err(failed)?;
    if metadata.draft
        || metadata.prerelease
        || !version.pre.is_empty()
        || !version.build.is_empty()
        || metadata.assets.len() > 32
        || metadata.html_url
            != format!(
                "https://github.com/{REPOSITORY}/releases/tag/{}",
                metadata.tag_name
            )
    {
        return Err(failed(
            "Only official stable published releases can be installed",
        ));
    }
    if version <= Version::parse(current).map_err(failed)? {
        return Ok(None);
    }
    let mut matches = metadata.assets.iter().filter(|asset| asset.name == ASSET);
    let asset = matches
        .next()
        .ok_or_else(|| failed("The release has no Windows x64 executable"))?;
    if matches.next().is_some()
        || asset.size == 0
        || asset.size > MAX_EXE
        || asset.browser_download_url
            != format!(
                "https://github.com/{REPOSITORY}/releases/download/{}/{ASSET}",
                metadata.tag_name
            )
    {
        return Err(failed("The update asset has an unexpected source or size"));
    }
    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|hash| valid_hash(hash))
        .ok_or_else(|| failed("The update has no valid published SHA-256 digest"))?;
    Ok(Some(Release {
        version,
        tag: metadata.tag_name,
        size: asset.size,
        sha256: sha256.to_ascii_lowercase(),
        url: asset.browser_download_url.clone(),
    }))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn check() -> Result<Option<Release>> {
    let mut bytes = Vec::new();
    http::download(
        &format!("https://api.github.com/repos/{REPOSITORY}/releases/latest"),
        MAX_METADATA,
        Duration::from_secs(20),
        &mut bytes,
        &mut |_| {},
    )?;
    parse_release(&bytes, VERSION)
}

pub fn verify_executable(bytes: &[u8], release: &Release) -> Result<()> {
    verify_binary(bytes, release.size, &release.sha256)
}

pub(crate) fn verify_binary(bytes: &[u8], size: u64, sha256: &str) -> Result<()> {
    if bytes.len() as u64 != size || format!("{:x}", Sha256::digest(bytes)) != sha256 {
        return Err(failed(
            "Executable SHA-256 or size mismatch. Nothing was replaced.",
        ));
    }
    if bytes.len() < 256 || bytes.get(..2) != Some(b"MZ") {
        return Err(failed("Update is not a Windows executable"));
    }
    let offset = u32::from_le_bytes(bytes[0x3c..0x40].try_into().map_err(failed)?) as usize;
    let end = offset
        .checked_add(26)
        .ok_or_else(|| failed("Invalid PE header offset"))?;
    if end > bytes.len()
        || bytes.get(offset..offset + 4) != Some(b"PE\0\0")
        || bytes.get(offset + 4..offset + 6) != Some(&[0x64, 0x86])
        || bytes.get(offset + 24..end) != Some(&[0x0b, 0x02])
    {
        return Err(failed("Update is not a Windows x64 PE32+ application"));
    }
    Ok(())
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    repository: String,
    version: String,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shortcuts: Option<bool>,
}

fn file_hash(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    if file.metadata()?.len() > MAX_EXE {
        return Err(failed("Current executable is too large"));
    }
    let mut digest = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            return Ok(format!("{:x}", digest.finalize()));
        }
        digest.update(&bytes[..count]);
    }
}

fn write_receipt(path: &Path, receipt: &Receipt) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| failed("Receipt has no parent directory"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, receipt).map_err(failed)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn read_receipt(executable: &Path) -> Result<Option<Receipt>> {
    let path = executable
        .parent()
        .ok_or_else(|| failed("Application has no parent directory"))?
        .join(".maccanvas-install.json");
    match fs::metadata(&path) {
        Ok(metadata) => {
            if metadata.len() > 4096
                || !executable
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.eq_ignore_ascii_case("MacCanvas.exe"))
            {
                return Err(failed(
                    "The managed installation receipt does not match this executable",
                ));
            }
            let receipt: Receipt = serde_json::from_slice(&fs::read(path)?).map_err(failed)?;
            if receipt.repository != REPOSITORY
                || !valid_hash(&receipt.sha256)
                || receipt.sha256.to_ascii_lowercase() != file_hash(executable)?
            {
                return Err(failed(
                    "The installation was changed; refusing to modify its shortcuts or receipt",
                ));
            }
            Ok(Some(receipt))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn maintain_shortcuts_at(
    executable: &Path,
    paths: impl FnOnce() -> Result<[PathBuf; 2]>,
) -> Result<Option<usize>> {
    let Some(mut receipt) = read_receipt(executable)? else {
        return Ok(None);
    };
    if receipt.shortcuts == Some(false) {
        return Ok(Some(0));
    }
    let links = shortcuts::Links::new()?;
    let paths = paths()?;
    let enabled = match receipt.shortcuts {
        Some(enabled) => enabled,
        // Legacy opt-outs have no policy field and no app-owned shortcuts.
        None => links.owns_any(executable, &paths)?,
    };
    let changed = if enabled {
        links.repair(executable, &paths)?
    } else {
        0
    };
    if receipt.shortcuts.is_none() {
        receipt.shortcuts = Some(enabled);
        write_receipt(
            &executable
                .parent()
                .ok_or_else(|| failed("Application has no parent directory"))?
                .join(".maccanvas-install.json"),
            &receipt,
        )?;
    }
    Ok(Some(changed))
}

pub fn maintain_shortcuts(scope: &Scope) -> Result<()> {
    let executable = std::env::current_exe()?;
    let _lock = UpdateLock::acquire(&executable)?;
    match maintain_shortcuts_at(&executable, shortcuts::paths) {
        Ok(Some(changed)) => scope.event(
            "application.shortcuts.maintained",
            Level::Info,
            json!({"process.executable.path":executable,"shortcut.changed_count":changed}),
        ),
        Ok(None) => Ok(()),
        Err(error) => {
            scope.event(
                "application.shortcuts.failed",
                Level::Warn,
                audit::error_attributes(&error),
            )?;
            Err(error)
        }
    }
}

fn install_at(
    executable: &Path,
    source: &Path,
    release: &Release,
    replace: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<()> {
    let parent = executable
        .parent()
        .ok_or_else(|| failed("Application has no parent directory"))?;
    let old_hash = file_hash(executable)?;
    let receipt_path = parent.join(".maccanvas-install.json");
    let receipt = read_receipt(executable)?;
    let backup = tempfile::Builder::new()
        .prefix(".maccanvas-rollback-")
        .tempfile_in(parent)?;
    fs::copy(executable, backup.path())?;
    backup.as_file().sync_all()?;
    if let Some(old) = &receipt {
        write_receipt(
            &receipt_path,
            &Receipt {
                repository: old.repository.clone(),
                version: release.tag.clone(),
                sha256: release.sha256.clone(),
                shortcuts: old.shortcuts,
            },
        )?;
    }
    if let Err(error) = replace(source) {
        match file_hash(executable) {
            Ok(actual) if actual == release.sha256 => {
                return Err(recovery(format!(
                    "The new version is installed, but cleanup reported: {error}."
                )));
            }
            Ok(actual) if actual == old_hash => (),
            Err(Error::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
                let restored = (|| -> Result<()> {
                    let mut restored = tempfile::NamedTempFile::new_in(parent)?;
                    let mut original = fs::File::open(backup.path())?;
                    std::io::copy(&mut original, &mut restored)?;
                    restored.as_file().sync_all()?;
                    restored.persist(executable).map_err(|error| error.error)?;
                    Ok(())
                })();
                if let Err(restore_error) = restored {
                    let (_, path) = backup.keep().map_err(|error| error.error)?;
                    return Err(recovery(format!(
                        "Could not restore the executable: {restore_error}. A recovery copy is at {}.",
                        path.display()
                    )));
                }
            }
            _ => {
                let (_, path) = backup.keep().map_err(|error| error.error)?;
                return Err(recovery(format!(
                    "The executable changed unexpectedly. A recovery copy is preserved at {}.",
                    path.display()
                )));
            }
        }
        if let Some(old) = &receipt {
            write_receipt(&receipt_path, old).map_err(recovery)?;
        }
        return Err(recovery(format!(
            "Could not replace the application: {error}. The previous version and receipt were preserved."
        )));
    }
    if file_hash(executable)? != release.sha256 {
        let (_, path) = backup.keep().map_err(|error| error.error)?;
        return Err(recovery(format!(
            "The replaced executable does not match the verified update. A recovery copy is preserved at {}.",
            path.display()
        )));
    }
    Ok(())
}

struct UpdateLock(HANDLE);
impl UpdateLock {
    fn acquire(executable: &Path) -> Result<Self> {
        let name = HSTRING::from(format!(
            "Local\\MacCanvas.Update.{:x}",
            Sha256::digest(executable.to_string_lossy().to_lowercase().as_bytes())
        ));
        let handle = unsafe { CreateMutexW(None, false, &name) }.map_err(failed)?;
        let result = unsafe { WaitForSingleObject(handle, 0) };
        if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
            unsafe { CloseHandle(handle) }.map_err(failed)?;
            return Err(failed(
                "Another MacCanvas process is already installing an update",
            ));
        }
        Ok(Self(handle))
    }
}
impl Drop for UpdateLock {
    fn drop(&mut self) {
        if let Err(error) = unsafe { ReleaseMutex(self.0) } {
            eprintln!("MacCanvas update lock: {error}");
        }
        if let Err(error) = unsafe { CloseHandle(self.0) } {
            eprintln!("MacCanvas update lock cleanup: {error}");
        }
    }
}

fn install(release: &Release, progress: &AtomicU64, scope: &Scope) -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let _lock = UpdateLock::acquire(&executable)?;
    let parent = executable
        .parent()
        .ok_or_else(|| failed("Application has no parent directory"))?;
    let mut staged = tempfile::Builder::new()
        .prefix(".maccanvas-update-")
        .suffix(".exe")
        .tempfile_in(parent)?;
    scope.critical("update.download.started",Level::Info,json!({"artifact.version":release.version.to_string(),"artifact.size":release.size,"artifact.hash.sha256":release.sha256,"url.full":release.url}))?;
    http::download(
        &release.url,
        release.size,
        Duration::from_secs(180),
        &mut staged,
        &mut |received| {
            progress.store(received, Ordering::Relaxed);
        },
    )?;
    staged.flush()?;
    staged.as_file().sync_all()?;
    verify_executable(&fs::read(staged.path())?, release)?;
    scope.critical("update.artifact.verified",Level::Info,json!({"artifact.version":release.version.to_string(),"artifact.size":release.size,"artifact.hash.sha256":release.sha256,"event.outcome":"success"}))?;
    let staged = staged.into_temp_path();
    validate_candidate(&staged, &release.version)?;
    scope.critical(
        "update.preflight.completed",
        Level::Info,
        json!({"event.outcome":"success"}),
    )?;
    scope.critical("update.replacement.requested",Level::Info,json!({"process.executable.path":executable,"artifact.previous.hash.sha256":file_hash(&executable)?}))?;
    let replaced = install_at(&executable, &staged, release, |source| {
        self_replace::self_replace(source)
    });
    if let Err(error) = &replaced {
        let mut attributes = audit::error_attributes(error);
        attributes["artifact.expected.hash.sha256"] = json!(release.sha256);
        match file_hash(&executable) {
            Ok(hash) => attributes["artifact.current.hash.sha256"] = json!(hash),
            Err(error) => {
                attributes["artifact.current.error.type"] = json!(audit::error_type(&error))
            }
        }
        if let Err(audit_error) =
            scope.critical("update.replacement.failed", Level::Error, attributes)
        {
            eprintln!("MacCanvas could not persist update recovery evidence: {audit_error}");
        }
    }
    replaced?;
    scope.critical("update.replacement.completed",Level::Info,json!({"process.executable.path":executable,"artifact.version":release.version.to_string(),"artifact.hash.sha256":release.sha256,"event.outcome":"success"}))
        .map_err(|error| recovery(format!("The new version is installed, but its final audit receipt failed: {error}.")))?;
    Ok(executable)
}

pub fn validate_candidate(executable: &Path, version: &Version) -> Result<()> {
    let mut child = Command::new(executable)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0)
        .spawn()
        .map_err(|error| {
            failed(format!(
                "Windows could not run the verified update: {error}. Nothing was replaced."
            ))
        })?;
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            result => {
                if let Err(error) = child.kill() {
                    eprintln!("MacCanvas candidate check cleanup: {error}");
                }
                if let Err(error) = child.wait() {
                    eprintln!("MacCanvas candidate check wait: {error}");
                }
                return Err(failed(match result {
                    Err(error) => format!(
                        "Could not check the update process: {error}. Nothing was replaced."
                    ),
                    _ => {
                        "The verified update did not start correctly. Nothing was replaced.".into()
                    }
                }));
            }
        }
    };
    let mut output = String::new();
    child
        .stdout
        .take()
        .ok_or_else(|| failed("Update version output is unavailable"))?
        .take(8192)
        .read_to_string(&mut output)?;
    if !status.success() || output.trim() != format!("MacCanvas {version}") {
        return Err(failed(
            "The verified update could not report its expected version. Nothing was replaced.",
        ));
    }
    Ok(())
}

pub enum Event {
    Checked(Result<Option<Release>>),
    Installed(Result<PathBuf>),
}

pub struct Updater {
    receiver: Receiver<Event>,
    sender: mpsc::Sender<Event>,
    pub progress: Arc<AtomicU64>,
    pub release: Option<Release>,
    pub checking: bool,
    pub installing: bool,
    pub restart_required: bool,
}

impl Updater {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            receiver,
            sender,
            progress: Arc::new(AtomicU64::new(0)),
            release: None,
            checking: false,
            installing: false,
            restart_required: false,
        }
    }
    pub fn check(&mut self, scope: &Scope) -> Result<()> {
        if self.checking || self.installing || self.restart_required {
            return Ok(());
        }

        let scope =
            scope.child(json!({"network.protocol.name":"https","server.address":"api.github.com"}));
        scope.critical(
            "update.check.requested",
            Level::Info,
            json!({"service.current.version":VERSION}),
        )?;
        let sender = self.sender.clone();
        thread::Builder::new().name("maccanvas-update-check".into()).spawn(move || {
            let result = check();
            let attributes = match &result {
                Ok(Some(release)) => json!({"event.outcome":"available","artifact.version":release.version.to_string(),"artifact.hash.sha256":release.sha256}),
                Ok(None) => json!({"event.outcome":"up_to_date"}),
                Err(error) => audit::error_attributes(error),
            };
            let recorded = scope.event("update.check.completed",if result.is_ok() { Level::Info } else { Level::Warn },attributes);
            let result = result.and_then(|release| recorded.map(|_| release));
            if sender.send(Event::Checked(result)).is_err() {
                eprintln!("MacCanvas update check finished after the window closed");
            }
        })?;
        self.checking = true;
        Ok(())
    }
    #[cfg(debug_assertions)]
    pub fn preview_available(&mut self) -> Result<()> {
        let mut version = Version::parse(VERSION).map_err(failed)?;
        version.patch += 1;
        let tag = format!("v{version}");
        self.sender
            .send(Event::Checked(Ok(Some(Release {
                version,
                tag: tag.clone(),
                size: 1,
                sha256: "0".repeat(64),
                url: format!("https://github.com/{REPOSITORY}/releases/download/{tag}/{ASSET}"),
            }))))
            .map_err(failed)
    }

    pub fn install(&mut self, scope: &Scope) -> Result<()> {
        if self.installing || self.restart_required {
            return Err(failed(
                "An update is already installing or MacCanvas needs a restart",
            ));
        }
        let release = self
            .release
            .clone()
            .ok_or_else(|| failed("No verified newer release is available"))?;
        let scope =
            scope.child(json!({"network.protocol.name":"https","server.address":"github.com"}));
        scope.critical("update.install.approved",Level::Info,json!({"artifact.version":release.version.to_string(),"artifact.hash.sha256":release.sha256}))?;
        self.progress.store(0, Ordering::Relaxed);
        let sender = self.sender.clone();
        let progress = self.progress.clone();
        thread::Builder::new()
            .name("maccanvas-update-install".into())
            .spawn(move || {
                let result = install(&release, &progress, &scope).map_err(|error| match error {
                    Error::Io(io) => failed(io),
                    other => other,
                });
                if let Err(error) = &result
                    && let Err(audit_error) = scope.critical(
                        "update.install.failed",
                        Level::Error,
                        audit::error_attributes(error),
                    )
                {
                    eprintln!("MacCanvas update audit failed: {audit_error}");
                }
                if sender.send(Event::Installed(result)).is_err() {
                    eprintln!("MacCanvas update finished after the window closed");
                }
            })?;
        self.installing = true;
        Ok(())
    }
    pub fn event(&self) -> Result<Option<Event>> {
        match self.receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(failed("Update worker disconnected")),
        }
    }
}
impl Default for Updater {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Vec<u8>, Release) {
        let mut bytes = vec![0; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&128u32.to_le_bytes());
        bytes[128..132].copy_from_slice(b"PE\0\0");
        bytes[132..134].copy_from_slice(&[0x64, 0x86]);
        bytes[152..154].copy_from_slice(&[0x0b, 0x02]);
        let release = Release {
            version: Version::new(9, 0, 0), tag: "v9.0.0".into(), size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)), url: "https://github.com/false00/maccanvas/releases/download/v9.0.0/MacCanvas-windows-x64.exe".into(),
        };
        (bytes, release)
    }
    fn metadata(release: &Release) -> serde_json::Value {
        serde_json::json!({
            "tag_name": release.tag, "draft": false, "prerelease": false,
            "html_url": format!("https://github.com/{REPOSITORY}/releases/tag/{}", release.tag),
            "assets": [{ "name": ASSET, "size": release.size, "browser_download_url": release.url, "digest": format!("sha256:{}", release.sha256) }]
        })
    }
    #[test]
    fn compares_stable_semantic_versions_without_downgrading() {
        let (_, release) = fixture();
        let json = serde_json::to_vec(&metadata(&release)).unwrap();
        assert!(parse_release(&json, "0.2.1").unwrap().is_some());
        assert!(parse_release(&json, "9.0.0").unwrap().is_none());
        assert!(parse_release(&json, "10.0.0").unwrap().is_none());
    }
    #[test]
    fn optional_audio_assets_do_not_change_the_viewer_update_identity() {
        let (_, release) = fixture();
        let mut json = metadata(&release);
        json["assets"].as_array_mut().unwrap().push(serde_json::json!({
            "name": audio_codec_tool::ASSET, "size": audio_codec_tool::size(),
            "digest": format!("sha256:{}", audio_codec_tool::SHA256),
            "browser_download_url": format!("https://github.com/{REPOSITORY}/releases/download/{}/{}", release.tag, audio_codec_tool::ASSET),
        }));
        let selected = parse_release(&serde_json::to_vec(&json).unwrap(), "0.4.2")
            .unwrap()
            .unwrap();
        assert_eq!(selected.sha256, release.sha256);
        assert_eq!(selected.size, release.size);
        assert_eq!(selected.url, release.url);
    }
    #[test]
    fn rejects_foreign_assets_missing_hashes_and_prereleases() {
        let (_, release) = fixture();
        for mutation in 0..4 {
            let mut json = metadata(&release);
            match mutation {
                0 => {
                    json["assets"][0]["browser_download_url"] =
                        "https://github.com/other/project/app.exe".into()
                }
                1 => json["assets"][0]["digest"] = serde_json::Value::Null,
                2 => json["prerelease"] = true.into(),
                _ => {
                    let duplicate = json["assets"][0].clone();
                    json["assets"].as_array_mut().unwrap().push(duplicate);
                }
            }
            assert!(parse_release(&serde_json::to_vec(&json).unwrap(), "0.2.1").is_err());
        }
    }
    #[test]
    fn rejects_changed_executables_and_incorrect_architecture() {
        let (mut bytes, mut release) = fixture();
        verify_executable(&bytes, &release).unwrap();
        bytes[300] ^= 1;
        assert!(verify_executable(&bytes, &release).is_err());
        bytes[132] = 0;
        release.sha256 = format!("{:x}", Sha256::digest(&bytes));
        assert!(verify_executable(&bytes, &release).is_err());
    }
    #[test]
    fn updates_managed_receipt_and_preserves_uninstall_compatibility() {
        for shortcuts in [None, Some(true), Some(false)] {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("MacCanvas.exe");
            let staged = directory.path().join("staged.exe");
            let receipt = directory.path().join(".maccanvas-install.json");
            fs::write(&executable, b"old fixture").unwrap();
            write_receipt(
                &receipt,
                &Receipt {
                    repository: REPOSITORY.into(),
                    version: "v0.1.1".into(),
                    sha256: file_hash(&executable).unwrap(),
                    shortcuts,
                },
            )
            .unwrap();
            let (bytes, release) = fixture();
            fs::write(&staged, &bytes).unwrap();
            install_at(&executable, &staged, &release, |source| {
                fs::copy(source, &executable)?;
                Ok(())
            })
            .unwrap();
            let updated: Receipt = serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
            assert_eq!(updated.sha256, file_hash(&executable).unwrap());
            assert_eq!(updated.version, release.tag);
            assert_eq!(updated.shortcuts, shortcuts);
        }
    }
    #[test]
    fn managed_executable_names_follow_windows_case_insensitivity() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("maccanvas.exe");
        let staged = directory.path().join("staged.exe");
        let receipt = directory.path().join(".maccanvas-install.json");
        fs::write(&executable, b"old fixture").unwrap();
        write_receipt(
            &receipt,
            &Receipt {
                repository: REPOSITORY.into(),
                version: "v0.1.1".into(),
                sha256: file_hash(&executable).unwrap(),
                shortcuts: None,
            },
        )
        .unwrap();
        let (bytes, release) = fixture();
        fs::write(&staged, bytes).unwrap();
        install_at(&executable, &staged, &release, |source| {
            fs::copy(source, &executable)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(file_hash(&executable).unwrap(), release.sha256);
    }

    #[test]
    fn failed_replacement_restores_the_original_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("MacCanvas.exe");
        let receipt_path = directory.path().join(".maccanvas-install.json");
        fs::write(&executable, b"old fixture").unwrap();
        let old_hash = file_hash(&executable).unwrap();
        write_receipt(
            &receipt_path,
            &Receipt {
                repository: REPOSITORY.into(),
                version: "v0.1.1".into(),
                sha256: old_hash.clone(),
                shortcuts: Some(false),
            },
        )
        .unwrap();
        let (_, release) = fixture();
        assert!(
            install_at(&executable, Path::new("unused fixture"), &release, |_| Err(
                std::io::Error::other("fixture failure")
            ))
            .is_err()
        );
        let receipt: Receipt = serde_json::from_slice(&fs::read(receipt_path).unwrap()).unwrap();
        assert_eq!(receipt.sha256, old_hash);
        assert_eq!(receipt.shortcuts, Some(false));
        assert_eq!(file_hash(&executable).unwrap(), old_hash);
    }

    #[test]
    fn failure_after_renaming_the_old_executable_restores_a_runnable_path() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("MacCanvas.exe");
        fs::write(&executable, b"old fixture").unwrap();
        let (_, release) = fixture();
        assert!(
            install_at(&executable, Path::new("unused fixture"), &release, |_| {
                fs::rename(&executable, directory.path().join("relocated.fixture"))?;
                Err(std::io::Error::other("fixture copy failure"))
            })
            .is_err()
        );
        assert_eq!(fs::read(executable).unwrap(), b"old fixture");
    }

    #[test]
    fn managed_startup_repairs_shortcuts_and_migrates_legacy_installs() {
        for policy in [None, Some(true)] {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("MacCanvas.exe");
            let receipt_path = directory.path().join(".maccanvas-install.json");
            let paths = [
                directory.path().join("Start menu").join("MacCanvas.lnk"),
                directory.path().join("Desktop").join("MacCanvas.lnk"),
            ];
            fs::write(&executable, b"not executed").unwrap();
            write_receipt(
                &receipt_path,
                &Receipt {
                    repository: REPOSITORY.into(),
                    version: "v0.3.0".into(),
                    sha256: file_hash(&executable).unwrap(),
                    shortcuts: policy,
                },
            )
            .unwrap();
            if policy.is_none() {
                shortcuts::Links::new()
                    .unwrap()
                    .repair(&executable, &paths[..1])
                    .unwrap();
            }
            let expected = if policy.is_none() { 1 } else { 2 };
            assert_eq!(
                maintain_shortcuts_at(&executable, || Ok(paths.clone())).unwrap(),
                Some(expected)
            );
            assert_eq!(
                read_receipt(&executable).unwrap().unwrap().shortcuts,
                Some(true)
            );
            fs::remove_file(&paths[1]).unwrap();
            assert_eq!(
                maintain_shortcuts_at(&executable, || Ok(paths.clone())).unwrap(),
                Some(1)
            );
            assert_eq!(
                maintain_shortcuts_at(&executable, || Ok(paths.clone())).unwrap(),
                Some(0)
            );
        }
    }

    #[test]
    fn startup_preserves_portable_and_shortcut_free_installs() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("MacCanvas.exe");
        fs::write(&executable, b"not executed").unwrap();
        assert_eq!(
            maintain_shortcuts_at(&executable, || panic!("Portable copies have no shortcuts"))
                .unwrap(),
            None
        );
        let receipt_path = directory.path().join(".maccanvas-install.json");
        for policy in [None, Some(false)] {
            write_receipt(
                &receipt_path,
                &Receipt {
                    repository: REPOSITORY.into(),
                    version: "v0.3.0".into(),
                    sha256: file_hash(&executable).unwrap(),
                    shortcuts: policy,
                },
            )
            .unwrap();
            assert_eq!(
                maintain_shortcuts_at(&executable, || {
                    assert!(
                        policy.is_none(),
                        "Explicit opt-out must not inspect shortcuts"
                    );
                    Ok([
                        directory.path().join("missing-menu.lnk"),
                        directory.path().join("missing-desktop.lnk"),
                    ])
                })
                .unwrap(),
                Some(0)
            );
            assert_eq!(
                read_receipt(&executable).unwrap().unwrap().shortcuts,
                Some(false)
            );
        }
        fs::write(&executable, b"changed").unwrap();
        assert!(
            maintain_shortcuts_at(&executable, || panic!(
                "Changed installs must not touch links"
            ))
            .is_err()
        );
    }
}
