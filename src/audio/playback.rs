use std::{
    sync::{atomic::Ordering, mpsc::Sender},
    time::{Duration, Instant},
};

use serde_json::json;
use wasapi::{
    DeviceEnumerator, DeviceEventCallbacks, Direction, Role, SampleType, StreamMode, WaveFormat,
};

use crate::{
    audit::{Level, Scope},
    error::Result,
};

use super::{
    CHANNELS, MAX_RENDER_FRAMES, SAMPLE_RATE, Shared, Status, device_error, failed, report_failure,
};

struct Com;

impl Com {
    fn new() -> Result<Self> {
        wasapi::initialize_mta()
            .ok()
            .map_err(|error| crate::error::Error::Audio {
                code: Some(error.code().0),
                message: error.to_string(),
            })?;
        Ok(Self)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

pub(super) fn run(
    shared: &std::sync::Arc<Shared>,
    scope: &Scope,
    status: &Sender<Status>,
) -> Result<()> {
    let _com = Com::new()?;
    let enumerator = DeviceEnumerator::new().map_err(device_error)?;
    let mut callbacks = DeviceEventCallbacks::new();
    let changed = shared.clone();
    callbacks.set_default_device_callback(move |direction, role, _| {
        if direction == Direction::Render && role == Role::Console {
            changed.epoch.fetch_add(1, Ordering::AcqRel);
            changed.changed.notify_all();
        }
    });
    let _notifications = enumerator
        .register_notification_callback(callbacks)
        .map_err(device_error)?;
    while !shared.stopping.load(Ordering::Acquire) {
        let mut buffer = shared
            .buffer
            .lock()
            .map_err(|_| failed("Audio buffer is unavailable"))?;
        while !buffer.received && !shared.stopping.load(Ordering::Acquire) {
            buffer = shared
                .changed
                .wait(buffer)
                .map_err(|_| failed("Audio buffer is unavailable"))?;
        }
        drop(buffer);
        if shared.stopping.load(Ordering::Acquire) {
            break;
        }
        let epoch = shared.epoch.load(Ordering::Acquire);
        if let Err(error) = device_session(&enumerator, shared, scope, status, epoch) {
            report_failure(scope, status, &error);
            let mut buffer = shared
                .buffer
                .lock()
                .map_err(|_| failed("Audio buffer is unavailable"))?;
            while epoch == shared.epoch.load(Ordering::Acquire)
                && !shared.stopping.load(Ordering::Acquire)
            {
                buffer = shared
                    .changed
                    .wait(buffer)
                    .map_err(|_| failed("Audio buffer is unavailable"))?;
            }
        }
    }
    Ok(())
}

fn fill(
    client: &wasapi::AudioClient,
    render: &wasapi::AudioRenderClient,
    shared: &Shared,
) -> Result<u32> {
    let frames = client
        .get_available_space_in_frames()
        .map_err(device_error)? as usize;
    if frames > MAX_RENDER_FRAMES {
        return Err(failed("Windows returned an oversized audio buffer"));
    }
    if frames == 0 {
        return Ok(0);
    }
    let block = shared
        .buffer
        .lock()
        .map_err(|_| failed("Audio buffer is unavailable"))?
        .render(frames)?;
    render
        .write_to_device(frames, &block.bytes, None)
        .map_err(device_error)?;
    shared
        .rendered
        .fetch_add(u64::from(block.source_frames), Ordering::Relaxed);
    Ok(block.source_frames)
}

fn device_session(
    enumerator: &DeviceEnumerator,
    shared: &Shared,
    scope: &Scope,
    status: &Sender<Status>,
    epoch: u64,
) -> Result<()> {
    let device = enumerator
        .get_default_device(&Direction::Render)
        .map_err(device_error)?;
    let mut client = device.get_iaudioclient().map_err(device_error)?;
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        SAMPLE_RATE as usize,
        CHANNELS,
        None,
    );
    let (period, _) = client.get_device_period().map_err(device_error)?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: period.max(400_000),
    };
    client
        .initialize_client(&format, &Direction::Render, &mode)
        .map_err(device_error)?;
    let event = client.set_get_eventhandle().map_err(device_error)?;
    let render = client.get_audiorenderclient().map_err(device_error)?;
    let deadline = Instant::now() + Duration::from_millis(80);
    let mut buffer = shared
        .buffer
        .lock()
        .map_err(|_| failed("Audio buffer is unavailable"))?;
    while buffer.len < SAMPLE_RATE as usize * CHANNELS * 40 / 1000
        && !shared.stopping.load(Ordering::Acquire)
        && Instant::now() < deadline
    {
        let wait = deadline.saturating_duration_since(Instant::now());
        buffer = shared
            .changed
            .wait_timeout(buffer, wait)
            .map_err(|_| failed("Audio buffer is unavailable"))?
            .0;
    }
    drop(buffer);
    if shared.stopping.load(Ordering::Acquire) {
        return Ok(());
    }
    scope.critical("media.audio.output.starting", Level::Info, json!({
        "audio.sample_rate":SAMPLE_RATE, "audio.channels":CHANNELS, "audio.output":"wasapi_shared",
    }))?;
    let mut first = fill(&client, &render, shared)? > 0;
    client.start_stream().map_err(device_error)?;
    let result = (|| -> Result<()> {
        let mut reported = false;
        loop {
            if first && !reported {
                scope.critical("media.audio.output.started", Level::Info, json!({
                    "audio.sample_rate":SAMPLE_RATE, "audio.channels":CHANNELS, "audio.output":"wasapi_shared",
                }))?;
                status
                    .send(Status::Playing)
                    .map_err(|_| failed("Audio status window closed"))?;
                reported = true;
            }
            if shared.stopping.load(Ordering::Acquire)
                || epoch != shared.epoch.load(Ordering::Acquire)
            {
                break;
            }
            match event.wait_for_event(100) {
                Ok(()) => (),
                Err(wasapi::WasapiError::EventTimeout)
                    if shared.stopping.load(Ordering::Acquire)
                        || epoch != shared.epoch.load(Ordering::Acquire) =>
                {
                    break;
                }
                Err(error) => return Err(device_error(error)),
            }
            first |= fill(&client, &render, shared)? > 0;
        }
        Ok(())
    })();
    let stopped = client.stop_stream().map_err(device_error);
    if let Err(error) = &stopped {
        scope.event(
            "media.audio.output.cleanup.failed",
            Level::Error,
            crate::audit::error_attributes(error),
        )?;
    }
    result.and(stopped)
}
