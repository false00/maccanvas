use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_NO_MORE_FILES, FILETIME, HANDLE, HLOCAL, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSidSubAuthority,
            GetSidSubAuthorityCount, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
            PSECURITY_DESCRIPTOR, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_MANDATORY_LABEL,
            TOKEN_QUERY, TOKEN_STATISTICS, TOKEN_USER, TokenElevation, TokenIntegrityLevel,
            TokenSessionId, TokenStatistics, TokenUser,
        },
        Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            LOCKFILE_EXCLUSIVE_LOCK, LockFileEx, UnlockFileEx,
        },
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            IO::OVERLAPPED,
            SystemInformation::{
                ComputerNameDnsHostname, GetComputerNameExW, GetVersionExW, OSVERSIONINFOW,
            },
            Threading::{
                GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
                PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        },
    },
    core::{BOOL, HSTRING, PWSTR},
};

use crate::error::{Error, Result};

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if let Err(error) = unsafe { CloseHandle(self.0) } {
            eprintln!("MacCanvas audit handle cleanup: {error}");
        }
    }
}

struct Allocation(*mut std::ffi::c_void);

impl Drop for Allocation {
    fn drop(&mut self) {
        let remaining = unsafe { LocalFree(Some(HLOCAL(self.0))) };
        if !remaining.0.is_null() {
            eprintln!("MacCanvas audit security descriptor cleanup failed");
        }
    }
}

fn win_error(error: windows::core::Error) -> Error {
    Error::Audit(error.to_string())
}

fn token_buffer(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u64>> {
    let mut needed = 0;
    let probe = unsafe { GetTokenInformation(token, class, None, 0, &mut needed) };
    if needed == 0 || needed > 64 * 1024 {
        return Err(Error::Audit(format!(
            "Cannot query Windows token metadata: {probe:?}"
        )));
    }
    let mut buffer = vec![0_u64; (needed as usize).div_ceil(8)];
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(win_error)?;
    Ok(buffer)
}

fn current_token() -> Result<Handle> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(win_error)?;
    Ok(Handle(token))
}

pub fn current_sid() -> Result<String> {
    let token = current_token()?;
    let buffer = token_buffer(token.0, TokenUser)?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut sid) }.map_err(win_error)?;
    let _allocation = Allocation(sid.0.cast());
    unsafe { sid.to_string() }.map_err(|error| Error::Audit(error.to_string()))
}

pub fn protect(path: &Path, sid: &str, directory: bool) -> Result<()> {
    reject_reparse(path)?;
    let inherit = if directory { "OICI" } else { "" };
    let sddl = HSTRING::from(format!("D:P(A;{inherit};FA;;;SY)(A;{inherit};FA;;;{sid})"));
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(&sddl, 1, &mut descriptor, None)
    }
    .map_err(win_error)?;
    let _allocation = Allocation(descriptor.0);
    let mut present = BOOL::default();
    let mut defaulted = BOOL::default();
    let mut acl = std::ptr::null_mut();
    unsafe { GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) }
        .map_err(win_error)?;
    if !present.as_bool() || acl.is_null() {
        return Err(Error::Audit("Windows returned an invalid audit ACL".into()));
    }
    unsafe {
        SetNamedSecurityInfoW(
            &HSTRING::from(path.as_os_str()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(acl),
            None,
        )
    }
    .ok()
    .map_err(win_error)
}

pub fn reject_reparse(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_attributes() & 0x400 != 0 {
        return Err(Error::Audit(format!(
            "Refusing an audit reparse point: {}",
            path.display()
        )));
    }
    Ok(())
}

pub struct LockFile(File);

impl LockFile {
    pub fn open(directory: &Path, sid: &str) -> Result<Self> {
        let path = directory.join(".audit.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(&path)?;
        protect(&path, sid, false)?;
        Ok(Self(file))
    }

    pub fn acquire(&self) -> Result<LockGuard<'_>> {
        // Let Windows queue contending writers instead of starving a process
        // that repeatedly loses a try-lock race.
        unsafe {
            LockFileEx(
                HANDLE(self.0.as_raw_handle()),
                LOCKFILE_EXCLUSIVE_LOCK,
                None,
                u32::MAX,
                u32::MAX,
                &mut OVERLAPPED::default(),
            )
        }
        .map_err(win_error)?;
        Ok(LockGuard(self))
    }
}

pub struct LockGuard<'a>(&'a LockFile);

impl Drop for LockGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = unsafe {
            UnlockFileEx(
                HANDLE(self.0.0.as_raw_handle()),
                None,
                u32::MAX,
                u32::MAX,
                &mut OVERLAPPED::default(),
            )
        } {
            eprintln!("MacCanvas audit lock release failed: {error}");
        }
    }
}

pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() > 256 * 1024 * 1024 {
        return Err(Error::Audit(
            "Executable is larger than the 256 MiB evidence limit".into(),
        ));
    }
    let mut digest = Sha256::new();
    let mut bytes = [0_u8; 64 * 1024];
    loop {
        let size = file.read(&mut bytes)?;
        if size == 0 {
            break;
        }
        digest.update(&bytes[..size]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn file_time(value: FILETIME) -> (u64, String) {
    let ticks = (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
    let unix = i128::from(ticks) - 116_444_736_000_000_000_i128;
    let stamp = i64::try_from(unix.div_euclid(10_000_000))
        .ok()
        .and_then(|seconds| {
            chrono::DateTime::from_timestamp(seconds, (unix.rem_euclid(10_000_000) * 100) as u32)
        });
    (
        ticks,
        stamp.map_or_else(
            || format!("unrepresentable_filetime:{ticks}"),
            |value| value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        ),
    )
}

fn creation(handle: HANDLE) -> Result<(u64, String)> {
    let mut created = FILETIME::default();
    unsafe {
        GetProcessTimes(
            handle,
            &mut created,
            &mut FILETIME::default(),
            &mut FILETIME::default(),
            &mut FILETIME::default(),
        )
    }
    .map_err(win_error)?;
    Ok(file_time(created))
}

fn image(handle: HANDLE) -> Result<PathBuf> {
    let mut text = vec![0_u16; 32_768];
    let mut length = text.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(text.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(win_error)?;
    use std::os::windows::ffi::OsStringExt;
    Ok(std::ffi::OsString::from_wide(&text[..length as usize]).into())
}

fn identity() -> Result<Value> {
    let token = current_token()?;
    let elevated = token_buffer(token.0, TokenElevation)?;
    let elevated = unsafe { &*elevated.as_ptr().cast::<TOKEN_ELEVATION>() };
    let integrity = token_buffer(token.0, TokenIntegrityLevel)?;
    let integrity = unsafe { &*integrity.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    let count = unsafe { *GetSidSubAuthorityCount(integrity.Label.Sid) };
    if count == 0 {
        return Err(Error::Audit("Windows integrity SID has no level".into()));
    }
    let level = unsafe { *GetSidSubAuthority(integrity.Label.Sid, u32::from(count - 1)) };
    let stats = token_buffer(token.0, TokenStatistics)?;
    let stats = unsafe { &*stats.as_ptr().cast::<TOKEN_STATISTICS>() };
    let session = token_buffer(token.0, TokenSessionId)?;
    let session = unsafe { *session.as_ptr().cast::<u32>() };
    Ok(json!({
        "user.id": current_sid()?,
        "process.windows.elevated": elevated.TokenIsElevated != 0,
        "process.windows.integrity_rid": level,
        "process.windows.integrity_level": match level {
            0..0x1000 => "untrusted", 0x1000..0x2000 => "low", 0x2000..0x3000 => "medium",
            0x3000..0x4000 => "high", 0x4000..0x5000 => "system", _ => "protected",
        },
        "process.windows.logon_id": format!("{:08x}{:08x}", stats.AuthenticationId.HighPart as u32, stats.AuthenticationId.LowPart),
        "process.windows.session_id": session,
    }))
}

fn ancestry() -> Result<Value> {
    let snapshot =
        Handle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }.map_err(win_error)?);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    unsafe { Process32FirstW(snapshot.0, &mut entry) }.map_err(win_error)?;
    let mut processes = HashMap::new();
    loop {
        processes.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        match unsafe { Process32NextW(snapshot.0, &mut entry) } {
            Ok(()) => (),
            Err(error) if error.code() == ERROR_NO_MORE_FILES.to_hresult() => break,
            Err(error) => return Err(win_error(error)),
        }
    }
    let own = std::process::id();
    let parent = processes
        .get(&own)
        .copied()
        .ok_or_else(|| Error::Audit("The process snapshot did not include MacCanvas".into()))?;
    let (mut child_created, own_created) = creation(unsafe { GetCurrentProcess() })?;
    let mut pid = parent;
    let mut seen = HashSet::from([own]);
    let mut entries = Vec::new();
    for depth in 0..8 {
        if pid == 0 || !seen.insert(pid) {
            break;
        }
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) => Handle(handle),
            Err(error) => {
                entries.push(json!({"process.pid": pid, "evidence.status": "unavailable", "error.code": error.code().0}));
                break;
            }
        };
        let mut item = json!({"process.pid": pid});
        let (created, stamp) = match creation(handle.0) {
            Ok(value) => value,
            Err(error) => {
                item["evidence.status"] = json!("creation_time_unavailable");
                item["error.message"] = json!(error.to_string());
                entries.push(item);
                break;
            }
        };
        item["process.creation_time"] = json!(stamp);
        if created > child_created {
            item["evidence.status"] = json!("pid_reused");
            entries.push(item);
            break;
        }
        item["evidence.status"] = json!("creation_order_verified");
        match image(handle.0) {
            Ok(path) => {
                item["process.executable.path"] = json!(path.to_string_lossy());
                if depth == 0 {
                    match hash_file(&path) {
                        Ok(hash) => item["process.executable.hash.sha256"] = json!(hash),
                        Err(error) => {
                            item["process.executable.hash.error"] = json!(error.to_string())
                        }
                    }
                }
            }
            Err(error) => item["process.executable.error"] = json!(error.to_string()),
        }
        entries.push(item);
        child_created = created;
        let Some(next) = processes.get(&pid) else {
            break;
        };
        pid = *next;
    }
    Ok(
        json!({"process.parent_pid": parent, "process.creation_time": own_created, "process.ancestry": entries}),
    )
}

pub fn resource(instance: &str) -> Value {
    let mut resource = json!({
        "service.name": "MacCanvas", "service.version": crate::VERSION, "service.instance.id": instance,
        "process.pid": std::process::id(), "process.runtime.name": "Rust", "process.runtime.version": crate::RUSTC,
        "os.type": "windows", "host.arch": "amd64",
        "build.time": crate::BUILD_UTC, "build.time.source": crate::BUILD_CLOCK, "build.target": crate::TARGET,
        "vcs.ref.head.revision": crate::REVISION,
    });
    match std::env::current_exe().and_then(fs::canonicalize) {
        Ok(path) => {
            resource["process.executable.path"] = json!(path.to_string_lossy());
            match hash_file(&path) {
                Ok(hash) => resource["process.executable.hash.sha256"] = json!(hash),
                Err(error) => resource["process.executable.hash.error"] = json!(error.to_string()),
            }
        }
        Err(error) => resource["process.executable.error"] = json!(error.to_string()),
    }
    for (name, result) in [("identity", identity()), ("ancestry", ancestry())] {
        match result {
            Ok(Value::Object(fields)) => resource
                .as_object_mut()
                .expect("resource is an object")
                .extend(fields),
            Ok(_) => unreachable!("Windows evidence is an object"),
            Err(error) => resource[format!("evidence.{name}.error")] = json!(error.to_string()),
        }
    }
    let mut hostname = [0_u16; 256];
    let mut length = hostname.len() as u32;
    match unsafe {
        GetComputerNameExW(
            ComputerNameDnsHostname,
            Some(PWSTR(hostname.as_mut_ptr())),
            &mut length,
        )
    } {
        Ok(()) => {
            resource["host.name"] = json!(String::from_utf16_lossy(&hostname[..length as usize]))
        }
        Err(error) => resource["host.name.error"] = json!(error.code().0),
    }
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    match unsafe { GetVersionExW(&mut version) } {
        Ok(()) => {
            resource["os.version"] = json!(format!(
                "{}.{}.{}",
                version.dwMajorVersion, version.dwMinorVersion, version.dwBuildNumber
            ))
        }
        Err(error) => resource["os.version.error"] = json!(error.code().0),
    }
    resource
}

pub fn incomplete_tail(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    Ok(last != [b'\n'])
}
