use windows::Win32::{
    Foundation::{ERROR_SUCCESS, GetLastError, GlobalFree, HANDLE, HGLOBAL, HWND, SetLastError},
    System::{
        DataExchange::{
            CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock},
    },
    UI::WindowsAndMessaging::GetForegroundWindow,
};
use zeroize::Zeroizing;

use super::MAX_TEXT;
use crate::error::{Error, Result};

const UNICODE_TEXT: u32 = 13;

struct Clipboard(bool);

impl Clipboard {
    fn open(owner: isize) -> Result<Self> {
        unsafe { OpenClipboard(Some(HWND(owner as *mut std::ffi::c_void))) }.map_err(|error| {
            Error::Clipboard(format!("Windows clipboard is busy or unavailable: {error}"))
        })?;
        Ok(Self(true))
    }

    fn close(&mut self) -> Result<()> {
        unsafe { CloseClipboard() }.map_err(|error| Error::Clipboard(error.to_string()))?;
        self.0 = false;
        Ok(())
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        if self.0
            && let Err(error) = unsafe { CloseClipboard() }
        {
            eprintln!("MacCanvas clipboard close failed: {error}");
        }
    }
}

fn unlock(handle: HGLOBAL) -> Result<()> {
    unsafe {
        SetLastError(ERROR_SUCCESS);
    }
    let result = unsafe { GlobalUnlock(handle) };
    match result {
        Ok(()) => Ok(()),
        Err(_) if unsafe { GetLastError() } == ERROR_SUCCESS => Ok(()),
        Err(error) => Err(Error::Clipboard(error.to_string())),
    }
}

pub fn sequence() -> u32 {
    unsafe { GetClipboardSequenceNumber() }
}

pub fn foreground(owner: isize) -> bool {
    unsafe { GetForegroundWindow().0 as isize == owner }
}

pub fn read(owner: isize) -> Result<Option<Zeroizing<String>>> {
    Ok(read_snapshot(owner)?.map(|(_, text)| text))
}

pub fn read_snapshot(owner: isize) -> Result<Option<(u32, Zeroizing<String>)>> {
    let mut clipboard = Clipboard::open(owner)?;
    if unsafe { IsClipboardFormatAvailable(UNICODE_TEXT) }.is_err() {
        clipboard.close()?;
        return Ok(None);
    }
    let handle = unsafe { GetClipboardData(UNICODE_TEXT) }
        .map_err(|error| Error::Clipboard(error.to_string()))?;
    let global = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(global) };
    if size == 0 || size > (MAX_TEXT * 2 + 1) * 2 || !size.is_multiple_of(2) {
        return Err(Error::Clipboard(
            "Windows clipboard text is malformed or larger than 1 MiB".into(),
        ));
    }
    let pointer = unsafe { GlobalLock(global) };
    if pointer.is_null() {
        return Err(Error::Clipboard(
            "Windows could not lock the clipboard text".into(),
        ));
    }
    let units = unsafe { std::slice::from_raw_parts(pointer.cast::<u16>(), size / 2) };
    let result = match units.iter().position(|&unit| unit == 0) {
        Some(end) => String::from_utf16(&units[..end])
            .map(Zeroizing::new)
            .map_err(|_| Error::Clipboard("Windows clipboard contains invalid UTF-16".into())),
        None => Err(Error::Clipboard(
            "Windows clipboard text has no terminator".into(),
        )),
    };
    unlock(global)?;
    let text = Zeroizing::new(super::normalize(&result?));
    super::validate(&text, true)?;
    let sequence = sequence();
    clipboard.close()?;
    Ok(Some((sequence, text)))
}

pub fn write(owner: isize, text: &str) -> Result<()> {
    super::validate(text, true)?;
    let text = Zeroizing::new(super::normalize(text).replace('\n', "\r\n"));
    let units = Zeroizing::new(text.encode_utf16().chain(Some(0)).collect::<Vec<_>>());
    let allocation = unsafe { GlobalAlloc(GMEM_MOVEABLE, units.len() * 2) }
        .map_err(|error| Error::Clipboard(error.to_string()))?;
    let pointer = unsafe { GlobalLock(allocation) };
    if pointer.is_null() {
        unsafe { GlobalFree(Some(allocation)) }
            .map_err(|error| Error::Clipboard(error.to_string()))?;
        return Err(Error::Clipboard(
            "Windows could not allocate clipboard text".into(),
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(units.as_ptr(), pointer.cast(), units.len());
    }
    let mut transferred = false;
    let result = (|| {
        unlock(allocation)?;
        let mut clipboard = Clipboard::open(owner)?;
        unsafe { EmptyClipboard() }.map_err(|error| Error::Clipboard(error.to_string()))?;
        unsafe { SetClipboardData(UNICODE_TEXT, Some(HANDLE(allocation.0))) }
            .map_err(|error| Error::Clipboard(error.to_string()))?;
        transferred = true;
        clipboard.close()?;
        Ok(())
    })();
    if result.is_err() && !transferred {
        unsafe { GlobalFree(Some(allocation)) }
            .map_err(|error| Error::Clipboard(error.to_string()))?;
    }
    result
}

#[derive(Default)]
pub struct Watcher {
    sent: Option<u32>,
    remote: Option<u32>,
}

impl Watcher {
    pub fn paste(&self, owner: isize) -> Result<Option<(u32, Zeroizing<String>)>> {
        let sequence = sequence();
        if self.remote == Some(sequence) || self.sent == Some(sequence) {
            return Ok(None);
        }
        read_snapshot(owner)?.map(Some).ok_or_else(|| {
            Error::Clipboard(
                "Only plain-text clipboard sharing is supported; nothing was pasted".into(),
            )
        })
    }

    pub fn sent(&mut self, sequence: u32) {
        self.sent = Some(sequence);
    }

    pub fn receive(&mut self, owner: isize, text: &str) -> Result<()> {
        write(owner, text)?;
        let sequence = sequence();
        self.remote = Some(sequence);
        Ok(())
    }
}
