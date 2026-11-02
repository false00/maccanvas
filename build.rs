fn main() {
    println!("cargo:rerun-if-changed=assets/logo.svg");
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new().with_style("fluent".into()),
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
