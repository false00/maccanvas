fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argument = std::env::args_os()
        .nth(1)
        .ok_or("Pass a replacement executable or --check")?;
    if argument == "--check" {
        match maccanvas::updates::check()? {
            Some(release) => println!("Available: {}", release.version),
            None => println!("Current: {}", maccanvas::VERSION),
        }
    } else if argument == "--preflight" {
        let candidate = std::env::args_os()
            .nth(2)
            .ok_or("Pass a candidate executable")?;
        maccanvas::updates::validate_candidate(
            &std::path::PathBuf::from(candidate),
            &semver::Version::parse(maccanvas::VERSION)?,
        )?;
        println!("Candidate preflight: PASS");
    } else {
        self_replace::self_replace(std::path::PathBuf::from(argument))?;
    }
    Ok(())
}
