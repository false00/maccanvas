use std::{error, fmt};

/// A rejected input, native decoding failure, or resource limit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    InvalidInput(&'static str),
    InputTooLarge { actual: usize, maximum: usize },
    InvalidDimensions { width: u32, height: u32 },
    UnsupportedBitDepth { luma: u32, chroma: u32 },
    UnsupportedChroma(u32),
    Native { code: i32, message: String },
    AllocationFailed,
    ResourceLimit(&'static str),
    IncompleteStream { submitted: u64, decoded: u64 },
    Flushed,
    Poisoned,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(f, "invalid Annex-B HEVC input: {reason}"),
            Self::InputTooLarge { actual, maximum } => {
                write!(f, "HEVC input is {actual} bytes; maximum is {maximum}")
            }
            Self::InvalidDimensions { width, height } => write!(
                f,
                "invalid HEVC dimensions {width}x{height}; maximum dimension is 8192 and maximum pixel count is 16777216"
            ),
            Self::UnsupportedBitDepth { luma, chroma } => {
                write!(
                    f,
                    "unsupported HEVC bit depth {luma}/{chroma}; expected 8/8"
                )
            }
            Self::UnsupportedChroma(format) => {
                write!(
                    f,
                    "unsupported HEVC chroma format {format}; expected 4:2:0 or 4:4:4"
                )
            }
            Self::Native { code, message } => write!(f, "libde265 error {code}: {message}"),
            Self::AllocationFailed => f.write_str("unable to allocate HEVC output memory"),
            Self::ResourceLimit(reason) => write!(f, "HEVC resource limit: {reason}"),
            Self::IncompleteStream { submitted, decoded } => write!(
                f,
                "incomplete HEVC stream: submitted {submitted} pictures but decoded {decoded}"
            ),
            Self::Flushed => f.write_str("HEVC decoder has been flushed; create a new decoder"),
            Self::Poisoned => f.write_str("HEVC decoder previously failed; create a new decoder"),
        }
    }
}

impl error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
