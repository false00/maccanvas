#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();
mod app;

fn start() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let mut data_directory = None;
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
            "--help" | "-h" => {
                println!(
                    "MacCanvas {}\n\nUsage: maccanvas.exe [--data-dir DIRECTORY]\n       maccanvas.exe --version\n       maccanvas.exe --licenses\n\nPasswords are entered in the window, never on the command line.",
                    maccanvas::VERSION
                );
                return Ok(());
            }
            "--data-dir" if data_directory.is_none() => {
                data_directory = Some(std::path::PathBuf::from(
                    arguments.next().ok_or("--data-dir requires a directory")?,
                ));
            }
            _ => return Err(format!("Unsupported argument: {argument}. Use --help.").into()),
        }
    }
    app::run(data_directory)?;
    Ok(())
}

fn main() {
    if let Err(error) = start() {
        eprintln!("MacCanvas: {error}");
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
        std::process::exit(1);
    }
}
