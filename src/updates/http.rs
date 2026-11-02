use std::{
    ffi::c_void,
    io::Write,
    ptr,
    time::{Duration, Instant},
};

use url::Url;
use windows::{
    Win32::Networking::WinHttp::*,
    core::{HSTRING, PCWSTR, w},
};

use crate::error::{Error, Result};

fn failed(error: impl std::fmt::Display) -> Error {
    Error::Update(error.to_string())
}

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void) -> Result<Self> {
        if raw.is_null() {
            return Err(failed(std::io::Error::last_os_error()));
        }
        Ok(Self(raw))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let Err(error) = unsafe { WinHttpCloseHandle(self.0) } {
            eprintln!("MacCanvas update HTTP cleanup: {error}");
        }
    }
}

pub(super) fn allowed_url(input: &str) -> Result<Url> {
    let url = Url::parse(input).map_err(failed)?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(
            url.host_str(),
            Some(
                "api.github.com"
                    | "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
    {
        return Err(failed(
            "Refusing an update download outside the HTTPS GitHub allowlist",
        ));
    }
    Ok(url)
}

fn location(request: &Handle) -> Result<String> {
    let mut buffer = vec![0u16; 8192];
    let mut length = (buffer.len() * 2) as u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_LOCATION,
            PCWSTR::null(),
            Some(buffer.as_mut_ptr().cast()),
            &mut length,
            ptr::null_mut(),
        )
    }
    .map_err(failed)?;
    if !length.is_multiple_of(2) || length as usize > buffer.len() * 2 {
        return Err(failed("Invalid update redirect length"));
    }
    buffer.truncate(length as usize / 2);
    if buffer.last() == Some(&0) {
        buffer.pop();
    }
    String::from_utf16(&buffer).map_err(failed)
}

pub(super) fn download(
    input: &str,
    maximum: u64,
    budget: Duration,
    writer: &mut impl Write,
    progress: &mut impl FnMut(u64),
) -> Result<u64> {
    let deadline = Instant::now() + budget;
    let mut url = allowed_url(input)?;
    let session = Handle::new(unsafe {
        WinHttpOpen(
            w!("MacCanvas-updater"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        )
    })?;
    unsafe { WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 8000) }.map_err(failed)?;
    for _ in 0..=5 {
        let host = HSTRING::from(
            url.host_str()
                .ok_or_else(|| failed("Update URL has no host"))?,
        );
        let connection = Handle::new(unsafe { WinHttpConnect(session.0, &host, 443, 0) })?;
        let path = HSTRING::from(match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        });
        let request = Handle::new(unsafe {
            WinHttpOpenRequest(
                connection.0,
                w!("GET"),
                &path,
                PCWSTR::null(),
                PCWSTR::null(),
                ptr::null(),
                WINHTTP_FLAG_SECURE,
            )
        })?;
        unsafe {
            WinHttpSetOption(
                Some(request.0.cast_const()),
                WINHTTP_OPTION_REDIRECT_POLICY,
                Some(&WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes()),
            )
        }
        .map_err(failed)?;
        let headers: Vec<u16> = "Accept: application/vnd.github+json\r\n"
            .encode_utf16()
            .collect();
        unsafe { WinHttpSendRequest(request.0, Some(&headers), None, 0, 0, 0) }.map_err(failed)?;
        unsafe { WinHttpReceiveResponse(request.0, ptr::null_mut()) }.map_err(failed)?;
        let mut status = 0u32;
        let mut length = 4;
        unsafe {
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&mut status as *mut u32).cast()),
                &mut length,
                ptr::null_mut(),
            )
        }
        .map_err(failed)?;
        if [301, 302, 303, 307, 308].contains(&status) {
            url = allowed_url(url.join(&location(&request)?).map_err(failed)?.as_str())?;
            if Instant::now() >= deadline {
                return Err(failed("Update request timed out"));
            }
            continue;
        }
        if status != 200 {
            return Err(failed(format!(
                "GitHub returned HTTP {status}. Check your network or try again later."
            )));
        }
        let mut received = 0u64;
        let mut buffer = [0; 65536];
        loop {
            if Instant::now() >= deadline {
                return Err(failed("Update download timed out"));
            }
            let mut count = 0;
            unsafe {
                WinHttpReadData(
                    request.0,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as u32,
                    &mut count,
                )
            }
            .map_err(failed)?;
            if count == 0 {
                return Ok(received);
            }
            received += u64::from(count);
            if received > maximum {
                return Err(failed("Update download exceeded its size limit"));
            }
            writer.write_all(&buffer[..count as usize])?;
            progress(received);
        }
    }
    Err(failed("Too many update download redirects"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redirects_cannot_escape_https_github_hosts() {
        for input in [
            "http://github.com/app.exe",
            "https://github.com.evil.example/app.exe",
            "https://user@github.com/app.exe",
            "https://github.com:444/app.exe",
            "https://github.com/file#fragment",
        ] {
            assert!(allowed_url(input).is_err());
        }
        assert!(
            allowed_url("https://release-assets.githubusercontent.com/binary?token=fixture")
                .is_ok()
        );
    }
}
