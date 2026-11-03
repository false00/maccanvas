use sha2::{Digest, Sha256};

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
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor\\fdk-aac");
    let output = cmake::Config::new("native")
        .define("FDK_ROOT", source)
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreaded")
        .static_crt(true)
        .profile("Release")
        .build();
    let executable = output.join("bin\\aac-to-wav.exe");
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
