//! `UIApplication` glue: the app and scene delegates, the main menu
//! (`UIMenuBuilder`), pasteboard, keychain, document pickers and the rest
//! of the `Platform` trait.

use super::{
    BoolExt, IosDisplay, IosWindow, MacDispatcher, NSRect, NSStringExt, id, nil, ns_string,
    presenting_view_controller, renderer,
};
use crate::{
    Action, AnyWindowHandle, BackgroundExecutor, ClipboardEntry, ClipboardItem, ClipboardString,
    CursorStyle, DummyKeyboardMapper, ForegroundExecutor, Image, ImageFormat, KeyContext, Keymap,
    Menu, MenuItem, OsAction, OwnedMenu, OwnedMenuItem, PathPromptOptions, Platform,
    PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper, PlatformTextSystem,
    PlatformWindow, Result, Task, WindowAppearance, WindowParams, appearance_from_style,
    platform::ios::events::{
        UI_KEY_MODIFIER_ALTERNATE, UI_KEY_MODIFIER_COMMAND, UI_KEY_MODIFIER_CONTROL,
        UI_KEY_MODIFIER_SHIFT, key_to_ui_key_input,
    },
};
use anyhow::{Context as _, anyhow};
use block::Block;
use core_foundation::{
    base::{CFType, CFTypeRef, OSStatus, TCFType},
    boolean::CFBoolean,
    data::CFData,
    dictionary::{CFDictionary, CFDictionaryRef, CFMutableDictionary},
    runloop::CFRunLoopRun,
    string::{CFString, CFStringRef},
};
use ctor::ctor;
use futures::channel::oneshot;
use itertools::Itertools;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, NO, Object, Protocol, Sel, YES},
    sel, sel_impl,
};
use parking_lot::Mutex;
use std::{
    ffi::{CString, c_char, c_int, c_void},
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    slice,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicPtr, Ordering},
    },
};
use strum::IntoEnumIterator;

static mut APP_DELEGATE_CLASS: *const Class = ptr::null();
static mut SCENE_DELEGATE_CLASS: *const Class = ptr::null();
static mut DOCUMENT_PICKER_DELEGATE_CLASS: *const Class = ptr::null();

/// The platform lives for the whole process (`UIApplicationMain` never
/// returns), so the delegates and the windows reach it through this.
static PLATFORM: AtomicPtr<IosPlatform> = AtomicPtr::new(ptr::null_mut());

/// Ivar holding a boxed `oneshot::Sender` on a document picker delegate.
const SENDER_IVAR: &str = "gpuiSender";

/// `UIMenuOptionsDisplayInline`.
const UI_MENU_OPTIONS_DISPLAY_INLINE: usize = 1;
/// `UIMenuElementAttributesDisabled`.
const UI_MENU_ELEMENT_ATTRIBUTES_DISABLED: usize = 1;

/// Identifier prefix of the top-level menus gpui publishes.
const MENU_IDENTIFIER_PREFIX: &str = "gpui.menu.";
/// `UIApplicationShortcutItem.type` prefix for dock-menu items.
const SHORTCUT_TYPE_PREFIX: &str = "gpui.dock.";

pub(super) fn platform() -> Option<&'static IosPlatform> {
    let ptr = PLATFORM.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

#[ctor]
unsafe fn build_classes() {
    unsafe {
        APP_DELEGATE_CLASS = {
            let mut decl = ClassDecl::new("GPUIApplicationDelegate", class!(UIResponder)).unwrap();
            decl.add_method(
                sel!(application:didFinishLaunchingWithOptions:),
                did_finish_launching as extern "C" fn(&mut Object, Sel, id, id) -> BOOL,
            );
            decl.add_method(
                sel!(applicationWillTerminate:),
                will_terminate as extern "C" fn(&mut Object, Sel, id),
            );
            decl.add_method(
                sel!(application:configurationForConnectingSceneSession:options:),
                scene_configuration as extern "C" fn(&mut Object, Sel, id, id, id) -> id,
            );
            decl.add_method(
                sel!(application:openURL:options:),
                app_open_url as extern "C" fn(&mut Object, Sel, id, id, id) -> BOOL,
            );
            decl.add_method(
                sel!(application:performActionForShortcutItem:completionHandler:),
                app_perform_shortcut as extern "C" fn(&mut Object, Sel, id, id, id),
            );
            decl.add_method(
                sel!(buildMenuWithBuilder:),
                build_menu as extern "C" fn(&mut Object, Sel, id),
            );
            decl.add_method(
                sel!(validateCommand:),
                validate_command as extern "C" fn(&mut Object, Sel, id),
            );
            decl.add_method(
                sel!(canPerformAction:withSender:),
                delegate_can_perform_action as extern "C" fn(&mut Object, Sel, Sel, id) -> BOOL,
            );
            decl.add_method(
                sel!(handleGPUIMenuItem:),
                handle_menu_item as extern "C" fn(&mut Object, Sel, id),
            );
            for selector in [
                sel!(cut:),
                sel!(copy:),
                sel!(paste:),
                sel!(selectAll:),
                sel!(undo:),
                sel!(redo:),
            ] {
                decl.add_method(
                    selector,
                    handle_os_action as extern "C" fn(&mut Object, Sel, id),
                );
            }
            if let Some(protocol) = Protocol::get("UIApplicationDelegate") {
                decl.add_protocol(protocol);
            }
            decl.register()
        };

        SCENE_DELEGATE_CLASS = {
            let mut decl = ClassDecl::new("GPUISceneDelegate", class!(NSObject)).unwrap();
            decl.add_method(
                sel!(scene:willConnectToSession:options:),
                scene_will_connect as extern "C" fn(&mut Object, Sel, id, id, id),
            );
            decl.add_method(
                sel!(scene:openURLContexts:),
                scene_open_urls as extern "C" fn(&mut Object, Sel, id, id),
            );
            decl.add_method(
                sel!(windowScene:performActionForShortcutItem:completionHandler:),
                scene_perform_shortcut as extern "C" fn(&mut Object, Sel, id, id, id),
            );
            // UIKit refuses a scene delegate that does not declare the
            // protocol, whatever methods it implements.
            if let Some(protocol) = Protocol::get("UIWindowSceneDelegate") {
                decl.add_protocol(protocol);
            }
            decl.register()
        };

        DOCUMENT_PICKER_DELEGATE_CLASS = {
            let mut decl = ClassDecl::new("GPUIDocumentPickerDelegate", class!(NSObject)).unwrap();
            decl.add_ivar::<*mut c_void>(SENDER_IVAR);
            decl.add_method(
                sel!(documentPicker:didPickDocumentsAtURLs:),
                did_pick_documents as extern "C" fn(&mut Object, Sel, id, id),
            );
            decl.add_method(
                sel!(documentPickerWasCancelled:),
                picker_cancelled as extern "C" fn(&mut Object, Sel, id),
            );
            if let Some(protocol) = Protocol::get("UIDocumentPickerDelegate") {
                decl.add_protocol(protocol);
            }
            decl.register()
        };
    }
}

pub(crate) struct IosPlatform(pub(super) Mutex<IosPlatformState>);

pub(crate) struct IosPlatformState {
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    pub(super) renderer_context: renderer::Context,
    headless: bool,
    finish_launching: Option<Box<dyn FnOnce()>>,
    quit: Option<Box<dyn FnMut()>>,
    reopen: Option<Box<dyn FnMut()>>,
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    menu_command: Option<Box<dyn FnMut(&dyn Action)>>,
    validate_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    will_open_menu: Option<Box<dyn FnMut()>>,
    /// The main menu, as last set.
    menus: Option<Vec<OwnedMenu>>,
    /// The main menu with key equivalents resolved, which the
    /// `UIMenuBuilder` pass turns into `UIMenu`s.
    prepared_menus: Vec<PreparedMenu>,
    /// Actions of the main menu, indexed by `UICommand.propertyList`.
    menu_actions: Vec<Box<dyn Action>>,
    /// Which main-menu action stands for each OS edit action.
    os_actions: Vec<(OsAction, usize)>,
    /// Dock-menu actions, published as home-screen quick actions.
    dock_actions: Vec<Box<dyn Action>>,
    /// The connected `UIWindowScene`, once there is one.
    pub(super) window_scene: id,
    /// Windows opened before the scene connected, to attach then.
    pub(super) pending_windows: Vec<id>,
    /// Keeps the last document-interaction controller alive while shown.
    interaction_controller: id,
    /// Pending URLs delivered before the callback was registered.
    queued_urls: Vec<String>,
}

unsafe impl Send for IosPlatformState {}

impl IosPlatform {
    pub(crate) fn new(headless: bool) -> Self {
        let dispatcher = Arc::new(MacDispatcher);

        #[cfg(feature = "font-kit")]
        let text_system = Arc::new(crate::MacTextSystem::new());
        #[cfg(not(feature = "font-kit"))]
        let text_system = Arc::new(crate::NoopTextSystem::new());

        Self(Mutex::new(IosPlatformState {
            background_executor: BackgroundExecutor::new(dispatcher.clone()),
            foreground_executor: ForegroundExecutor::new(dispatcher),
            text_system,
            renderer_context: renderer::Context::default(),
            headless,
            finish_launching: None,
            quit: None,
            reopen: None,
            open_urls: None,
            menu_command: None,
            validate_menu_command: None,
            will_open_menu: None,
            menus: None,
            prepared_menus: Vec::new(),
            menu_actions: Vec::new(),
            os_actions: Vec::new(),
            dock_actions: Vec::new(),
            window_scene: nil,
            pending_windows: Vec::new(),
            interaction_controller: nil,
            queued_urls: Vec::new(),
        }))
    }

    /// Runs the app-menu action callback for `action`. Used by the main
    /// menu, quick actions, the edit menu and native context menus alike.
    pub(super) fn dispatch_menu_action(&self, action: &dyn Action) {
        let mut lock = self.0.lock();
        if let Some(mut callback) = lock.menu_command.take() {
            drop(lock);
            callback(action);
            self.0.lock().menu_command.get_or_insert(callback);
        }
    }

    /// The main-menu action registered for an OS edit action, if any.
    pub(super) fn os_action(&self, os_action: OsAction) -> Option<Box<dyn Action>> {
        let lock = self.0.lock();
        let index = lock
            .os_actions
            .iter()
            .find(|(candidate, _)| *candidate == os_action)
            .map(|(_, index)| *index)?;
        lock.menu_actions
            .get(index)
            .map(|action| action.boxed_clone())
    }

    /// Whether `action` is currently available, per the app's validator.
    pub(super) fn validate_action(&self, action: &dyn Action) -> bool {
        let mut lock = self.0.lock();
        if let Some(mut callback) = lock.validate_menu_command.take() {
            drop(lock);
            let result = callback(action);
            self.0.lock().validate_menu_command.get_or_insert(callback);
            result
        } else {
            true
        }
    }

    /// Whether the app registered an action for the OS edit action and it
    /// is currently available.
    pub(super) fn can_perform_os_action(&self, os_action: OsAction) -> bool {
        self.os_action(os_action)
            .is_some_and(|action| self.validate_action(action.as_ref()))
    }

    pub(super) fn perform_os_action(&self, os_action: OsAction) {
        if let Some(action) = self.os_action(os_action) {
            self.dispatch_menu_action(action.as_ref());
        }
    }

    fn deliver_urls(&self, urls: Vec<String>) {
        let mut lock = self.0.lock();
        if let Some(mut callback) = lock.open_urls.take() {
            drop(lock);
            callback(urls);
            self.0.lock().open_urls.get_or_insert(callback);
        } else {
            lock.queued_urls.extend(urls);
        }
    }

    fn perform_shortcut(&self, item: id) {
        let index = unsafe {
            let kind: id = msg_send![item, type];
            kind.to_str()
                .strip_prefix(SHORTCUT_TYPE_PREFIX)
                .and_then(|index| index.parse::<usize>().ok())
        };
        let action = index.and_then(|index| {
            self.0
                .lock()
                .dock_actions
                .get(index)
                .map(|action| action.boxed_clone())
        });
        if let Some(action) = action {
            self.dispatch_menu_action(action.as_ref());
        }
    }

    /// Attaches `window` to the connected scene and shows it, or queues it
    /// until the scene connects.
    pub(super) fn attach_window(&self, window: id) {
        let mut lock = self.0.lock();
        if lock.window_scene.is_null() {
            lock.pending_windows.push(window);
        } else {
            let scene = lock.window_scene;
            drop(lock);
            unsafe {
                let _: () = msg_send![window, setWindowScene: scene];
                let _: () = msg_send![window, makeKeyAndVisible];
            }
        }
    }

    pub(super) fn foreground_executor(&self) -> ForegroundExecutor {
        self.0.lock().foreground_executor.clone()
    }

    unsafe fn pasteboard() -> id {
        unsafe { msg_send![class!(UIPasteboard), generalPasteboard] }
    }

    unsafe fn pasteboard_data(pasteboard: id, kind: &str) -> Option<Vec<u8>> {
        unsafe {
            let data: id = msg_send![pasteboard, dataForPasteboardType: ns_string(kind)];
            if data.is_null() {
                return None;
            }
            let bytes: *const u8 = msg_send![data, bytes];
            let length: usize = msg_send![data, length];
            if bytes.is_null() || length == 0 {
                return Some(Vec::new());
            }
            Some(slice::from_raw_parts(bytes, length).to_vec())
        }
    }

    unsafe fn write_plaintext_to_pasteboard(string: &ClipboardString) {
        unsafe {
            let item: id = msg_send![class!(NSMutableDictionary), dictionary];
            let _: () = msg_send![item, setObject: ns_string(&string.text) forKey: ns_string(UTI_UTF8_TEXT)];
            if let Some(metadata) = string.metadata.as_ref() {
                let hash = ClipboardString::text_hash(&string.text).to_be_bytes();
                let _: () =
                    msg_send![item, setObject: ns_data(&hash) forKey: ns_string(TEXT_HASH_TYPE)];
                let _: () = msg_send![item, setObject: ns_data(metadata.as_bytes()) forKey: ns_string(METADATA_TYPE)];
            }
            let items: id = msg_send![class!(NSArray), arrayWithObject: item];
            let _: () = msg_send![Self::pasteboard(), setItems: items];
        }
    }
}

/// Uniform type identifiers used on the pasteboard.
const UTI_UTF8_TEXT: &str = "public.utf8-plain-text";
const TEXT_HASH_TYPE: &str = "zed-text-hash";
const METADATA_TYPE: &str = "zed-metadata";

fn image_format_uti(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "public.png",
        ImageFormat::Jpeg => "public.jpeg",
        ImageFormat::Gif => "com.compuserve.gif",
        ImageFormat::Webp => "org.webmproject.webp",
        ImageFormat::Bmp => "com.microsoft.bmp",
        ImageFormat::Svg => "public.svg-image",
        ImageFormat::Tiff => "public.tiff",
    }
}

unsafe fn ns_data(bytes: &[u8]) -> id {
    unsafe { msg_send![class!(NSData), dataWithBytes: bytes.as_ptr() length: bytes.len()] }
}

impl Platform for IosPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.0.lock().background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.0.lock().foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.0.lock().text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        PLATFORM.store(self as *const Self as *mut Self, Ordering::Release);

        let mut state = self.0.lock();
        if state.headless {
            drop(state);
            on_finish_launching();
            unsafe { CFRunLoopRun() };
            return;
        }
        state.finish_launching = Some(on_finish_launching);
        drop(state);

        // `UIApplicationMain` owns the process from here on: it creates the
        // application and our delegate, whose launch callback opens the
        // windows, and never returns.
        let args: Vec<CString> = std::env::args()
            .filter_map(|arg| CString::new(arg).ok())
            .collect();
        let mut argv: Vec<*mut c_char> =
            args.iter().map(|arg| arg.as_ptr() as *mut c_char).collect();
        argv.push(ptr::null_mut());
        unsafe {
            UIApplicationMain(
                args.len() as c_int,
                argv.as_mut_ptr(),
                nil,
                ns_string("GPUIApplicationDelegate"),
            );
        }
    }

    fn quit(&self) {
        // iOS apps are not meant to exit themselves, but the request is
        // explicit. Run the quit callbacks off the current stack (they may
        // borrow app state the caller still holds) and then leave.
        use super::dispatcher::{dispatch_get_main_queue, dispatch_sys::dispatch_async_f};

        unsafe extern "C" fn quit(_: *mut c_void) {
            if let Some(platform) = platform() {
                let mut lock = platform.0.lock();
                if let Some(mut callback) = lock.quit.take() {
                    drop(lock);
                    callback();
                }
            }
            std::process::exit(0);
        }

        unsafe {
            dispatch_async_f(dispatch_get_main_queue(), ptr::null_mut(), Some(quit));
        }
    }

    fn restart(&self, _binary_path: Option<PathBuf>) {
        log::warn!("gpui: restarting the process is not possible on iOS");
    }

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![Rc::new(IosDisplay)]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay))
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        IosWindow::active_window()
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        Some(IosWindow::ordered_windows())
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let renderer_context = self.0.lock().renderer_context.clone();
        let window = IosWindow::open(
            handle,
            options,
            self.foreground_executor(),
            renderer_context,
        );
        self.attach_window(window.native_window());
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        unsafe {
            let screen: id = msg_send![class!(UIScreen), mainScreen];
            if screen.is_null() {
                return WindowAppearance::Light;
            }
            let traits: id = msg_send![screen, traitCollection];
            let style: isize = msg_send![traits, userInterfaceStyle];
            appearance_from_style(style)
        }
    }

    fn open_url(&self, url: &str) {
        unsafe {
            let url: id = msg_send![class!(NSURL), URLWithString: ns_string(url)];
            if url.is_null() {
                return;
            }
            let app: id = msg_send![class!(UIApplication), sharedApplication];
            let options: id = msg_send![class!(NSDictionary), dictionary];
            let _: () = msg_send![app, openURL: url options: options completionHandler: nil];
        }
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        let queued = {
            let mut lock = self.0.lock();
            lock.open_urls = Some(callback);
            std::mem::take(&mut lock.queued_urls)
        };
        if !queued.is_empty() {
            self.deliver_urls(queued);
        }
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "URL schemes are declared in Info.plist (CFBundleURLTypes) on iOS"
        )))
    }

    fn prompt_for_paths(
        &self,
        options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (tx, rx) = oneshot::channel();
        unsafe {
            let types: id = msg_send![class!(NSMutableArray), array];
            if options.files {
                let item: id =
                    msg_send![class!(UTType), typeWithIdentifier: ns_string("public.item")];
                let _: () = msg_send![types, addObject: item];
            }
            if options.directories {
                let folder: id =
                    msg_send![class!(UTType), typeWithIdentifier: ns_string("public.folder")];
                let _: () = msg_send![types, addObject: folder];
            }
            let picker: id = msg_send![class!(UIDocumentPickerViewController), alloc];
            let picker: id = msg_send![picker, initForOpeningContentTypes: types asCopy: NO];
            let _: () = msg_send![picker, setAllowsMultipleSelection: options.multiple.to_objc()];
            present_document_picker(picker, tx);
        }
        rx
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        // The document picker cannot name a path that does not exist yet,
        // so export an empty placeholder file; the URL it lands at is the
        // chosen path and the caller writes the real contents there.
        let (tx, rx) = oneshot::channel();
        let name = suggested_name.unwrap_or("Untitled");
        let temp_dir = std::env::temp_dir().join("gpui-save-as");
        let placeholder = temp_dir.join(name);
        if let Err(error) =
            std::fs::create_dir_all(&temp_dir).and_then(|_| std::fs::write(&placeholder, b""))
        {
            tx.send(Err(error.into())).ok();
            return rx;
        }
        unsafe {
            let url: id = msg_send![class!(NSURL), fileURLWithPath: ns_string(&placeholder.to_string_lossy())];
            let urls: id = msg_send![class!(NSArray), arrayWithObject: url];
            let picker: id = msg_send![class!(UIDocumentPickerViewController), alloc];
            let picker: id = msg_send![picker, initForExportingURLs: urls asCopy: YES];
            let (single_tx, single_rx) = oneshot::channel::<Result<Option<Vec<PathBuf>>>>();
            present_document_picker(picker, single_tx);
            self.foreground_executor()
                .spawn(async move {
                    let result = match single_rx.await {
                        Ok(Ok(paths)) => Ok(paths.and_then(|paths| paths.into_iter().next())),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Ok(None),
                    };
                    tx.send(result).ok();
                })
                .detach();
        }
        rx
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        true
    }

    fn reveal_path(&self, path: &Path) {
        // There is no file manager to reveal a path in; offer the system's
        // share/open-with options for it instead.
        self.open_with_system(path);
    }

    fn open_with_system(&self, path: &Path) {
        unsafe {
            let Some(controller) = presenting_view_controller() else {
                return;
            };
            let url: id =
                msg_send![class!(NSURL), fileURLWithPath: ns_string(&path.to_string_lossy())];
            let interaction: id = msg_send![class!(UIDocumentInteractionController), interactionControllerWithURL: url];
            let _: () = msg_send![interaction, retain];
            let mut lock = self.0.lock();
            let previous = std::mem::replace(&mut lock.interaction_controller, interaction);
            drop(lock);
            if !previous.is_null() {
                let _: () = msg_send![previous, release];
            }
            let view: id = msg_send![controller, view];
            let bounds: NSRect = msg_send![view, bounds];
            let _: BOOL = msg_send![interaction, presentOptionsMenuFromRect: bounds inView: view animated: YES];
        }
    }

    fn on_quit(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().reopen = Some(callback);
    }

    fn set_menus(&self, menus: Vec<Menu>, keymap: &Keymap) {
        {
            let mut lock = self.0.lock();
            let menus: Vec<OwnedMenu> = menus.into_iter().map(|menu| menu.owned()).collect();
            let mut actions = Vec::new();
            let mut os_actions = Vec::new();
            lock.prepared_menus = menus
                .iter()
                .map(|menu| PreparedMenu {
                    name: menu.name.to_string(),
                    items: prepare_items(&menu.items, keymap, &mut actions, &mut os_actions),
                })
                .collect();
            lock.menus = Some(menus);
            lock.menu_actions = actions;
            lock.os_actions = os_actions;
        }
        unsafe {
            let system: id = msg_send![class!(UIMenuSystem), mainSystem];
            let _: () = msg_send![system, setNeedsRebuild];
        }
    }

    fn get_menus(&self) -> Option<Vec<OwnedMenu>> {
        self.0.lock().menus.clone()
    }

    fn set_dock_menu(&self, menu: Vec<MenuItem>, _keymap: &Keymap) {
        // The nearest thing to a dock menu is the home-screen quick action
        // list: a flat list of titled actions.
        let mut actions = Vec::new();
        unsafe {
            let items: id = msg_send![class!(NSMutableArray), array];
            for item in menu {
                if let MenuItem::Action { name, action, .. } = item {
                    let kind = ns_string(&format!("{SHORTCUT_TYPE_PREFIX}{}", actions.len()));
                    let shortcut: id = msg_send![class!(UIApplicationShortcutItem), alloc];
                    let shortcut: id =
                        msg_send![shortcut, initWithType: kind localizedTitle: ns_string(&name)];
                    let _: () = msg_send![items, addObject: shortcut];
                    let _: () = msg_send![shortcut, release];
                    actions.push(action);
                }
            }
            let app: id = msg_send![class!(UIApplication), sharedApplication];
            let _: () = msg_send![app, setShortcutItems: items];
        }
        self.0.lock().dock_actions = actions;
    }

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.0.lock().menu_command = Some(callback);
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().will_open_menu = Some(callback);
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.0.lock().validate_menu_command = Some(callback);
    }

    fn app_path(&self) -> Result<PathBuf> {
        unsafe {
            let bundle: id = msg_send![class!(NSBundle), mainBundle];
            let path: id = msg_send![bundle, bundlePath];
            if path.is_null() {
                return Err(anyhow!("app is not running from a bundle"));
            }
            Ok(PathBuf::from(path.to_str()))
        }
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        Err(anyhow!("iOS apps cannot launch auxiliary executables"))
    }

    fn set_cursor_style(&self, _style: CursorStyle) {
        // The iPad pointer is shaped by UIPointerInteraction per view, not
        // set globally; the default pointer is left alone.
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        unsafe {
            match item.entries.len() {
                0 => {
                    let empty: id = msg_send![class!(NSArray), array];
                    let _: () = msg_send![Self::pasteboard(), setItems: empty];
                }
                1 => match &item.entries[0] {
                    ClipboardEntry::String(string) => Self::write_plaintext_to_pasteboard(string),
                    ClipboardEntry::Image(image) => {
                        let _: () = msg_send![
                            Self::pasteboard(),
                            setData: ns_data(&image.bytes)
                            forPasteboardType: ns_string(image_format_uti(image.format))
                        ];
                    }
                },
                _ => {
                    // Several entries: the text of all string entries, as
                    // the macOS backend does; images do not concatenate.
                    let text = item
                        .entries
                        .iter()
                        .filter_map(|entry| match entry {
                            ClipboardEntry::String(string) => Some(string.text.as_str()),
                            ClipboardEntry::Image(_) => None,
                        })
                        .collect::<String>();
                    Self::write_plaintext_to_pasteboard(&ClipboardString::new(text));
                }
            }
        }
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        unsafe {
            let pasteboard = Self::pasteboard();
            let has_strings: BOOL = msg_send![pasteboard, hasStrings];
            if has_strings == YES {
                let string: id = msg_send![pasteboard, string];
                let text = string.to_str().to_string();
                let metadata = Self::pasteboard_data(pasteboard, TEXT_HASH_TYPE)
                    .and_then(|hash| {
                        let hash = u64::from_be_bytes(hash.try_into().ok()?);
                        (hash == ClipboardString::text_hash(&text)).then_some(())
                    })
                    .and_then(|_| Self::pasteboard_data(pasteboard, METADATA_TYPE))
                    .and_then(|bytes| String::from_utf8(bytes).ok());
                return Some(ClipboardItem {
                    entries: vec![ClipboardEntry::String(ClipboardString { text, metadata })],
                });
            }
            for format in ImageFormat::iter() {
                if let Some(bytes) = Self::pasteboard_data(pasteboard, image_format_uti(format)) {
                    if bytes.is_empty() {
                        continue;
                    }
                    return Some(ClipboardItem {
                        entries: vec![ClipboardEntry::Image(Image::from_bytes(format, bytes))],
                    });
                }
            }
        }
        None
    }

    fn write_credentials(&self, url: &str, username: &str, password: &[u8]) -> Task<Result<()>> {
        let url = url.to_string();
        let username = username.to_string();
        let password = password.to_vec();
        self.background_executor().spawn(async move {
            unsafe {
                use security::*;

                let url = CFString::from(url.as_str());
                let username = CFString::from(username.as_str());
                let password = CFData::from_buffer(&password);

                let mut verb = "updating";
                let mut query_attrs = CFMutableDictionary::with_capacity(2);
                query_attrs.set(kSecClass as *const _, kSecClassInternetPassword as *const _);
                query_attrs.set(kSecAttrServer as *const _, url.as_CFTypeRef());

                let mut attrs = CFMutableDictionary::with_capacity(4);
                attrs.set(kSecClass as *const _, kSecClassInternetPassword as *const _);
                attrs.set(kSecAttrServer as *const _, url.as_CFTypeRef());
                attrs.set(kSecAttrAccount as *const _, username.as_CFTypeRef());
                attrs.set(kSecValueData as *const _, password.as_CFTypeRef());

                let mut status = SecItemUpdate(
                    query_attrs.as_concrete_TypeRef(),
                    attrs.as_concrete_TypeRef(),
                );
                if status == errSecItemNotFound {
                    verb = "creating";
                    status = SecItemAdd(attrs.as_concrete_TypeRef(), ptr::null_mut());
                }
                anyhow::ensure!(
                    status != errSecMissingEntitlement,
                    "{verb} password failed: {MISSING_ENTITLEMENT}"
                );
                anyhow::ensure!(status == errSecSuccess, "{verb} password failed: {status}");
            }
            Ok(())
        })
    }

    fn read_credentials(&self, url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        let url = url.to_string();
        self.background_executor().spawn(async move {
            let url = CFString::from(url.as_str());
            let cf_true = CFBoolean::true_value().as_CFTypeRef();

            unsafe {
                use security::*;

                let mut attrs = CFMutableDictionary::with_capacity(5);
                attrs.set(kSecClass as *const _, kSecClassInternetPassword as *const _);
                attrs.set(kSecAttrServer as *const _, url.as_CFTypeRef());
                attrs.set(kSecReturnAttributes as *const _, cf_true);
                attrs.set(kSecReturnData as *const _, cf_true);

                let mut result = CFTypeRef::from(ptr::null());
                let status = SecItemCopyMatching(attrs.as_concrete_TypeRef(), &mut result);
                match status {
                    security::errSecSuccess => {}
                    security::errSecItemNotFound | security::errSecUserCanceled => return Ok(None),
                    // An app signed without keychain entitlements (a bare
                    // Simulator bundle, say) has no keychain, which for a
                    // read means nothing stored; a write says why it
                    // cannot be kept.
                    security::errSecMissingEntitlement => {
                        log::warn!("{}", security::MISSING_ENTITLEMENT);
                        return Ok(None);
                    }
                    _ => anyhow::bail!("reading password failed: {status}"),
                }

                let result = CFType::wrap_under_create_rule(result)
                    .downcast::<CFDictionary>()
                    .context("keychain item was not a dictionary")?;
                let username = result
                    .find(kSecAttrAccount as *const _)
                    .context("account was missing from keychain item")?;
                let username = CFType::wrap_under_get_rule(*username)
                    .downcast::<CFString>()
                    .context("account was not a string")?;
                let password = result
                    .find(kSecValueData as *const _)
                    .context("password was missing from keychain item")?;
                let password = CFType::wrap_under_get_rule(*password)
                    .downcast::<CFData>()
                    .context("password was not a string")?;

                Ok(Some((username.to_string(), password.bytes().to_vec())))
            }
        })
    }

    fn delete_credentials(&self, url: &str) -> Task<Result<()>> {
        let url = url.to_string();
        self.background_executor().spawn(async move {
            unsafe {
                use security::*;

                let url = CFString::from(url.as_str());
                let mut query_attrs = CFMutableDictionary::with_capacity(2);
                query_attrs.set(kSecClass as *const _, kSecClassInternetPassword as *const _);
                query_attrs.set(kSecAttrServer as *const _, url.as_CFTypeRef());

                let status = SecItemDelete(query_attrs.as_concrete_TypeRef());
                if status == errSecItemNotFound || status == errSecMissingEntitlement {
                    return Ok(());
                }
                anyhow::ensure!(status == errSecSuccess, "delete password failed: {status}");
            }
            Ok(())
        })
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(IosKeyboardLayout::current())
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}

/// The active `UITextInputMode`'s language, which is as close to a keyboard
/// layout as iOS reports.
pub(crate) struct IosKeyboardLayout {
    id: String,
    name: String,
}

impl IosKeyboardLayout {
    fn current() -> Self {
        let language = unsafe {
            let mode: id = msg_send![class!(UITextInputMode), currentInputMode];
            if mode.is_null() {
                None
            } else {
                let language: id = msg_send![mode, primaryLanguage];
                let language = language.to_str();
                (!language.is_empty()).then(|| language.to_string())
            }
        };
        let id = language.unwrap_or_else(|| "unknown".to_string());
        Self {
            name: id.clone(),
            id,
        }
    }
}

impl PlatformKeyboardLayout for IosKeyboardLayout {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// A main-menu item with its action index and key equivalent resolved.
enum PreparedItem {
    Separator,
    Action {
        name: String,
        index: usize,
        os_action: Option<OsAction>,
        /// `(input, modifierFlags)` of the `UIKeyCommand`, if bound.
        key: Option<(String, usize)>,
    },
    Submenu(PreparedMenu),
}

struct PreparedMenu {
    name: String,
    items: Vec<PreparedItem>,
}

/// Resolves a menu's items: assigns each action an index into `actions`
/// (the `UICommand.propertyList`) and looks up its key equivalent, chosen
/// the same way as on macOS.
fn prepare_items(
    items: &[OwnedMenuItem],
    keymap: &Keymap,
    actions: &mut Vec<Box<dyn Action>>,
    os_actions: &mut Vec<(OsAction, usize)>,
) -> Vec<PreparedItem> {
    items
        .iter()
        .filter_map(|item| match item {
            OwnedMenuItem::Separator => Some(PreparedItem::Separator),
            OwnedMenuItem::Action {
                name,
                action,
                os_action,
            } => {
                let index = actions.len();
                if let Some(os_action) = os_action {
                    os_actions.push((*os_action, index));
                }
                actions.push(action.boxed_clone());
                let key = key_equivalent(keymap, action.as_ref()).and_then(|keystrokes| {
                    let [keystroke] = keystrokes else {
                        return None;
                    };
                    let modifiers = keystroke.modifiers();
                    let mut flags = 0usize;
                    if modifiers.platform {
                        flags |= UI_KEY_MODIFIER_COMMAND;
                    }
                    if modifiers.control {
                        flags |= UI_KEY_MODIFIER_CONTROL;
                    }
                    if modifiers.alt {
                        flags |= UI_KEY_MODIFIER_ALTERNATE;
                    }
                    if modifiers.shift {
                        flags |= UI_KEY_MODIFIER_SHIFT;
                    }
                    Some((key_to_ui_key_input(keystroke.key()).into_owned(), flags))
                });
                Some(PreparedItem::Action {
                    name: name.clone(),
                    index,
                    os_action: *os_action,
                    key,
                })
            }
            OwnedMenuItem::Submenu(menu) => Some(PreparedItem::Submenu(PreparedMenu {
                name: menu.name.to_string(),
                items: prepare_items(&menu.items, keymap, actions, os_actions),
            })),
            // The Services menu has no iOS equivalent.
            OwnedMenuItem::SystemMenu(_) => None,
        })
        .collect()
}

/// The keystrokes shown for a menu action, chosen the same way as on macOS.
fn key_equivalent<'a>(
    keymap: &'a Keymap,
    action: &'a dyn Action,
) -> Option<&'a [crate::KeybindingKeystroke]> {
    static DEFAULT_CONTEXT: OnceLock<Vec<KeyContext>> = OnceLock::new();
    keymap
        .bindings_for_action(action)
        .find_or_first(|binding| {
            binding.predicate().is_none_or(|predicate| {
                predicate.eval(DEFAULT_CONTEXT.get_or_init(|| {
                    let mut workspace_context = KeyContext::new_with_defaults();
                    workspace_context.add("Workspace");
                    let mut pane_context = KeyContext::new_with_defaults();
                    pane_context.add("Pane");
                    let mut editor_context = KeyContext::new_with_defaults();
                    editor_context.add("Editor");
                    pane_context.extend(&editor_context);
                    workspace_context.extend(&pane_context);
                    vec![workspace_context]
                }))
            })
        })
        .map(|binding| binding.keystrokes())
}

/// Builds the `UIMenuElement`s for a menu's items. Separators have no UIKit
/// counterpart; runs of items between them become inline submenus, which
/// UIKit draws separated. Returns the top-level children.
unsafe fn build_menu_children(items: &[PreparedItem], identifier_prefix: &str) -> id {
    unsafe {
        let children: id = msg_send![class!(NSMutableArray), array];
        let mut group: id = msg_send![class!(NSMutableArray), array];
        let mut group_count = 0usize;
        let mut group_index = 0usize;

        let flush =
            |children: id, group: &mut id, group_count: &mut usize, group_index: &mut usize| {
                if *group_count == 0 {
                    return;
                }
                let inline: id = msg_send![
                    class!(UIMenu),
                    menuWithTitle: ns_string("")
                    image: nil
                    identifier: ns_string(&format!("{identifier_prefix}.group{}", *group_index))
                    options: UI_MENU_OPTIONS_DISPLAY_INLINE
                    children: *group
                ];
                let _: () = msg_send![children, addObject: inline];
                *group = msg_send![class!(NSMutableArray), array];
                *group_count = 0;
                *group_index += 1;
            };

        for item in items {
            match item {
                PreparedItem::Separator => {
                    flush(children, &mut group, &mut group_count, &mut group_index)
                }
                PreparedItem::Action {
                    name,
                    index,
                    os_action,
                    key,
                } => {
                    let selector = match os_action {
                        Some(OsAction::Cut) => sel!(cut:),
                        Some(OsAction::Copy) => sel!(copy:),
                        Some(OsAction::Paste) => sel!(paste:),
                        Some(OsAction::SelectAll) => sel!(selectAll:),
                        Some(OsAction::Undo) | Some(OsAction::Redo) | None => {
                            sel!(handleGPUIMenuItem:)
                        }
                    };
                    let property: id = msg_send![class!(NSNumber), numberWithUnsignedLong: *index];
                    let command: id = match key {
                        Some((input, flags)) => msg_send![
                            class!(UIKeyCommand),
                            commandWithTitle: ns_string(name)
                            image: nil
                            action: selector
                            input: ns_string(input)
                            modifierFlags: *flags
                            propertyList: property
                        ],
                        None => msg_send![
                            class!(UICommand),
                            commandWithTitle: ns_string(name)
                            image: nil
                            action: selector
                            propertyList: property
                        ],
                    };
                    let _: () = msg_send![group, addObject: command];
                    group_count += 1;
                }
                PreparedItem::Submenu(menu) => {
                    let identifier = format!("{identifier_prefix}.{group_index}.{group_count}");
                    let submenu_children = build_menu_children(&menu.items, &identifier);
                    let submenu: id = msg_send![
                        class!(UIMenu),
                        menuWithTitle: ns_string(&menu.name)
                        image: nil
                        identifier: ns_string(&identifier)
                        options: 0usize
                        children: submenu_children
                    ];
                    let _: () = msg_send![group, addObject: submenu];
                    group_count += 1;
                }
            }
        }
        flush(children, &mut group, &mut group_count, &mut group_index);
        children
    }
}

/// `UIApplicationDelegate` / `UIResponder` callbacks.
extern "C" fn did_finish_launching(_this: &mut Object, _: Sel, _: id, _: id) -> BOOL {
    if let Some(platform) = platform() {
        let callback = platform.0.lock().finish_launching.take();
        if let Some(callback) = callback {
            callback();
        }
    }
    YES
}

extern "C" fn will_terminate(_this: &mut Object, _: Sel, _: id) {
    if let Some(platform) = platform() {
        let mut lock = platform.0.lock();
        if let Some(mut callback) = lock.quit.take() {
            drop(lock);
            callback();
            platform.0.lock().quit.get_or_insert(callback);
        }
    }
}

extern "C" fn scene_configuration(
    _this: &mut Object,
    _: Sel,
    _app: id,
    session: id,
    _options: id,
) -> id {
    unsafe {
        let role: id = msg_send![session, role];
        let configuration: id = msg_send![class!(UISceneConfiguration), alloc];
        let configuration: id =
            msg_send![configuration, initWithName: ns_string("gpui") sessionRole: role];
        let _: () = msg_send![configuration, setDelegateClass: SCENE_DELEGATE_CLASS];
        let _: () = msg_send![configuration, autorelease];
        configuration
    }
}

extern "C" fn app_open_url(_this: &mut Object, _: Sel, _app: id, url: id, _options: id) -> BOOL {
    if let Some(platform) = platform() {
        let url = unsafe {
            let string: id = msg_send![url, absoluteString];
            string.to_str().to_string()
        };
        platform.deliver_urls(vec![url]);
        YES
    } else {
        NO
    }
}

extern "C" fn app_perform_shortcut(_this: &mut Object, _: Sel, _app: id, item: id, handler: id) {
    if let Some(platform) = platform() {
        platform.perform_shortcut(item);
    }
    unsafe {
        let handler: &Block<(BOOL,), ()> = &*(handler as *const Block<(BOOL,), ()>);
        handler.call((YES,));
    }
}

extern "C" fn build_menu(_this: &mut Object, _: Sel, builder: id) {
    let Some(platform) = platform() else {
        return;
    };
    unsafe {
        let system: id = msg_send![builder, system];
        let main_system: id = msg_send![class!(UIMenuSystem), mainSystem];
        if system != main_system {
            return;
        }
    }

    let mut lock = platform.0.lock();
    if let Some(mut callback) = lock.will_open_menu.take() {
        drop(lock);
        callback();
        lock = platform.0.lock();
        lock.will_open_menu.get_or_insert(callback);
    }
    if lock.prepared_menus.is_empty() {
        return;
    }
    let menus = std::mem::take(&mut lock.prepared_menus);
    drop(lock);

    unsafe {
        // The app supplies the whole menu bar, as on macOS, so the standard
        // File/Edit/View/Format/Window/Help menus go; the application menu
        // keeps its system items (About, Settings, Hide, Quit) and gains
        // the first gpui menu's items, which is what that menu is for.
        for identifier in [
            UIMenuFile,
            UIMenuEdit,
            UIMenuView,
            UIMenuFormat,
            UIMenuWindow,
            UIMenuHelp,
        ] {
            let _: () = msg_send![builder, removeMenuForIdentifier: identifier];
        }

        let mut previous_identifier: id = UIMenuApplication;
        for (menu_index, menu) in menus.iter().enumerate() {
            let identifier = format!("{MENU_IDENTIFIER_PREFIX}{menu_index}");
            let children = build_menu_children(&menu.items, &identifier);
            if menu_index == 0 {
                let count: usize = msg_send![children, count];
                for i in (0..count).rev() {
                    let child: id = msg_send![children, objectAtIndex: i];
                    let _: () = msg_send![builder, insertChildMenu: child atStartOfMenuForIdentifier: UIMenuApplication];
                }
                continue;
            }
            let ui_menu: id = msg_send![
                class!(UIMenu),
                menuWithTitle: ns_string(&menu.name)
                image: nil
                identifier: ns_string(&identifier)
                options: 0usize
                children: children
            ];
            let _: () = msg_send![builder, insertSiblingMenu: ui_menu afterMenuForIdentifier: previous_identifier];
            previous_identifier = ns_string(&identifier);
        }
    }
    platform.0.lock().prepared_menus = menus;
}

fn command_action(command: id) -> Option<Box<dyn Action>> {
    let platform = platform()?;
    unsafe {
        let property: id = msg_send![command, propertyList];
        if property.is_null() {
            return None;
        }
        let is_number: BOOL = msg_send![property, isKindOfClass: class!(NSNumber)];
        if is_number == NO {
            return None;
        }
        let index: usize = msg_send![property, unsignedLongValue];
        platform
            .0
            .lock()
            .menu_actions
            .get(index)
            .map(|action| action.boxed_clone())
    }
}

extern "C" fn validate_command(_this: &mut Object, _: Sel, command: id) {
    let Some(platform) = platform() else {
        return;
    };
    if let Some(action) = command_action(command) {
        let enabled = platform.validate_action(action.as_ref());
        unsafe {
            let attributes: usize = if enabled {
                0
            } else {
                UI_MENU_ELEMENT_ATTRIBUTES_DISABLED
            };
            let _: () = msg_send![command, setAttributes: attributes];
        }
    }
}

extern "C" fn delegate_can_perform_action(
    _this: &mut Object,
    _: Sel,
    action: Sel,
    _sender: id,
) -> BOOL {
    let Some(platform) = platform() else {
        return NO;
    };
    if action == sel!(handleGPUIMenuItem:) {
        return YES;
    }
    os_action_for_selector(action)
        .is_some_and(|os_action| platform.can_perform_os_action(os_action))
        .to_objc()
}

pub(super) fn os_action_for_selector(selector: Sel) -> Option<OsAction> {
    if selector == sel!(cut:) {
        Some(OsAction::Cut)
    } else if selector == sel!(copy:) {
        Some(OsAction::Copy)
    } else if selector == sel!(paste:) {
        Some(OsAction::Paste)
    } else if selector == sel!(selectAll:) {
        Some(OsAction::SelectAll)
    } else if selector == sel!(undo:) {
        Some(OsAction::Undo)
    } else if selector == sel!(redo:) {
        Some(OsAction::Redo)
    } else {
        None
    }
}

extern "C" fn handle_menu_item(_this: &mut Object, _: Sel, sender: id) {
    let Some(platform) = platform() else {
        return;
    };
    if let Some(action) = command_action(sender) {
        platform.dispatch_menu_action(action.as_ref());
    }
}

extern "C" fn handle_os_action(_this: &mut Object, selector: Sel, sender: id) {
    let Some(platform) = platform() else {
        return;
    };
    // A menu command carries its own index; the edit menu and the standard
    // edit actions only carry the selector.
    if let Some(action) = command_action(sender) {
        platform.dispatch_menu_action(action.as_ref());
    } else if let Some(os_action) = os_action_for_selector(selector) {
        platform.perform_os_action(os_action);
    }
}

/// `UIWindowSceneDelegate` callbacks.
extern "C" fn scene_will_connect(_this: &mut Object, _: Sel, scene: id, _session: id, options: id) {
    let Some(platform) = platform() else {
        return;
    };
    let pending = {
        let mut lock = platform.0.lock();
        unsafe {
            let _: () = msg_send![scene, retain];
        }
        lock.window_scene = scene;
        std::mem::take(&mut lock.pending_windows)
    };
    for window in pending {
        platform.attach_window(window);
    }
    // A file or URL the app was launched for arrives with the scene,
    // not through `scene:openURLContexts:`; it waits in the queue until
    // the app registers its callback.
    if !options.is_null() {
        let contexts: id = unsafe { msg_send![options, URLContexts] };
        let urls = unsafe { urls_from_contexts(contexts) };
        if !urls.is_empty() {
            platform.deliver_urls(urls);
        }
    }
}

extern "C" fn scene_open_urls(_this: &mut Object, _: Sel, _scene: id, contexts: id) {
    let Some(platform) = platform() else {
        return;
    };
    let urls = unsafe { urls_from_contexts(contexts) };
    platform.deliver_urls(urls);
}

/// The URLs in a set of `UIOpenURLContext`s. A file another app hands
/// over to be opened in place (`openInPlace`) is readable only inside a
/// security scope, which is started here and never ended: the app may
/// keep the file open for its whole life, as a document editor does.
unsafe fn urls_from_contexts(contexts: id) -> Vec<String> {
    unsafe {
        if contexts.is_null() {
            return Vec::new();
        }
        let contexts: id = msg_send![contexts, allObjects];
        let count: usize = msg_send![contexts, count];
        (0..count)
            .map(|i| {
                let context: id = msg_send![contexts, objectAtIndex: i];
                let url: id = msg_send![context, URL];
                let options: id = msg_send![context, options];
                if !options.is_null() {
                    let in_place: BOOL = msg_send![options, openInPlace];
                    if in_place == YES {
                        let _: BOOL = msg_send![url, startAccessingSecurityScopedResource];
                    }
                }
                let string: id = msg_send![url, absoluteString];
                string.to_str().to_string()
            })
            .collect()
    }
}

extern "C" fn scene_perform_shortcut(
    _this: &mut Object,
    _: Sel,
    _scene: id,
    item: id,
    handler: id,
) {
    if let Some(platform) = platform() {
        platform.perform_shortcut(item);
    }
    unsafe {
        let handler: &Block<(BOOL,), ()> = &*(handler as *const Block<(BOOL,), ()>);
        handler.call((YES,));
    }
}

/// Document picker plumbing. The delegate object owns the sender and is
/// released once it has reported.
type PathsSender = oneshot::Sender<Result<Option<Vec<PathBuf>>>>;

unsafe fn present_document_picker(picker: id, sender: PathsSender) {
    unsafe {
        let Some(controller) = presenting_view_controller() else {
            sender
                .send(Err(anyhow!(
                    "no window to present the document picker from"
                )))
                .ok();
            let _: () = msg_send![picker, release];
            return;
        };
        let delegate: id = msg_send![DOCUMENT_PICKER_DELEGATE_CLASS, new];
        (*delegate).set_ivar(SENDER_IVAR, Box::into_raw(Box::new(sender)) as *mut c_void);
        let _: () = msg_send![picker, setDelegate: delegate];
        // The picker holds its delegate weakly; the delegate keeps the
        // picker until it reports, and releases both then.
        objc_set_associated_picker(delegate, picker);
        let _: () =
            msg_send![controller, presentViewController: picker animated: YES completion: nil];
        let _: () = msg_send![picker, release];
    }
}

const PICKER_KEY: &[u8] = b"gpui.picker\0";

unsafe fn objc_set_associated_picker(delegate: id, picker: id) {
    unsafe {
        // OBJC_ASSOCIATION_RETAIN_NONATOMIC
        objc_setAssociatedObject(delegate, PICKER_KEY.as_ptr() as *const c_void, picker, 1);
    }
}

unsafe fn take_sender(this: &mut Object) -> Option<PathsSender> {
    unsafe {
        let ptr: *mut c_void = *this.get_ivar(SENDER_IVAR);
        if ptr.is_null() {
            return None;
        }
        this.set_ivar(SENDER_IVAR, ptr::null_mut::<c_void>());
        Some(*Box::from_raw(ptr as *mut PathsSender))
    }
}

extern "C" fn did_pick_documents(this: &mut Object, _: Sel, _picker: id, urls: id) {
    unsafe {
        let paths = {
            let count: usize = msg_send![urls, count];
            (0..count)
                .map(|i| {
                    let url: id = msg_send![urls, objectAtIndex: i];
                    // Security-scoped access is kept for the process
                    // lifetime; the caller gets a plain path back.
                    let _: BOOL = msg_send![url, startAccessingSecurityScopedResource];
                    let path: id = msg_send![url, path];
                    PathBuf::from(path.to_str())
                })
                .collect::<Vec<_>>()
        };
        if let Some(sender) = take_sender(this) {
            sender.send(Ok(Some(paths))).ok();
        }
        let _: () = msg_send![this, autorelease];
    }
}

extern "C" fn picker_cancelled(this: &mut Object, _: Sel, _picker: id) {
    unsafe {
        if let Some(sender) = take_sender(this) {
            sender.send(Ok(None)).ok();
        }
        let _: () = msg_send![this, autorelease];
    }
}

#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {
    fn UIApplicationMain(argc: c_int, argv: *mut *mut c_char, principal: id, delegate: id)
    -> c_int;
    static UIMenuApplication: id;
    static UIMenuFile: id;
    static UIMenuEdit: id;
    static UIMenuView: id;
    static UIMenuFormat: id;
    static UIMenuWindow: id;
    static UIMenuHelp: id;
}

unsafe extern "C" {
    fn objc_setAssociatedObject(object: id, key: *const c_void, value: id, policy: usize);
}

mod security {
    #![allow(non_upper_case_globals)]
    use super::*;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        pub static kSecClass: CFStringRef;
        pub static kSecClassInternetPassword: CFStringRef;
        pub static kSecAttrServer: CFStringRef;
        pub static kSecAttrAccount: CFStringRef;
        pub static kSecValueData: CFStringRef;
        pub static kSecReturnAttributes: CFStringRef;
        pub static kSecReturnData: CFStringRef;

        pub fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        pub fn SecItemUpdate(query: CFDictionaryRef, attributes: CFDictionaryRef) -> OSStatus;
        pub fn SecItemDelete(query: CFDictionaryRef) -> OSStatus;
        pub fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
    }

    pub const errSecSuccess: OSStatus = 0;
    pub const errSecUserCanceled: OSStatus = -128;
    pub const errSecItemNotFound: OSStatus = -25300;
    pub const errSecMissingEntitlement: OSStatus = -34018;
    pub const MISSING_ENTITLEMENT: &str = "the app is not signed with keychain entitlements \
        (keychain-access-groups), so it has no keychain";
}
