use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
        UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        UI::WindowsAndMessaging::{
            SPI_GETCLIENTAREAANIMATION, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
            SystemParametersInfoW,
        },
    },
    core::{BOOL, w},
};

use crate::error::{Error, Result};

pub fn system_dark() -> Result<bool> {
    let mut value = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    match result {
        ERROR_SUCCESS if size == 4 && value <= 1 => Ok(value == 0),
        // A new Windows account without a preference uses the light app theme.
        ERROR_FILE_NOT_FOUND => Ok(false),
        _ => Err(Error::Configuration(format!(
            "Cannot read Windows app theme: {}",
            result.0
        ))),
    }
}

pub fn motion_enabled() -> Result<bool> {
    let mut enabled = BOOL::default();
    unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut enabled as *mut BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .map_err(|error| {
        Error::Configuration(format!("Cannot read Windows animation preference: {error}"))
    })?;
    Ok(enabled.as_bool())
}

pub fn glass_enabled() -> Result<bool> {
    let mut contrast = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            Some((&mut contrast as *mut HIGHCONTRASTW).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .map_err(|error| {
        Error::Configuration(format!("Cannot read Windows contrast preference: {error}"))
    })?;
    if contrast.dwFlags.contains(HCF_HIGHCONTRASTON) {
        return Ok(false);
    }
    let mut value = 1u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("EnableTransparency"),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    match result {
        ERROR_SUCCESS if size == 4 && value <= 1 => Ok(value == 1),
        ERROR_FILE_NOT_FOUND => Ok(true),
        _ => Err(Error::Configuration(format!(
            "Cannot read Windows transparency preference: {}",
            result.0
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_current_windows_preferences_without_changing_them() {
        system_dark().unwrap();
        motion_enabled().unwrap();
        glass_enabled().unwrap();
    }
}
