#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Connection error: {0}")]
    Io(#[from] std::io::Error),
    #[error("VNC protocol error: {0}")]
    Protocol(String),
    #[error("Connection interrupted: {0}")]
    ConnectionLost(String),
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
    #[error("Audio: {message}")]
    Audio { code: Option<i32>, message: String },
    #[error("Update: {0}")]
    Update(String),
    #[error("Update recovery: {0}")]
    UpdateRecovery(String),
    #[error("Security audit: {0}")]
    Audit(String),
    #[error("Clipboard: {0}")]
    Clipboard(String),
    #[error("File transfer: {0}")]
    Transfer(String),
    #[error("File transfer: {message}")]
    Sftp { code: u32, message: String },
    #[error("Operation cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;
