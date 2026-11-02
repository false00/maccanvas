pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod auth;
pub mod credentials;
pub mod error;
pub mod framebuffer;
pub mod geometry;
pub mod input;
pub mod profiles;
pub mod protocol;
pub mod render;
pub mod session;
pub mod streaming;
pub mod telemetry;
pub mod theme;
