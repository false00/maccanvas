mod files_ui;

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::atomic::Ordering,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use serde_json::json;
use slint::{
    ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
    language::{PointerEventButton, PointerEventKind},
};
use uuid::Uuid;

use maccanvas::{
    audit::{Level, Scope},
    auth::Credentials,
    clipboard::native,
    credentials::{AuditedVault, CredentialSource, CredentialStore},
    error::{Error, Result},
    framebuffer::Framebuffer,
    geometry::{Extent, Viewport},
    input::{self, Command},
    profiles::{self, AuthMode, Profile, Settings},
    render::{Job, Renderer},
    session::{Event, Frames, Request, Session},
    streaming::{Encoding, StreamSettings, encoding_name, raw_estimate},
    telemetry::{self, Monitor},
    theme,
    updates::{self, Updater},
};

use crate::{App, CodecGuide, Machine, TrafficBar};

struct Reconnect {
    profile: Profile,
    credentials: CredentialSource,
}

struct State {
    settings: Settings,
    path: PathBuf,
    restart_data_directory: Option<PathBuf>,
    selected: Option<usize>,
    session: Option<Session>,
    connection_profile: Option<Profile>,
    connection_credentials: Option<CredentialSource>,
    reconnect: Option<Reconnect>,
    files: files_ui::Files,
    retired: Vec<Session>,
    audit: Scope,
    audit_reported: bool,
    audio_install: Option<mpsc::Receiver<Result<PathBuf>>>,
    audio_persist_at: Option<Instant>,
    audio_restart: bool,
    network_logged: Instant,
    clipboard: native::Watcher,
    pending_clipboard: Option<(u32, zeroize::Zeroizing<String>)>,
    generation: u64,
    frames: Frames,
    frame: Option<Arc<Framebuffer>>,
    source_size: Option<Extent>,
    viewport: Option<Viewport>,
    renderer: Renderer,
    ticket: u64,
    layout: (f32, f32, f32),
    buttons: u8,
    pointer: (u16, u16),
    pending_pointer: Option<(u16, u16)>,
    scroll: input::ScrollAccumulator,
    scroll_scale: input::ScrollScale,
    keys: HashMap<String, u32>,
    modifiers: [bool; 4],
    monitor: Monitor,
    frame_number: u64,
    last_displayed_frame: u64,
    theme_checked: Instant,
    updater: Updater,
}

impl State {
    fn persist(&self) -> Result<()> {
        self.audit.critical(
            "configuration.save.requested",
            Level::Info,
            json!({"configuration.file.path":self.path}),
        )?;
        if let Err(error) = self.settings.save(&self.path) {
            self.audit.critical(
                "configuration.save.failed",
                Level::Error,
                maccanvas::audit::error_attributes(&error),
            )?;
            return Err(error);
        }
        self.audit.event("configuration.saved", Level::Info, json!({
            "profile.count":self.settings.profiles.len(), "ui.sidebar_open":self.settings.sidebar_open,
            "ui.show_fps":self.settings.show_fps, "input.ctrl_as_command":self.settings.ctrl_as_command,
            "rfb.streaming":self.settings.streaming,
            "audio.playback":self.settings.audio,
        }))
    }

    fn send(&self, command: Command) -> Result<()> {
        self.session
            .as_ref()
            .ok_or_else(|| Error::Protocol("No active connection".into()))?
            .send(command)
    }

    fn flush_pointer(&mut self) -> Result<()> {
        if let Some((x, y)) = self.pending_pointer.take() {
            self.pointer = (x, y);
            self.send(Command::Pointer {
                x,
                y,
                buttons: self.buttons,
            })?;
        }
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        self.pending_pointer = None;
        self.scroll.clear();
        let keys: Vec<_> = self.keys.drain().map(|(_, value)| value).collect();
        let modifiers = std::mem::take(&mut self.modifiers);
        let buttons = std::mem::take(&mut self.buttons);
        if self.session.is_none() {
            return Ok(());
        }
        for keysym in keys {
            self.send(Command::Key {
                keysym,
                down: false,
            })?;
        }
        for (pressed, keysym) in
            modifiers
                .into_iter()
                .zip([input::SHIFT, input::CONTROL, input::OPTION, input::COMMAND])
        {
            if pressed {
                self.send(Command::Key {
                    keysym,
                    down: false,
                })?;
            }
        }
        if buttons != 0 {
            self.send(Command::Pointer {
                x: self.pointer.0,
                y: self.pointer.1,
                buttons: 0,
            })?;
        }
        Ok(())
    }

    fn flush_scroll(&mut self) -> Result<()> {
        let steps = self.scroll.drain();
        if !steps.is_empty() {
            self.send(Command::Scroll {
                x: self.pointer.0,
                y: self.pointer.1,
                buttons: self.buttons,
                steps,
            })?;
        }
        Ok(())
    }
}

fn notify(app: &App, error: impl std::fmt::Display) {
    app.set_error_text(error.to_string().into());
}

fn model(app: &App, state: &State) {
    let indices = filtered_indices(app, state);
    let rows: Vec<_> = state
        .settings
        .profiles
        .iter()
        .enumerate()
        .filter(|(index, _)| indices.contains(index))
        .map(|(_, profile)| Machine {
            name: profile.name.clone().into(),
            detail: format!("{}:{}", profile.host, profile.port).into(),
            saved: profile.has_saved_credentials,
        })
        .collect();
    app.set_machines(ModelRc::from(Rc::new(VecModel::from(rows))));
    app.set_selected(
        state
            .selected
            .and_then(|selected| indices.iter().position(|&index| index == selected))
            .map_or(-1, |index| index as i32),
    );
}

fn filtered_indices(app: &App, state: &State) -> Vec<usize> {
    let query = app.get_machine_filter().trim().to_lowercase();
    state
        .settings
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| {
            profile.name.to_lowercase().contains(&query)
                || profile.host.to_lowercase().contains(&query)
        })
        .map(|(index, _)| index)
        .collect()
}

fn row_index(app: &App, state: &State, index: i32) -> Result<usize> {
    usize::try_from(index)
        .ok()
        .and_then(|index| filtered_indices(app, state).get(index).copied())
        .ok_or_else(|| Error::Configuration("The selected Mac is no longer in this list".into()))
}

fn profile_from_form(app: &App, state: &State) -> Result<Profile> {
    let host = app.get_host().trim().to_owned();
    let name = app.get_machine_name().trim().to_owned();
    let port = app
        .get_port()
        .parse::<u16>()
        .map_err(|_| Error::Configuration("Port must be between 1 and 65535".into()))?;
    let authentication = match app.get_auth_mode() {
        0 => AuthMode::MacLogin,
        1 => AuthMode::VncPassword,
        _ => {
            return Err(Error::Configuration(
                "Choose a supported login method".into(),
            ));
        }
    };
    let previous = state
        .selected
        .and_then(|index| state.settings.profiles.get(index));
    if let Some(previous) = previous
        && previous.has_saved_credentials
        && (previous.host != host
            || previous.port != port
            || previous.authentication != authentication)
    {
        return Err(Error::Configuration(
                "This saved login is tied to the original address and login method. Create a new machine rather than sending it to a different endpoint.".into()
            ));
    }
    let profile = Profile {
        id: previous.map_or_else(Uuid::new_v4, |profile| profile.id),
        name: if name.is_empty() { host.clone() } else { name },
        host,
        port,
        authentication,
        has_saved_credentials: previous.is_some_and(|profile| profile.has_saved_credentials),
    };
    profile.validate()?;
    Ok(profile)
}

fn select(app: &App, state: &Rc<RefCell<State>>, index: Option<usize>) -> Result<()> {
    let profile = {
        let mut state = state.borrow_mut();
        state.selected = index;
        index
            .map(|index| {
                state.settings.profiles.get(index).cloned().ok_or_else(|| {
                    Error::Configuration("The selected machine no longer exists".into())
                })
            })
            .transpose()?
    };
    if let Some(profile) = profile {
        app.set_machine_name(profile.name.into());
        app.set_host(profile.host.into());
        app.set_port(profile.port.to_string().into());
        app.set_auth_mode(if profile.authentication == AuthMode::MacLogin {
            0
        } else {
            1
        });
        app.set_has_saved(profile.has_saved_credentials);
        app.set_use_saved(profile.has_saved_credentials);
    } else {
        app.set_machine_name("".into());
        app.set_host("".into());
        app.set_port("5900".into());
        app.set_auth_mode(0);
        app.set_has_saved(false);
        app.set_use_saved(false);
    }
    app.set_username("".into());
    app.set_password("".into());
    app.set_remember_password(false);
    app.set_editor_open(true);
    app.set_sidebar_open(true);
    app.set_confirm_delete(false);
    app.set_error_text("".into());
    model(app, &state.borrow());
    Ok(())
}

fn preferences(app: &App, state: &Rc<RefCell<State>>) -> Result<()> {
    let mut state = state.borrow_mut();
    state.settings.ctrl_as_command = app.get_ctrl_as_command();
    state.settings.sidebar_open = app.get_sidebar_open();
    state.settings.show_fps = app.get_show_fps();
    state.persist()
}

fn refresh_layout(app: &App, state: &Rc<RefCell<State>>) -> Result<()> {
    let width = app.get_canvas_width();
    let height = app.get_canvas_height();
    let dpi = app.window().scale_factor();
    if width <= 0.0 || height <= 0.0 {
        return Ok(());
    }
    let viewport = {
        let mut state = state.borrow_mut();
        let Some(size) = state.source_size else {
            return Ok(());
        };
        let viewport = Viewport::fit(size, width, height, dpi)?;
        state.layout = (width, height, dpi);
        state.viewport = Some(viewport);
        state.ticket += 1;
        if let Some(frame) = state.frame.clone() {
            state.renderer.request(Job {
                frame,
                viewport,
                generation: state.generation,
                ticket: state.ticket,
                frame_number: state.frame_number,
            })?;
        }
        viewport
    };
    app.set_draw_left(viewport.left);
    app.set_draw_top(viewport.top);
    app.set_draw_width(viewport.width);
    app.set_draw_height(viewport.height);
    app.set_desktop_description(
        format!(
            "{} x {}  |  Lanczos fit",
            viewport.source.width, viewport.source.height
        )
        .into(),
    );
    Ok(())
}

fn disconnect(app: &App, state: &Rc<RefCell<State>>) -> Result<()> {
    let result = close_stream(app, state);
    {
        let mut state = state.borrow_mut();
        state.connection_profile = None;
        state.files.stop(app);
    }
    app.set_files_open(false);
    result
}

fn close_stream(app: &App, state: &Rc<RefCell<State>>) -> Result<()> {
    let result = {
        let mut state = state.borrow_mut();
        let release = state.release();
        let session = state.session.take();
        state.generation += 1;
        state.frame = None;
        state.source_size = None;
        state.viewport = None;
        state.monitor = Monitor::new(Instant::now());
        state.frame_number = 0;
        state.last_displayed_frame = 0;
        state.connection_credentials = None;
        state.reconnect = None;
        state.audio_restart = false;
        state.clipboard = native::Watcher::default();
        state.pending_clipboard = None;
        state
            .frames
            .lock()
            .map_err(|_| Error::Display("Frame queue is unavailable".into()))?
            .take();
        let close = if let Some(session) = session {
            let recorded =
                session
                    .scope()
                    .critical("session.disconnect.requested", Level::Info, json!({}));
            let close = session.close();
            state.retired.push(session);
            recorded.and(close)
        } else {
            Ok(())
        };
        release.and(close)
    };
    app.set_connected(false);
    app.set_hevc_active(false);
    app.set_clipboard_shared(state.borrow().settings.clipboard_enabled);
    app.set_connecting(false);
    app.set_clipboard_open(false);
    app.set_audio_open(false);
    app.set_audio_status("Audio is available with HEVC.".into());
    app.set_frame_image(slint::Image::default());
    app.set_active_encoding("Waiting for pixels".into());
    app.set_status("Disconnected".into());
    app.set_desktop_description("".into());
    app.set_download_rate("0.0 kbps".into());
    app.set_upload_rate("0.0 kbps".into());
    app.set_fps_text("0 FPS".into());
    app.set_traffic(ModelRc::from(Rc::new(VecModel::from(
        vec![TrafficBar::default(); telemetry::HISTORY],
    ))));
    result
}

fn start_connection(
    app: &App,
    state: &mut State,
    profile: Profile,
    credentials: CredentialSource,
    remember: bool,
    events: &mpsc::Sender<(u64, Event)>,
) -> Result<()> {
    if state.updater.installing {
        return Err(Error::Update(
            "Finish the application update before connecting".into(),
        ));
    }
    if state.session.is_some() || state.reconnect.is_some() {
        return Err(Error::Protocol("A connection is already active".into()));
    }
    if state.retired.len() >= 8 {
        return Err(Error::Protocol(
            "Wait for the previous connections to finish closing".into(),
        ));
    }
    profile.validate()?;
    state
        .settings
        .streaming
        .validate_authentication(profile.authentication)?;
    let hwnd = window_handle(app)?;
    let reconnect_credentials = credentials.clone();
    state.generation += 1;
    state.frames = Arc::new(Mutex::new(None));
    state.monitor = Monitor::new(Instant::now());
    state.frame_number = 0;
    state.last_displayed_frame = 0;
    state.clipboard = native::Watcher::default();
    state.pending_clipboard = None;
    state.network_logged = Instant::now();
    state.files.reset_network();
    let request = Request {
        audit: state.audit.child(json!({
            "profile.id":profile.id, "server.address":profile.host, "server.port":profile.port,
            "vnc.session.generation":state.generation,
        })),
        clipboard: state.settings.clipboard_enabled,
        profile: profile.clone(),
        credentials,
        streaming: state.settings.streaming,
        audio: state.settings.audio,
        remember,
        window: hwnd,
        generation: state.generation,
    };
    let session = Session::start(request, events.clone(), state.frames.clone())?;
    state.connection_profile = Some(profile);
    state.connection_credentials = Some(reconnect_credentials);
    state.session = Some(session);
    app.set_error_text("".into());
    app.set_connecting(true);
    app.set_status("Verifying and connecting...".into());
    Ok(())
}

fn apply_streaming(
    app: &App,
    state: &Rc<RefCell<State>>,
    settings: StreamSettings,
    restart_audio: bool,
) -> Result<()> {
    if app.get_connecting() {
        return Err(Error::Configuration(
            "Finish or cancel the current connection before applying stream settings".into(),
        ));
    }
    let reconnect = {
        let mut state = state.borrow_mut();
        let reconnect = if state
            .session
            .as_ref()
            .is_some_and(|session| session.requires_reconnect(settings) || restart_audio)
        {
            let profile = state.connection_profile.clone().ok_or_else(|| {
                Error::Protocol("The active connection profile is unavailable".into())
            })?;
            settings.validate_authentication(profile.authentication)?;
            let credentials = state.connection_credentials.clone().ok_or_else(|| {
                Error::Credential("The active connection cannot be restarted".into())
            })?;
            state.audit.critical(
                "session.reconnect.requested",
                Level::Info,
                json!({
                    "profile.id":profile.id, "rfb.streaming.previous":state.settings.streaming,
                    "rfb.streaming":settings, "vnc.session.generation":state.generation,
                    "session.reconnect.reason":if restart_audio { "audio" } else { "stream_settings" },
                }),
            )?;
            Some(Reconnect {
                profile,
                credentials,
            })
        } else {
            None
        };
        let previous = state.settings.streaming;
        state.settings.streaming = settings;
        if let Err(error) = state.persist() {
            state.settings.streaming = previous;
            return Err(error);
        }
        if reconnect.is_none()
            && let Some(session) = &state.session
        {
            session.send(Command::Configure(settings))?;
        }
        reconnect
    };
    if let Some(reconnect) = reconnect {
        close_stream(app, state)?;
        state.borrow_mut().reconnect = Some(reconnect);
        app.set_connecting(true);
        app.set_status(format!("Switching to {}...", settings.encoding.label()).into());
    }
    stream_summary(app, settings);
    app.set_quality_open(false);
    app.set_error_text("".into());
    Ok(())
}

fn tick_reconnect(
    app: &App,
    state: &Rc<RefCell<State>>,
    events: &mpsc::Sender<(u64, Event)>,
) -> Result<()> {
    let pending = {
        let mut state = state.borrow_mut();
        if state.retired.is_empty() {
            state.reconnect.take()
        } else {
            None
        }
    };
    if let Some(pending) = pending {
        let result = start_connection(
            app,
            &mut state.borrow_mut(),
            pending.profile,
            pending.credentials,
            false,
            events,
        );
        if let Err(error) = result {
            disconnect(app, state)?;
            return Err(error);
        }
    }
    Ok(())
}

fn paste_windows(app: &App, state: &mut State) -> Result<()> {
    if state.session.as_ref().is_some_and(Session::uses_hevc) {
        return Err(Error::Clipboard(
            "Use the encrypted SSH clipboard in Files while using HEVC".into(),
        ));
    }
    let owner = window_handle(app)?;
    if let Some((sequence, text)) = state.clipboard.paste(owner)? {
        let session = state
            .session
            .as_ref()
            .ok_or_else(|| Error::Clipboard("Connect to your Mac first".into()))?;
        session.send_clipboard(text)?;
        state.clipboard.sent(sequence);
    }
    Ok(())
}

fn tick_clipboard(app: &App, state: &mut State) -> Result<()> {
    let Some(session) = &state.session else {
        return Ok(());
    };
    if session.uses_hevc() {
        app.set_clipboard_mode("HEVC uses the encrypted SSH clipboard in Files.".into());
        return Ok(());
    }
    app.set_clipboard_mode(
        if session.clipboard_utf8() {
            "UTF-8 negotiated: Unicode plain text is available."
        } else {
            "Standard VNC: Latin-1 text only. Use SSH for other Unicode characters."
        }
        .into(),
    );
    if !state.settings.clipboard_enabled {
        return Ok(());
    }
    if let Some(text) = session.take_clipboard()? {
        state.pending_clipboard = Some((native::sequence(), text));
    }
    let owner = window_handle(app)?;
    if !native::foreground(owner) || !app.get_canvas_focused() || app.get_modal_open() {
        return Ok(());
    }
    if let Some((sequence, text)) = state.pending_clipboard.take() {
        if sequence != native::sequence() {
            session.scope().event(
                "clipboard.superseded",
                Level::Info,
                json!({"clipboard.reason":"windows_clipboard_changed"}),
            )?;
        } else {
            state.clipboard.receive(owner, &text)?;
            session.scope().event(
                "clipboard.applied",
                Level::Info,
                json!({
                    "clipboard.direction":"mac_to_windows", "clipboard.text_bytes":text.len(),
                }),
            )?;
        }
    }
    Ok(())
}

fn stream_form(app: &App, settings: StreamSettings) {
    app.set_encoding_index(settings.encoding.index());
    app.set_compression_level(f32::from(settings.compression));
    app.set_jpeg_quality(f32::from(settings.jpeg_quality));
    app.set_manual_bandwidth(settings.target_kbps != 0);
    app.set_target_mbps(
        if settings.target_kbps == 0 {
            "10".to_owned()
        } else {
            (f64::from(settings.target_kbps) / 1000.0).to_string()
        }
        .into(),
    );
    app.set_max_fps(settings.max_fps.to_string().into());
    app.set_encoding_description(settings.encoding.description().into());
}

fn read_stream_form(app: &App) -> Result<StreamSettings> {
    if !app.get_compression_level().is_finite()
        || !app.get_jpeg_quality().is_finite()
        || !(0.0..=9.0).contains(&app.get_compression_level())
        || !(0.0..=9.0).contains(&app.get_jpeg_quality())
    {
        return Err(Error::Configuration(
            "Compression and JPEG quality must be between 0 and 9".into(),
        ));
    }
    let encoding = Encoding::from_index(app.get_encoding_index())?;
    let target_kbps = if app.get_manual_bandwidth() && !encoding.is_hevc() {
        let value = app.get_target_mbps().parse::<f64>().map_err(|_| {
            Error::Configuration("Enter a bandwidth target in Mbps, for example 10 or 0.5".into())
        })?;
        if !value.is_finite() || !(0.1..=1000.0).contains(&value) {
            return Err(Error::Configuration(
                "Bandwidth target must be 0.1-1000 Mbps".into(),
            ));
        }
        (value * 1000.0).round() as u32
    } else {
        0
    };
    let settings = StreamSettings {
        encoding,
        compression: app.get_compression_level().round() as u8,
        jpeg_quality: app.get_jpeg_quality().round() as u8,
        target_kbps,
        max_fps: app
            .get_max_fps()
            .parse::<u16>()
            .map_err(|_| Error::Configuration("Maximum update rate must be 1-120".into()))?,
    };
    settings.validate()?;
    Ok(settings)
}

fn stream_summary(app: &App, settings: StreamSettings) {
    let target = if settings.encoding.is_hevc() {
        "Adaptive video bitrate".into()
    } else if settings.target_kbps == 0 {
        "Automatic (no target)".into()
    } else {
        format!(
            "{} target",
            telemetry::rate(f64::from(settings.target_kbps) * 1000.0)
        )
    };
    app.set_bandwidth_description(
        format!(
            "{}  |  {}  |  Up to {} updates/s",
            settings.encoding.label(),
            target,
            settings.max_fps.min(settings.encoding.fps_ceiling())
        )
        .into(),
    );
}

fn guide(app: &App, size: Option<Extent>) {
    let (width, height) = size.map_or((3840, 2160), |size| (size.width, size.height));
    let baseline = raw_estimate(width, height, 30) * 0.1;
    app.set_estimate_context(format!(
        "Planning illustration, not benchmarks: {width} x {height}, 30 updates/s, 10% of pixels changing each time. Raw is {}. The ranges use the explicit example ratios below; real usage depends on content, the Mac, and motion. Video/noise can exceed them. Idle screens use almost no pixel traffic.",
        telemetry::rate(baseline)
    ).into());
    let rows = Encoding::ALL
        .into_iter()
        .map(|encoding| {
            if encoding.is_hevc() {
                return CodecGuide {
                    name: encoding.label().into(),
                    detail: encoding.description().into(),
                    estimate: "Adaptive HEVC video: use the live traffic meter. 60 FPS generally costs more bandwidth and CPU than 30 FPS; VNC rectangle-compression ratios do not apply.".into(),
                };
            }
            let (low, high) = match encoding {
                Encoding::Auto | Encoding::Zrle => (2.0, 12.0),
                Encoding::Tight => (3.0, 20.0),
                Encoding::TightJpeg => (5.0, 50.0),
                Encoding::Hextile => (1.0, 4.0),
                Encoding::Zlib => (2.0, 6.0),
                Encoding::Rre | Encoding::Corre => (0.5, 4.0),
                Encoding::Raw => (1.0, 1.0),
                Encoding::Hevc30 | Encoding::Hevc60 => unreachable!("handled HEVC guide"),
            };
            CodecGuide {
                name: encoding.label().into(),
                detail: encoding.description().into(),
                estimate: format!(
                    "Illustration: {} - {}, assuming {low}x - {high}x size reduction.",
                    telemetry::rate(baseline / high),
                    telemetry::rate(baseline / low)
                )
                .into(),
            }
        })
        .collect::<Vec<_>>();
    app.set_codec_guide(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn window_handle(app: &App) -> Result<isize> {
    let provider = app.window().window_handle();
    let handle = provider
        .window_handle()
        .map_err(|error| Error::Consent(error.to_string()))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get()),
        _ => Err(Error::Consent("A native Windows window is required".into())),
    }
}

fn tick_audio(app: &App, state: &Rc<RefCell<State>>) -> Result<()> {
    let result = {
        let mut state = state.borrow_mut();
        if state
            .audio_persist_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            state.audio_persist_at = None;
            state.persist()?;
        }
        match state.audio_install.as_ref().map(mpsc::Receiver::try_recv) {
            Some(Ok(result)) => {
                state.audio_install = None;
                Some(result)
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                state.audio_install = None;
                Some(Err(Error::Audio {
                    code: None,
                    message: "Audio installation stopped unexpectedly".into(),
                }))
            }
            _ => None,
        }
    };
    if let Some(result) = result {
        app.set_audio_installing(false);
        match result {
            Ok(_) => {
                app.set_audio_codec_missing(false);
                app.set_audio_status("Audio codec installed.".into());
                if app.get_connected() && app.get_hevc_active() {
                    let settings = state.borrow().settings.streaming;
                    apply_streaming(app, state, settings, true)?;
                } else if app.get_connecting() {
                    state.borrow_mut().audio_restart = true;
                }
            }
            Err(error) => {
                state.borrow().audit.critical(
                    "component.audio.install.failed",
                    Level::Error,
                    maccanvas::audit::error_attributes(&error),
                )?;
                app.set_audio_status(error.to_string().into());
                app.set_audio_codec_missing(true);
                return Err(error);
            }
        }
    }
    Ok(())
}

fn tick_updates(app: &App, state: &Rc<RefCell<State>>) -> Result<()> {
    loop {
        let event = state.borrow().updater.event()?;
        let Some(event) = event else { break };
        let mut state = state.borrow_mut();
        match event {
            updates::Event::Checked(result) => {
                state.updater.checking = false;
                app.set_update_checking(false);
                match result {
                    Ok(Some(release)) => {
                        app.set_update_available(true);
                        app.set_update_version(release.version.to_string().into());
                        app.set_update_status("A new version is available. The download will be verified before installation. Saved Macs and logins stay.".into());
                        app.set_update_banner(true);
                        state.updater.release = Some(release);
                    }
                    Ok(None) => {
                        state.updater.release = None;
                        app.set_update_available(false);
                        app.set_update_banner(false);
                        app.set_update_status(
                            format!("You're up to date ({}).", maccanvas::VERSION).into(),
                        );
                    }
                    Err(error) => {
                        app.set_update_status(
                            format!("Could not check for updates. {error}").into(),
                        );
                        app.set_update_banner(true);
                    }
                }
            }
            updates::Event::Installed(result) => {
                state.updater.installing = false;
                app.set_update_installing(false);
                match result {
                    Ok(executable) => {
                        let mut command = std::process::Command::new(executable);
                        if let Some(directory) = &state.restart_data_directory {
                            command.arg("--data-dir").arg(directory);
                        }
                        match command.spawn() {
                            Ok(_) => slint::quit_event_loop()
                                .map_err(|error| Error::Update(error.to_string()))?,
                            Err(error) => {
                                app.set_update_available(false);
                                state.updater.restart_required = true;
                                app.set_update_restart_required(true);
                                app.set_update_status(format!("The new version is installed, but could not restart: {error}. Close and reopen MacCanvas.").into());
                                app.set_update_banner(true);
                            }
                        }
                    }
                    Err(error) => {
                        if matches!(&error, Error::UpdateRecovery(_)) {
                            state.updater.restart_required = true;
                            app.set_update_restart_required(true);
                        }
                        app.set_update_status(
                            format!("Could not finish the update. {error}").into(),
                        );
                        app.set_update_banner(true);
                    }
                }
            }
        }
    }
    let state = state.borrow();
    if state.updater.installing {
        let size = state
            .updater
            .release
            .as_ref()
            .map_or(1, |release| release.size.max(1));
        let received = state.updater.progress.load(Ordering::Relaxed);
        let fraction = (received as f64 / size as f64).min(1.0);
        app.set_update_progress(fraction as f32);
        app.set_update_status(
            if received < size {
                format!("Downloading {:.0}%...", fraction * 100.0)
            } else {
                "Verifying and replacing the executable...".into()
            }
            .into(),
        );
    }
    Ok(())
}

fn keysym(text: &str, control: bool) -> Option<u32> {
    use slint::platform::Key;
    let special = [
        (Key::Backspace, 0xff08),
        (Key::Tab, 0xff09),
        (Key::Return, 0xff0d),
        (Key::Escape, 0xff1b),
        (Key::Delete, 0xffff),
        (Key::Home, 0xff50),
        (Key::End, 0xff57),
        (Key::LeftArrow, 0xff51),
        (Key::UpArrow, 0xff52),
        (Key::RightArrow, 0xff53),
        (Key::DownArrow, 0xff54),
        (Key::PageUp, 0xff55),
        (Key::PageDown, 0xff56),
        (Key::Insert, 0xff63),
        (Key::F1, 0xffbe),
        (Key::F2, 0xffbf),
        (Key::F3, 0xffc0),
        (Key::F4, 0xffc1),
        (Key::F5, 0xffc2),
        (Key::F6, 0xffc3),
        (Key::F7, 0xffc4),
        (Key::F8, 0xffc5),
        (Key::F9, 0xffc6),
        (Key::F10, 0xffc7),
        (Key::F11, 0xffc8),
        (Key::F12, 0xffc9),
    ];
    for (key, symbol) in special {
        let character: char = key.into();
        if text == character.to_string() {
            return Some(symbol);
        }
    }
    let mut characters = text.chars();
    let character = characters.next()?;
    if characters.next().is_some() {
        return None;
    }
    let code = character as u32;
    if control && (1..=26).contains(&code) {
        return Some(u32::from(b'a') + code - 1);
    }
    if character.is_control() || (0xe000..=0xf8ff).contains(&code) {
        return None;
    }
    Some(input::character_keysym(character))
}

pub fn run(
    data_directory: Option<PathBuf>,
    preview_theme: Option<bool>,
    preview_update: bool,
    audit: Scope,
) -> Result<()> {
    let restart_data_directory = data_directory.clone();
    let directory = match data_directory {
        Some(directory) => directory,
        None => profiles::default_directory()?,
    };
    let path = directory.join("settings.json");
    let settings = Settings::load(&path)?;
    let app = App::new().map_err(|error| Error::Display(error.to_string()))?;
    app.set_dark_theme(match preview_theme {
        Some(dark) => dark,
        None => theme::system_dark()?,
    });
    app.set_motion_enabled(theme::motion_enabled()?);
    app.set_glass_enabled(theme::glass_enabled()?);
    app.set_ctrl_as_command(settings.ctrl_as_command);
    app.set_sidebar_open(settings.sidebar_open || settings.profiles.is_empty());
    app.set_show_fps(settings.show_fps);
    app.set_clipboard_shared(settings.clipboard_enabled);
    app.set_audio_muted(settings.audio.muted);
    app.set_audio_volume(f32::from(settings.audio.volume));
    match maccanvas::audio::codec_available() {
        Ok(path) => app.set_audio_codec_missing(path.is_none()),
        Err(error) => {
            app.set_audio_codec_missing(true);
            app.set_audio_status(error.to_string().into());
            notify(&app, error);
        }
    }
    app.set_editor_open(settings.profiles.is_empty());
    app.set_app_version(maccanvas::VERSION.into());
    app.set_build_date(maccanvas::BUILD_UTC.into());
    app.set_build_revision(maccanvas::REVISION.into());
    app.set_build_target(maccanvas::TARGET.into());
    app.set_build_compiler(maccanvas::RUSTC.into());
    app.set_author_name(maccanvas::AUTHOR.into());
    app.set_log_path(audit.audit().path().to_string_lossy().as_ref().into());
    if let Err(error) = updates::maintain_shortcuts(&audit) {
        notify(
            &app,
            format!("Could not refresh application shortcuts. {error}"),
        );
    }
    let mut updater = Updater::new();
    #[cfg(debug_assertions)]
    if preview_update {
        updater.preview_available()?;
    }
    if preview_theme.is_none() && !(cfg!(debug_assertions) && preview_update) {
        updater.check(&audit)?;
    } else {
        app.set_update_status("Checks for updates on every launch.".into());
    }
    app.set_update_checking(updater.checking);
    app.set_traffic(ModelRc::from(Rc::new(VecModel::from(
        vec![TrafficBar::default(); telemetry::HISTORY],
    ))));
    stream_form(&app, settings.streaming);
    stream_summary(&app, settings.streaming);
    guide(&app, None);
    let (events, receiver) = mpsc::channel::<(u64, Event)>();
    let state = Rc::new(RefCell::new(State {
        settings,
        path,
        restart_data_directory,
        selected: None,
        session: None,
        connection_profile: None,
        connection_credentials: None,
        reconnect: None,
        files: files_ui::Files::default(),
        retired: Vec::new(),
        audit,
        audit_reported: false,
        audio_install: None,
        audio_persist_at: None,
        audio_restart: false,
        network_logged: Instant::now(),
        clipboard: native::Watcher::default(),
        pending_clipboard: None,
        generation: 0,
        frames: Arc::new(Mutex::new(None)),
        frame: None,
        source_size: None,
        viewport: None,
        renderer: Renderer::new(),
        ticket: 0,
        layout: (0.0, 0.0, 0.0),
        buttons: 0,
        pointer: (0, 0),
        pending_pointer: None,
        scroll: input::ScrollAccumulator::default(),
        scroll_scale: input::ScrollScale::system()?,
        keys: HashMap::new(),
        modifiers: [false; 4],
        monitor: Monitor::new(Instant::now()),
        frame_number: 0,
        last_displayed_frame: 0,
        theme_checked: Instant::now(),
        updater,
    }));
    model(&app, &state.borrow());
    let weak = app.as_weak();
    let connection_state = state.clone();
    let connection_events = events.clone();
    app.on_connect_requested(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = connection_state.borrow_mut();
            let profile = profile_from_form(&app, &state)?;
            let source = if app.get_use_saved() {
                CredentialSource::Saved
            } else {
                let credentials = Credentials::new(
                    app.get_username().to_string(),
                    app.get_password().to_string(),
                );
                credentials.validate(profile.authentication)?;
                CredentialSource::Manual(credentials)
            };
            start_connection(
                &app,
                &mut state,
                profile,
                source,
                app.get_remember_password() && !app.get_use_saved(),
                &connection_events,
            )
        })();
        app.set_password("".into());
        app.set_username("".into());
        match result {
            Ok(()) => {
                app.set_error_text("".into());
                app.set_connecting(true);
                app.set_status("Verifying and connecting...".into());
            }
            Err(error) => notify(&app, error),
        }
    });
    let weak = app.as_weak();
    let check_state = state.clone();
    app.on_check_updates(move || {
        let Some(app) = weak.upgrade() else { return };
        let mut state = check_state.borrow_mut();
        if state.updater.release.is_some() && !state.updater.checking && !state.updater.installing {
            app.set_update_banner(true);
            return;
        }
        let audit = state.audit.clone();
        if let Err(error) = state.updater.check(&audit) {
            notify(&app, error);
            return;
        }
        app.set_update_checking(state.updater.checking);
        app.set_update_status("Checking the latest official release...".into());
    });
    let weak = app.as_weak();
    let update_state = state.clone();
    app.on_install_update(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = update_state.borrow_mut();
            if state.session.is_some() || state.reconnect.is_some() || state.files.active() || state.audio_install.is_some() {
                return Err(Error::Update(
                    "Disconnect from the Mac and wait for SSH cleanup before updating and restarting".into(),
                ));
            }
            state.persist()?;
            let audit = state.audit.clone();
            state.updater.install(&audit)
        })();
        match result {
            Ok(()) => {
                app.set_update_installing(true);
                app.set_update_progress(0.0);
                app.set_update_status("Starting the verified download...".into());
            }
            Err(error) => {
                app.set_update_status(error.to_string().into());
                app.set_update_banner(true);
            }
        }
    });
    let weak = app.as_weak();
    app.on_dismiss_update(move || {
        if let Some(app) = weak.upgrade() {
            app.set_update_banner(false);
        }
    });
    let weak = app.as_weak();
    let about_state = state.clone();
    app.on_about_link(move |index| {
        if let Some(app) = weak.upgrade() {
            let result = (|| -> Result<()> {
                about_state.borrow().audit.event(
                    "ui.about.link_opened",
                    Level::Info,
                    json!({"ui.link.index":index}),
                )?;
                maccanvas::links::open_about_link(index)
            })();
            if let Err(error) = result {
                notify(&app, error);
            }
        }
    });
    let weak = app.as_weak();
    let log_state = state.clone();
    app.on_open_logs(move || {
        if let Some(app) = weak.upgrade() {
            let result = (|| -> Result<()> {
                let state = log_state.borrow();
                let path = state.audit.audit().path();
                let audit = state.audit.clone();
                drop(state);
                let recorded = audit.event("audit.folder.open_requested", Level::Info, json!({}));
                let opened = maccanvas::links::open_directory(
                    path.parent()
                        .ok_or_else(|| Error::Audit("Missing audit directory".into()))?,
                );
                opened.and(recorded)
            })();
            if let Err(error) = result {
                notify(&app, error);
            }
        }
    });
    let weak = app.as_weak();
    let clipboard_state = state.clone();
    app.on_clipboard_changed(move || {
        if let Some(app) = weak.upgrade() {
            let result = (|| -> Result<()> {
                let mut state = clipboard_state.borrow_mut();
                let enabled = app.get_clipboard_shared();
                if let Some(session) = &state.session {
                    session.share_clipboard(enabled)?;
                }
                state.settings.clipboard_enabled = enabled;
                state.pending_clipboard = None;
                state.clipboard = native::Watcher::default();
                state.persist()
            })();
            if let Err(error) = result {
                notify(&app, error);
            }
        }
    });
    let weak = app.as_weak();
    let paste_state = state.clone();
    app.on_paste_windows_text(move || {
        if let Some(app) = weak.upgrade() {
            let result = (|| -> Result<()> {
                let mut state = paste_state.borrow_mut();
                paste_windows(&app, &mut state)?;
                state.release()?;
                for command in [
                    Command::Key {
                        keysym: input::COMMAND,
                        down: true,
                    },
                    Command::Key {
                        keysym: u32::from(b'v'),
                        down: true,
                    },
                    Command::Key {
                        keysym: u32::from(b'v'),
                        down: false,
                    },
                    Command::Key {
                        keysym: input::COMMAND,
                        down: false,
                    },
                ] {
                    state.send(command)?;
                }
                app.set_clipboard_open(false);
                app.invoke_focus_canvas();
                Ok(())
            })();
            if let Err(error) = result {
                notify(&app, error);
            }
        }
    });
    let close_state = state.clone();
    app.window().on_close_requested(move || {
        if close_state.borrow().updater.installing || close_state.borrow().audio_install.is_some() {
            slint::CloseRequestResponse::KeepWindowShown
        } else {
            slint::CloseRequestResponse::HideWindow
        }
    });
    let weak = app.as_weak();
    let disconnect_state = state.clone();
    app.on_disconnect_requested(move || {
        if let Some(app) = weak.upgrade()
            && let Err(error) = disconnect(&app, &disconnect_state)
        {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let new_state = state.clone();
    app.on_new_machine(move || {
        if let Some(app) = weak.upgrade()
            && let Err(error) = select(&app, &new_state, None)
        {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let selected_state = state.clone();
    app.on_select_machine(move |index| {
        if let Some(app) = weak.upgrade() {
            let selected = row_index(&app, &selected_state.borrow(), index);
            match selected {
                Ok(index) => {
                    if let Err(error) = select(&app, &selected_state, Some(index)) {
                        notify(&app, error);
                    }
                }
                Err(error) => notify(&app, error),
            }
        }
    });
    let weak = app.as_weak();
    let quick_state = state.clone();
    app.on_quick_connect(move |index| {
        let Some(app) = weak.upgrade() else { return };
        let index = row_index(&app, &quick_state.borrow(), index);
        match index.and_then(|index| select(&app, &quick_state, Some(index))) {
            Ok(()) if app.get_has_saved() => app.invoke_connect_requested(),
            Ok(()) => (),
            Err(error) => notify(&app, error),
        }
    });
    let weak = app.as_weak();
    let filter_state = state.clone();
    app.on_filter_machines(move || {
        if let Some(app) = weak.upgrade() {
            model(&app, &filter_state.borrow());
        }
    });
    let weak = app.as_weak();
    app.on_quality_changed(move || {
        if let Some(app) = weak.upgrade() {
            match Encoding::from_index(app.get_encoding_index()) {
                Ok(encoding) => {
                    app.set_encoding_description(encoding.description().into());
                    if app.get_quality_open() && encoding.is_hevc() {
                        app.set_max_fps(encoding.fps_ceiling().to_string().into());
                        app.set_manual_bandwidth(false);
                    }
                }
                Err(error) => notify(&app, error),
            }
        }
    });
    let weak = app.as_weak();
    let quality_state = state.clone();
    app.on_apply_streaming(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = read_stream_form(&app)
            .and_then(|settings| apply_streaming(&app, &quality_state, settings, false));
        if let Err(error) = result {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    app.on_reset_streaming(move || {
        if let Some(app) = weak.upgrade() {
            stream_form(&app, StreamSettings::default());
        }
    });
    let weak = app.as_weak();
    let save_state = state.clone();
    app.on_save_machine(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = save_state.borrow_mut();
            let profile = profile_from_form(&app, &state)?;
            if let Some(index) = state.selected {
                state.settings.profiles[index] = profile;
            } else {
                state.settings.profiles.push(profile);
                state.selected = Some(state.settings.profiles.len() - 1);
            }
            state.persist()
        })();
        match result {
            Ok(()) => {
                model(&app, &save_state.borrow());
                app.set_status("Machine saved; no password was written to a file".into());
            }
            Err(error) => notify(&app, error),
        }
    });
    let weak = app.as_weak();
    let delete_state = state.clone();
    app.on_delete_machine(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = delete_state.borrow_mut();
            let index = state
                .selected
                .ok_or_else(|| Error::Configuration("Select a machine first".into()))?;
            let id = state.settings.profiles[index].id;
            (AuditedVault {
                scope: &state.audit,
            })
            .delete(id)?;
            state.settings.profiles.remove(index);
            state.selected = None;
            state
                .audit
                .critical("profile.removed", Level::Info, json!({"profile.id":id}))?;
            state.persist()
        })();
        match result {
            Ok(()) => {
                if let Err(error) = select(&app, &delete_state, None) {
                    notify(&app, error);
                } else {
                    app.set_editor_open(false);
                }
            }
            Err(error) => notify(&app, error),
        }
    });
    let weak = app.as_weak();
    let preference_state = state.clone();
    app.on_save_preferences(move || {
        if let Some(app) = weak.upgrade()
            && let Err(error) = preferences(&app, &preference_state)
        {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let layout_state = state.clone();
    app.on_canvas_resized(move || {
        if let Some(app) = weak.upgrade()
            && let Err(error) = refresh_layout(&app, &layout_state)
        {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let refresh_state = state.clone();
    app.on_refresh_desktop(move || {
        if let Some(app) = weak.upgrade() {
            let state = refresh_state.borrow();
            if let Some(size) = state.source_size
                && let Err(error) = state.send(Command::Refresh {
                    size,
                    incremental: false,
                })
            {
                notify(&app, error);
            }
        }
    });
    let weak = app.as_weak();
    app.on_fullscreen_requested(move || {
        if let Some(app) = weak.upgrade() {
            let fullscreen = !app.window().is_fullscreen();
            app.set_immersive(fullscreen);
            app.window().set_fullscreen(fullscreen);
        }
    });
    let weak = app.as_weak();
    let release_state = state.clone();
    app.on_release_input(move || {
        if let Some(app) = weak.upgrade()
            && let Err(error) = release_state.borrow_mut().release()
        {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let pointer_state = state.clone();
    app.on_pointer_event(move |x, y, event| {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = pointer_state.borrow_mut();
            let Some(viewport) = state.viewport else {
                return Ok(());
            };
            if event.kind == PointerEventKind::Cancel {
                return state.release();
            }
            let button = match event.button {
                PointerEventButton::Left => 1,
                PointerEventButton::Middle => 2,
                PointerEventButton::Right => 4,
                _ => 0,
            };
            let position = viewport.to_remote(x, y, state.buttons != 0);
            if let Some((x, y)) = position {
                if event.kind == PointerEventKind::Move {
                    state.pending_pointer = Some((x, y));
                } else if button != 0 {
                    state.flush_pointer()?;
                    if event.kind == PointerEventKind::Down {
                        state.buttons |= button;
                    }
                    if event.kind == PointerEventKind::Up {
                        state.buttons &= !button;
                    }
                    state.pointer = (x, y);
                    state.send(Command::Pointer {
                        x,
                        y,
                        buttons: state.buttons,
                    })?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let wheel_state = state.clone();
    app.on_wheel_event(move |x, y, event| {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = wheel_state.borrow_mut();
            let Some(viewport) = state.viewport else {
                return Ok(());
            };
            let Some((x, y)) = viewport.to_remote(x, y, false) else {
                return Ok(());
            };
            state.flush_pointer()?;
            state.pointer = (x, y);
            let scale = state.scroll_scale;
            state.scroll.add(event.delta_x, event.delta_y, scale)?;
            state.flush_scroll()
        })();
        if let Err(error) = result {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let key_state = state.clone();
    app.on_keyboard_event(move |event, down| {
        let Some(app) = weak.upgrade() else {
            return false;
        };
        if !app.get_connected() || app.get_modal_open() {
            return false;
        }
        let result = (|| -> Result<()> {
            let mut state = key_state.borrow_mut();
            state.flush_pointer()?;
            if down
                && app.get_clipboard_shared()
                && (event.modifiers.meta || (event.modifiers.control && app.get_ctrl_as_command()))
                && !event.modifiers.alt
                && matches!(event.text.as_str(), "v" | "V" | "\u{16}")
            {
                paste_windows(&app, &mut state)?;
            }
            let modifiers = [
                event.modifiers.shift,
                event.modifiers.control && !app.get_ctrl_as_command(),
                event.modifiers.alt,
                event.modifiers.meta || (event.modifiers.control && app.get_ctrl_as_command()),
            ];
            for (index, keysym) in [input::SHIFT, input::CONTROL, input::OPTION, input::COMMAND]
                .into_iter()
                .enumerate()
            {
                if state.modifiers[index] != modifiers[index] {
                    state.send(Command::Key {
                        keysym,
                        down: modifiers[index],
                    })?;
                    state.modifiers[index] = modifiers[index];
                }
            }
            if let Some(symbol) = keysym(event.text.as_str(), event.modifiers.control) {
                let identifier = if symbol <= 255 {
                    char::from_u32(symbol)
                        .unwrap_or('\0')
                        .to_ascii_lowercase()
                        .to_string()
                } else {
                    event.text.to_string()
                };
                if down {
                    state.keys.insert(identifier, symbol);
                    state.send(Command::Key {
                        keysym: symbol,
                        down: true,
                    })?;
                } else if let Some(symbol) = state.keys.remove(&identifier) {
                    state.send(Command::Key {
                        keysym: symbol,
                        down: false,
                    })?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            notify(&app, error);
        }
        true
    });
    let weak = app.as_weak();
    let audio_state = state.clone();
    app.on_audio_changed(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let volume = app.get_audio_volume();
            if !volume.is_finite() || !(0.0..=100.0).contains(&volume) {
                return Err(Error::Configuration(
                    "Audio volume must be between 0 and 100".into(),
                ));
            }
            let settings = maccanvas::audio::Settings {
                muted: app.get_audio_muted(),
                volume: volume.round() as u8,
            };
            settings.validate()?;
            let mut state = audio_state.borrow_mut();
            state.settings.audio = settings;
            state.audio_persist_at = Some(Instant::now() + Duration::from_millis(300));
            if let Some(session) = &state.session
                && session.uses_hevc()
            {
                session.send(Command::Audio(settings))?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let install_audio_state = state.clone();
    app.on_audio_install(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = install_audio_state.borrow_mut();
            if state.audio_install.is_some() || state.updater.installing {
                return Err(Error::Configuration(
                    "Wait for the current installation to finish".into(),
                ));
            }
            let scope = state.audit.clone();
            scope.critical(
                "component.audio.install.requested",
                Level::Info,
                json!({
                    "artifact.hash.sha256":audio_codec_tool::SHA256,
                    "artifact.size":audio_codec_tool::size(),
                }),
            )?;
            let (sender, receiver) = mpsc::channel();
            let _worker = std::thread::Builder::new()
                .name("maccanvas-audio-install".into())
                .spawn(move || {
                    let result = maccanvas::audio::install_codec(&scope);
                    if sender.send(result).is_err() {
                        eprintln!("MacCanvas audio installer window closed");
                    }
                })?;
            state.audio_install = Some(receiver);
            app.set_audio_installing(true);
            app.set_audio_status("Downloading and verifying the audio codec...".into());
            Ok(())
        })();
        if let Err(error) = result {
            notify(&app, error);
        }
    });
    let weak = app.as_weak();
    let retry_audio_state = state.clone();
    app.on_audio_retry(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            if !app.get_connected() || !app.get_hevc_active() {
                return Err(Error::Configuration(
                    "Connect with HEVC before retrying audio".into(),
                ));
            }
            let settings = retry_audio_state.borrow().settings.streaming;
            apply_streaming(&app, &retry_audio_state, settings, true)
        })();
        if let Err(error) = result {
            notify(&app, error);
        }
    });
    files_ui::wire(&app, state.clone());
    let timer = slint::Timer::default();
    let weak = app.as_weak();
    let timer_state = state.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            let Some(app) = weak.upgrade() else { return };
            if let Err(error) = tick_updates(&app, &timer_state) {
                app.set_update_status(error.to_string().into());
                app.set_update_banner(true);
            }
            if let Err(error) = tick_audio(&app, &timer_state) {
                notify(&app, error);
            }
            for (generation, event) in receiver.try_iter() {
                if generation != timer_state.borrow().generation {
                    continue;
                }
                match event {
                    Event::Connected {
                        profile,
                        server_name,
                        credentials_saved,
                    } => {
                        let saved = {
                            let mut state = timer_state.borrow_mut();
                            if credentials_saved {
                                state.connection_credentials = Some(CredentialSource::Saved);
                            }
                            state.connection_profile = Some(profile.clone());
                            if let Some(index) = state
                                .settings
                                .profiles
                                .iter()
                                .position(|old| old.id == profile.id)
                            {
                                state.settings.profiles[index] = profile.clone();
                                state.selected = Some(index);
                            } else {
                                state.settings.profiles.push(profile.clone());
                                state.selected = Some(state.settings.profiles.len() - 1);
                            }
                            state.persist()
                        };
                        app.set_connecting(false);
                        app.set_connected(true);
                        let hevc = timer_state.borrow().session.as_ref().is_some_and(Session::uses_hevc);
                        app.set_hevc_active(hevc);
                        app.set_clipboard_shared(!hevc && timer_state.borrow().settings.clipboard_enabled);
                        app.set_status(
                            format!("Connected to {} ({server_name})", profile.name).into(),
                        );
                        app.set_has_saved(profile.has_saved_credentials);
                        app.set_use_saved(profile.has_saved_credentials);
                        model(&app, &timer_state.borrow());
                        if let Err(error) = saved {
                            notify(&app, error);
                        }
                        app.set_editor_open(false);
                        app.invoke_focus_canvas();
                        let restart_audio = {
                            let mut state = timer_state.borrow_mut();
                            std::mem::take(&mut state.audio_restart)
                        };
                        if restart_audio && hevc {
                            let settings = timer_state.borrow().settings.streaming;
                            if let Err(error) = apply_streaming(&app, &timer_state, settings, true) {
                                notify(&app, error);
                            }
                        }
                    }
                    Event::Warning(message) => notify(&app, message),
                    Event::Audio(status) => {
                        let missing = matches!(status, maccanvas::audio::Status::CodecMissing(_));
                        app.set_audio_codec_missing(missing);
                        let description = match status {
                            maccanvas::audio::Status::Waiting => "Waiting for Mac audio...".to_owned(),
                            maccanvas::audio::Status::CodecMissing(message) => message,
                            maccanvas::audio::Status::Playing => "Mac system audio - 48 kHz stereo".to_owned(),
                            maccanvas::audio::Status::Unavailable(message) => {
                                notify(&app, &message);
                                message
                            }
                        };
                        app.set_audio_status(description.into());
                    }
                    Event::Error(message) => notify(&app, message),
                    Event::Disconnected => {
                        if let Err(error) = disconnect(&app, &timer_state) {
                            notify(&app, error);
                        }
                    }
                }
            }
            let result = (|| -> Result<()> {
                {
                    let mut state = timer_state.borrow_mut();
                    while let Some(index) = state.retired.iter().position(Session::is_finished) {
                        if let Err(error) = state.retired.remove(index).finish() {
                            state.reconnect = None;
                            app.set_connecting(false);
                            return Err(error);
                        }
                    }
                    if let Some(error) = state.audit.audit().failure()
                        && !state.audit_reported
                    {
                        state.audit_reported = true;
                        let closed = state.session.as_ref().map(Session::close).transpose();
                        state.reconnect = None;
                        state.connection_credentials = None;
                        app.set_connecting(false);
                        state.files.stop(&app);
                        let mut message = format!("Security logging failed. Connections are disabled until MacCanvas is restarted. {error}");
                        if let Err(error) = closed { message.push_str(&format!(" Socket cleanup also reported: {error}")); }
                        app.set_audit_error(message.clone().into());
                        notify(&app, Error::Audit(message));
                    }
                }
                tick_reconnect(&app, &timer_state, &events)?;
                tick_clipboard(&app, &mut timer_state.borrow_mut())?;
                files_ui::tick(&app,&mut timer_state.borrow_mut())?;
                let frame = timer_state
                    .borrow()
                    .frames
                    .lock()
                    .map_err(|_| Error::Display("Frame queue is unavailable".into()))?
                    .take();
                let changed = frame.is_some();
                if let Some(frame) = frame {
                    let mut state = timer_state.borrow_mut();
                    let size_changed = state.source_size != Some(frame.size);
                    state.source_size = Some(frame.size);
                    state.frame = Some(frame);
                    state.frame_number += 1;
                    if size_changed {
                        guide(&app, state.source_size);
                    }
                }
                let layout = (
                    app.get_canvas_width(),
                    app.get_canvas_height(),
                    app.window().scale_factor(),
                );
                if changed || layout != timer_state.borrow().layout {
                    refresh_layout(&app, &timer_state)?;
                }
                let rendered = timer_state.borrow().renderer.take()?;
                if let Some(rendered) = rendered {
                    let valid = {
                        let state = timer_state.borrow();
                        rendered.generation == state.generation
                            && rendered.ticket <= state.ticket
                            && state.viewport == Some(rendered.viewport)
                    };
                    if valid {
                        let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(
                            rendered.viewport.pixels.width,
                            rendered.viewport.pixels.height,
                        );
                        pixels.make_mut_bytes().copy_from_slice(&rendered.rgba);
                        app.set_frame_image(slint::Image::from_rgba8(pixels));
                        let mut state = timer_state.borrow_mut();
                        if rendered.frame_number != state.last_displayed_frame {
                            state.last_displayed_frame = rendered.frame_number;
                            state.monitor.displayed();
                        }
                    }
                }
                if app.get_connected() {
                    let mut state = timer_state.borrow_mut();
                    state.flush_pointer()?;
                    state.flush_scroll()?;
                }
                let mut state = timer_state.borrow_mut();
                let now = Instant::now();
                if now.saturating_duration_since(state.theme_checked) >= Duration::from_secs(1) {
                    app.set_dark_theme(match preview_theme {
                        Some(dark) => dark,
                        None => theme::system_dark()?,
                    });
                    app.set_motion_enabled(theme::motion_enabled()?);
                    app.set_glass_enabled(theme::glass_enabled()?);
                    state.scroll_scale = input::ScrollScale::system()?;
                    state.theme_checked = now;
                    app.set_pulse(!app.get_pulse());
                }
                if let Some(counters) = state
                    .session
                    .as_ref()
                    .map(|session| session.counters.clone())
                {
                    app.set_active_encoding(
                        encoding_name(counters.encoding.load(Ordering::Relaxed)).into(),
                    );
                    let (ssh_received,ssh_sent) = state.files.network();
                    let combined = telemetry::Counters::default();
                    combined.received.store(counters.received.load(Ordering::Relaxed).saturating_add(ssh_received),Ordering::Relaxed);
                    combined.sent.store(counters.sent.load(Ordering::Relaxed).saturating_add(ssh_sent),Ordering::Relaxed);
                    if let Some(sample) = state.monitor.sample(&combined, now) {
                        app.set_download_rate(telemetry::rate(sample.download_bps).into());
                        app.set_upload_rate(telemetry::rate(sample.upload_bps).into());
                        app.set_fps_text(format!("{:.0} FPS", sample.fps).into());
                        let bars = state
                            .monitor
                            .bars()
                            .into_iter()
                            .map(|(download, upload)| TrafficBar { download, upload })
                            .collect::<Vec<_>>();
                        app.set_traffic(ModelRc::from(Rc::new(VecModel::from(bars))));
                        if now.saturating_duration_since(state.network_logged) >= Duration::from_secs(60) {
                            if state.session.is_some() {
                                state.audit.event("application.network.summary", Level::Info, json!({
                                    "network.protocol.name":"rfb+ssh",
                                    "vnc.session.generation":state.generation,
                                    "network.io.received":combined.received.load(Ordering::Relaxed),
                                    "network.io.sent":combined.sent.load(Ordering::Relaxed),
                                    "network.receive.bits_per_second":sample.download_bps,
                                    "network.send.bits_per_second":sample.upload_bps,
                                    "rfb.displayed.fps":sample.fps, "rfb.encoding":counters.encoding.load(Ordering::Relaxed),
                                }))?;
                            }
                            state.network_logged = now;
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                notify(&app, error);
            }
        },
    );
    app.run()
        .map_err(|error| Error::Display(error.to_string()))?;
    let save_audio = state.borrow_mut().audio_persist_at.take().is_some();
    if save_audio {
        state.borrow().persist()?;
    }
    state.borrow_mut().files.stop(&app);
    if let Some(session) = state.borrow_mut().session.take() {
        session.finish()?;
    }
    state.borrow_mut().files.finish(&app)?;
    for session in std::mem::take(&mut state.borrow_mut().retired) {
        session.finish()?;
    }
    Ok(())
}
