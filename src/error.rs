#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Connection error: {0}")]
    Io(#[from] std::io::Error),
    #[error("VNC protocol error: {0}")]
    Protocol(String),
    #[error("{0}")]
    Authentication(String),
    #[error("Windows Credential Manager: {0}")]
    Credential(String),
    #[error("Windows Hello: {0}")]
    Consent(String),
    #[error("Settings: {0}")]
    Configuration(String),
    #[error("Display: {0}")]
    Display(String),
}

pub type Result<T> = std::result::Result<T, Error>;
