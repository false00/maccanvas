use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::{Error, Result},
    streaming::StreamSettings,
};

pub fn default_directory() -> Result<PathBuf> {
    dirs::data_local_dir()
        .map(|directory| directory.join("MacCanvas"))
        .ok_or_else(|| {
            Error::Configuration("Windows did not return a local app-data folder".into())
        })
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    #[default]
    MacLogin,
    VncPassword,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: Uuid,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub authentication: AuthMode,
    pub has_saved_credentials: bool,
}

impl Profile {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() || self.name.len() > 100 || self.name.chars().any(char::is_control)
        {
            return Err(Error::Configuration(
                "Machine name must be 1-100 characters".into(),
            ));
        }
        if self.host.is_empty()
            || self.host.len() > 253
            || self
                .host
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "/\\@".contains(c))
            || (self.host.contains(':') && self.host.parse::<std::net::Ipv6Addr>().is_err())
            || self.port == 0
        {
            return Err(Error::Configuration(
                "Enter a host/IP and a separate port (1-65535)".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshHostKey {
    pub host: String,
    pub port: u16,
    pub algorithm: String,
    pub sha256: String,
}

impl SshHostKey {
    pub fn validate(&self) -> Result<()> {
        Profile {
            id: Uuid::nil(),
            name: "SSH host".into(),
            host: self.host.clone(),
            port: self.port,
            authentication: AuthMode::MacLogin,
            has_saved_credentials: false,
        }
        .validate()?;
        if self.algorithm.is_empty()
            || self.algorithm.len() > 100
            || !self
                .algorithm
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-@.".contains(&byte))
            || !self.sha256.strip_prefix("SHA256:").is_some_and(|value| {
                value.len() == 43
                    && value
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"+/".contains(&byte))
            })
        {
            return Err(Error::Configuration(
                "Invalid SSH host-key fingerprint".into(),
            ));
        }
        Ok(())
    }

    pub fn endpoint(&self, host: &str, port: u16) -> bool {
        self.port == port && self.host.eq_ignore_ascii_case(host)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub profiles: Vec<Profile>,
    pub ctrl_as_command: bool,
    #[serde(default, rename = "hello_for_all_connections", skip_serializing)]
    pub legacy_hello_for_all: Option<bool>,
    #[serde(default, rename = "dark_theme", skip_serializing)]
    pub legacy_dark_theme: Option<bool>,
    #[serde(default = "enabled")]
    pub sidebar_open: bool,
    #[serde(default = "enabled")]
    pub show_fps: bool,
    #[serde(default)]
    pub streaming: StreamSettings,
    #[serde(default)]
    pub audio: crate::audio::Settings,
    #[serde(default)]
    pub clipboard_enabled: bool,
    #[serde(default)]
    pub ssh_host_keys: Vec<SshHostKey>,
}

fn enabled() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            profiles: Vec::new(),
            ctrl_as_command: true,
            legacy_hello_for_all: None,
            legacy_dark_theme: None,
            sidebar_open: true,
            show_fps: true,
            streaming: StreamSettings::default(),
            audio: crate::audio::Settings::default(),
            clipboard_enabled: false,
            ssh_host_keys: Vec::new(),
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        self.streaming.validate()?;
        self.audio.validate()?;
        if self.version != 1 || self.profiles.len() > 128 || self.ssh_host_keys.len() > 128 {
            return Err(Error::Configuration(
                "Unsupported settings version or too many machines".into(),
            ));
        }
        let mut identifiers = HashSet::new();
        for profile in &self.profiles {
            profile.validate()?;
            if !identifiers.insert(profile.id) {
                return Err(Error::Configuration("Duplicate machine identifier".into()));
            }
        }
        let mut endpoints = HashSet::new();
        for key in &self.ssh_host_keys {
            key.validate()?;
            if !endpoints.insert((key.host.to_ascii_lowercase(), key.port)) {
                return Err(Error::Configuration(
                    "Duplicate SSH host-key endpoint".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = match fs::read(path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error.into()),
        };
        if data.len() > 256 * 1024 {
            return Err(Error::Configuration("Settings file is too large".into()));
        }
        let settings: Self = serde_json::from_slice(&data)
            .map_err(|error| Error::Configuration(error.to_string()))?;
        settings.validate()?;
        Ok(settings)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let directory = path
            .parent()
            .ok_or_else(|| Error::Configuration("Missing settings directory".into()))?;
        fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|error| Error::Configuration(error.to_string()))?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_keys_are_validated_even_when_no_machines_are_saved() {
        let settings = Settings {
            ssh_host_keys: vec![SshHostKey {
                host: "mac.local".into(),
                port: 22,
                algorithm: "ssh-ed25519".into(),
                sha256: "not-a-fingerprint".into(),
            }],
            ..Settings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn settings_never_serialize_credentials() {
        let settings = Settings {
            profiles: vec![Profile {
                id: Uuid::new_v4(),
                name: "Mac mini".into(),
                host: "100.64.0.10".into(),
                port: 5900,
                authentication: AuthMode::MacLogin,
                has_saved_credentials: true,
            }],
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("password"));
        assert!(!json.contains("username"));
    }

    #[test]
    fn settings_reject_accidental_secret_fields() {
        let json = r#"{"version":1,"profiles":[],"ctrl_as_command":true,"hello_for_all_connections":false,"dark_theme":false,"password":"never-save-this"}"#;
        assert!(serde_json::from_str::<Settings>(json).is_err());
    }

    #[test]
    fn atomic_save_and_reload_preserves_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut settings = Settings::default();
        settings.save(&path).unwrap();
        settings.show_fps = false;
        settings.save(&path).unwrap();
        assert!(!Settings::load(&path).unwrap().show_fps);
    }

    #[test]
    fn fresh_settings_use_hevc60_and_explicit_existing_choices_are_preserved() {
        use crate::streaming::Encoding;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let fresh = Settings::load(&path).unwrap();
        assert_eq!(fresh.streaming.encoding, Encoding::Hevc60);
        assert_eq!(fresh.streaming.max_fps, 60);
        for (encoding, fps) in [
            (Encoding::Auto, 60),
            (Encoding::Zrle, 60),
            (Encoding::Hevc30, 30),
            (Encoding::Hevc60, 60),
        ] {
            let mut existing = fresh.clone();
            existing.streaming.encoding = encoding;
            existing.streaming.max_fps = fps;
            existing.save(&path).unwrap();
            assert_eq!(Settings::load(&path).unwrap().streaming, existing.streaming);
        }
    }

    #[test]
    fn custom_hevc_bitrate_survives_saving_and_reloading_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut settings = Settings::default();
        for bitrate in [1, 125_500_000, u32::MAX, 0] {
            settings.streaming.hevc_bitrate_bps = bitrate;
            settings.save(&path).unwrap();
            assert_eq!(Settings::load(&path).unwrap().streaming, settings.streaming);
        }
    }

    #[test]
    fn rejects_credentials_in_host_urls() {
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "Mac".into(),
            host: "user:secret@example.test".into(),
            port: 5900,
            authentication: AuthMode::MacLogin,
            has_saved_credentials: false,
        };
        assert!(profile.validate().is_err());
    }

    #[test]
    fn migrates_old_theme_and_hello_preferences_without_saving_toggles() {
        let json = r#"{"version":1,"profiles":[],"ctrl_as_command":true,"hello_for_all_connections":false,"dark_theme":true}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        settings.validate().unwrap();
        let saved = serde_json::to_string(&settings).unwrap();
        assert!(!saved.contains("hello_for_all"));
        assert!(!saved.contains("dark_theme"));
        assert_eq!(settings.streaming, StreamSettings::default());
        assert!(settings.show_fps);
    }
}
