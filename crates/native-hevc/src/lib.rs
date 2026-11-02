//! Statically linked, 8-bit HEVC decoding for Windows x64 with the static MSVC CRT.
//!
//! [`Decoder::push`] accepts complete Annex-B access units (one or more complete
//! pictures, or just parameter sets). Fragmented NALs/pictures must be assembled
//! by the caller. Every picture in a push receives that push's `pts` unchanged.
//! Output is display-ordered RGBA8, with alpha 255 and a BT.709 matrix. Full-range
//! input is used directly; limited-range VUI input is expanded to full range.
//! Chroma is nearest-neighbor upsampled for 4:2:0; 4:4:4 is not subsampled.
//! Full-range 4:4:4 uses mandatory x64 SSE2 with at most one RGB level of rounding
//! difference from scalar BT.709; limited-range and subsampled formats stay scalar.
//!
//! Delayed pictures may remain pending until subsequent pushes or [`Decoder::flush`].
//! A fatal native failure poisons the decoder, rather than returning an empty
//! successful result. Instantiate a new decoder to recover or start a new stream.

mod annex_b;
mod color;
mod error;
mod ffi;

#[cfg(test)]
mod perf_tests;

use std::{
    ffi::{CStr, c_void},
    ptr::NonNull,
};

pub use error::{Error, Result};

pub const MAX_DIMENSION: u32 = 8192;
pub const MAX_PIXELS: u64 = 16_777_216;
pub const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
const MAX_OUTPUT_FRAMES: usize = 256;

/// An owned, tightly packed, display-oriented RGBA8 image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedFrame {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A single HEVC stream, supporting 8-bit 4:2:0 and RExt 4:4:4.
///
/// The decoder is movable between threads, but cannot be used concurrently.
/// Each push is bounded to 64 MiB/4096 NALs, and each call's output to
/// 256 MiB/256 frames. Coded and visible images must have nonzero dimensions,
/// at most 8192 samples per dimension, and at most 16,777,216 pixels.
pub struct Decoder {
    native: NonNull<c_void>,
    has_sps: bool,
    submitted: u64,
    decoded: u64,
    flushed: bool,
    poisoned: bool,
}

// The native context has exclusive ownership, no thread-affine resources, and
// synchronized global initialization. NonNull intentionally keeps Decoder !Sync.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self> {
        let mut code = 0;
        // The bridge catches C++ exceptions; a null result has no owned context.
        let native = unsafe { ffi::nh_new(&mut code) };
        let native = NonNull::new(native).ok_or_else(|| native_error(code))?;
        Ok(Self {
            native,
            has_sps: false,
            submitted: 0,
            decoded: 0,
            flushed: false,
            poisoned: false,
        })
    }

    /// Decode complete Annex-B access units in decoding order.
    ///
    /// Validation errors leave the decoder usable; native/conversion/resource
    /// failures after input has been accepted poison it. Empty input is invalid.
    /// Parameter-set-only input and legitimately delayed frames may yield no output.
    pub fn push(&mut self, annex_b: &[u8], pts: i64) -> Result<Vec<DecodedFrame>> {
        self.ensure_usable()?;
        if self.flushed {
            return Err(Error::Flushed);
        }
        let batch = annex_b::validate(annex_b, self.has_sps)?;
        let submitted = self
            .submitted
            .checked_add(batch.pictures)
            .ok_or(Error::ResourceLimit("picture count overflow"))?;
        self.has_sps = batch.has_sps;
        self.submitted = submitted;

        let result = (|| {
            for nal in batch.nals {
                // Input is bounded below i32::MAX and remains borrowed for this call.
                check_native(unsafe {
                    ffi::nh_push(self.native.as_ptr(), nal.as_ptr(), nal.len() as i32, pts)
                })?;
            }
            check_native(unsafe { ffi::nh_end_frame(self.native.as_ptr()) })?;
            self.drain(false)
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    /// Signal end of stream and return all delayed pictures.
    ///
    /// A successful flush is terminal; repeated flushes return an empty vector.
    /// Missing/truncated pictures are errors, never silently dropped.
    pub fn flush(&mut self) -> Result<Vec<DecodedFrame>> {
        self.ensure_usable()?;
        if self.flushed {
            return Ok(Vec::new());
        }
        let result = (|| {
            check_native(unsafe { ffi::nh_flush(self.native.as_ptr()) })?;
            let frames = self.drain(true)?;
            if self.submitted != self.decoded {
                return Err(Error::IncompleteStream {
                    submitted: self.submitted,
                    decoded: self.decoded,
                });
            }
            self.flushed = true;
            Ok(frames)
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn ensure_usable(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    fn drain(&mut self, flushing: bool) -> Result<Vec<DecodedFrame>> {
        let mut frames = Vec::new();
        let mut output_bytes = 0_usize;
        for _ in 0..(annex_b::MAX_NALS * 4 + 512) {
            let mut more = 0;
            let code = unsafe { ffi::nh_decode(self.native.as_ptr(), &mut more) };
            // libde265 sometimes reports damaged pictures through its warning
            // queue instead of its return code. Reject both before releasing pixels.
            check_native(unsafe { ffi::nh_warning(self.native.as_ptr()) })?;
            if !matches!(code, 0 | 9 | 13) {
                return Err(native_error(code));
            }
            let previous = frames.len();
            loop {
                let image = unsafe { ffi::nh_picture(self.native.as_ptr()) };
                let Some(image) = (unsafe { image.as_ref() }) else {
                    break;
                };
                // The native picture remains valid until release; copy only its
                // visible row spans, not stride*height from a cropped plane origin.
                let frame = color::copy_frame(image);
                unsafe { ffi::nh_release(self.native.as_ptr()) };
                let frame = frame?;
                output_bytes = output_bytes
                    .checked_add(frame.rgba.len())
                    .filter(|&bytes| bytes <= MAX_OUTPUT_BYTES)
                    .ok_or(Error::ResourceLimit("output exceeds 256 MiB in one call"))?;
                if frames.len() == MAX_OUTPUT_FRAMES {
                    return Err(Error::ResourceLimit(
                        "more than 256 output frames in one call",
                    ));
                }
                frames.try_reserve(1).map_err(|_| Error::AllocationFailed)?;
                frames.push(frame);
                self.decoded += 1;
            }
            match code {
                13 if flushing => {
                    return Err(Error::ResourceLimit(
                        "native decoder stalled while flushing",
                    ));
                }
                13 => return Ok(frames),
                9 if frames.len() == previous => {
                    return Err(Error::ResourceLimit(
                        "native picture buffer is full without output",
                    ));
                }
                0 if more == 0 => return Ok(frames),
                _ => {}
            }
        }
        Err(Error::ResourceLimit(
            "native decoder made no bounded progress",
        ))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // Context cleanup is infallible for an owned, initialized decoder. There
        // is no decoding in Drop and no external/native worker threads to detach.
        unsafe { ffi::nh_free(self.native.as_ptr()) };
    }
}

fn check_native(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(native_error(code))
    }
}

fn native_error(code: i32) -> Error {
    let message = if code == -1 {
        "native image allocation exceeds dimension/pixel limits".to_owned()
    } else {
        let text = unsafe { ffi::de265_get_error_text(code) };
        if text.is_null() {
            "unknown native error".to_owned()
        } else {
            unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned()
        }
    };
    Error::Native { code, message }
}

fn validate_dimensions(width: u32, height: u32) -> Result<()> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        Err(Error::InvalidDimensions { width, height })
    } else {
        Ok(())
    }
}
