use std::{io::Write, time::Duration};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    Auto,
    Zrle,
    Tight,
    TightJpeg,
    Hextile,
    Zlib,
    Rre,
    Corre,
    Raw,
    Hevc30,
    #[default]
    Hevc60,
}

impl Encoding {
    pub const ALL: [Self; 11] = [
        Self::Hevc60,
        Self::Hevc30,
        Self::Auto,
        Self::Zrle,
        Self::Tight,
        Self::TightJpeg,
        Self::Hextile,
        Self::Zlib,
        Self::Raw,
        Self::Rre,
        Self::Corre,
    ];

    pub fn is_hevc(self) -> bool {
        matches!(self, Self::Hevc30 | Self::Hevc60)
    }

    pub fn requires_reconnect(self, next: Self) -> bool {
        self != next && (self.is_hevc() || next.is_hevc())
    }

    pub fn fps_ceiling(self) -> u16 {
        match self {
            Self::Hevc30 => 30,
            Self::Hevc60 => 60,
            _ => 120,
        }
    }

    pub fn from_index(index: i32) -> Result<Self> {
        usize::try_from(index)
            .ok()
            .and_then(|index| Self::ALL.get(index).copied())
            .ok_or_else(|| Error::Configuration("Choose a supported compression method".into()))
    }

    pub fn index(self) -> i32 {
        Self::ALL
            .iter()
            .position(|&encoding| encoding == self)
            .expect("encoding is listed") as i32
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto (lossless)",
            Self::Zrle => "ZRLE",
            Self::Tight => "Tight lossless",
            Self::TightJpeg => "Tight + JPEG",
            Self::Hextile => "Hextile",
            Self::Zlib => "Zlib",
            Self::Rre => "RRE",
            Self::Corre => "CoRRE",
            Self::Raw => "Raw",
            Self::Hevc30 => "HEVC / H.265 - 30 FPS",
            Self::Hevc60 => "HEVC / H.265 - 60 FPS",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => {
                "Lossless VNC: prefer ZRLE, then Tight and legacy fallbacks. The Mac chooses an offered encoding; the live label shows what it actually sends. Use this for Macs or networks that do not support HEVC."
            }
            Self::Zrle => {
                "ZRLE: lossless compressed tiles. Sharp text and a good desktop default; more CPU than Raw. Noise and video can still use substantial bandwidth."
            }
            Self::Tight => {
                "Tight lossless: deflate with palette and gradient filters. Excellent for text and flat-color apps. More encoding CPU; no JPEG artifacts."
            }
            Self::TightJpeg => {
                "Tight + JPEG: may compress photographs and motion more efficiently. JPEG quality 0-9 trades clarity for smaller updates; text or images can show artifacts. The Mac may ignore this hint."
            }
            Self::Hextile => {
                "Hextile: small lossless tiles with solid-color subrectangles. Low CPU for simple desktops; images and video often approach Raw bandwidth."
            }
            Self::Zlib => {
                "Zlib: lossless deflate over pixel data. Simple and broadly compatible; more CPU and usually less efficient than ZRLE or Tight."
            }
            Self::Rre => {
                "RRE: lossless background colors and colored rectangles. Useful for legacy flat-color screens; photos can be larger than Raw."
            }
            Self::Corre => {
                "CoRRE: compact RRE subrectangles. A legacy low-CPU choice for small simple regions; poor for modern photo-heavy desktops."
            }
            Self::Raw => {
                "Raw: no pixel compression and the lowest compression CPU cost. Best only on fast trusted links; full-frame 4K motion can require several gigabits per second."
            }
            Self::Hevc30 => {
                "Apple High Performance HEVC/H.265 at up to 30 FPS. Lower stream and decoding load than 60 FPS; preserves the Mac's physical desktop. Requires Mac account login and UDP on the VNC port and the next port. A native decoder is bundled; no codec extension is needed. Apply reconnects automatically when needed. Use Clipboard for encrypted text sharing."
            }
            Self::Hevc60 => {
                "Default: Apple High Performance HEVC/H.265 at up to 60 FPS. Smoother motion with higher decoding and bandwidth use. Uses the same physical Mac desktop, encrypted control and SRTP video, with native decoding and software fallback. Requires a compatible Mac, Mac account login and UDP on both media ports. Apply reconnects automatically when needed."
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StreamSettings {
    pub encoding: Encoding,
    pub compression: u8,
    pub jpeg_quality: u8,
    pub target_kbps: u32,
    pub max_fps: u16,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            encoding: Encoding::Hevc60,
            compression: 6,
            jpeg_quality: 8,
            target_kbps: 0,
            max_fps: 60,
        }
    }
}

impl StreamSettings {
    pub fn validate_authentication(self, authentication: crate::profiles::AuthMode) -> Result<()> {
        self.validate()?;
        if self.encoding.is_hevc() && authentication != crate::profiles::AuthMode::MacLogin {
            return Err(Error::Configuration(
                "HEVC requires Mac account login. Choose a lossless VNC mode in Quality to use a classic VNC password.".into(),
            ));
        }
        Ok(())
    }

    pub fn validate(self) -> Result<()> {
        if self.encoding.is_hevc() && self.target_kbps != 0 {
            return Err(Error::Configuration(
                "HEVC uses the Mac's adaptive video bitrate; the VNC request-bandwidth target does not apply".into()
            ));
        }
        if self.compression > 9
            || self.jpeg_quality > 9
            || !(1..=120).contains(&self.max_fps)
            || (self.target_kbps != 0 && !(100..=1_000_000).contains(&self.target_kbps))
        {
            return Err(Error::Configuration(
                "Use compression/quality 0-9, 1-120 FPS, and a bandwidth target of 0.1-1000 Mbps (or Automatic).".into()
            ));
        }
        Ok(())
    }

    pub fn encodings(self) -> Result<Vec<i32>> {
        if self.encoding.is_hevc() {
            return Err(Error::Configuration(
                "HEVC uses Apple's High Performance transport, not an RFB rectangle encoding"
                    .into(),
            ));
        }
        let preferred = match self.encoding {
            Encoding::Auto | Encoding::Zrle => 16,
            Encoding::Tight | Encoding::TightJpeg => 7,
            Encoding::Hextile => 5,
            Encoding::Zlib => 6,
            Encoding::Rre => 2,
            Encoding::Corre => 4,
            Encoding::Raw => 0,
            Encoding::Hevc30 | Encoding::Hevc60 => unreachable!("checked HEVC transport"),
        };
        let mut encodings = vec![1, preferred];
        if self.encoding == Encoding::Auto {
            for encoding in [7, 5, 6, 2, 4] {
                if !encodings.contains(&encoding) {
                    encodings.push(encoding);
                }
            }
        }
        if preferred != 0 {
            encodings.push(0);
        }
        encodings.extend([
            crate::clipboard::ENCODING,
            -223,
            -308,
            -239,
            -240,
            -224,
            -256 + i32::from(self.compression),
        ]);
        if self.encoding == Encoding::TightJpeg {
            encodings.push(-32 + i32::from(self.jpeg_quality));
        }
        Ok(encodings)
    }

    pub fn write_encodings(self, writer: &mut impl Write) -> Result<()> {
        self.validate()?;
        let encodings = self.encodings()?;
        writer.write_all(&[2, 0])?;
        writer.write_all(&(encodings.len() as u16).to_be_bytes())?;
        for encoding in encodings {
            writer.write_all(&encoding.to_be_bytes())?;
        }
        writer.flush()?;
        Ok(())
    }

    pub fn request_interval(self, bytes: u64) -> Duration {
        let frame = Duration::from_secs_f64(1.0 / f64::from(self.max_fps));
        if self.target_kbps == 0 {
            return frame;
        }
        frame.max(Duration::from_secs_f64(
            bytes as f64 * 8.0 / (f64::from(self.target_kbps) * 1000.0),
        ))
    }
}

pub fn encoding_name(id: i32) -> &'static str {
    match id {
        0 => "Raw",
        1 => "CopyRect",
        2 => "RRE",
        4 => "CoRRE",
        5 => "Hextile",
        6 => "Zlib",
        7 => "Tight",
        16 => "ZRLE",
        crate::apple_hps::COUNTER_ENCODING => "HEVC / H.265",
        _ => "Waiting for pixels",
    }
}

pub fn raw_estimate(width: u32, height: u32, fps: u16) -> f64 {
    f64::from(width) * f64::from(height) * 32.0 * f64::from(fps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_hevc60_with_adaptive_bitrate() {
        let settings = StreamSettings::default();
        assert_eq!(settings.encoding, Encoding::Hevc60);
        assert_eq!(Encoding::default(), Encoding::Hevc60);
        assert_eq!(settings.max_fps, 60);
        assert_eq!(settings.target_kbps, 0);
        assert_eq!(settings.compression, 6);
        assert!(settings.encodings().is_err());
        settings.validate().unwrap();
    }

    #[test]
    fn methods_are_ordered_by_recommendation_with_hevc_first() {
        assert_eq!(
            Encoding::ALL,
            [
                Encoding::Hevc60,
                Encoding::Hevc30,
                Encoding::Auto,
                Encoding::Zrle,
                Encoding::Tight,
                Encoding::TightJpeg,
                Encoding::Hextile,
                Encoding::Zlib,
                Encoding::Raw,
                Encoding::Rre,
                Encoding::Corre,
            ]
        );
        for (index, encoding) in Encoding::ALL.into_iter().enumerate() {
            assert_eq!(Encoding::from_index(index as i32).unwrap(), encoding);
            assert_eq!(encoding.index(), index as i32);
        }
    }

    #[test]
    fn transport_and_hevc_preset_changes_require_restart_but_live_settings_do_not() {
        for current in Encoding::ALL {
            for next in Encoding::ALL {
                let expected = current != next && (current.is_hevc() || next.is_hevc());
                assert_eq!(current.requires_reconnect(next), expected);
            }
        }
    }

    #[test]
    fn hevc_requires_mac_account_authentication_without_a_silent_downgrade() {
        use crate::profiles::AuthMode;
        assert!(
            StreamSettings::default()
                .validate_authentication(AuthMode::MacLogin)
                .is_ok()
        );
        assert!(
            StreamSettings::default()
                .validate_authentication(AuthMode::VncPassword)
                .is_err()
        );
        StreamSettings {
            encoding: Encoding::Auto,
            ..Default::default()
        }
        .validate_authentication(AuthMode::VncPassword)
        .unwrap();
    }

    #[test]
    fn every_method_advertises_only_supported_encodings_and_raw_fallback() {
        for encoding in Encoding::ALL {
            let settings = StreamSettings {
                encoding,
                ..Default::default()
            };
            if encoding.is_hevc() {
                assert!(settings.encodings().is_err());
                assert!(settings.write_encodings(&mut Vec::new()).is_err());
            } else {
                assert!(settings.encodings().unwrap().contains(&0));
                assert!(settings.encodings().unwrap().contains(&1));
                assert!(settings.encodings().unwrap().contains(&-250));
            }
            settings.validate().unwrap();
            assert_eq!(Encoding::from_index(encoding.index()).unwrap(), encoding);
        }
    }

    #[test]
    fn only_explicit_jpeg_mode_requests_lossy_quality() {
        let settings = StreamSettings {
            encoding: Encoding::TightJpeg,
            jpeg_quality: 4,
            ..Default::default()
        };
        assert!(settings.encodings().unwrap().contains(&-28));
        assert!(settings.encodings().unwrap().contains(&7));
    }

    #[test]
    fn manual_target_and_fps_determine_the_next_request_not_input() {
        let settings = StreamSettings {
            encoding: Encoding::Auto,
            target_kbps: 8000,
            max_fps: 30,
            ..Default::default()
        };
        assert_eq!(settings.request_interval(1_000_000), Duration::from_secs(1));
        assert!((settings.request_interval(100).as_secs_f64() - 1.0 / 30.0).abs() < 0.00001);
        assert_eq!(
            StreamSettings::default().request_interval(1_000_000),
            Duration::from_secs_f64(1.0 / 60.0)
        );
    }

    #[test]
    fn invalid_targets_and_indices_do_not_fall_back_silently() {
        assert!(
            StreamSettings {
                target_kbps: 1,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            StreamSettings {
                max_fps: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(Encoding::from_index(-1).is_err());
    }

    #[test]
    fn full_frame_4k_raw_estimate_is_measurable() {
        assert_eq!(raw_estimate(3840, 2160, 30), 7_962_624_000.0);
    }
}
