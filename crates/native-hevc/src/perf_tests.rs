use std::{hint::black_box, time::Instant};

use crate::{Decoder, annex_b, check_native, color, ffi};

const FOUR_K: &[u8] = include_bytes!(r"..\tests\data\quadrants444_4k.hevc");

fn milliseconds(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn profile_stages() -> (f64, f64) {
    let decoder = Decoder::new().unwrap();
    let batch = annex_b::validate(FOUR_K, false).unwrap();
    let start = Instant::now();
    for nal in batch.nals {
        check_native(unsafe {
            ffi::nh_push(decoder.native.as_ptr(), nal.as_ptr(), nal.len() as i32, 7)
        })
        .unwrap();
    }
    check_native(unsafe { ffi::nh_end_frame(decoder.native.as_ptr()) }).unwrap();
    let mut native_ms = milliseconds(start);
    let mut conversion_ms = 0.0;
    let mut pictures = 0;
    let mut done = false;
    for _ in 0..4096 {
        let start = Instant::now();
        let mut more = 0;
        check_native(unsafe { ffi::nh_decode(decoder.native.as_ptr(), &mut more) }).unwrap();
        check_native(unsafe { ffi::nh_warning(decoder.native.as_ptr()) }).unwrap();
        let image = unsafe { ffi::nh_picture(decoder.native.as_ptr()) };
        native_ms += milliseconds(start);
        if let Some(image) = unsafe { image.as_ref() } {
            let start = Instant::now();
            let frame = color::copy_frame(image).unwrap();
            conversion_ms += milliseconds(start);
            unsafe { ffi::nh_release(decoder.native.as_ptr()) };
            assert_eq!((frame.width, frame.height, frame.pts), (3840, 2160, 7));
            black_box(frame);
            pictures += 1;
        }
        if more == 0 {
            done = true;
            break;
        }
    }
    assert!(done);
    assert_eq!(pictures, 1);
    (native_ms, conversion_ms)
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

#[test]
#[ignore = "manual 4K stage profiling; use --release for comparable timings"]
fn profile_4k_stages() {
    const RUNS: usize = 11;
    let mut native = Vec::new();
    let mut conversion = Vec::new();
    let mut push = Vec::new();
    for run in 0..RUNS + 2 {
        let (native_ms, conversion_ms) = profile_stages();
        let mut decoder = Decoder::new().unwrap();
        let start = Instant::now();
        let frames = decoder.push(FOUR_K, 7).unwrap();
        let push_ms = milliseconds(start);
        assert_eq!(frames.len(), 1);
        assert!(decoder.flush().unwrap().is_empty());
        black_box(frames);
        if run >= 2 {
            native.push(native_ms);
            conversion.push(conversion_ms);
            push.push(push_ms);
        }
    }
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "4K {profile} median over {RUNS} warmed runs: native={:.3} ms; RGBA allocation/conversion={:.3} ms; public push={:.3} ms",
        median(&mut native),
        median(&mut conversion),
        median(&mut push)
    );
}
