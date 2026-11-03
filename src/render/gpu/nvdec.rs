use std::{
    ffi::{CStr, c_char, c_void},
    ptr::NonNull,
    sync::Arc,
};

use windows::{
    Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
    core::Interface,
};

use crate::{
    error::{Error, Result},
    geometry::Extent,
};

unsafe extern "C" {
    fn nhg_new(adapter: u32, error: *mut c_char, capacity: usize) -> *mut c_void;
    fn nhg_free(decoder: *mut c_void);
    fn nhg_reset(decoder: *mut c_void, error: *mut c_char, capacity: usize) -> i32;
    fn nhg_push(
        decoder: *mut c_void,
        data: *const u8,
        length: usize,
        timestamp: i64,
        drain: i32,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn nhg_take(decoder: *mut c_void) -> *mut c_void;
    fn nhg_frame_free(frame: *mut c_void);
    fn nhg_frame_width(frame: *mut c_void) -> u32;
    fn nhg_frame_height(frame: *mut c_void) -> u32;
    fn nhg_frame_texture(frame: *mut c_void) -> *mut c_void;
    fn nhg_frame_device(frame: *mut c_void) -> *mut c_void;
    fn nhg_frame_read(
        frame: *mut c_void,
        bytes: *mut u8,
        length: usize,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
}

fn failure(error: &[c_char; 512]) -> Error {
    Error::Display(format!(
        "NVDEC: {}",
        unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
    ))
}

pub struct Decoder {
    native: NonNull<c_void>,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(adapter: u32) -> Result<Self> {
        let mut error = [0; 512];
        let native = unsafe { nhg_new(adapter, error.as_mut_ptr(), error.len()) };
        Ok(Self {
            native: NonNull::new(native).ok_or_else(|| failure(&error))?,
        })
    }

    pub fn push(&mut self, data: &[u8], timestamp: i64) -> Result<Vec<Arc<Frame>>> {
        self.decode(data, timestamp, false)
    }

    pub fn drain(&mut self) -> Result<Vec<Arc<Frame>>> {
        self.decode(&[], 0, true)
    }

    pub fn reset(&mut self) -> Result<()> {
        let mut error = [0; 512];
        if unsafe { nhg_reset(self.native.as_ptr(), error.as_mut_ptr(), error.len()) } == 0 {
            return Err(failure(&error));
        }
        Ok(())
    }

    fn decode(&mut self, data: &[u8], timestamp: i64, drain: bool) -> Result<Vec<Arc<Frame>>> {
        let mut error = [0; 512];
        if unsafe {
            nhg_push(
                self.native.as_ptr(),
                data.as_ptr(),
                data.len(),
                timestamp,
                i32::from(drain),
                error.as_mut_ptr(),
                error.len(),
            )
        } == 0
        {
            return Err(failure(&error));
        }
        let mut frames = Vec::new();
        while let Some(native) = NonNull::new(unsafe { nhg_take(self.native.as_ptr()) }) {
            let size = Extent::new(unsafe { nhg_frame_width(native.as_ptr()) }, unsafe {
                nhg_frame_height(native.as_ptr())
            })?;
            frames.push(Arc::new(Frame { native, size }));
        }
        Ok(frames)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { nhg_free(self.native.as_ptr()) }
    }
}

pub struct Frame {
    native: NonNull<c_void>,
    pub size: Extent,
}

unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}

impl Frame {
    pub fn texture(&self) -> ID3D11Texture2D {
        unsafe { ID3D11Texture2D::from_raw(nhg_frame_texture(self.native.as_ptr())) }
    }

    pub fn device(&self) -> ID3D11Device {
        unsafe { ID3D11Device::from_raw(nhg_frame_device(self.native.as_ptr())) }
    }

    pub fn read_rgba(&self) -> Result<Vec<u8>> {
        let mut rgba = vec![0; self.size.bytes()];
        let mut error = [0; 512];
        if unsafe {
            nhg_frame_read(
                self.native.as_ptr(),
                rgba.as_mut_ptr(),
                rgba.len(),
                error.as_mut_ptr(),
                error.len(),
            )
        } == 0
        {
            return Err(failure(&error));
        }
        Ok(rgba)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { nhg_frame_free(self.native.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "manual NVIDIA unsupported-format fallback check"]
    fn unsupported_chroma_is_rejected_for_software_fallback() {
        let adapter = super::super::adapters()
            .unwrap()
            .into_iter()
            .find(|adapter| adapter.vendor_id == 0x10de)
            .expect("NVIDIA adapter");
        let mut decoder = super::Decoder::new(adapter.index).unwrap();
        let sample = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/native-hevc/tests/data/pattern420.hevc"
        ));
        let error = match decoder.push(sample, 0) {
            Ok(_) => panic!("Unsupported chroma must not silently enter the 4:4:4 shader path"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("software fallback"));
    }

    #[test]
    #[ignore = "manual NVIDIA GPU decode and pixel-conformance check"]
    fn decodes_real_444_into_a_gpu_texture() {
        let adapter = super::super::adapters()
            .unwrap()
            .into_iter()
            .find(|adapter| adapter.vendor_id == 0x10de)
            .expect("NVIDIA adapter");
        let mut decoder = super::Decoder::new(adapter.index).unwrap();
        let sample = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/native-hevc/tests/data/quadrants444_4k.hevc"
        ));
        let mut frames = decoder.push(sample, 17).unwrap();
        assert_eq!(
            frames.len(),
            1,
            "A complete picture must not wait for another frame or EOS"
        );
        frames.extend(decoder.drain().unwrap());
        assert_eq!(frames.len(), 1);
        assert_eq!((frames[0].size.width, frames[0].size.height), (3840, 2160));
        let rgba = frames[0].read_rgba().unwrap();
        let mut software = native_hevc::Decoder::new().unwrap();
        let reference = software.push(sample, 17).unwrap().remove(0);
        for (x, y) in [(100, 100), (3000, 100), (100, 1800), (3000, 1800)] {
            let offset = (y * 3840 + x) * 4;
            for channel in 0..4 {
                assert!(
                    rgba[offset + channel].abs_diff(reference.rgba[offset + channel]) <= 1,
                    "GPU pixel mismatch at {x},{y}, channel {channel}: {} vs {}",
                    rgba[offset + channel],
                    reference.rgba[offset + channel]
                );
            }
        }
        decoder.reset().unwrap();
        let replacement = decoder.push(sample, 18).unwrap();
        assert_eq!(replacement.len(), 1);
        assert_eq!(
            frames[0].read_rgba().unwrap(),
            rgba,
            "A held presentation texture must survive decoder recovery"
        );
        let mut elapsed = Vec::new();
        for timestamp in 19..32 {
            let started = std::time::Instant::now();
            let decoded = decoder.push(sample, timestamp).unwrap();
            assert_eq!(decoded.len(), 1);
            std::hint::black_box(decoded[0].read_rgba().unwrap());
            elapsed.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        elapsed.sort_by(f64::total_cmp);
        println!(
            "4K NVDEC decode + GPU color conversion + validation readback median: {:.3} ms",
            elapsed[elapsed.len() / 2]
        );
        println!(
            "NVDEC decoded HEVC 4:4:4 into a 3840x2160 GPU RGBA texture; fixture pixels match software"
        );
    }
}
