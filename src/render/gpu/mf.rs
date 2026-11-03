use std::{
    ffi::{CStr, c_char, c_void},
    marker::PhantomData,
    mem::ManuallyDrop,
    ptr::{self, NonNull},
    rc::Rc,
    sync::Arc,
};

use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::{
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11Texture2D,
            },
            Dxgi::{CreateDXGIFactory1, IDXGIFactory1},
        },
        Media::MediaFoundation::*,
        System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
    },
    core::Interface,
};

use crate::{
    error::{Error, Result},
    geometry::Extent,
};

use super::nvdec::Frame;

unsafe extern "C" {
    fn nhg_converter_new(device: *mut c_void, error: *mut c_char, capacity: usize) -> *mut c_void;
    fn nhg_converter_free(converter: *mut c_void);
    fn nhg_converter_frame(
        converter: *mut c_void,
        texture: *mut c_void,
        subresource: u32,
        width: u32,
        height: u32,
        full_range: i32,
        sample: *mut c_void,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
}

fn native(error: windows::core::Error) -> Error {
    Error::Display(format!("D3D11 HEVC: {error}"))
}

fn conversion_error(error: &[c_char; 512]) -> Error {
    Error::Display(format!(
        "D3D11 HEVC: {}",
        unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
    ))
}

struct Runtime {
    _thread: PhantomData<Rc<()>>,
}

impl Runtime {
    fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(native)?;
            if let Err(error) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(native(error));
            }
        }
        Ok(Self {
            _thread: PhantomData,
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}

struct Converter(NonNull<c_void>);

impl Drop for Converter {
    fn drop(&mut self) {
        unsafe { nhg_converter_free(self.0.as_ptr()) }
    }
}

pub struct Decoder {
    transform: IMFTransform,
    activation: IMFActivate,
    converter: Converter,
    _manager: IMFDXGIDeviceManager,
    _device: ID3D11Device,
    size: Extent,
    full_range: bool,
    verified: bool,
    _runtime: Runtime,
}

impl Decoder {
    pub fn new(adapter_index: u32, size: Extent) -> Result<Self> {
        let runtime = Runtime::new()?;
        let (transform, activation, manager, device, converter) = (|| -> Result<_> {
            unsafe {
                let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(native)?;
                let adapter = factory.EnumAdapters1(adapter_index).map_err(native)?;
                let mut device = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
                .map_err(native)?;
                let device =
                    device.ok_or_else(|| Error::Display("D3D11 device was not created".into()))?;
                let mut error = [0; 512];
                let converter = NonNull::new(nhg_converter_new(
                    device.as_raw(),
                    error.as_mut_ptr(),
                    error.len(),
                ))
                .ok_or_else(|| conversion_error(&error))?;
                let converter = Converter(converter);
                let mut token = 0;
                let mut manager = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager).map_err(native)?;
                let manager = manager
                    .ok_or_else(|| Error::Display("DXGI device manager was not created".into()))?;
                manager.ResetDevice(&device, token).map_err(native)?;
                let filter = MFT_REGISTER_TYPE_INFO {
                    guidMajorType: MFMediaType_Video,
                    guidSubtype: MFVideoFormat_HEVC,
                };
                let mut activations = ptr::null_mut();
                let mut count = 0;
                MFTEnumEx(
                    MFT_CATEGORY_VIDEO_DECODER,
                    MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                    Some(&filter),
                    None,
                    &mut activations,
                    &mut count,
                )
                .map_err(native)?;
                if count > 128 {
                    CoTaskMemFree(Some(activations.cast()));
                    return Err(Error::Display(
                        "Too many installed HEVC decoder components".into(),
                    ));
                }
                let mut selected = None;
                if count != 0 {
                    for activation in std::slice::from_raw_parts_mut(activations, count as usize) {
                        if let Some(activation) = activation.take()
                            && selected.is_none()
                        {
                            selected = Some(activation);
                        }
                    }
                }
                CoTaskMemFree(Some(activations.cast()));
                let activation = selected.ok_or_else(|| {
                    Error::Display("The Windows HEVC decoder component is not installed".into())
                })?;
                let transform: IMFTransform = activation.ActivateObject().map_err(native)?;
                let attributes = transform.GetAttributes().map_err(native)?;
                if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                    return Err(Error::Display(
                        "The installed HEVC decoder does not support D3D11 hardware surfaces"
                            .into(),
                    ));
                }
                attributes.SetUINT32(&MF_LOW_LATENCY, 1).map_err(native)?;
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .map_err(native)?;
                let input = MFCreateMediaType().map_err(native)?;
                input
                    .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                    .map_err(native)?;
                input
                    .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)
                    .map_err(native)?;
                input
                    .SetUINT64(
                        &MF_MT_FRAME_SIZE,
                        (u64::from(size.width) << 32) | u64::from(size.height),
                    )
                    .map_err(native)?;
                input
                    .SetUINT64(&MF_MT_FRAME_RATE, (60_u64 << 32) | 1)
                    .map_err(native)?;
                input
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
                    .map_err(native)?;
                input.SetUINT32(&MF_MT_MPEG2_PROFILE, 4).map_err(native)?;
                transform.SetInputType(0, &input, 0).map_err(native)?;
                let output = transform.GetOutputAvailableType(0, 0).map_err(native)?;
                transform.SetOutputType(0, &output, 0).map_err(native)?;
                if let Ok(attributes) = transform.GetOutputStreamAttributes(0) {
                    let _ = attributes.SetUINT32(&MF_SA_MINIMUM_OUTPUT_SAMPLE_COUNT, 6);
                }
                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                    .map_err(native)?;
                transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    .map_err(native)?;
                Ok((transform, activation, manager, device, converter))
            }
        })()?;
        Ok(Self {
            transform,
            activation,
            converter,
            _manager: manager,
            _device: device,
            size,
            full_range: true,
            verified: false,
            _runtime: runtime,
        })
    }

    pub fn push(&mut self, data: &[u8], timestamp: i64) -> Result<Vec<Arc<Frame>>> {
        if data.is_empty() || data.len() > native_hevc::MAX_INPUT_BYTES {
            return Err(Error::Display(
                "D3D11 HEVC input exceeds its size limit".into(),
            ));
        }
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32).map_err(native)?;
            let mut destination = ptr::null_mut();
            buffer.Lock(&mut destination, None, None).map_err(native)?;
            ptr::copy_nonoverlapping(data.as_ptr(), destination, data.len());
            buffer.Unlock().map_err(native)?;
            buffer.SetCurrentLength(data.len() as u32).map_err(native)?;
            let sample = MFCreateSample().map_err(native)?;
            sample.AddBuffer(&buffer).map_err(native)?;
            sample.SetSampleTime(timestamp).map_err(native)?;
            sample.SetSampleDuration(10_000_000 / 60).map_err(native)?;
            self.transform.ProcessInput(0, &sample, 0).map_err(native)?;
        }
        self.output()
    }

    pub fn drain(&mut self) -> Result<Vec<Arc<Frame>>> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .map_err(native)?;
        }
        self.output()
    }

    pub fn reset(&mut self) -> Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .map_err(native)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(native)?;
        }
        Ok(())
    }

    unsafe fn negotiate_output(&mut self) -> Result<()> {
        unsafe {
            for index in 0..64 {
                let output = match self.transform.GetOutputAvailableType(0, index) {
                    Ok(output) => output,
                    Err(_) => break,
                };
                if output.GetGUID(&MF_MT_SUBTYPE).map_err(native)? != MFVideoFormat_AYUV {
                    continue;
                }
                self.transform
                    .SetOutputType(0, &output, 0)
                    .map_err(native)?;
                let dimensions = output.GetUINT64(&MF_MT_FRAME_SIZE).map_err(native)?;
                self.size = Extent::new((dimensions >> 32) as u32, dimensions as u32)?;
                self.full_range = output
                    .GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE)
                    .unwrap_or(MFNominalRange_0_255.0 as u32)
                    == MFNominalRange_0_255.0 as u32;
                self.verified = true;
                return Ok(());
            }
        }
        Err(Error::Display(
            "The Windows HEVC decoder cannot preserve 8-bit 4:4:4 output on this adapter".into(),
        ))
    }

    fn output(&mut self) -> Result<Vec<Arc<Frame>>> {
        let mut frames = Vec::new();
        for _ in 0..64 {
            let mut output = MFT_OUTPUT_DATA_BUFFER::default();
            let mut status = 0;
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
            };
            let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
            drop(unsafe { ManuallyDrop::take(&mut output.pEvents) });
            match result {
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(frames),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    unsafe {
                        self.negotiate_output()?;
                    }
                    continue;
                }
                Err(error) => return Err(native(error)),
                Ok(()) => (),
            }
            if !self.verified {
                return Err(Error::Display(
                    "The hardware decoder did not verify 4:4:4 output".into(),
                ));
            }
            let sample = sample.ok_or_else(|| {
                Error::Display("The hardware decoder returned no video surface".into())
            })?;
            unsafe {
                let dxgi: IMFDXGIBuffer = sample
                    .GetBufferByIndex(0)
                    .map_err(native)?
                    .cast()
                    .map_err(native)?;
                let mut resource = ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut resource)
                    .map_err(native)?;
                let texture = ID3D11Texture2D::from_raw(resource);
                let mut error = [0; 512];
                let frame = nhg_converter_frame(
                    self.converter.0.as_ptr(),
                    texture.as_raw(),
                    dxgi.GetSubresourceIndex().map_err(native)?,
                    self.size.width,
                    self.size.height,
                    i32::from(self.full_range),
                    sample.as_raw(),
                    error.as_mut_ptr(),
                    error.len(),
                );
                if let Some(frame) = NonNull::new(frame) {
                    frames.push(Frame::from_native(frame)?);
                } else if error[0] != 0 {
                    return Err(conversion_error(&error));
                }
            }
        }
        Err(Error::Display(
            "The hardware decoder exceeded its output-progress limit".into(),
        ))
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self.activation.ShutdownObject();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "manual integrated GPU decode and pixel-conformance check"]
    fn integrated_gpu_preserves_real_444_pixels_and_recovery() {
        let adapter = super::super::adapters()
            .unwrap()
            .into_iter()
            .find(|adapter| matches!(adapter.vendor_id, 0x8086 | 0x1002))
            .expect("Intel or AMD adapter");
        let mut decoder = super::Decoder::new(
            adapter.index,
            crate::geometry::Extent::new(3840, 2160).unwrap(),
        )
        .unwrap();
        let sample = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/native-hevc/tests/data/quadrants444_4k.hevc"
        ));
        let mut frames = decoder.push(sample, 17).unwrap();
        assert_eq!(
            frames.len(),
            1,
            "A complete integrated-GPU picture must not wait for another frame or EOS"
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
                    "D3D11 pixel mismatch at {x},{y}, channel {channel}: {} vs {}",
                    rgba[offset + channel],
                    reference.rgba[offset + channel]
                );
            }
        }
        decoder.reset().unwrap();
        let mut replacement = decoder.push(sample, 18).unwrap();
        replacement.extend(decoder.drain().unwrap());
        assert_eq!(replacement.len(), 1);
        assert_eq!(frames[0].read_rgba().unwrap(), rgba);
        decoder.reset().unwrap();
        let started = std::time::Instant::now();
        let mut submitted = 0;
        let mut decoded = 0;
        while started.elapsed() < std::time::Duration::from_secs(3) {
            let output = decoder.push(sample, 19 + submitted).unwrap();
            decoded += output.len();
            for frame in output {
                std::hint::black_box(frame.read_rgba().unwrap());
            }
            submitted += 1;
        }
        decoded += decoder.drain().unwrap().len();
        assert_eq!(decoded, submitted as usize);
        println!(
            "Integrated GPU workload process {}: {} 4K frames in {:.3}s ({:.1} FPS including GPU conversion/readback)",
            std::process::id(),
            decoded,
            started.elapsed().as_secs_f64(),
            decoded as f64 / started.elapsed().as_secs_f64()
        );
        println!(
            "{}: actual D3D11 HEVC 4:4:4 decode, GPU color conversion, and recovery passed",
            adapter.name
        );
    }
}
