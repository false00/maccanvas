use windows::{
    Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    core::{HSTRING, PCWSTR, w},
};

use crate::error::{Error, Result};

pub fn open_about_link(index: i32) -> Result<()> {
    let link = match index {
        0 => crate::AUTHOR_GITHUB,
        1 => crate::REPOSITORY,
        2 => "https://github.com/false00/maccanvas/releases/latest",
        3 => "https://curl.red",
        4 => "https://github.com/false00/maccanvas/blob/main/LICENSE",
        _ => return Err(Error::Configuration("Unknown About link".into())),
    };
    open(&HSTRING::from(link))
}

pub fn open_directory(path: &std::path::Path) -> Result<()> {
    if !path.is_dir() {
        return Err(Error::Configuration(
            "The requested log directory does not exist".into(),
        ));
    }
    open(&HSTRING::from(path.as_os_str()))
}

fn open(target: &HSTRING) -> Result<()> {
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            target,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = result.0 as isize;
    if code <= 32 {
        return Err(Error::Configuration(format!(
            "Windows could not open the link or folder (Shell error {code})"
        )));
    }
    Ok(())
}
