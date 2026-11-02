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
use slint::{
    ComponentHandle, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
    language::{PointerEventButton, PointerEventKind},
};
use uuid::Uuid;

use maccanvas::{
    auth::Credentials,
    credentials::{CredentialSource, CredentialStore, WindowsVault},
    error::{Error, Result},
    framebuffer::Framebuffer,
    geometry::{Extent, Viewport},
    input::{self, Command},
    profiles::{AuthMode, Profile, Settings},
    render::{Job, Renderer},
    session::{Event, Frames, Request, Session},
    streaming::{Encoding, StreamSettings, encoding_name, raw_estimate},
    telemetry::{self, Monitor},
    theme,
    updates::{self, Updater},
};

use crate::{App, CodecGuide, Machine, TrafficBar};

struct State {
    settings: Settings,
    path: PathBuf,
    selected: Option<usize>,
    session: Option<Session>,
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
    keys: HashMap<String, u32>,
    modifiers: [bool; 4],
    monitor: Monitor,
    frame_number: u64,
    last_displayed_frame: u64,
    theme_checked: Instant,
    updater: Updater,
}

impl State {
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
    state.settings.save(&state.path)
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
        state
            .frames
            .lock()
            .map_err(|_| Error::Display("Frame queue is unavailable".into()))?
            .take();
        let close = session.map_or(Ok(()), |session| session.close());
        release.and(close)
    };
    app.set_connected(false);
    app.set_connecting(false);
    app.set_frame_image(slint::Image::default());
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
    let target_kbps = if app.get_manual_bandwidth() {
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
        encoding: Encoding::from_index(app.get_encoding_index())?,
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
    let target = if settings.target_kbps == 0 {
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
            settings.max_fps
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
            let (low, high) = match encoding {
                Encoding::Auto | Encoding::Zrle => (2.0, 12.0),
                Encoding::Tight => (3.0, 20.0),
                Encoding::TightJpeg => (5.0, 50.0),
                Encoding::Hextile => (1.0, 4.0),
                Encoding::Zlib => (2.0, 6.0),
                Encoding::Rre | Encoding::Corre => (0.5, 4.0),
                Encoding::Raw => (1.0, 1.0),
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
                        let directory = state.path.parent().ok_or_else(|| {
                            Error::Configuration("Settings directory is unavailable".into())
                        })?;
                        match std::process::Command::new(executable)
                            .arg("--data-dir")
                            .arg(directory)
                            .spawn()
                        {
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
) -> Result<()> {
    let directory = match data_directory {
        Some(directory) => directory,
        None => dirs::data_local_dir()
            .ok_or_else(|| {
                Error::Configuration("Windows did not return a local app-data folder".into())
            })?
            .join("MacCanvas"),
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
    app.set_editor_open(settings.profiles.is_empty());
    app.set_app_version(maccanvas::VERSION.into());
    let mut updater = Updater::new();
    #[cfg(debug_assertions)]
    if preview_update {
        updater.preview_available()?;
    }
    if preview_theme.is_none() && !(cfg!(debug_assertions) && preview_update) {
        updater.check();
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
        selected: None,
        session: None,
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
    app.on_connect_requested(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = connection_state.borrow_mut();
            if state.updater.installing {
                return Err(Error::Update(
                    "Finish the application update before connecting".into(),
                ));
            }
            if state.session.is_some() {
                return Err(Error::Protocol("A connection is already active".into()));
            }
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
            let hwnd = window_handle(&app)?;
            state.generation += 1;
            state.frames = Arc::new(Mutex::new(None));
            state.monitor = Monitor::new(Instant::now());
            state.frame_number = 0;
            state.last_displayed_frame = 0;
            let request = Request {
                profile,
                credentials: source,
                streaming: state.settings.streaming,
                remember: app.get_remember_password() && !app.get_use_saved(),
                window: hwnd,
                generation: state.generation,
            };
            state.session = Some(Session::start(
                request,
                events.clone(),
                state.frames.clone(),
            ));
            Ok(())
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
        state.updater.check();
        app.set_update_checking(state.updater.checking);
        app.set_update_status("Checking the latest official release...".into());
    });
    let weak = app.as_weak();
    let update_state = state.clone();
    app.on_install_update(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = update_state.borrow_mut();
            if state.session.is_some() {
                return Err(Error::Update(
                    "Disconnect from the Mac before updating and restarting".into(),
                ));
            }
            state.settings.save(&state.path)?;
            state.updater.install()
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
    let close_state = state.clone();
    app.window().on_close_requested(move || {
        if close_state.borrow().updater.installing {
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
                Ok(encoding) => app.set_encoding_description(encoding.description().into()),
                Err(error) => notify(&app, error),
            }
        }
    });
    let weak = app.as_weak();
    let quality_state = state.clone();
    app.on_apply_streaming(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<StreamSettings> {
            let settings = read_stream_form(&app)?;
            let mut state = quality_state.borrow_mut();
            if let Some(session) = state.session.as_ref() {
                session.send(Command::Configure(settings))?;
            }
            state.settings.streaming = settings;
            state.settings.save(&state.path)?;
            Ok(settings)
        })();
        match result {
            Ok(settings) => {
                stream_summary(&app, settings);
                app.set_quality_open(false);
                app.set_error_text("".into());
            }
            Err(error) => notify(&app, error),
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
            state.settings.save(&state.path)
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
            WindowsVault.delete(id)?;
            state.settings.profiles.remove(index);
            state.selected = None;
            state.settings.save(&state.path)
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
            let mask = (if event.delta_y > 0.0 {
                8
            } else if event.delta_y < 0.0 {
                16
            } else {
                0
            }) | (if event.delta_x > 0.0 {
                32
            } else if event.delta_x < 0.0 {
                64
            } else {
                0
            });
            if mask != 0 {
                state.send(Command::Pointer {
                    x,
                    y,
                    buttons: state.buttons | mask,
                })?;
                state.send(Command::Pointer {
                    x,
                    y,
                    buttons: state.buttons,
                })?;
            }
            Ok(())
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
        if !app.get_connected() || app.get_quality_open() || app.get_help_open() {
            return false;
        }
        let result = (|| -> Result<()> {
            let mut state = key_state.borrow_mut();
            state.flush_pointer()?;
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
            for (generation, event) in receiver.try_iter() {
                if generation != timer_state.borrow().generation {
                    continue;
                }
                match event {
                    Event::Connected {
                        profile,
                        server_name,
                    } => {
                        let saved = {
                            let mut state = timer_state.borrow_mut();
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
                            state.settings.save(&state.path)
                        };
                        app.set_connecting(false);
                        app.set_connected(true);
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
                    }
                    Event::Warning(message) => notify(&app, message),
                    Event::Error(message) => notify(&app, message),
                    Event::Disconnected => {
                        if let Err(error) = disconnect(&app, &timer_state) {
                            notify(&app, error);
                        }
                    }
                }
            }
            let result = (|| -> Result<()> {
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
                    timer_state.borrow_mut().flush_pointer()?;
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
                    if let Some(sample) = state.monitor.sample(&counters, now) {
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
    if let Some(session) = state.borrow_mut().session.take() {
        session.close()?;
    }
    Ok(())
}
