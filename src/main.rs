#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();
mod app;

fn start() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let mut data_directory = None;
    let mut preview_theme = None;
    let mut preview_update = false;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--version" => {
                println!("MacCanvas {}", maccanvas::VERSION);
                return Ok(());
            }
            "--licenses" => {
                println!(
                    "{}\n\n{}",
                    include_str!("../LICENSE"),
                    include_str!("../THIRD_PARTY_NOTICES.txt")
                );
                return Ok(());
            }
            "--audio-component-info" => {
                println!(
                    "{}",
                    serde_json::json!({
                        "version": maccanvas::VERSION, "asset": audio_codec_tool::ASSET,
                        "sha256": audio_codec_tool::SHA256, "size": audio_codec_tool::size(),
                    })
                );
                return Ok(());
            }
            "--verify-log" => {
                let paths: Vec<_> = arguments.map(std::path::PathBuf::from).collect();
                if paths.is_empty() {
                    return Err("--verify-log requires JSONL files in chronological order".into());
                }
                let count = maccanvas::audit::verify(&paths)?;
                println!(
                    "Verified {count} JSONL records. Hash chains are not signatures or proof against a local attacker."
                );
                return Ok(());
            }
            "--help" | "-h" => {
                println!(
                    "MacCanvas {}\n\nUsage: maccanvas.exe [--data-dir DIRECTORY]\n       maccanvas.exe --version\n       maccanvas.exe --licenses\n       maccanvas.exe --audio-component-info\n       maccanvas.exe --verify-log FILE [FILE...]\n\nPasswords are entered in the window, never on the command line.",
                    maccanvas::VERSION
                );
                return Ok(());
            }
            "--data-dir" if data_directory.is_none() => {
                data_directory = Some(std::path::PathBuf::from(
                    arguments.next().ok_or("--data-dir requires a directory")?,
                ));
            }
            "--preview-theme" if cfg!(debug_assertions) && preview_theme.is_none() => {
                preview_theme = Some(match arguments.next().as_deref() {
                    Some("dark") => true,
                    Some("light") => false,
                    _ => return Err("Development theme preview must be light or dark".into()),
                });
            }
            "--preview-update" if cfg!(debug_assertions) && !preview_update => {
                preview_update = true;
            }
            _ => return Err(format!("Unsupported argument: {argument}. Use --help.").into()),
        }
    }
    if let Some(directory) = &data_directory {
        let standard = maccanvas::profiles::default_directory()?;
        let directory = std::path::absolute(directory)?
            .to_string_lossy()
            .replace('/', "\\");
        let standard = std::path::absolute(standard)?
            .to_string_lossy()
            .replace('/', "\\");
        if directory
            .trim_end_matches('\\')
            .eq_ignore_ascii_case(standard.trim_end_matches('\\'))
        {
            data_directory = None;
        }
    }
    let audit_directory = match &data_directory {
        Some(directory) => directory.join("audit"),
        None => maccanvas::audit::default_directory()?,
    };
    let mut guard = maccanvas::audit::Audit::start(audit_directory)?;
    let scope = guard.audit().scope(serde_json::json!({}));
    scope.critical("application.started", maccanvas::audit::Level::Info, serde_json::json!({
        "application.mode": if preview_theme.is_some() || preview_update { "development_preview" } else { "normal" },
        "log.file.path": guard.audit().path(),
    }))?;
    let panic_scope = scope.clone();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some("maccanvas-audit") {
            let location = info.location();
            if let Err(error) = panic_scope.critical(
                "process.panic",
                maccanvas::audit::Level::Fatal,
                serde_json::json!({
                    "code.filepath": location.map(|value| value.file()),
                    "code.lineno": location.map(|value| value.line()),
                    "code.column": location.map(|value| value.column()),
                    "thread.name": std::thread::current().name(),
                }),
            ) {
                eprintln!("MacCanvas could not persist the panic audit: {error}");
            }
        }
        previous_hook(info);
    }));
    let result = app::run(data_directory, preview_theme, preview_update, scope.clone());
    scope.critical("application.stopped", if result.is_ok() { maccanvas::audit::Level::Info } else { maccanvas::audit::Level::Error },
        serde_json::json!({"event.outcome": if result.is_ok() { "success" } else { "failure" }, "error.type": result.as_ref().err().map(maccanvas::audit::error_type)}))?;
    guard.shutdown()?;
    result?;
    Ok(())
}

fn main() {
    let terminal = std::env::args().skip(1).any(|argument| {
        matches!(
            argument.as_str(),
            "--version"
                | "--licenses"
                | "--audio-component-info"
                | "--verify-log"
                | "--help"
                | "-h"
        )
    });
    if let Err(error) = start() {
        eprintln!("MacCanvas: {error}");
        if !terminal {
            unsafe {
                use windows::{
                    Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW},
                    core::HSTRING,
                };
                MessageBoxW(
                    None,
                    &HSTRING::from(error.to_string()),
                    &HSTRING::from("MacCanvas"),
                    MB_ICONERROR | MB_OK,
                );
            }
        }
        std::process::exit(1);
    }
}
