use native_hevc::Decoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut decoder = Decoder::new()?;
    let mut frames = decoder.push(include_bytes!(r"..\tests\data\pattern444.hevc"), 12345)?;
    frames.extend(decoder.flush()?);
    if frames.len() != 1 {
        return Err("expected exactly one decoded sample picture".into());
    }
    let frame = &frames[0];
    if (frame.pts, frame.width, frame.height, frame.rgba.len()) != (12345, 64, 48, 12288)
        || frame.rgba.chunks_exact(4).any(|pixel| pixel[3] != 255)
    {
        return Err("invalid decoded sample picture".into());
    }
    let checksum: u64 = frame.rgba.iter().map(|&byte| u64::from(byte)).sum();
    println!(
        "{}x{} RGBA8 pts={} bytes={} checksum={checksum}",
        frame.width,
        frame.height,
        frame.pts,
        frame.rgba.len()
    );
    Ok(())
}
