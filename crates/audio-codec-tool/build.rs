use sha2::{Digest, Sha256};

fn prebuilt(output: &std::path::Path) -> Option<std::path::PathBuf> {
    println!("cargo:rerun-if-env-changed=MACCANVAS_AUDIO_COMPONENT");
    println!("cargo:rerun-if-env-changed=MACCANVAS_AUDIO_COMPONENT_SHA256");
    let path = std::env::var_os("MACCANVAS_AUDIO_COMPONENT");
    let expected = std::env::var_os("MACCANVAS_AUDIO_COMPONENT_SHA256");
    let Some(path) = path else {
        assert!(
            expected.is_none(),
            "An audio checksum requires a component path"
        );
        return None;
    };
    let expected = expected
        .expect("A reused audio component requires its published SHA-256")
        .into_string()
        .expect("The reused audio component SHA-256 must be valid text");
    assert!(
        expected.len() == 64 && expected.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "The reused audio component SHA-256 is invalid"
    );
    let path = std::path::PathBuf::from(path);
    println!("cargo:rerun-if-changed={}", path.display());
    let size = std::fs::metadata(&path)
        .expect("The reused audio component exists")
        .len();
    assert!(
        (256..=8 * 1024 * 1024).contains(&size),
        "Invalid audio component size"
    );
    let bytes = std::fs::read(path).expect("The reused audio component is readable");
    assert_eq!(
        format!("{:x}", Sha256::digest(&bytes)),
        expected.to_ascii_lowercase(),
        "The reused audio component does not match its published SHA-256"
    );
    assert_eq!(
        &bytes[..2],
        b"MZ",
        "The audio component is not a Windows executable"
    );
    let pe = u32::from_le_bytes(bytes[0x3c..0x40].try_into().expect("checked DOS header")) as usize;
    let end = pe.checked_add(26).expect("Invalid audio PE header offset");
    assert!(
        end <= bytes.len()
            && bytes.get(pe..pe + 4) == Some(b"PE\0\0")
            && bytes.get(pe + 4..pe + 6) == Some(&[0x64, 0x86])
            && bytes.get(pe + 24..end) == Some(&[0x0b, 0x02]),
        "The audio component is not a Windows x64 PE32+ application"
    );
    let bin = output.join("bin");
    std::fs::create_dir_all(&bin).expect("Audio component output directory can be created");
    let executable = bin.join("aac-to-wav.exe");
    std::fs::write(&executable, bytes).expect("Verified audio component can be staged");
    Some(executable)
}

fn main() {
    let target = std::env::var("TARGET").expect("Cargo provides TARGET");
    assert_eq!(
        target, "x86_64-pc-windows-msvc",
        "The converter targets Windows x64"
    );
    println!("cargo:rerun-if-changed=native");
    println!("cargo:rerun-if-changed=vendor");
    println!("cargo:rerun-if-changed=LICENSE");
    println!("cargo:rerun-if-changed=DISTRIBUTION-NOTICES.txt");
    let output =
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"));
    let executable = prebuilt(&output).unwrap_or_else(|| {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor\\fdk-aac");
        cmake::Config::new("native")
            .define("FDK_ROOT", source)
            .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
            .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreaded")
            .static_crt(true)
            .profile("Release")
            .build()
            .join("bin\\aac-to-wav.exe")
    });
    let bytes = std::fs::read(&executable).expect("The independent converter was built");
    let digest = format!("{:x}", Sha256::digest(&bytes));
    println!("cargo:rustc-env=AAC_TOOL_SHA256={digest}");
    println!("cargo:rustc-env=AAC_TOOL_SIZE={}", bytes.len());
    println!(
        "cargo:rustc-env=AAC_TOOL_BUILD_PATH={}",
        executable.display()
    );
    std::fs::write(
        output.join("codec-metadata.json"),
        format!("{{\"sha256\":\"{digest}\",\"size\":{}}}\n", bytes.len()),
    )
    .expect("Converter packaging metadata can be written");
}
