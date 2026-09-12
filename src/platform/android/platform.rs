//! The `Platform`: the `android_main` event loop, and the services the
//! activity provides.

use super::{
    AndroidDispatcher, AndroidDisplay, AndroidWindow, AndroidWindowState, entry, jni,
    window as window_impl,
};
use crate::{
    Action, AnyWindowHandle, BackgroundExecutor, ClipboardEntry, ClipboardItem, ClipboardString,
    CursorStyle, DummyKeyboardMapper, ForegroundExecutor, Keymap, Menu, MenuItem, OwnedMenu,
    PathPromptOptions, Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper,
    PlatformTextSystem, PlatformWindow, Result, Task, WindowAppearance, WindowParams,
    platform::blade::BladeContext,
};
use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use anyhow::{Context as _, anyhow};
use async_task::Runnable;
use futures::channel::oneshot;
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
    time::{Duration, Instant},
};

/// How often the window insets are re-read while frames are being paced;
/// the software keyboard comes and goes without a native event.
const INSETS_POLL_EVERY_FRAMES: u32 = 4;

pub(crate) struct AndroidPlatform {
    state: RefCell<AndroidPlatformState>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
}

struct AndroidPlatformState {
    /// The activity, or `None` for a headless process (a test binary run
    /// from a shell, say), which has an executor and no windows.
    app: Option<AndroidApp>,
    main_receiver: flume::Receiver<Runnable>,
    renderer_context: Option<BladeContext>,
    quit: Option<Box<dyn FnMut()>>,
    reopen: Option<Box<dyn FnMut()>>,
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    /// The app-menu callbacks. Nothing shows an app menu on Android, so
    /// these are kept and never run.
    #[allow(dead_code)]
    menu_command: Option<Box<dyn FnMut(&dyn Action)>>,
    #[allow(dead_code)]
    validate_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    #[allow(dead_code)]
    will_open_menu: Option<Box<dyn FnMut()>>,
    menus: Option<Vec<OwnedMenu>>,
    /// Every window opened, oldest first. The first live one owns the
    /// activity's surface.
    windows: Vec<Weak<RefCell<AndroidWindowState>>>,
    queued_urls: Vec<String>,
    quit_requested: bool,
    /// Between `Pause` and `Resume` no frames are produced.
    paused: bool,
    frame_interval: Duration,
    frames: u32,
}

impl AndroidPlatform {
    pub(crate) fn new(headless: bool) -> Self {
        let app = if headless { None } else { entry::app() };
        let (main_sender, main_receiver) = flume::unbounded();
        let waker = app.as_ref().map(|app| app.create_waker());
        let dispatcher = Arc::new(AndroidDispatcher::new(main_sender, waker));
        let text_system: Arc<dyn PlatformTextSystem> = Arc::new(crate::CosmicTextSystem::new());
        Self {
            background_executor: BackgroundExecutor::new(dispatcher.clone()),
            foreground_executor: ForegroundExecutor::new(dispatcher),
            text_system,
            state: RefCell::new(AndroidPlatformState {
                app,
                main_receiver,
                renderer_context: None,
                quit: None,
                reopen: None,
                open_urls: None,
                menu_command: None,
                validate_menu_command: None,
                will_open_menu: None,
                menus: None,
                windows: Vec::new(),
                queued_urls: Vec::new(),
                quit_requested: false,
                paused: false,
                frame_interval: Duration::from_micros(16_667),
                frames: 0,
            }),
        }
    }

    fn app(&self) -> Option<AndroidApp> {
        self.state.borrow().app.clone()
    }

    /// The window that owns the surface (the oldest still open), if any.
    fn primary_window(&self) -> Option<Rc<RefCell<AndroidWindowState>>> {
        let mut state = self.state.borrow_mut();
        state.windows.retain(|window| window.strong_count() > 0);
        state.windows.first().and_then(Weak::upgrade)
    }

    fn live_windows(&self) -> Vec<Rc<RefCell<AndroidWindowState>>> {
        let mut state = self.state.borrow_mut();
        state.windows.retain(|window| window.strong_count() > 0);
        state.windows.iter().filter_map(Weak::upgrade).collect()
    }

    /// Runs every main-thread runnable that has been queued.
    fn drain_runnables(&self) {
        loop {
            let runnable = self.state.borrow().main_receiver.try_recv();
            match runnable {
                Ok(runnable) => {
                    runnable.run();
                }
                Err(_) => break,
            }
        }
    }

    /// Whether frames should be paced: there is a surface to draw to and
    /// the activity is in the foreground.
    fn wants_frames(&self) -> bool {
        if self.state.borrow().paused {
            return false;
        }
        self.primary_window()
            .is_some_and(|window| window.borrow().has_surface())
    }

    /// One paced frame: touch timers, the insets poll, the frame callback
    /// and the keyboard state.
    fn frame(&self, app: &AndroidApp) {
        let Some(window) = self.primary_window() else {
            return;
        };
        let frames = {
            let mut state = self.state.borrow_mut();
            state.frames = state.frames.wrapping_add(1);
            state.frames
        };
        window_impl::tick_touch_timers(&window);
        if frames % INSETS_POLL_EVERY_FRAMES == 0 {
            window_impl::poll_insets(&window, app);
        }
        window_impl::request_frame(&window);
        window_impl::sync_keyboard(&window, app);
    }

    fn handle_main_event(&self, app: &AndroidApp, event: MainEvent<'_>) {
        match event {
            MainEvent::InitWindow { .. } => {
                if let Some(native_window) = app.native_window() {
                    self.state.borrow_mut().frame_interval = frame_interval(app);
                    if let Some(window) = self.primary_window() {
                        window_impl::attach_surface(&window, native_window);
                        window_impl::poll_insets(&window, app);
                    }
                }
            }
            MainEvent::TerminateWindow { .. } => {
                // The surface is gone once this callback returns; every
                // window (only the primary has one) lets go of it now.
                for window in self.live_windows() {
                    window_impl::detach_surface(&window);
                }
            }
            MainEvent::WindowResized { .. } | MainEvent::ContentRectChanged { .. } => {
                if let Some(window) = self.primary_window() {
                    window_impl::update_geometry(&window);
                    window_impl::poll_insets(&window, app);
                }
            }
            MainEvent::RedrawNeeded { .. } => {
                if let Some(window) = self.primary_window() {
                    window_impl::update_geometry(&window);
                    window_impl::request_frame(&window);
                }
            }
            MainEvent::GainedFocus => {
                if let Some(window) = self.primary_window() {
                    window_impl::set_active(&window, true);
                }
            }
            MainEvent::LostFocus => {
                if let Some(window) = self.primary_window() {
                    window_impl::set_active(&window, false);
                }
            }
            MainEvent::ConfigChanged { .. } => {
                for window in self.live_windows() {
                    window_impl::update_geometry(&window);
                    window_impl::update_appearance(&window);
                }
            }
            MainEvent::Pause => {
                self.state.borrow_mut().paused = true;
            }
            MainEvent::Resume { .. } => {
                self.state.borrow_mut().paused = false;
                let mut state = self.state.borrow_mut();
                if let Some(mut callback) = state.reopen.take() {
                    drop(state);
                    callback();
                    self.state.borrow_mut().reopen.get_or_insert(callback);
                }
            }
            MainEvent::Destroy => {
                self.state.borrow_mut().quit_requested = true;
            }
            MainEvent::InputAvailable => {
                self.handle_input(app);
            }
            MainEvent::LowMemory
            | MainEvent::Start
            | MainEvent::Stop
            | MainEvent::SaveState { .. }
            | MainEvent::InsetsChanged { .. } => {}
            _ => {}
        }
    }

    fn handle_input(&self, app: &AndroidApp) {
        let mut events = match app.input_events_iter() {
            Ok(events) => events,
            Err(error) => {
                log::error!("gpui: failed to read input events: {error:?}");
                return;
            }
        };
        let window = self.primary_window();
        loop {
            let more = events.next(|event| match &window {
                Some(window) => window_impl::handle_input(window, app, event),
                None => InputStatus::Unhandled,
            });
            if !more {
                break;
            }
        }
    }

    /// Runs the loop for a process that is not an activity: runnables and
    /// timers only, until `quit`.
    fn run_headless(&self) {
        loop {
            let receiver = self.state.borrow().main_receiver.clone();
            match receiver.recv() {
                Ok(runnable) => {
                    runnable.run();
                }
                Err(_) => return,
            }
            if self.state.borrow().quit_requested {
                return;
            }
        }
    }

    fn deliver_urls(&self, urls: Vec<String>) {
        let mut state = self.state.borrow_mut();
        if let Some(mut callback) = state.open_urls.take() {
            drop(state);
            callback(urls);
            self.state.borrow_mut().open_urls.get_or_insert(callback);
        } else {
            state.queued_urls.extend(urls);
        }
    }

    fn credentials_path(app: &AndroidApp, url: &str) -> Result<PathBuf> {
        let dir = app
            .internal_data_path()
            .context("the app has no private storage directory")?
            .join("gpui-credentials");
        std::fs::create_dir_all(&dir)?;
        let mut hasher = seahash::SeaHasher::new();
        std::hash::Hasher::write(&mut hasher, url.as_bytes());
        Ok(dir.join(format!("{:016x}", std::hash::Hasher::finish(&hasher))))
    }
}

/// The display's frame interval, from its refresh rate.
fn frame_interval(app: &AndroidApp) -> Duration {
    let rate = jni::refresh_rate(app).unwrap_or(60.0);
    let rate = if rate.is_finite() && rate >= 24.0 {
        rate
    } else {
        60.0
    };
    Duration::from_secs_f32(1.0 / rate)
}

impl Platform for AndroidPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        let Some(app) = self.app() else {
            on_finish_launching();
            self.run_headless();
            return;
        };

        // A file or link the activity was launched for waits in the queue
        // until the app registers its callback.
        match jni::launch_url(&app) {
            Ok(Some(url)) => self.deliver_urls(vec![url]),
            Ok(None) => {}
            Err(error) => log::warn!("gpui: could not read the launch intent: {error}"),
        }

        on_finish_launching();

        let mut next_frame = Instant::now();
        loop {
            let timeout = if self.wants_frames() {
                Some(next_frame.saturating_duration_since(Instant::now()))
            } else {
                None
            };
            app.poll_events(timeout, |event| {
                if let PollEvent::Main(event) = event {
                    self.handle_main_event(&app, event);
                }
            });
            self.drain_runnables();
            if self.state.borrow().quit_requested {
                break;
            }
            let now = Instant::now();
            if self.wants_frames() && now >= next_frame {
                self.frame(&app);
                let interval = self.state.borrow().frame_interval;
                next_frame = now + interval;
                self.drain_runnables();
            }
        }

        let mut state = self.state.borrow_mut();
        if let Some(mut callback) = state.quit.take() {
            drop(state);
            callback();
        }
        for window in self.live_windows() {
            window_impl::detach_surface(&window);
        }
    }

    fn quit(&self) {
        self.state.borrow_mut().quit_requested = true;
        if let Some(app) = self.app() {
            // Finishing the activity is what ends the loop, through
            // `Destroy`; the flag covers a process without one.
            if let Err(error) = jni::finish_activity(&app) {
                log::warn!("gpui: could not finish the activity: {error}");
            }
            app.create_waker().wake();
        }
    }

    fn restart(&self, _binary_path: Option<PathBuf>) {
        log::warn!("gpui: restarting the process is not possible on Android");
    }

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        self.primary_display().into_iter().collect()
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        if let Some(window) = self.primary_window() {
            let window = window.borrow();
            if window.has_surface() {
                return Some(Rc::new(AndroidDisplay::new(window.bounds())));
            }
        }
        let app = self.app()?;
        Some(Rc::new(AndroidDisplay::from_config(&app)))
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        let window = self.primary_window()?;
        let window = window.borrow();
        window.active.then_some(window.handle)
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        Some(
            self.live_windows()
                .iter()
                .map(|window| window.borrow().handle)
                .collect(),
        )
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> anyhow::Result<Box<dyn PlatformWindow>> {
        let app = self
            .app()
            .context("a headless process cannot open windows")?;
        let renderer_context = {
            let mut state = self.state.borrow_mut();
            if state.renderer_context.is_none() {
                state.renderer_context =
                    Some(BladeContext::new().context("failed to initialize Vulkan")?);
            }
            state.renderer_context.clone().unwrap()
        };
        let is_primary = self.primary_window().is_none();
        let window = AndroidWindow::open(
            handle,
            options,
            app.clone(),
            self.foreground_executor.clone(),
            renderer_context,
        )?;
        self.state
            .borrow_mut()
            .windows
            .push(Rc::downgrade(window.state()));
        if is_primary {
            if let Some(native_window) = app.native_window() {
                window_impl::attach_surface(window.state(), native_window);
                window_impl::poll_insets(window.state(), &app);
            }
        } else {
            log::warn!("gpui: an Android activity shows one window; this one will not be visible");
        }
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        match self.app() {
            Some(app) => window_impl::appearance_from_config(&app),
            None => WindowAppearance::Light,
        }
    }

    fn open_url(&self, url: &str) {
        let Some(app) = self.app() else {
            return;
        };
        if let Err(error) = jni::open_url(&app, url) {
            log::warn!("gpui: could not open {url}: {error}");
        }
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        let queued = {
            let mut state = self.state.borrow_mut();
            state.open_urls = Some(callback);
            std::mem::take(&mut state.queued_urls)
        };
        if !queued.is_empty() {
            self.deliver_urls(queued);
        }
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "URL schemes are declared with an intent filter in AndroidManifest.xml"
        )))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow!(
            "the system file picker needs an activity result, which a NativeActivity cannot receive"
        )))
        .ok();
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Err(anyhow!(
            "the system file picker needs an activity result, which a NativeActivity cannot receive"
        )))
        .ok();
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, path: &Path) {
        log::warn!(
            "gpui: there is no file manager to reveal {} in on Android",
            path.display()
        );
    }

    fn open_with_system(&self, path: &Path) {
        // A file:// URI cannot be handed to another app (it needs a
        // content provider the app would have to declare).
        log::warn!(
            "gpui: opening {} with another app is not supported on Android",
            path.display()
        );
    }

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().reopen = Some(callback);
    }

    fn set_menus(&self, menus: Vec<Menu>, _keymap: &Keymap) {
        // There is no menu bar; the keymap bindings still work with a
        // hardware keyboard, and the menus are kept for `get_menus`.
        self.state.borrow_mut().menus = Some(menus.into_iter().map(|menu| menu.owned()).collect());
    }

    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        self.state.borrow().menus.clone()
    }

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.state.borrow_mut().menu_command = Some(callback);
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().will_open_menu = Some(callback);
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.state.borrow_mut().validate_menu_command = Some(callback);
    }

    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow!("an Android app has no executable path"))
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow!("Android apps cannot launch auxiliary executables"))
    }

    fn set_cursor_style(&self, _style: CursorStyle) {
        // A mouse pointer's shape is set per view through
        // `PointerIcon`, which NativeActivity's view does not expose.
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        let Some(app) = self.app() else {
            return;
        };
        // Only text goes on the Android clipboard: an image needs a
        // content URI served by a provider the app would have to declare.
        let text = item
            .entries
            .iter()
            .filter_map(|entry| match entry {
                ClipboardEntry::String(string) => Some(string.text.as_str()),
                ClipboardEntry::Image(_) => None,
            })
            .collect::<String>();
        if let Err(error) = jni::set_clipboard_text(&app, &text) {
            log::warn!("gpui: could not write the clipboard: {error}");
        }
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        let app = self.app()?;
        match jni::clipboard_text(&app) {
            Ok(Some(text)) => Some(ClipboardItem {
                entries: vec![ClipboardEntry::String(ClipboardString::new(text))],
            }),
            Ok(None) => None,
            Err(error) => {
                log::warn!("gpui: could not read the clipboard: {error}");
                None
            }
        }
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        let Some(app) = self.app() else {
            return Task::ready(Err(anyhow!("no activity")));
        };
        let url = url.to_string();
        let username = username.to_string();
        let password = password.to_vec();
        self.background_executor.spawn(async move {
            let path = Self::credentials_path(&app, &url)?;
            let mut plaintext = Vec::with_capacity(4 + username.len() + password.len());
            plaintext.extend_from_slice(&(username.len() as u32).to_be_bytes());
            plaintext.extend_from_slice(username.as_bytes());
            plaintext.extend_from_slice(&password);
            let ciphertext =
                jni::encrypt(&app, &plaintext).context("encrypting with the keystore")?;
            std::fs::write(path, ciphertext)?;
            Ok(())
        })
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        let Some(app) = self.app() else {
            return Task::ready(Ok(None));
        };
        let url = url.to_string();
        self.background_executor.spawn(async move {
            let path = Self::credentials_path(&app, &url)?;
            let ciphertext = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            let plaintext =
                jni::decrypt(&app, &ciphertext).context("decrypting with the keystore")?;
            let (len, rest) = plaintext
                .split_first_chunk::<4>()
                .context("stored credentials are corrupt")?;
            let len = u32::from_be_bytes(*len) as usize;
            anyhow::ensure!(rest.len() >= len, "stored credentials are corrupt");
            let (username, password) = rest.split_at(len);
            Ok(Some((
                String::from_utf8(username.to_vec())?,
                password.to_vec(),
            )))
        })
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        let Some(app) = self.app() else {
            return Task::ready(Ok(()));
        };
        let url = url.to_string();
        self.background_executor.spawn(async move {
            let path = Self::credentials_path(&app, &url)?;
            match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        })
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        let language = self
            .app()
            .and_then(|app| app.config().language())
            .unwrap_or_else(|| "unknown".to_string());
        Box::new(AndroidKeyboardLayout { name: language })
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}

/// Android has no keyboard-layout API; the configuration's language is
/// the nearest thing.
pub(crate) struct AndroidKeyboardLayout {
    name: String,
}

impl PlatformKeyboardLayout for AndroidKeyboardLayout {
    fn id(&self) -> &str {
        &self.name
    }

    fn name(&self) -> &str {
        &self.name
    }
}
