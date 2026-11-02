fn main() {
    build_metadata();
    println!("cargo:rerun-if-changed=assets/logo.svg");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("cupertino".into()),
    )
    .expect("Slint interface compilation failed");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon(generate_icon().to_str().expect("Icon path is UTF-8"))
            .set("ProductName", "MacCanvas")
            .set("FileDescription", "MacCanvas - Windows viewer for macOS")
            .set("LegalCopyright", "MacCanvas contributors. See --licenses.")
            .set_manifest(
                r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
<trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges>
<requestedExecutionLevel level="asInvoker" uiAccess="false"/>
</requestedPrivileges></security></trustInfo>
<application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings>
<dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
<longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
</windowsSettings></application>
</assembly>"#,
            )
            .compile()
            .expect("Windows resource compilation failed");
    }

    fn build_metadata() {
        use std::{path::PathBuf, process::Command};
        println!("cargo:rerun-if-changed=src");
        println!("cargo:rerun-if-changed=Cargo.toml");
        println!("cargo:rerun-if-changed=LICENSE");
        println!("cargo:rerun-if-changed=THIRD_PARTY_NOTICES.txt");
        println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
        let (built, clock) = match std::env::var("SOURCE_DATE_EPOCH") {
            Ok(value) => (
                chrono::DateTime::from_timestamp(
                    value.parse().expect("SOURCE_DATE_EPOCH must be an integer"),
                    0,
                )
                .expect("SOURCE_DATE_EPOCH must be a representable UTC timestamp"),
                "SOURCE_DATE_EPOCH",
            ),
            Err(std::env::VarError::NotPresent) => (chrono::Utc::now(), "build_clock"),
            Err(error) => panic!("Cannot read SOURCE_DATE_EPOCH: {error}"),
        };
        println!(
            "cargo:rustc-env=MACCANVAS_BUILD_UTC={}",
            built.format("%Y-%m-%d %H:%M:%S UTC")
        );
        println!("cargo:rustc-env=MACCANVAS_BUILD_CLOCK={clock}");
        println!(
            "cargo:rustc-env=MACCANVAS_TARGET={}",
            std::env::var("TARGET").expect("Cargo provides TARGET")
        );
        let rustc = Command::new(std::env::var_os("RUSTC").expect("Cargo provides RUSTC"))
            .arg("--version")
            .output()
            .expect("Cannot query the Rust compiler version");
        assert!(
            rustc.status.success(),
            "Cannot query the Rust compiler version"
        );
        println!(
            "cargo:rustc-env=MACCANVAS_RUSTC={}",
            String::from_utf8(rustc.stdout)
                .expect("rustc version is UTF-8")
                .trim()
        );
        let git = |arguments: &[&str]| Command::new("git").args(arguments).output();
        let revision = match git(&["rev-parse", "HEAD"]) {
            Ok(output) if output.status.success() => {
                let revision = String::from_utf8(output.stdout)
                    .expect("Git revision is UTF-8")
                    .trim()
                    .to_owned();
                let status =
                    git(&["status", "--porcelain"]).expect("Cannot query source worktree status");
                assert!(
                    status.status.success(),
                    "Cannot query source worktree status"
                );
                format!(
                    "{revision}{}",
                    if status.stdout.is_empty() {
                        ""
                    } else {
                        "-dirty"
                    }
                )
            }
            _ => {
                println!(
                    "cargo:warning=Git source revision unavailable; this build is marked as a source archive"
                );
                "source-archive".into()
            }
        };
        println!("cargo:rustc-env=MACCANVAS_REVISION={revision}");
        if let Ok(output) = git(&["rev-parse", "--git-dir"])
            && output.status.success()
        {
            let directory = String::from_utf8(output.stdout).expect("Git directory is UTF-8");
            let directory = if cfg!(windows) {
                directory.trim().replace('/', "\\")
            } else {
                directory.trim().into()
            };
            let directory = PathBuf::from(directory);
            for path in [directory.join("HEAD"), directory.join("packed-refs")] {
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
            if let Ok(head) = std::fs::read_to_string(directory.join("HEAD"))
                && let Some(reference) = head.strip_prefix("ref: ")
            {
                let mut path = directory;
                for segment in reference.trim().split('/') {
                    path.push(segment);
                }
                if path.exists() {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
    }

    fn generate_icon() -> std::path::PathBuf {
        use std::io::Write;
        let tree = resvg::usvg::Tree::from_data(
            include_bytes!("assets/logo.svg"),
            &resvg::usvg::Options::default(),
        )
        .expect("Logo SVG is valid");
        let mut images = Vec::new();
        for size in [16u32, 24, 32, 48, 64, 128, 256] {
            let mut pixels =
                resvg::tiny_skia::Pixmap::new(size, size).expect("Icon dimensions are valid");
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(size as f32 / 128.0, size as f32 / 128.0),
                &mut pixels.as_mut(),
            );
            images.push((
                size,
                pixels.encode_png().expect("Icon PNG encoding succeeds"),
            ));
        }
        let path =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo provides OUT_DIR"))
                .join("maccanvas.ico");
        let mut file = std::fs::File::create(&path).expect("Can create the generated icon");
        file.write_all(&[0, 0, 1, 0, images.len() as u8, 0])
            .expect("Can write icon header");
        let mut offset = 6 + images.len() as u32 * 16;
        for (size, image) in &images {
            let dimension = if *size == 256 { 0 } else { *size as u8 };
            file.write_all(&[dimension, dimension, 0, 0, 1, 0, 32, 0])
                .expect("Can write icon entry");
            file.write_all(&(image.len() as u32).to_le_bytes())
                .expect("Can write icon length");
            file.write_all(&offset.to_le_bytes())
                .expect("Can write icon offset");
            offset += image.len() as u32;
        }
        for (_, image) in images {
            file.write_all(&image).expect("Can write icon image");
        }
        path
    }
}
