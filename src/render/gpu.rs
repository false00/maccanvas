use serde::Serialize;
use windows::{
    Win32::{
        Foundation::{HMODULE, LUID},
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_11_1,
            },
            Direct3D11::{
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
                D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11VideoDevice,
            },
            Dxgi::{
                Common::{
                    DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
                    DXGI_FORMAT_Y410,
                },
                CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIAdapter,
                IDXGIDevice, IDXGIFactory1,
            },
        },
    },
    core::{GUID, Interface},
};

use crate::error::{Error, Result};
use crate::geometry::Extent;

pub mod interop;
pub mod mf;
pub mod nvdec;

const HEVC_MAIN_444: GUID = GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33);
const HEVC_MAIN10_444: GUID = GUID::from_u128(0x0dabeffa_4458_4602_bc03_0795659d617c);

fn preference(value: Option<&str>) -> Result<bool> {
    match value {
        None | Some("auto") | Some("d3d11") | Some("nvdec") => Ok(true),
        Some("software") => Ok(false),
        _ => Err(Error::Configuration(
            "MACCANVAS_VIDEO_DECODER must be auto, d3d11, nvdec, or software".into(),
        )),
    }
}

pub fn hardware_enabled() -> Result<bool> {
    let value =
        std::env::var("MACCANVAS_VIDEO_DECODER")
            .map(Some)
            .or_else(|error| match error {
                std::env::VarError::NotPresent => Ok(None),
                _ => Err(Error::Configuration(
                    "MACCANVAS_VIDEO_DECODER is not valid text".into(),
                )),
            })?;
    preference(value.as_deref())
}

fn sharing_preference(value: Option<&str>) -> bool {
    value == Some("shared")
}

pub fn direct_sharing_enabled() -> bool {
    sharing_preference(std::env::var("MACCANVAS_GPU_PRESENTATION").ok().as_deref())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Nvdec,
    D3d11,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Nvdec => "nvdec",
            Self::D3d11 => "d3d11",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub index: u32,
    pub name: String,
    pub backend: Backend,
}

fn candidate_order(
    adapters: &[Adapter],
    requested: Option<&str>,
    windows_adapter: Option<u64>,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    if matches!(requested, None | Some("auto")) {
        if let Some(adapter) = adapters.iter().find(|adapter| {
            Some(adapter.luid) == windows_adapter && adapter.hevc_444 && adapter.error.is_none()
        }) {
            if adapter.vendor_id == 0x10de {
                candidates.push(Candidate {
                    index: adapter.index,
                    name: adapter.name.clone(),
                    backend: Backend::Nvdec,
                });
            }
            candidates.push(Candidate {
                index: adapter.index,
                name: adapter.name.clone(),
                backend: Backend::D3d11,
            });
        }
        return candidates;
    }
    if requested != Some("d3d11") && requested != Some("software") {
        candidates.extend(
            adapters
                .iter()
                .filter(|adapter| {
                    adapter.vendor_id == 0x10de && adapter.hevc_444 && adapter.error.is_none()
                })
                .map(|adapter| Candidate {
                    index: adapter.index,
                    name: adapter.name.clone(),
                    backend: Backend::Nvdec,
                }),
        );
    }
    if requested != Some("nvdec") && requested != Some("software") {
        for vendors in [&[0x8086, 0x1002][..], &[0x10de][..]] {
            candidates.extend(
                adapters
                    .iter()
                    .filter(|adapter| {
                        vendors.contains(&adapter.vendor_id)
                            && adapter.hevc_444
                            && adapter.error.is_none()
                    })
                    .map(|adapter| Candidate {
                        index: adapter.index,
                        name: adapter.name.clone(),
                        backend: Backend::D3d11,
                    }),
            );
        }
    }
    candidates
}

fn luid(value: LUID) -> u64 {
    (u64::from(value.HighPart as u32) << 32) | u64::from(value.LowPart)
}

fn windows_adapter_luid() -> Result<u64> {
    let failed = |error| Error::Display(format!("Cannot query Windows' default GPU: {error}"));
    let mut device = None;
    unsafe {
        // A null adapter lets Windows/driver policy choose instead of requesting a vendor.
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(failed)?;
        let device = device.ok_or_else(|| Error::Display("Windows did not select a GPU".into()))?;
        let dxgi: IDXGIDevice = device.cast().map_err(failed)?;
        let adapter = dxgi.GetAdapter().map_err(failed)?;
        Ok(luid(adapter.GetDesc().map_err(failed)?.AdapterLuid))
    }
}

fn selected_candidates(adapters: &[Adapter], requested: Option<&str>) -> Result<Vec<Candidate>> {
    preference(requested)?;
    let preferred = if matches!(requested, None | Some("auto")) {
        let preferred = windows_adapter_luid()?;
        if !adapters.iter().any(|adapter| adapter.luid == preferred) {
            return Err(Error::Display(
                "The GPU selected by Windows is no longer available".into(),
            ));
        }
        Some(preferred)
    } else {
        None
    };
    Ok(candidate_order(adapters, requested, preferred))
}

pub fn candidates() -> Result<Vec<Candidate>> {
    if !hardware_enabled()? {
        return Ok(Vec::new());
    }
    let requested = std::env::var("MACCANVAS_VIDEO_DECODER").ok();
    selected_candidates(&adapters()?, requested.as_deref())
}

pub enum Decoder {
    Nvdec(nvdec::Decoder),
    D3d11(mf::Decoder),
}

impl Decoder {
    pub fn new(candidate: &Candidate, size: Extent) -> Result<Self> {
        match candidate.backend {
            Backend::Nvdec => nvdec::Decoder::new(candidate.index).map(Self::Nvdec),
            Backend::D3d11 => mf::Decoder::new(candidate.index, size).map(Self::D3d11),
        }
    }

    pub fn push(
        &mut self,
        data: &[u8],
        timestamp: i64,
    ) -> Result<Vec<std::sync::Arc<nvdec::Frame>>> {
        match self {
            Self::Nvdec(decoder) => decoder.push(data, timestamp),
            Self::D3d11(decoder) => decoder.push(data, timestamp),
        }
    }

    pub fn reset(&mut self) -> Result<()> {
        match self {
            Self::Nvdec(decoder) => decoder.reset(),
            Self::D3d11(decoder) => decoder.reset(),
        }
    }

    pub fn counter(&self) -> i32 {
        match self {
            Self::Nvdec(_) => 1,
            Self::D3d11(_) => 2,
        }
    }
}

pub fn report() -> Result<serde_json::Value> {
    let adapters = adapters()?;
    let requested = std::env::var("MACCANVAS_VIDEO_DECODER").ok();
    let hardware = selected_candidates(&adapters, requested.as_deref())?
        .into_iter()
        .map(|candidate| {
            let result = Decoder::new(&candidate, Extent::new(64, 48).expect("fixed probe size"));
            serde_json::json!({
                "adapter_index":candidate.index,"adapter":candidate.name,"decoder":candidate.backend.name(),
                "available":result.is_ok(),"error":result.err().map(|error| error.to_string()),
                "stream_verification_required":candidate.backend == Backend::D3d11,
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "hardware_enabled":hardware_enabled()?,"software_decoder":"libde265",
        "selection_policy":if matches!(requested.as_deref(), None | Some("auto")) {"windows"} else {"explicit_decoder_override"},
        "adapters":adapters,"hardware_decoders":hardware,
    }))
}

#[derive(Debug, Serialize)]
pub struct Adapter {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub luid: u64,
    pub hevc_420: bool,
    pub hevc_420_10bit: bool,
    pub hevc_444: bool,
    pub hevc_444_10bit: bool,
    pub error: Option<String>,
}

fn supports(device: &ID3D11VideoDevice, profile: &GUID, format: DXGI_FORMAT) -> bool {
    unsafe { device.CheckVideoDecoderFormat(profile, format) }
        .is_ok_and(|supported| supported.as_bool())
}

pub fn adapters() -> Result<Vec<Adapter>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| Error::Display(format!("Cannot enumerate graphics adapters: {error}")))?;
    let mut output = Vec::new();
    for index in 0..32 {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => {
                return Err(Error::Display(format!(
                    "Cannot read graphics adapter: {error}"
                )));
            }
        };
        let description = unsafe { adapter.GetDesc1() }.map_err(|error| {
            Error::Display(format!("Cannot describe graphics adapter: {error}"))
        })?;
        if description.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            continue;
        }
        let length = description
            .Description
            .iter()
            .position(|&value| value == 0)
            .unwrap_or(description.Description.len());
        let mut information = Adapter {
            index,
            name: String::from_utf16_lossy(&description.Description[..length]),
            vendor_id: description.VendorId,
            device_id: description.DeviceId,
            luid: luid(description.AdapterLuid),
            hevc_420: false,
            hevc_420_10bit: false,
            hevc_444: false,
            hevc_444_10bit: false,
            error: None,
        };
        let mut device: Option<ID3D11Device> = None;
        let created = unsafe {
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
        };
        match created.and_then(|()| {
            device
                .expect("created D3D11 device")
                .cast::<ID3D11VideoDevice>()
        }) {
            Ok(video) => {
                information.hevc_420 = supports(
                    &video,
                    &D3D11_DECODER_PROFILE_HEVC_VLD_MAIN,
                    DXGI_FORMAT_NV12,
                );
                information.hevc_420_10bit = supports(
                    &video,
                    &D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10,
                    DXGI_FORMAT_P010,
                );
                information.hevc_444 = supports(&video, &HEVC_MAIN_444, DXGI_FORMAT_AYUV);
                information.hevc_444_10bit = supports(&video, &HEVC_MAIN10_444, DXGI_FORMAT_Y410);
            }
            Err(error) => information.error = Some(error.to_string()),
        }
        output.push(information);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    fn adapter(index: u32, vendor_id: u32, capable: bool) -> super::Adapter {
        super::Adapter {
            index,
            name: format!("Adapter {index}"),
            vendor_id,
            device_id: index,
            luid: u64::from(index),
            hevc_420: true,
            hevc_420_10bit: true,
            hevc_444: capable,
            hevc_444_10bit: capable,
            error: None,
        }
    }

    #[test]
    fn integrated_only_systems_get_a_cross_vendor_decoder_candidate() {
        for vendor in [0x8086, 0x1002] {
            let candidates = super::candidate_order(&[adapter(0, vendor, true)], None, Some(0));
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].backend, super::Backend::D3d11);
        }
        assert!(super::candidate_order(&[adapter(0, 0x8086, false)], None, Some(0)).is_empty());
        assert!(
            super::candidate_order(&[adapter(0, 0x1002, true)], Some("software"), None).is_empty()
        );
    }

    #[test]
    fn hybrid_selection_follows_windows_and_preserves_explicit_decoder_overrides() {
        let adapters = [
            adapter(0, 0x8086, true),
            adapter(1, 0x10de, true),
            adapter(2, 0x1002, true),
        ];
        let candidates = super::candidate_order(&adapters, None, Some(0));
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            (candidates[0].index, candidates[0].backend),
            (0, super::Backend::D3d11)
        );
        let candidates = super::candidate_order(&adapters, Some("auto"), Some(1));
        assert_eq!(candidates.len(), 2);
        assert_eq!(
            (candidates[0].index, candidates[0].backend),
            (1, super::Backend::Nvdec)
        );
        assert_eq!(
            (candidates[1].index, candidates[1].backend),
            (1, super::Backend::D3d11)
        );
        let candidates = super::candidate_order(&adapters, None, Some(2));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 2);
        let candidates = super::candidate_order(&adapters, Some("d3d11"), None);
        assert_eq!(candidates[0].index, 0);
        assert!(
            candidates
                .iter()
                .all(|candidate| candidate.backend == super::Backend::D3d11)
        );
        let candidates = super::candidate_order(&adapters, Some("nvdec"), None);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].index, 1);
    }

    #[test]
    fn automatic_selection_does_not_override_an_unsupported_or_missing_windows_gpu() {
        let adapters = [adapter(0, 0x8086, false), adapter(1, 0x10de, true)];
        assert!(super::candidate_order(&adapters, None, Some(0)).is_empty());
        assert!(super::candidate_order(&adapters, None, Some(99)).is_empty());
        let mut failed = adapter(0, 0x8086, true);
        failed.error = Some("driver unavailable".into());
        assert!(super::candidate_order(&[failed], Some("auto"), Some(0)).is_empty());
    }

    #[test]
    #[ignore = "manual integrated GPU HEVC 4:4:4 stream negotiation check"]
    fn installed_integrated_transform_decodes_real_444() -> windows::core::Result<()> {
        use std::{mem::ManuallyDrop, ptr};
        use windows::Win32::{
            Graphics::{
                Direct3D::{
                    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
                },
                Direct3D11::{
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice,
                    ID3D11Device,
                },
                Dxgi::{CreateDXGIFactory1, IDXGIFactory1},
            },
            Media::MediaFoundation::*,
            System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        };
        use windows::core::Interface;

        let adapter = super::adapters()
            .unwrap()
            .into_iter()
            .find(|adapter| matches!(adapter.vendor_id, 0x8086 | 0x1002))
            .expect("Integrated Intel or AMD adapter");
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            let result = (|| -> windows::core::Result<()> {
                let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
                let adapter = factory.EnumAdapters1(adapter.index)?;
                let mut device: Option<ID3D11Device> = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    super::HMODULE::default(),
                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )?;
                let device = device.unwrap();
                let mut token = 0;
                let mut manager = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                let manager = manager.unwrap();
                manager.ResetDevice(&device, token)?;
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
                )?;
                assert!(count > 0 && count <= 128);
                let entries = std::slice::from_raw_parts_mut(activations, count as usize);
                let activation = entries[0].take().unwrap();
                for entry in entries.iter_mut() {
                    drop(entry.take());
                }
                CoTaskMemFree(Some(activations.cast()));
                let transform: IMFTransform = activation.ActivateObject()?;
                assert_eq!(transform.GetAttributes()?.GetUINT32(&MF_SA_D3D11_AWARE)?, 1);
                transform.GetAttributes()?.SetUINT32(&MF_LOW_LATENCY, 1)?;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
                let input = MFCreateMediaType()?;
                input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC_ES)?;
                input.SetUINT64(&MF_MT_FRAME_SIZE, (64_u64 << 32) | 48)?;
                input.SetUINT64(&MF_MT_FRAME_RATE, (60_u64 << 32) | 1)?;
                input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                input.SetUINT32(&MF_MT_MPEG2_PROFILE, 4)?;
                transform.SetInputType(0, &input, 0)?;
                let first = transform.GetOutputAvailableType(0, 0)?;
                println!(
                    "Initial integrated decoder output: {:?}",
                    first.GetGUID(&MF_MT_SUBTYPE)?
                );
                transform.SetOutputType(0, &first, 0)?;
                transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
                transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
                let data = include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/crates/native-hevc/tests/data/pattern444.hevc"
                ));
                let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
                let mut destination = ptr::null_mut();
                buffer.Lock(&mut destination, None, None)?;
                ptr::copy_nonoverlapping(data.as_ptr(), destination, data.len());
                buffer.Unlock()?;
                buffer.SetCurrentLength(data.len() as u32)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(0)?;
                sample.SetSampleDuration(10_000_000 / 60)?;
                transform.ProcessInput(0, &sample, 0)?;
                transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
                for _ in 0..8 {
                    let mut output = MFT_OUTPUT_DATA_BUFFER::default();
                    let mut status = 0;
                    let result =
                        transform.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
                    let sample = ManuallyDrop::take(&mut output.pSample);
                    drop(ManuallyDrop::take(&mut output.pEvents));
                    match result {
                        Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                            let mut selected = None;
                            for index in 0..64 {
                                let available = match transform.GetOutputAvailableType(0, index) {
                                    Ok(available) => available,
                                    Err(_) => break,
                                };
                                let subtype = available.GetGUID(&MF_MT_SUBTYPE)?;
                                println!("Post-SPS integrated decoder output: {subtype:?}");
                                if subtype == MFVideoFormat_AYUV {
                                    selected = Some(available);
                                }
                            }
                            let selected = selected.ok_or_else(|| windows::core::Error::new(MF_E_INVALIDMEDIATYPE,
                                "The integrated Windows HEVC decoder exposes no 4:4:4 output after the real SPS"))?;
                            transform.SetOutputType(0, &selected, 0)?;
                        }
                        Err(error) => return Err(error),
                        Ok(()) => {
                            let sample = sample.expect("Decoded sample");
                            assert_eq!(
                                transform.GetOutputCurrentType(0)?.GetGUID(&MF_MT_SUBTYPE)?,
                                MFVideoFormat_AYUV
                            );
                            let _: IMFDXGIBuffer = sample.GetBufferByIndex(0)?.cast()?;
                            activation.ShutdownObject()?;
                            return Ok(());
                        }
                    }
                }
                Err(windows::core::Error::from_hresult(
                    MF_E_TRANSFORM_NEED_MORE_INPUT,
                ))
            })();
            MFShutdown()?;
            CoUninitialize();
            result
        }
    }

    #[test]
    fn unverified_direct_sharing_is_never_the_default() {
        assert!(!super::sharing_preference(None));
        assert!(!super::sharing_preference(Some("readback")));
        assert!(!super::sharing_preference(Some("unknown")));
        assert!(super::sharing_preference(Some("shared")));
    }

    #[test]
    fn decoder_preference_preserves_a_forced_software_fallback() {
        assert!(super::preference(None).unwrap());
        assert!(super::preference(Some("auto")).unwrap());
        assert!(!super::preference(Some("software")).unwrap());
        assert!(super::preference(Some("d3d11")).unwrap());
        assert!(super::preference(Some("nvdec")).unwrap());
        assert!(super::preference(Some("unknown")).is_err());
    }

    #[test]
    #[ignore = "manual Windows HEVC decoder probe; initializes Media Foundation"]
    fn report_installed_media_foundation_decoders() {
        use windows::{
            Win32::{
                Media::MediaFoundation::*,
                System::Com::{
                    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
                },
            },
            core::PWSTR,
        };

        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_HEVC,
            };
            let mut transforms = std::ptr::null_mut();
            let mut count = 0;
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_DECODER,
                MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                None,
                &mut transforms,
                &mut count,
            )
            .unwrap();
            assert!(count <= 128);
            let mut output = Vec::new();
            if count != 0 {
                for activate in std::slice::from_raw_parts_mut(transforms, count as usize) {
                    let activate = activate.take().unwrap();
                    let mut name = PWSTR::null();
                    let mut length = 0;
                    activate
                        .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut length)
                        .unwrap();
                    let name_text = String::from_utf16_lossy(std::slice::from_raw_parts(
                        name.0,
                        length as usize,
                    ));
                    CoTaskMemFree(Some(name.0.cast()));
                    let transform: IMFTransform = activate.ActivateObject().unwrap();
                    let aware = transform
                        .GetAttributes()
                        .and_then(|attributes| attributes.GetUINT32(&MF_SA_D3D11_AWARE))
                        .unwrap_or(0);
                    output.push(serde_json::json!({"name":name_text,"d3d11_aware":aware != 0}));
                    activate.ShutdownObject().unwrap();
                }
            }
            CoTaskMemFree(Some(transforms.cast()));
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
            MFShutdown().unwrap();
            CoUninitialize();
            assert!(
                !output.is_empty(),
                "No installed HEVC Media Foundation decoder"
            );
        }
    }

    #[test]
    #[ignore = "manual hardware capability probe; wakes installed graphics adapters"]
    fn report_installed_hevc_capabilities() {
        let adapters = super::adapters().unwrap();
        assert!(!adapters.is_empty());
        println!("{}", serde_json::to_string_pretty(&adapters).unwrap());
    }
}
