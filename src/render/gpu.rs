use serde::Serialize;
use windows::{
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
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
                CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
                IDXGIFactory1,
            },
        },
    },
    core::{GUID, Interface},
};

use crate::error::{Error, Result};

pub mod interop;
pub mod nvdec;

const HEVC_MAIN_444: GUID = GUID::from_u128(0x4008018f_f537_4b36_98cf_61af8a2c1a33);
const HEVC_MAIN10_444: GUID = GUID::from_u128(0x0dabeffa_4458_4602_bc03_0795659d617c);

fn preference(value: Option<&str>) -> Result<bool> {
    match value {
        None | Some("auto") => Ok(true),
        Some("software") => Ok(false),
        _ => Err(Error::Configuration(
            "MACCANVAS_VIDEO_DECODER must be auto or software".into(),
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

pub fn report() -> Result<serde_json::Value> {
    let adapters = adapters()?;
    let hardware = adapters
        .iter()
        .filter(|adapter| adapter.vendor_id == 0x10de)
        .map(|adapter| {
            let result = nvdec::Decoder::new(adapter.index);
            serde_json::json!({
                "adapter_index":adapter.index,"adapter":adapter.name,"decoder":"nvdec",
                "available":result.is_ok(),"error":result.err().map(|error| error.to_string()),
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "hardware_enabled":hardware_enabled()?,"software_decoder":"libde265",
        "adapters":adapters,"hardware_decoders":hardware,
    }))
}

#[derive(Debug, Serialize)]
pub struct Adapter {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
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
