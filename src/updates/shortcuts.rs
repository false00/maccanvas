use std::{
    ffi::OsString,
    fs,
    marker::PhantomData,
    os::windows::ffi::OsStringExt,
    path::{Component, Path, PathBuf, Prefix},
    rc::Rc,
};

use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize, IPersistFile, STGM_READ,
        },
        UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, KF_FLAG_DEFAULT,
            SHGetKnownFolderPath, SLGP_RAWPATH, ShellLink,
        },
    },
    core::{HSTRING, Interface},
};

use crate::error::Result;

use super::failed;

const DESCRIPTION: &str = "Windows VNC viewer for macOS";

pub(super) fn paths() -> Result<[PathBuf; 2]> {
    let mut paths = Vec::new();
    for folder in [FOLDERID_Programs, FOLDERID_Desktop] {
        let value =
            unsafe { SHGetKnownFolderPath(&folder, KF_FLAG_DEFAULT, None) }.map_err(failed)?;
        let path = PathBuf::from(OsString::from_wide(unsafe {
            std::slice::from_raw_parts(value.0, value.len())
        }));
        unsafe { CoTaskMemFree(Some(value.0.cast())) };
        if !path.is_absolute() {
            return Err(failed("Windows could not locate a shortcut folder"));
        }
        paths.push(path.join("MacCanvas.lnk"));
    }
    Ok([paths.remove(0), paths.remove(0)])
}

pub(super) struct Links(PhantomData<Rc<()>>);

impl Drop for Links {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

impl Links {
    pub(super) fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(failed)?;
        Ok(Self(PhantomData))
    }

    pub(super) fn owns_any(&self, executable: &Path, paths: &[PathBuf]) -> Result<bool> {
        for path in paths {
            if let Some(link) = load(path)?
                && same_path(&target(&link)?, executable)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(super) fn repair(&self, executable: &Path, paths: &[PathBuf]) -> Result<usize> {
        let directory = executable
            .parent()
            .ok_or_else(|| failed("Application has no parent directory"))?;
        let mut links = Vec::new();
        for path in paths {
            let link = load(path)?;
            if let Some(link) = &link
                && !same_path(&target(link)?, executable)?
            {
                return Err(failed(format!(
                    "Existing MacCanvas shortcut points elsewhere; it was not overwritten: {}",
                    path.display()
                )));
            }
            links.push(link);
        }
        let mut changed = 0;
        for (path, link) in paths.iter().zip(links) {
            if let Some(link) = &link
                && current(link, executable, directory)?
            {
                continue;
            }
            let link = match link {
                Some(link) => link,
                None => new_link()?,
            };
            fs::create_dir_all(
                path.parent()
                    .ok_or_else(|| failed("Shortcut has no parent directory"))?,
            )?;
            unsafe {
                link.SetPath(&HSTRING::from(executable.as_os_str()))
                    .map_err(failed)?;
                link.SetWorkingDirectory(&HSTRING::from(directory.as_os_str()))
                    .map_err(failed)?;
                link.SetArguments(&HSTRING::new()).map_err(failed)?;
                link.SetIconLocation(&HSTRING::from(executable.as_os_str()), 0)
                    .map_err(failed)?;
                link.SetDescription(&HSTRING::from(DESCRIPTION))
                    .map_err(failed)?;
                link.cast::<IPersistFile>()
                    .map_err(failed)?
                    .Save(&HSTRING::from(path.as_os_str()), true)
                    .map_err(failed)?;
            }
            changed += 1;
        }
        Ok(changed)
    }
}

fn new_link() -> Result<IShellLinkW> {
    unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }.map_err(failed)
}

fn load(path: &Path) -> Result<Option<IShellLinkW>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let link = new_link()?;
    let file: IPersistFile = link.cast().map_err(failed)?;
    unsafe { file.Load(&HSTRING::from(path.as_os_str()), STGM_READ) }.map_err(failed)?;
    Ok(Some(link))
}

fn text(read: impl FnOnce(&mut [u16]) -> windows::core::Result<()>) -> Result<OsString> {
    let mut buffer = vec![0; 32768];
    read(&mut buffer).map_err(failed)?;
    let end = buffer
        .iter()
        .position(|value| *value == 0)
        .ok_or_else(|| failed("Windows returned an oversized shortcut property"))?;
    Ok(OsString::from_wide(&buffer[..end]))
}

fn target(link: &IShellLinkW) -> Result<PathBuf> {
    text(|buffer| unsafe { link.GetPath(buffer, std::ptr::null_mut(), SLGP_RAWPATH.0 as u32) })
        .map(PathBuf::from)
}

fn same_path(left: &Path, right: &Path) -> Result<bool> {
    let equal = |left: &Path, right: &Path| {
        left.as_os_str()
            .as_encoded_bytes()
            .eq_ignore_ascii_case(right.as_os_str().as_encoded_bytes())
    };
    if equal(left, right) {
        return Ok(true);
    }
    let drive = |path: &Path| match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(disk) | Prefix::VerbatimDisk(disk) => Some(disk.to_ascii_uppercase()),
            _ => None,
        },
        _ => None,
    };
    if drive(left).is_none() || drive(left) != drive(right) {
        return Ok(false);
    }
    // Shell links expand DOS 8.3 aliases and may omit the verbatim path prefix.
    let canonical = |path| match fs::canonicalize(path) {
        Ok(path) => Ok(Some(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(crate::error::Error::from(error)),
    };
    Ok(match (canonical(left)?, canonical(right)?) {
        (Some(left), Some(right)) => equal(&left, &right),
        _ => false,
    })
}

fn current(link: &IShellLinkW, executable: &Path, directory: &Path) -> Result<bool> {
    let working = text(|buffer| unsafe { link.GetWorkingDirectory(buffer) })?;
    let arguments = text(|buffer| unsafe { link.GetArguments(buffer) })?;
    let description = text(|buffer| unsafe { link.GetDescription(buffer) })?;
    let mut index = -1;
    let icon = text(|buffer| unsafe { link.GetIconLocation(buffer, &mut index) })?;
    Ok(same_path(Path::new(&working), directory)?
        && arguments.is_empty()
        && description == DESCRIPTION
        && same_path(Path::new(&icon), executable)?
        && index == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repairs_native_unicode_shortcuts_only_when_needed() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("MacCanvas \u{00e9}.exe");
        fs::write(&executable, b"not executed").unwrap();
        assert!(same_path(&executable, &fs::canonicalize(&executable).unwrap()).unwrap());
        assert!(!same_path(&root.path().join("missing.exe"), &executable).unwrap());
        assert!(
            !same_path(
                Path::new("\\\\maccanvas-invalid.example\\share\\MacCanvas.exe"),
                &executable
            )
            .unwrap()
        );
        let paths = [
            root.path().join("Start menu").join("MacCanvas.lnk"),
            root.path().join("Desktop \u{00e9}").join("MacCanvas.lnk"),
        ];
        let links = Links::new().unwrap();
        assert_eq!(links.repair(&executable, &paths).unwrap(), 2);
        for path in &paths {
            let link = load(path).unwrap().unwrap();
            assert!(same_path(&target(&link).unwrap(), &executable).unwrap());
            assert!(current(&link, &executable, root.path()).unwrap());
        }
        let before = fs::read(&paths[0]).unwrap();
        assert_eq!(links.repair(&executable, &paths).unwrap(), 0);
        assert_eq!(before, fs::read(&paths[0]).unwrap());
        let link = load(&paths[0]).unwrap().unwrap();
        unsafe {
            link.SetArguments(&HSTRING::from("--help")).unwrap();
            link.SetWorkingDirectory(&HSTRING::from("C:\\")).unwrap();
            link.SetIconLocation(&HSTRING::from("old-icon.exe"), 3)
                .unwrap();
            link.cast::<IPersistFile>()
                .unwrap()
                .Save(&HSTRING::from(paths[0].as_os_str()), true)
                .unwrap();
        }
        fs::remove_file(&paths[1]).unwrap();
        assert_eq!(links.repair(&executable, &paths).unwrap(), 2);
        assert_eq!(links.repair(&executable, &paths).unwrap(), 0);
    }

    #[test]
    fn never_overwrites_another_applications_shortcut() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("MacCanvas.exe");
        let other = root.path().join("Other.exe");
        fs::write(&executable, b"not executed").unwrap();
        fs::write(&other, b"not executed").unwrap();
        let paths = [
            root.path().join("start.lnk"),
            root.path().join("desktop.lnk"),
        ];
        let links = Links::new().unwrap();
        links.repair(&other, &paths[1..]).unwrap();
        let before = fs::read(&paths[1]).unwrap();
        assert!(links.repair(&executable, &paths).is_err());
        assert!(!paths[0].exists());
        assert_eq!(before, fs::read(&paths[1]).unwrap());
    }
}
