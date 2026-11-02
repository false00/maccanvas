use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};

use windows::{
    Win32::{
        Foundation::{ERROR_CANCELLED, HWND},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{
            FOS_DONTADDTORECENT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR,
            FOS_NOTESTFILECREATE, FOS_PATHMUSTEXIST, FileOpenDialog, FileSaveDialog, IFileDialog,
            SIGDN_FILESYSPATH,
        },
    },
    core::HSTRING,
};

use crate::error::{Error, Result};

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

pub fn pick(owner: isize) -> Result<Option<PathBuf>> {
    show(owner, None)
}
pub fn save(owner: isize, name: &str) -> Result<Option<PathBuf>> {
    show(owner, Some(name))
}

fn show(owner: isize, name: Option<&str>) -> Result<Option<PathBuf>> {
    let failed = |error| Error::Transfer(format!("Windows file dialog: {error}"));
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(failed)?;
    let _apartment = Apartment;
    let class = if name.is_some() {
        FileSaveDialog
    } else {
        FileOpenDialog
    };
    let dialog: IFileDialog =
        unsafe { CoCreateInstance(&class, None, CLSCTX_INPROC_SERVER) }.map_err(failed)?;
    let mut options =
        FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_NOCHANGEDIR | FOS_DONTADDTORECENT;
    if name.is_some() {
        options |= FOS_NOTESTFILECREATE;
    } else {
        options |= FOS_FILEMUSTEXIST;
    }
    unsafe { dialog.SetOptions(options) }.map_err(failed)?;
    unsafe {
        dialog.SetTitle(&HSTRING::from(if name.is_some() {
            "Receive a new file - existing files are not replaced"
        } else {
            "Send a file to this Mac folder"
        }))
    }
    .map_err(failed)?;
    if let Some(name) = name {
        unsafe { dialog.SetFileName(&HSTRING::from(name)) }.map_err(failed)?;
    }
    match unsafe { dialog.Show(Some(HWND(owner as *mut std::ffi::c_void))) } {
        Ok(()) => (),
        Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => return Ok(None),
        Err(error) => return Err(failed(error)),
    }
    let item = unsafe { dialog.GetResult() }.map_err(failed)?;
    let path = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(failed)?;
    let text = unsafe { std::slice::from_raw_parts(path.0, path.len()) };
    let result = PathBuf::from(OsString::from_wide(text));
    unsafe {
        CoTaskMemFree(Some(path.0.cast()));
    }
    Ok(Some(result))
}
