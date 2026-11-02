use native_hevc::{DecodedFrame, Decoder, Error, MAX_INPUT_BYTES};

const REXT_444: &[u8] = include_bytes!(r"data\pattern444.hevc");
const REXT_444_4K: &[u8] = include_bytes!(r"data\quadrants444_4k.hevc");
const MAIN_420: &[u8] = include_bytes!(r"data\pattern420.hevc");
const CROPPED_420: &[u8] = include_bytes!(r"data\cropped420.hevc");

fn nals(stream: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> = stream
        .windows(3)
        .enumerate()
        .filter_map(|(i, bytes)| (bytes == [0, 0, 1]).then_some(i + 3))
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let mut end = starts.get(index + 1).map_or(stream.len(), |next| next - 3);
            while end > start && stream[end - 1] == 0 {
                end -= 1;
            }
            &stream[start..end]
        })
        .collect()
}

fn annex_b(nals: &[&[u8]]) -> Vec<u8> {
    let mut result = Vec::new();
    for nal in nals {
        result.extend_from_slice(&[0, 0, 0, 1]);
        result.extend_from_slice(nal);
    }
    result
}

fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    let mut current = Vec::new();
    for nal in nals(stream) {
        if nal[0] >> 1 == 35 && !current.is_empty() {
            units.push(current);
            current = Vec::new();
        }
        current.extend_from_slice(&[0, 0, 0, 1]);
        current.extend_from_slice(nal);
    }
    if !current.is_empty() {
        units.push(current);
    }
    units
}

fn expected_pixel(x: usize, y: usize, frame: usize, full_range: bool) -> [u8; 4] {
    let luma = ((x * 3 + y * 5 + frame * 17) % 256) as f64;
    let cb = (64 + ((x / 8 * 29 + y / 8 * 17 + frame * 13) % 129)) as f64 - 128.0;
    let cr = (64 + ((x / 8 * 11 + y / 8 * 31 + frame * 19) % 129)) as f64 - 128.0;
    let (luma, cb, cr) = if full_range {
        (luma, cb, cr)
    } else {
        (
            (luma - 16.0) * 255.0 / 219.0,
            cb * 255.0 / 224.0,
            cr * 255.0 / 224.0,
        )
    };
    let byte = |value: f64| value.round().clamp(0.0, 255.0) as u8;
    [
        byte(luma + 1.5748 * cr),
        byte(luma - 0.187_324_272_930_648_8 * cb - 0.468_124_272_930_648_8 * cr),
        byte(luma + 1.8556 * cb),
        255,
    ]
}

fn assert_pattern(frame: &DecodedFrame, index: usize, width: u32, height: u32, full_range: bool) {
    assert_eq!((frame.width, frame.height), (width, height));
    assert_eq!(frame.rgba.len(), width as usize * height as usize * 4);
    for y in 0..height as usize {
        for x in 0..width as usize {
            let offset = (y * width as usize + x) * 4;
            let expected = expected_pixel(x, y, index, full_range);
            for (channel, &value) in expected.iter().enumerate() {
                let actual = frame.rgba[offset + channel];
                assert!(
                    actual.abs_diff(value) <= if channel == 3 { 0 } else { 1 },
                    "frame {index}, pixel {x},{y}, channel {channel}: got {actual}, expected {value}"
                );
            }
        }
    }
}

#[test]
fn decodes_real_rext_444_8bit_immediately_with_full_range_bt709() {
    let mut decoder = Decoder::new().unwrap();
    let frames = decoder.push(REXT_444, -9_876_543_210).unwrap();
    assert_eq!(
        frames.len(),
        1,
        "low-delay access units must not need a later picture"
    );
    assert_eq!(frames[0].pts, -9_876_543_210);
    assert_pattern(&frames[0], 0, 64, 48, true);
    assert!(decoder.flush().unwrap().is_empty());
}

#[test]
fn decodes_real_4k_444_quadrants_without_flush() {
    assert!(
        REXT_444_4K.len() <= 128 * 1024,
        "keep the synthetic fixture compact"
    );
    let mut decoder = Decoder::new().unwrap();
    let start = std::time::Instant::now();
    let frames = decoder.push(REXT_444_4K, 4_000_000).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(
        frames.len(),
        1,
        "the 4K intra picture must be emitted before EOF"
    );
    let frame = &frames[0];
    assert_eq!(
        (frame.width, frame.height, frame.pts),
        (3840, 2160, 4_000_000)
    );
    assert_eq!(frame.rgba.len(), 3840 * 2160 * 4);
    for y in [0, 31, 540, 1079, 1080, 1620, 2128, 2159] {
        for x in [0, 31, 960, 1919, 1920, 2880, 3808, 3839] {
            let expected: [u8; 4] = match (x < 1920, y < 1080) {
                (true, true) => [254, 0, 0, 255],
                (false, true) => [0, 255, 0, 255],
                (true, false) => [0, 0, 254, 255],
                (false, false) => [255, 255, 255, 255],
            };
            let offset = (y * 3840 + x) * 4;
            for (channel, &value) in expected.iter().enumerate() {
                let actual = frame.rgba[offset + channel];
                assert!(
                    actual.abs_diff(value) <= if channel == 3 { 0 } else { 1 },
                    "4K pixel {x},{y}, channel {channel}: got {actual}, expected {value}"
                );
            }
        }
    }
    assert!(decoder.flush().unwrap().is_empty());
    println!(
        "4K 4:4:4 push decoded and converted 3840x2160 in {:.3} ms; fixture={} bytes",
        elapsed.as_secs_f64() * 1000.0,
        REXT_444_4K.len()
    );
}

#[test]
fn decodes_real_main_420_b_pictures_in_display_order_and_preserves_pts() {
    let units = access_units(MAIN_420);
    assert_eq!(units.len(), 7);
    let display_index_in_decode_order = [0, 3, 2, 1, 6, 5, 4];
    let pts = [i64::MIN, -9, 0, 7, 7, 90_000, i64::MAX];
    let mut decoder = Decoder::new().unwrap();
    let mut frames = Vec::new();
    for (unit, index) in units.iter().zip(display_index_in_decode_order) {
        frames.extend(decoder.push(unit, pts[index]).unwrap());
    }
    let delayed = decoder.flush().unwrap();
    assert!(
        !delayed.is_empty(),
        "fixture must exercise delayed-picture flushing"
    );
    frames.extend(delayed);
    assert_eq!(frames.len(), 7);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(
            frame.pts, pts[index],
            "PTS must follow the reordered picture"
        );
        assert_pattern(frame, index, 64, 48, true);
    }
}

#[test]
fn decodes_multiple_access_units_in_a_push() {
    let mut decoder = Decoder::new().unwrap();
    let mut frames = decoder.push(MAIN_420, 42).unwrap();
    frames.extend(decoder.flush().unwrap());
    assert_eq!(frames.len(), 7);
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame.pts, 42);
        assert_pattern(frame, index, 64, 48, true);
    }
}

#[test]
fn decodes_cropped_limited_range_420_to_full_range_rgba() {
    let mut decoder = Decoder::new().unwrap();
    let mut frames = decoder.push(CROPPED_420, 777).unwrap();
    frames.extend(decoder.flush().unwrap());
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].pts, 777);
    assert_pattern(&frames[0], 0, 62, 46, false);
}

#[test]
fn parameter_sets_without_a_picture_are_not_a_decode_failure() {
    let sets: Vec<&[u8]> = nals(REXT_444)
        .into_iter()
        .filter(|nal| matches!(nal[0] >> 1, 32..=34))
        .collect();
    let mut decoder = Decoder::new().unwrap();
    assert!(decoder.push(&annex_b(&sets), 0).unwrap().is_empty());
    assert!(decoder.flush().unwrap().is_empty());
    assert!(decoder.flush().unwrap().is_empty());
    assert_eq!(decoder.push(REXT_444, 0), Err(Error::Flushed));
}

#[test]
fn rejects_malformed_annex_b_without_poisoning_the_decoder() {
    let mut decoder = Decoder::new().unwrap();
    for bad in [
        &[][..],
        &[0, 0, 2, 1, 9][..],
        &[9, 0, 0, 1, 0x46, 1, 0x50][..],
        &[0, 0, 1][..],
        &[0, 0, 1, 0x46, 1][..],
        &[0, 0, 1, 0xc6, 1, 0x50][..],
        &[0, 0, 1, 0x46, 0, 0x50][..],
        &[0, 0, 1, 0x47, 1, 0x50][..],
        &[0, 0, 1, 0x26, 1, 0x80][..],
    ] {
        assert!(matches!(decoder.push(bad, 0), Err(Error::InvalidInput(_))));
    }
    let frames = decoder.push(REXT_444, 99).unwrap();
    assert_eq!(frames.len(), 1);
    assert_pattern(&frames[0], 0, 64, 48, true);
}

#[test]
fn rejects_oversized_input_and_nal_counts_explicitly() {
    let mut decoder = Decoder::new().unwrap();
    let large = vec![0_u8; MAX_INPUT_BYTES + 1];
    assert_eq!(
        decoder.push(&large, 0),
        Err(Error::InputTooLarge {
            actual: MAX_INPUT_BYTES + 1,
            maximum: MAX_INPUT_BYTES
        })
    );
    drop(large);
    let many = [0, 0, 1, 0x46, 1, 0x50].repeat(4097);
    assert!(matches!(
        decoder.push(&many, 0),
        Err(Error::ResourceLimit(_))
    ));
    let mut parameter_set = vec![0xff; 65537];
    parameter_set[..6].copy_from_slice(&[0, 0, 1, 0x40, 1, 0xff]);
    parameter_set.extend_from_slice(&[0xff; 4]);
    assert!(matches!(
        decoder.push(&parameter_set, 0),
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(decoder.push(REXT_444, 0).unwrap().len(), 1);
}

#[test]
fn native_warnings_are_errors_and_poison_the_decoder() {
    let sps = nals(REXT_444)
        .into_iter()
        .find(|nal| nal[0] >> 1 == 33)
        .unwrap();
    let missing_pps = annex_b(&[sps, &[0x26, 1, 0x80]]);
    let mut decoder = Decoder::new().unwrap();
    assert!(matches!(
        decoder.push(&missing_pps, 0),
        Err(Error::Native { .. })
    ));
    assert_eq!(decoder.flush(), Err(Error::Poisoned));
    assert_eq!(decoder.push(REXT_444, 0), Err(Error::Poisoned));
}

#[test]
fn truncated_real_slice_is_not_a_successful_empty_frame() {
    let shortened: Vec<&[u8]> = nals(REXT_444)
        .into_iter()
        .map(|nal| if nal[0] >> 1 <= 31 { &nal[..8] } else { nal })
        .collect();
    let mut decoder = Decoder::new().unwrap();
    let result = decoder
        .push(&annex_b(&shortened), 0)
        .and_then(|_| decoder.flush());
    assert!(
        result.is_err(),
        "a truncated real slice must fail explicitly"
    );
    assert_eq!(decoder.flush(), Err(Error::Poisoned));
}

#[test]
fn decoder_can_move_between_threads() {
    fn assert_send<T: Send>() {}
    assert_send::<Decoder>();
    let decoder = Decoder::new().unwrap();
    let frames = std::thread::spawn(move || {
        let mut decoder = decoder;
        decoder.push(REXT_444, 123).unwrap()
    })
    .join()
    .unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].pts, 123);
}
