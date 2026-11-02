use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
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
};

use crate::{App, Machine};

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
    let rows: Vec<_> = state
        .settings
        .profiles
        .iter()
        .map(|profile| Machine {
            name: profile.name.clone().into(),
            detail: format!("{}:{}", profile.host, profile.port).into(),
            saved: profile.has_saved_credentials,
        })
        .collect();
    app.set_machines(ModelRc::from(Rc::new(VecModel::from(rows))));
    app.set_selected(state.selected.map_or(-1, |index| index as i32));
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
    app.set_error_text("".into());
    model(app, &state.borrow());
    Ok(())
}

fn preferences(app: &App, state: &Rc<RefCell<State>>) -> Result<()> {
    let mut state = state.borrow_mut();
    state.settings.ctrl_as_command = app.get_ctrl_as_command();
    state.settings.hello_for_all_connections = app.get_hello_all();
    state.settings.dark_theme = app.get_dark_theme();
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
            "{} x {}  |  Lanczos fit  |  Windows cursor",
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
    result
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

pub fn run(data_directory: Option<PathBuf>) -> Result<()> {
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
    app.set_dark_theme(settings.dark_theme);
    app.set_ctrl_as_command(settings.ctrl_as_command);
    app.set_hello_all(settings.hello_for_all_connections);
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
    }));
    model(&app, &state.borrow());
    let weak = app.as_weak();
    let connection_state = state.clone();
    app.on_connect_requested(move || {
        let Some(app) = weak.upgrade() else { return };
        let result = (|| -> Result<()> {
            let mut state = connection_state.borrow_mut();
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
            let request = Request {
                profile,
                credentials: source,
                hello_for_all: app.get_hello_all(),
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
            match usize::try_from(index) {
                Ok(index) => {
                    if let Err(error) = select(&app, &selected_state, Some(index)) {
                        notify(&app, error);
                    }
                }
                Err(_) => notify(&app, "Invalid machine selection"),
            }
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
        if !app.get_connected() {
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
                    state.source_size = Some(frame.size);
                    state.frame = Some(frame);
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
                    }
                }
                if app.get_connected() {
                    timer_state.borrow_mut().flush_pointer()?;
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
