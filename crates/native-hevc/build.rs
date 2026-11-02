use std::{env, fs, path::PathBuf};

fn main() {
    let target = env::var("TARGET").unwrap();
    assert_eq!(
        target, "x86_64-pc-windows-msvc",
        "native-hevc currently supports Windows x64 MSVC only"
    );
    assert!(
        env::var("CARGO_CFG_TARGET_FEATURE")
            .unwrap_or_default()
            .split(',')
            .any(|feature| feature == "crt-static"),
        "native-hevc requires RUSTFLAGS=\"-C target-feature=+crt-static\" for a portable EXE"
    );

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=native");
    println!("cargo:rerun-if-changed=vendor");

    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    fs::create_dir_all(out.join("libde265")).unwrap();
    fs::write(
        out.join("config.h"),
        "#define HAVE_MALLOC_H 1\n#define HAVE_SSE4_1 1\n",
    )
    .unwrap();
    let version = fs::read_to_string(r"vendor\libde265\libde265\de265-version.h.in")
        .unwrap()
        .replace("@NUMERIC_VERSION@", "0x01010300")
        .replace("@PACKAGE_VERSION@", "1.1.3");
    fs::write(out.join("libde265").join("de265-version.h"), version).unwrap();

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .static_crt(true)
        .opt_level(2)
        .debug(false)
        .warnings(false)
        .include(&out)
        .include(r"vendor\libde265")
        .include(r"vendor\libde265\libde265")
        .define("HAVE_CONFIG_H", "1")
        .define("LIBDE265_STATIC_BUILD", "1")
        .define("NOMINMAX", "1")
        .define("_CRT_SECURE_NO_WARNINGS", "1")
        .define("NDEBUG", "1")
        .flag("/EHsc")
        .flag("/utf-8");

    for source in [
        "alloc_pool",
        "bitstream",
        "cabac",
        "contextmodel",
        "de265",
        "deblock",
        "decctx",
        "dpb",
        "fallback-dct",
        "fallback-deblk",
        "fallback-intrapred",
        "fallback-motion",
        "fallback",
        "image-io",
        "image",
        "intrapred",
        "md5",
        "motion",
        "nal-parser",
        "nal",
        "pps",
        "quality",
        "refpic",
        "sao",
        "scan",
        "sei",
        "slice",
        "sps",
        "threads",
        "transform",
        "util",
        "visualize",
        "vps",
        "vui",
    ] {
        build.file(format!(r"vendor\libde265\libde265\{source}.cc"));
    }
    for source in ["sse", "sse-dct", "sse-deblk", "sse-intrapred", "sse-motion"] {
        build.file(format!(r"vendor\libde265\libde265\x86\{source}.cc"));
    }
    build.file(r"native\bridge.cc").compile("maccanvas_de265");

    cc::Build::new()
        .static_crt(true)
        .warnings(false)
        .opt_level(2)
        .debug(false)
        .define("_CRT_SECURE_NO_WARNINGS", "1")
        .file(r"vendor\libde265\extra\win32cond.c")
        .compile("maccanvas_de265_win32");
}
