pub const ASSET: &str = "MacCanvas-audio-windows-x64.exe";
pub const SHA256: &str = env!("AAC_TOOL_SHA256");

pub fn size() -> u64 {
    env!("AAC_TOOL_SIZE")
        .parse()
        .expect("The build provides a valid converter size")
}

pub const BUILD_PATH: &str = env!("AAC_TOOL_BUILD_PATH");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_independent_tool_is_available_without_linking_its_codec() {
        let output = std::process::Command::new(BUILD_PATH)
            .arg("--version")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "aac-to-wav 0.1.0"
        );
        assert_eq!(SHA256.len(), 64);
        assert!(size() > 0 && size() <= 8 * 1024 * 1024);
        let output = std::process::Command::new(BUILD_PATH)
            .arg("--licenses")
            .output()
            .unwrap();
        assert!(output.status.success());
        let notices = String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n");
        let glue_license = include_str!("../LICENSE").replace("\r\n", "\n");
        let codec_license = include_str!("../vendor/fdk-aac/NOTICE").replace("\r\n", "\n");
        assert!(notices.contains(glue_license.trim()));
        assert!(notices.contains(codec_license.trim()));
        assert!(notices.contains("not linked into, or embedded in"));
    }

    #[test]
    fn malformed_standard_media_fails_explicitly() {
        use std::{io::Write, process::Stdio};
        let mut child = std::process::Command::new(BUILD_PATH)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&[0, 0, 0]).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("Conversion failed")
        );
    }
}
