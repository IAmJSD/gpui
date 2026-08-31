use crate::{
    Action, AnyWindowHandle, BackgroundExecutor, ClipboardItem, CursorStyle, DummyKeyboardMapper,
    ForegroundExecutor, Keymap, Menu, MenuItem, PathPromptOptions, Platform,
    PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, Task, WindowAppearance, WindowParams,
};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use std::{
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use super::{WebDispatcher, WebDisplay, WebGpuContext, WebWindow};
use std::cell::RefCell;

pub(crate) struct WebKeyboardLayout;

impl PlatformKeyboardLayout for WebKeyboardLayout {
    fn id(&self) -> &str {
        "unknown"
    }

    fn name(&self) -> &str {
        "unknown"
    }
}

/// The web platform. Text and input are not implemented yet; windowing and
/// rendering go through a canvas and WebGPU.
pub(crate) struct WebPlatform {
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    /// Populated by `run()` before the launch callback fires; `open_window`
    /// requires it.
    gpu: Rc<RefCell<Option<Arc<WebGpuContext>>>>,
    active_window: RefCell<Option<AnyWindowHandle>>,
    /// The browser's clipboard is async and permission-gated while gpui's
    /// `read_from_clipboard` is synchronous, so reads come from this mirror
    /// of what the application last wrote. Text is additionally pushed to
    /// the real clipboard (best effort) so it can be pasted outside the app;
    /// content copied in other pages is not visible here.
    clipboard: RefCell<Option<ClipboardItem>>,
}

impl WebPlatform {
    pub(crate) fn new() -> Self {
        let dispatcher = Arc::new(WebDispatcher);
        Self {
            background_executor: BackgroundExecutor::new(dispatcher.clone()),
            foreground_executor: ForegroundExecutor::new(dispatcher),
            // Starts with an empty font database: there is no system font
            // enumeration in a browser. Applications add fonts with
            // `cx.text_system().add_fonts(...)`.
            text_system: Arc::new(crate::CosmicTextSystem::new()),
            gpu: Rc::new(RefCell::new(None)),
            active_window: RefCell::new(None),
            clipboard: RefCell::new(None),
        }
    }
}

impl Platform for WebPlatform {
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
        // The browser owns the event loop, so unlike the desktop backends this
        // does not block. WebGPU setup is async (adapter and device requests
        // return promises), so it happens here, before the launch callback --
        // that way `open_window` and everything after it stay synchronous.
        // The application lives on because `Application::run` leaks its root
        // reference on wasm; everything registered during launch (the frame
        // loop, the DOM listeners) holds only `Weak` references.
        let gpu = self.gpu.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match WebGpuContext::new().await {
                Ok(context) => {
                    *gpu.borrow_mut() = Some(Arc::new(context));
                    on_finish_launching();
                }
                Err(error) => {
                    web_sys::console::error_1(
                        &format!("gpui: failed to initialize WebGPU: {error:#}").into(),
                    );
                }
            }
        });
    }

    fn quit(&self) {}

    fn restart(&self, _binary_path: Option<PathBuf>) {}

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![Rc::new(WebDisplay)]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(WebDisplay))
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        *self.active_window.borrow()
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let gpu = self.gpu.borrow().clone().ok_or_else(|| {
            anyhow!(
                "the GPU is not initialized; on the web, windows can only be opened \
                 from (or after) the Application::run callback"
            )
        })?;
        let window = WebWindow::new(&gpu, handle, options)?;
        *self.active_window.borrow_mut() = Some(handle);
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        let dark = web_sys::window()
            .and_then(|window| window.match_media("(prefers-color-scheme: dark)").ok())
            .flatten()
            .is_some_and(|query| query.matches());
        if dark {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        }
    }

    fn open_url(&self, url: &str) {
        if let Some(window) = web_sys::window() {
            // May be blocked by the popup blocker when not called from a
            // user gesture; nothing to do about that here.
            window.open_with_url_and_target(url, "_blank").ok();
        }
    }

    fn on_open_urls(&self, _callback: Box<dyn FnMut(Vec<String>)>) {}

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "registering URL schemes is not supported on the web"
        )))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(None)).ok();
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (tx, rx) = oneshot::channel();
        tx.send(Ok(None)).ok();
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, _callback: Box<dyn FnMut()>) {}

    fn on_reopen(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}

    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow!("application paths do not exist on the web"))
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow!("auxiliary executables do not exist on the web"))
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        let css = match style {
            CursorStyle::Arrow => "default",
            CursorStyle::IBeam => "text",
            CursorStyle::Crosshair => "crosshair",
            CursorStyle::ClosedHand => "grabbing",
            CursorStyle::OpenHand => "grab",
            CursorStyle::PointingHand => "pointer",
            CursorStyle::ResizeLeft => "w-resize",
            CursorStyle::ResizeRight => "e-resize",
            CursorStyle::ResizeLeftRight => "ew-resize",
            CursorStyle::ResizeUp => "n-resize",
            CursorStyle::ResizeDown => "s-resize",
            CursorStyle::ResizeUpDown => "ns-resize",
            CursorStyle::ResizeUpLeftDownRight => "nesw-resize",
            CursorStyle::ResizeUpRightDownLeft => "nwse-resize",
            CursorStyle::ResizeColumn => "col-resize",
            CursorStyle::ResizeRow => "row-resize",
            CursorStyle::IBeamCursorForVerticalLayout => "vertical-text",
            CursorStyle::OperationNotAllowed => "not-allowed",
            CursorStyle::DragLink => "alias",
            CursorStyle::DragCopy => "copy",
            CursorStyle::ContextualMenu => "context-menu",
            CursorStyle::None => "none",
        };
        if let Some(body) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.body())
        {
            body.style().set_property("cursor", css).ok();
        }
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        if let Some(text) = item.text() {
            if let Some(window) = web_sys::window() {
                // Fire and forget; the promise resolves (or is denied) on its
                // own and the mirror below covers in-app paste either way.
                let _ = window.navigator().clipboard().write_text(&text);
            }
        }
        *self.clipboard.borrow_mut() = Some(item);
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.clipboard.borrow().clone()
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "credential storage is not supported on the web"
        )))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Err(anyhow!(
            "credential storage is not supported on the web"
        )))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "credential storage is not supported on the web"
        )))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(WebKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}
