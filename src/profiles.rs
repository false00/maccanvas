use std::{collections::HashSet, fs, io::Write, path::Path};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Error, Result};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub profiles: Vec<Profile>,
    pub ctrl_as_command: bool,
    pub hello_for_all_connections: bool,
    pub dark_theme: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            profiles: Vec::new(),
            ctrl_as_command: true,
            hello_for_all_connections: false,
            dark_theme: false,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 || self.profiles.len() > 128 {
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
        settings.dark_theme = true;
        settings.save(&path).unwrap();
        assert!(Settings::load(&path).unwrap().dark_theme);
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
}
