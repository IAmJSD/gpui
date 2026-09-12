//! A gpui window on Android: the activity's surface. The window exists
//! from `open_window` on; the surface (`ANativeWindow`) comes and goes with
//! the activity's lifecycle, and the renderer with it. The sprite atlas
//! outlives the renderer, so the app's glyphs and images survive a trip to
//! the background.
//!
//! # Touch synthesis
//!
//! gpui's input model is a mouse. A finger becomes one, exactly as on iOS:
//!
//! * a tap is a left button press and release;
//! * a drag past the slop distance becomes a scroll (the pending left press
//!   is cancelled with a release far outside the window, the way browsers
//!   cancel a pointer when they take over a scroll) and gets momentum when
//!   the finger lifts;
//! * a long press is a right click; moving the finger after a long press
//!   drags with the left button held, which is how Android itself starts
//!   drags;
//! * two fingers dragging scroll, and their pinch is a `PinchEvent`, both
//!   at once;
//! * a stylus is a left button with pressure (its barrel button the right
//!   button) and never scrolls;
//! * a mouse is a real mouse: hover is `MouseMove`, its wheel a
//!   `ScrollWheel`, its buttons the buttons they are.

use super::{
    events::{
        capslock_from_meta_state, is_modifier_key, keystroke_from_key_event,
        modifiers_from_meta_state,
    },
    jni,
};
use crate::{
    AnyWindowHandle, Bounds, Capslock, DispatchEventResult, Edges, ForegroundExecutor, GpuSpecs,
    KeyDownEvent, KeyUpEvent, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels, PlatformAtlas,
    PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton,
    PromptLevel, RequestFrameOptions, ScrollDelta, ScrollWheelEvent, Size, TouchPhase,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea, WindowParams,
    platform::blade::{BladeAtlas, BladeContext, BladeRenderer, BladeSurfaceConfig},
    point, px, size,
};
use android_activity::{
    AndroidApp, InputStatus,
    input::{Axis, InputEvent, KeyAction, MotionAction, MotionEvent, Source, ToolType},
    ndk::{configuration::UiModeNight, native_window::NativeWindow},
};
use anyhow::Context as _;
use futures::channel::oneshot;
use raw_window_handle as rwh;
use std::{
    cell::RefCell,
    collections::VecDeque,
    ffi::c_void,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

/// How far a finger may travel before a press turns into a scroll, in
/// points (Android's own touch slop).
const TOUCH_SLOP: f32 = 8.0;
/// How long a still finger is held before it is a long press.
const LONG_PRESS: Duration = Duration::from_millis(500);
/// Taps closer in time and space than this count as a multi-click.
const MULTI_TAP_INTERVAL: Duration = Duration::from_millis(350);
const MULTI_TAP_DISTANCE: f32 = 24.0;
/// Deceleration of a flung scroll, per millisecond, and the speed below
/// which momentum stops.
const MOMENTUM_DECELERATION: f32 = 0.998;
const MOMENTUM_MIN_VELOCITY: f32 = 20.0;
/// A release this far outside the window cancels a press without a click.
const CANCEL_POSITION: Point<Pixels> = Point {
    x: Pixels(-100_000.0),
    y: Pixels(-100_000.0),
};

/// What the primary pointer has turned into.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum TouchMode {
    /// Pressed, not yet moved far enough to tell a tap from a scroll.
    Undecided,
    /// Scrolling; the pending press has been cancelled.
    Scroll,
    /// Dragging with the button held: mice, styluses, and fingers that
    /// moved after a long press.
    Drag,
    /// Long-pressed and still; a move starts a drag, a lift does nothing.
    LongPressed,
    /// Taken over by a two-finger gesture; ignore until it lifts.
    Cancelled,
}

struct PrimaryTouch {
    pointer_id: i32,
    button: MouseButton,
    mode: TouchMode,
    /// A mouse rather than a finger or stylus: it keeps hovering after it
    /// lets go, where a finger leaves nothing under it.
    pointer: bool,
    start: Point<Pixels>,
    last: Point<Pixels>,
    down_at: Instant,
    /// Recent positions, for the fling velocity.
    samples: VecDeque<(Instant, Point<Pixels>)>,
}

/// A second finger down: the two scroll together and pinch.
struct TwoFingerGesture {
    pointer_id: i32,
    last_centroid: Point<Pixels>,
    last_distance: f32,
}

struct Momentum {
    velocity: Point<f32>,
    position: Point<Pixels>,
    modifiers: Modifiers,
    last_tick: Instant,
}

pub(crate) struct AndroidWindowState {
    pub(super) handle: AnyWindowHandle,
    app: AndroidApp,
    #[allow(dead_code)]
    executor: ForegroundExecutor,
    renderer_context: BladeContext,
    atlas: Arc<BladeAtlas>,
    native_window: Option<NativeWindow>,
    renderer: Option<BladeRenderer>,
    /// The content size in points and the scale that maps it to the
    /// surface's pixels.
    size: Size<Pixels>,
    scale_factor: f32,
    insets: Edges<Pixels>,
    appearance: WindowAppearance,
    pub(super) active: bool,
    request_frame_callback: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    event_callback: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    activate_callback: Option<Box<dyn FnMut(bool)>>,
    hover_callback: Option<Box<dyn FnMut(bool)>>,
    resize_callback: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved_callback: Option<Box<dyn FnMut()>>,
    should_close_callback: Option<Box<dyn FnMut() -> bool>>,
    close_callback: Option<Box<dyn FnOnce()>>,
    appearance_changed_callback: Option<Box<dyn FnMut()>>,
    hit_test_window_control_callback: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
    input_handler: Option<PlatformInputHandler>,
    /// Whether the software keyboard has been asked for; mirrors
    /// `input_handler` once a frame.
    keyboard_shown: bool,
    primary_touch: Option<PrimaryTouch>,
    two_finger: Option<TwoFingerGesture>,
    momentum: Option<Momentum>,
    last_tap: Option<(Instant, Point<Pixels>, usize)>,
    mouse_position: Point<Pixels>,
    modifiers: Modifiers,
    capslock: Capslock,
    title: String,
}

impl AndroidWindowState {
    pub(super) fn has_surface(&self) -> bool {
        self.renderer.is_some()
    }

    pub(super) fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(point(px(0.), px(0.)), self.size)
    }

    fn scale_from_config(&self) -> f32 {
        let density = self.app.config().density().unwrap_or(160).max(1) as f32;
        density / 160.0
    }

    /// Reads the surface's size and the display density; `true` when
    /// either changed.
    fn read_geometry(&mut self) -> bool {
        let scale_factor = self.scale_from_config();
        let size = match &self.native_window {
            Some(window) => size(
                px(window.width().max(1) as f32 / scale_factor),
                px(window.height().max(1) as f32 / scale_factor),
            ),
            None => self.size,
        };
        let changed = size != self.size || scale_factor != self.scale_factor;
        self.size = size;
        self.scale_factor = scale_factor;
        if changed {
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.update_drawable_size(size.to_device_pixels(scale_factor));
            }
        }
        changed
    }
}

pub(crate) struct AndroidWindow(Rc<RefCell<AndroidWindowState>>);

impl AndroidWindow {
    pub(crate) fn open(
        handle: AnyWindowHandle,
        params: WindowParams,
        app: AndroidApp,
        executor: ForegroundExecutor,
        renderer_context: BladeContext,
    ) -> anyhow::Result<Self> {
        let atlas = Arc::new(BladeAtlas::new(&renderer_context.gpu));
        let appearance = appearance_from_config(&app);
        let title = params
            .titlebar
            .as_ref()
            .and_then(|titlebar| titlebar.title.as_ref())
            .map(|title| title.to_string())
            .unwrap_or_default();
        let mut state = AndroidWindowState {
            handle,
            app,
            executor,
            renderer_context,
            atlas,
            native_window: None,
            renderer: None,
            size: params.bounds.size,
            scale_factor: 1.0,
            insets: Edges::default(),
            appearance,
            active: false,
            request_frame_callback: None,
            event_callback: None,
            activate_callback: None,
            hover_callback: None,
            resize_callback: None,
            moved_callback: None,
            should_close_callback: None,
            close_callback: None,
            appearance_changed_callback: None,
            hit_test_window_control_callback: None,
            input_handler: None,
            keyboard_shown: false,
            primary_touch: None,
            two_finger: None,
            momentum: None,
            last_tap: None,
            mouse_position: Point::default(),
            modifiers: Modifiers::default(),
            capslock: Capslock::default(),
            title,
        };
        // Before the surface exists the configuration's screen size is the
        // best guess at the window's.
        state.scale_factor = state.scale_from_config();
        let config = state.app.config();
        if let (Some(width), Some(height)) = (config.screen_width_dp(), config.screen_height_dp()) {
            if width > 0 && height > 0 {
                state.size = size(px(width as f32), px(height as f32));
            }
        }
        Ok(Self(Rc::new(RefCell::new(state))))
    }

    pub(super) fn state(&self) -> &Rc<RefCell<AndroidWindowState>> {
        &self.0
    }
}

impl Drop for AndroidWindow {
    fn drop(&mut self) {
        detach_surface(&self.0);
        let mut state = self.0.borrow_mut();
        state.input_handler.take();
        state.atlas.destroy();
    }
}

/// The dark-mode setting, from the configuration's night mode.
pub(super) fn appearance_from_config(app: &AndroidApp) -> WindowAppearance {
    match app.config().ui_mode_night() {
        UiModeNight::Yes => WindowAppearance::Dark,
        _ => WindowAppearance::Light,
    }
}

/// The activity's surface, as blade wants it.
struct RawWindow(NativeWindow);

impl rwh::HasWindowHandle for RawWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let handle = rwh::AndroidNdkWindowHandle::new(self.0.ptr().cast::<c_void>());
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl rwh::HasDisplayHandle for RawWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(rwh::AndroidDisplayHandle::new().into()) })
    }
}

/// Gives the window the activity's surface: creates the renderer on it,
/// keeping the atlas the window already has.
pub(super) fn attach_surface(state: &Rc<RefCell<AndroidWindowState>>, native_window: NativeWindow) {
    {
        let mut lock = state.borrow_mut();
        if lock.renderer.is_some() {
            detach_surface_locked(&mut lock);
        }
        lock.native_window = Some(native_window.clone());
        lock.read_geometry();
        let size = lock.size.to_device_pixels(lock.scale_factor);
        let renderer = BladeRenderer::new_with_atlas(
            &lock.renderer_context,
            &RawWindow(native_window),
            BladeSurfaceConfig {
                size: blade_graphics::Extent {
                    width: size.width.0.max(1) as u32,
                    height: size.height.0.max(1) as u32,
                    depth: 1,
                },
                transparent: false,
            },
            lock.atlas.clone(),
        )
        .context("failed to create the Vulkan surface");
        match renderer {
            Ok(renderer) => lock.renderer = Some(renderer),
            Err(error) => {
                log::error!("gpui: {error:#}");
                lock.native_window = None;
                return;
            }
        }
    }
    notify_resize(state);
    request_frame(state);
}

fn detach_surface_locked(lock: &mut AndroidWindowState) {
    if let Some(mut renderer) = lock.renderer.take() {
        renderer.destroy_keeping_atlas();
    }
    lock.native_window = None;
}

/// Takes the surface away: the activity is going to the background (or
/// the window is closing). Drawing stops until the next `attach_surface`.
pub(super) fn detach_surface(state: &Rc<RefCell<AndroidWindowState>>) {
    detach_surface_locked(&mut state.borrow_mut());
}

/// Re-reads the surface size and density after a resize or a
/// configuration change.
pub(super) fn update_geometry(state: &Rc<RefCell<AndroidWindowState>>) {
    let changed = state.borrow_mut().read_geometry();
    if changed {
        notify_resize(state);
    }
}

/// Re-reads the system UI insets; a change is a relayout.
pub(super) fn poll_insets(state: &Rc<RefCell<AndroidWindowState>>, app: &AndroidApp) {
    if !state.borrow().has_surface() {
        return;
    }
    let insets = match jni::window_insets(app) {
        Ok(Some([top, right, bottom, left])) => {
            let scale = state.borrow().scale_factor;
            Edges {
                top: px(top as f32 / scale),
                right: px(right as f32 / scale),
                bottom: px(bottom as f32 / scale),
                left: px(left as f32 / scale),
            }
        }
        Ok(None) => Edges::default(),
        Err(error) => {
            log::warn!("gpui: could not read the window insets: {error}");
            return;
        }
    };
    {
        let mut lock = state.borrow_mut();
        if lock.insets == insets {
            return;
        }
        lock.insets = insets;
    }
    notify_resize(state);
}

pub(super) fn update_appearance(state: &Rc<RefCell<AndroidWindowState>>) {
    let mut lock = state.borrow_mut();
    let appearance = appearance_from_config(&lock.app);
    if appearance == lock.appearance {
        return;
    }
    lock.appearance = appearance;
    if let Some(mut callback) = lock.appearance_changed_callback.take() {
        drop(lock);
        callback();
        state.borrow_mut().appearance_changed_callback = Some(callback);
    }
}

pub(super) fn set_active(state: &Rc<RefCell<AndroidWindowState>>, active: bool) {
    let mut lock = state.borrow_mut();
    lock.active = active;
    if let Some(mut callback) = lock.activate_callback.take() {
        drop(lock);
        callback(active);
        state.borrow_mut().activate_callback = Some(callback);
    }
    let mut lock = state.borrow_mut();
    if let Some(mut callback) = lock.hover_callback.take() {
        drop(lock);
        callback(active);
        state.borrow_mut().hover_callback = Some(callback);
    }
}

/// Runs the frame callback, which draws if anything changed.
pub(super) fn request_frame(state: &Rc<RefCell<AndroidWindowState>>) {
    let mut lock = state.borrow_mut();
    if !lock.has_surface() {
        return;
    }
    if let Some(mut callback) = lock.request_frame_callback.take() {
        drop(lock);
        callback(RequestFrameOptions::default());
        state.borrow_mut().request_frame_callback = Some(callback);
    }
}

/// Shows the software keyboard while a text element has focus (the input
/// handler is present exactly then) and hides it otherwise.
pub(super) fn sync_keyboard(state: &Rc<RefCell<AndroidWindowState>>, app: &AndroidApp) {
    let mut lock = state.borrow_mut();
    let wants_keyboard = lock.input_handler.is_some() && lock.active;
    if wants_keyboard == lock.keyboard_shown {
        return;
    }
    lock.keyboard_shown = wants_keyboard;
    drop(lock);
    if wants_keyboard {
        app.show_soft_input(false);
    } else {
        app.hide_soft_input(false);
    }
}

fn notify_resize(state: &Rc<RefCell<AndroidWindowState>>) {
    let mut lock = state.borrow_mut();
    if let Some(mut callback) = lock.resize_callback.take() {
        let size = lock.size;
        let scale_factor = lock.scale_factor;
        drop(lock);
        callback(size, scale_factor);
        state.borrow_mut().resize_callback = Some(callback);
    }
}

/// Runs the window's input callback with `event`; `true` when handled.
pub(super) fn send_event(state: &Rc<RefCell<AndroidWindowState>>, event: PlatformInput) -> bool {
    let callback = state.borrow_mut().event_callback.take();
    if let Some(mut callback) = callback {
        let result = callback(event);
        state.borrow_mut().event_callback = Some(callback);
        !result.propagate
    } else {
        false
    }
}

impl PlatformWindow for AndroidWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().bounds()
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.0.borrow().size
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // The activity's window is the screen.
    }

    fn scale_factor(&self) -> f32 {
        self.0.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        self.0.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(super::AndroidDisplay::new(self.bounds())))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.0.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.0.borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        // A native dialog needs a Java click listener; gpui draws its own.
        None
    }

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        self.0.borrow().active
    }

    fn is_hovered(&self) -> bool {
        self.is_active()
    }

    fn set_title(&mut self, title: &str) {
        self.0.borrow_mut().title = title.to_string();
    }

    fn get_title(&self) -> String {
        self.0.borrow().title.clone()
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        false
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.borrow_mut().request_frame_callback = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.borrow_mut().event_callback = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.borrow_mut().activate_callback = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.borrow_mut().hover_callback = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.borrow_mut().resize_callback = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.borrow_mut().moved_callback = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.borrow_mut().should_close_callback = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.0.borrow_mut().hit_test_window_control_callback = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.borrow_mut().close_callback = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.borrow_mut().appearance_changed_callback = Some(callback);
    }

    fn draw(&self, scene: &crate::Scene) {
        let mut lock = self.0.borrow_mut();
        if let Some(renderer) = lock.renderer.as_mut() {
            renderer.draw(scene);
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.borrow().atlas.clone()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.0
            .borrow()
            .renderer
            .as_ref()
            .map(|renderer| renderer.gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // The software keyboard has no candidate window to place; text goes
        // in as key events.
    }

    fn safe_area_insets(&self) -> Edges<Pixels> {
        self.0.borrow().insets
    }

    fn claim_touch_drag(&self) {
        let mut lock = self.0.borrow_mut();
        if let Some(primary) = lock.primary_touch.as_mut() {
            if primary.mode == TouchMode::Undecided {
                primary.mode = TouchMode::Drag;
            }
        }
    }
}

impl rwh::HasWindowHandle for AndroidWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let lock = self.0.borrow();
        let window = lock
            .native_window
            .as_ref()
            .ok_or(rwh::HandleError::Unavailable)?;
        let handle = rwh::AndroidNdkWindowHandle::new(window.ptr().cast::<c_void>());
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}

impl rwh::HasDisplayHandle for AndroidWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(rwh::AndroidDisplayHandle::new().into()) })
    }
}

/// Input.
pub(super) fn handle_input(
    state: &Rc<RefCell<AndroidWindowState>>,
    app: &AndroidApp,
    event: &InputEvent<'_>,
) -> InputStatus {
    match event {
        InputEvent::MotionEvent(event) => {
            handle_motion(state, event);
            InputStatus::Handled
        }
        InputEvent::KeyEvent(event) => handle_key(state, app, event),
        _ => InputStatus::Unhandled,
    }
}

fn distance(a: Point<Pixels>, b: Point<Pixels>) -> f32 {
    let dx = (a.x - b.x).0;
    let dy = (a.y - b.y).0;
    (dx * dx + dy * dy).sqrt()
}

fn update_modifiers(
    state: &Rc<RefCell<AndroidWindowState>>,
    modifiers: Modifiers,
    capslock: Capslock,
) {
    let mut lock = state.borrow_mut();
    if lock.modifiers == modifiers && lock.capslock == capslock {
        return;
    }
    lock.modifiers = modifiers;
    lock.capslock = capslock;
    drop(lock);
    send_event(
        state,
        PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers,
            capslock,
        }),
    );
}

/// A pointer's position in points.
fn pointer_position(lock: &AndroidWindowState, x: f32, y: f32) -> Point<Pixels> {
    point(px(x / lock.scale_factor), px(y / lock.scale_factor))
}

fn handle_motion(state: &Rc<RefCell<AndroidWindowState>>, event: &MotionEvent<'_>) {
    let meta = event.meta_state();
    update_modifiers(
        state,
        modifiers_from_meta_state(meta),
        capslock_from_meta_state(meta),
    );

    let index = event.pointer_index();
    let pointer = event.pointer_at_index(index);
    let tool = pointer.tool_type();
    let is_mouse = tool == ToolType::Mouse || event.source() == Source::Mouse;
    let position = pointer_position(&state.borrow(), pointer.x(), pointer.y());
    let pressure = if matches!(tool, ToolType::Stylus | ToolType::Eraser) {
        pointer.pressure().clamp(0.0, 1.0)
    } else {
        1.0
    };

    match event.action() {
        MotionAction::Down => {
            let button = if is_mouse {
                let buttons = event.button_state();
                if buttons.secondary() || buttons.stylus_primary() {
                    MouseButton::Right
                } else if buttons.teriary() {
                    MouseButton::Middle
                } else {
                    MouseButton::Left
                }
            } else if event.button_state().stylus_primary()
                || event.button_state().stylus_secondary()
            {
                MouseButton::Right
            } else {
                MouseButton::Left
            };
            let mode = if is_mouse || matches!(tool, ToolType::Stylus | ToolType::Eraser) {
                TouchMode::Drag
            } else {
                TouchMode::Undecided
            };
            begin_primary(
                state,
                pointer.pointer_id(),
                position,
                button,
                mode,
                is_mouse,
                pressure,
            );
        }
        MotionAction::PointerDown => {
            if is_mouse {
                return;
            }
            begin_two_finger(state, event, pointer.pointer_id());
        }
        MotionAction::Move => {
            if state.borrow().two_finger.is_some() {
                move_two_finger(state, event);
                return;
            }
            // A move may carry several pointers; only the primary counts.
            let primary_id = state.borrow().primary_touch.as_ref().map(|p| p.pointer_id);
            let Some(primary_id) = primary_id else {
                return;
            };
            for pointer in event.pointers() {
                if pointer.pointer_id() != primary_id {
                    continue;
                }
                let position = pointer_position(&state.borrow(), pointer.x(), pointer.y());
                let pressure = if matches!(pointer.tool_type(), ToolType::Stylus | ToolType::Eraser)
                {
                    pointer.pressure().clamp(0.0, 1.0)
                } else {
                    1.0
                };
                move_primary(state, position, pressure);
            }
        }
        MotionAction::PointerUp => {
            end_two_finger(state, pointer.pointer_id(), position);
        }
        MotionAction::Up => {
            finish_touch(state, pointer.pointer_id(), position, false);
        }
        MotionAction::Cancel => {
            end_two_finger(state, pointer.pointer_id(), position);
            finish_touch(state, pointer.pointer_id(), position, true);
        }
        MotionAction::HoverEnter | MotionAction::HoverMove => {
            let mut lock = state.borrow_mut();
            lock.mouse_position = position;
            let modifiers = lock.modifiers;
            drop(lock);
            send_event(
                state,
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers,
                    pressure,
                }),
            );
        }
        MotionAction::HoverExit => {
            let modifiers = state.borrow().modifiers;
            send_event(
                state,
                PlatformInput::MouseExited(MouseExitEvent {
                    position,
                    pressed_button: None,
                    modifiers,
                }),
            );
        }
        MotionAction::Scroll => {
            let mut lock = state.borrow_mut();
            lock.momentum = None;
            lock.mouse_position = position;
            let modifiers = lock.modifiers;
            drop(lock);
            let horizontal = pointer.axis_value(Axis::Hscroll);
            let vertical = pointer.axis_value(Axis::Vscroll);
            send_event(
                state,
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Lines(point(horizontal, vertical)),
                    modifiers,
                    touch_phase: TouchPhase::Moved,
                }),
            );
        }
        _ => {}
    }
}

fn begin_primary(
    state: &Rc<RefCell<AndroidWindowState>>,
    pointer_id: i32,
    position: Point<Pixels>,
    button: MouseButton,
    mode: TouchMode,
    pointer: bool,
    pressure: f32,
) {
    let mut lock = state.borrow_mut();
    lock.momentum = None;
    lock.two_finger = None;
    let now = Instant::now();
    let click_count = match lock.last_tap {
        Some((at, at_position, count))
            if now.duration_since(at) < MULTI_TAP_INTERVAL
                && distance(at_position, position) < MULTI_TAP_DISTANCE =>
        {
            count + 1
        }
        _ => 1,
    };
    lock.last_tap = Some((now, position, click_count));
    lock.mouse_position = position;
    let modifiers = lock.modifiers;
    let mut samples = VecDeque::new();
    samples.push_back((now, position));
    lock.primary_touch = Some(PrimaryTouch {
        pointer_id,
        button,
        mode,
        pointer,
        start: position,
        last: position,
        down_at: now,
        samples,
    });
    drop(lock);
    send_event(
        state,
        PlatformInput::MouseDown(MouseDownEvent {
            button,
            position,
            modifiers,
            click_count,
            first_mouse: false,
            pressure,
        }),
    );
}

/// Releases a press that is not going to be a click. A press still
/// undecided is released far outside the window, so nothing under it sees
/// a click; a drag in progress is released where the pointer last was.
fn cancel_press(state: &Rc<RefCell<AndroidWindowState>>, button: MouseButton, mode: TouchMode) {
    let lock = state.borrow();
    let modifiers = lock.modifiers;
    let position = match mode {
        TouchMode::Drag => lock.mouse_position,
        _ => CANCEL_POSITION,
    };
    drop(lock);
    send_event(
        state,
        PlatformInput::MouseUp(MouseUpEvent {
            button,
            position,
            modifiers,
            click_count: 1,
            pressure: 1.0,
        }),
    );
}

/// A finger that has lifted hovers nothing. Once no finger is down and no
/// fling is running, move the mouse off the window so hover styles clear.
fn lift_hover(state: &Rc<RefCell<AndroidWindowState>>) {
    let lock = state.borrow();
    if lock.primary_touch.is_some() || lock.momentum.is_some() {
        return;
    }
    let modifiers = lock.modifiers;
    drop(lock);
    send_event(
        state,
        PlatformInput::MouseMove(MouseMoveEvent {
            position: CANCEL_POSITION,
            pressed_button: None,
            modifiers,
            pressure: 0.0,
        }),
    );
}

fn move_primary(state: &Rc<RefCell<AndroidWindowState>>, position: Point<Pixels>, pressure: f32) {
    let mut lock = state.borrow_mut();
    let Some((last, start, button, mode)) = lock.primary_touch.as_mut().map(|primary| {
        let now = Instant::now();
        primary.samples.push_back((now, position));
        while primary.samples.len() > 8 {
            primary.samples.pop_front();
        }
        let last = primary.last;
        primary.last = position;
        (last, primary.start, primary.button, primary.mode)
    }) else {
        return;
    };
    lock.mouse_position = position;
    let modifiers = lock.modifiers;
    let set_mode = |lock: &mut AndroidWindowState, mode: TouchMode| {
        if let Some(primary) = lock.primary_touch.as_mut() {
            primary.mode = mode;
        }
    };

    match mode {
        TouchMode::Undecided => {
            if distance(start, position) < TOUCH_SLOP {
                return;
            }
            set_mode(&mut lock, TouchMode::Scroll);
            drop(lock);
            cancel_press(state, button, TouchMode::Undecided);
            send_event(
                state,
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(position - start),
                    modifiers,
                    touch_phase: TouchPhase::Started,
                }),
            );
        }
        TouchMode::Scroll => {
            drop(lock);
            send_event(
                state,
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(position - last),
                    modifiers,
                    touch_phase: TouchPhase::Moved,
                }),
            );
        }
        TouchMode::LongPressed => {
            if distance(start, position) < TOUCH_SLOP {
                return;
            }
            set_mode(&mut lock, TouchMode::Drag);
            drop(lock);
            send_event(
                state,
                PlatformInput::MouseDown(MouseDownEvent {
                    button,
                    position: start,
                    modifiers,
                    click_count: 1,
                    first_mouse: false,
                    pressure,
                }),
            );
            send_event(
                state,
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: Some(button),
                    modifiers,
                    pressure,
                }),
            );
        }
        TouchMode::Drag => {
            drop(lock);
            send_event(
                state,
                PlatformInput::MouseMove(MouseMoveEvent {
                    position,
                    pressed_button: Some(button),
                    modifiers,
                    pressure,
                }),
            );
        }
        TouchMode::Cancelled => {}
    }
}

fn finish_touch(
    state: &Rc<RefCell<AndroidWindowState>>,
    pointer_id: i32,
    position: Point<Pixels>,
    cancelled: bool,
) {
    let mut lock = state.borrow_mut();
    let Some(primary) = lock.primary_touch.as_ref() else {
        return;
    };
    if primary.pointer_id != pointer_id {
        return;
    }
    let primary = lock.primary_touch.take().unwrap();
    let modifiers = lock.modifiers;
    lock.mouse_position = position;
    let click_count = lock.last_tap.map_or(1, |(_, _, count)| count);
    drop(lock);

    match primary.mode {
        TouchMode::Undecided if cancelled => {
            cancel_press(state, primary.button, TouchMode::Undecided);
        }
        // A finger that lifted far from where it landed without a move
        // event in between (a very fast swipe) scrolled, not tapped.
        TouchMode::Undecided
            if !primary.pointer && distance(primary.start, position) >= TOUCH_SLOP =>
        {
            cancel_press(state, primary.button, TouchMode::Undecided);
            send_event(
                state,
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(position - primary.start),
                    modifiers,
                    touch_phase: TouchPhase::Started,
                }),
            );
            send_event(
                state,
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(Point::default()),
                    modifiers,
                    touch_phase: TouchPhase::Ended,
                }),
            );
            state.borrow_mut().last_tap = None;
            start_momentum(state, &primary, position, modifiers);
        }
        TouchMode::Undecided | TouchMode::Drag => {
            send_event(
                state,
                PlatformInput::MouseUp(MouseUpEvent {
                    button: primary.button,
                    position,
                    modifiers,
                    click_count,
                    pressure: 1.0,
                }),
            );
            if primary.mode == TouchMode::Drag && !primary.pointer {
                // Stylus ups are not taps.
                state.borrow_mut().last_tap = None;
            }
        }
        TouchMode::Scroll => {
            send_event(
                state,
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(Point::default()),
                    modifiers,
                    touch_phase: TouchPhase::Ended,
                }),
            );
            state.borrow_mut().last_tap = None;
            if !cancelled {
                start_momentum(state, &primary, position, modifiers);
            }
        }
        TouchMode::LongPressed | TouchMode::Cancelled => {
            state.borrow_mut().last_tap = None;
        }
    }
    if !primary.pointer {
        lift_hover(state);
    }
}

/// Two fingers: the press in progress is cancelled, and from here on the
/// pair scrolls and pinches until one lifts.
fn begin_two_finger(
    state: &Rc<RefCell<AndroidWindowState>>,
    event: &MotionEvent<'_>,
    pointer_id: i32,
) {
    let mut lock = state.borrow_mut();
    if lock.two_finger.is_some() || lock.primary_touch.is_none() {
        // A third finger, or a second with no first: ignored.
        return;
    }
    lock.momentum = None;
    let cancel = lock.primary_touch.as_mut().and_then(|primary| {
        let needs_cancel = matches!(
            primary.mode,
            TouchMode::Undecided | TouchMode::Drag | TouchMode::LongPressed
        );
        let result = needs_cancel.then_some((primary.button, primary.mode));
        primary.mode = TouchMode::Cancelled;
        result
    });
    let Some((centroid, distance)) = two_finger_geometry(&lock, event) else {
        return;
    };
    lock.two_finger = Some(TwoFingerGesture {
        pointer_id,
        last_centroid: centroid,
        last_distance: distance,
    });
    lock.mouse_position = centroid;
    let modifiers = lock.modifiers;
    drop(lock);
    if let Some((button, mode)) = cancel {
        cancel_press(state, button, mode);
    }
    send_event(
        state,
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: centroid,
            delta: ScrollDelta::Pixels(Point::default()),
            modifiers,
            touch_phase: TouchPhase::Started,
        }),
    );
    send_event(
        state,
        PlatformInput::Pinch(PinchEvent {
            position: centroid,
            delta: 1.0,
            scale: 1.0,
            modifiers,
            phase: TouchPhase::Started,
        }),
    );
}

/// The centroid and separation of the primary and second pointers.
fn two_finger_geometry(
    lock: &AndroidWindowState,
    event: &MotionEvent<'_>,
) -> Option<(Point<Pixels>, f32)> {
    let primary_id = lock.primary_touch.as_ref()?.pointer_id;
    let second_id = lock
        .two_finger
        .as_ref()
        .map(|gesture| gesture.pointer_id)
        .or_else(|| {
            event
                .pointers()
                .map(|pointer| pointer.pointer_id())
                .find(|id| *id != primary_id)
        })?;
    let mut first = None;
    let mut second = None;
    for pointer in event.pointers() {
        let position = pointer_position(lock, pointer.x(), pointer.y());
        if pointer.pointer_id() == primary_id {
            first = Some(position);
        } else if pointer.pointer_id() == second_id {
            second = Some(position);
        }
    }
    let (first, second) = (first?, second?);
    let centroid = point(
        px((first.x.0 + second.x.0) / 2.0),
        px((first.y.0 + second.y.0) / 2.0),
    );
    Some((centroid, distance(first, second).max(1.0)))
}

fn move_two_finger(state: &Rc<RefCell<AndroidWindowState>>, event: &MotionEvent<'_>) {
    let mut lock = state.borrow_mut();
    let Some((centroid, distance)) = two_finger_geometry(&lock, event) else {
        return;
    };
    let Some(gesture) = lock.two_finger.as_mut() else {
        return;
    };
    let translation = centroid - gesture.last_centroid;
    let scale_delta = if gesture.last_distance > 0.0 {
        distance / gesture.last_distance
    } else {
        1.0
    };
    gesture.last_centroid = centroid;
    gesture.last_distance = distance;
    lock.mouse_position = centroid;
    let modifiers = lock.modifiers;
    drop(lock);
    send_event(
        state,
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: centroid,
            delta: ScrollDelta::Pixels(translation),
            modifiers,
            touch_phase: TouchPhase::Moved,
        }),
    );
    send_event(
        state,
        PlatformInput::Pinch(PinchEvent {
            position: centroid,
            delta: scale_delta,
            scale: 1.0,
            modifiers,
            phase: TouchPhase::Moved,
        }),
    );
}

/// One of the two fingers lifted: the gesture ends, and whichever finger
/// remains is ignored until it lifts too.
fn end_two_finger(
    state: &Rc<RefCell<AndroidWindowState>>,
    pointer_id: i32,
    position: Point<Pixels>,
) {
    let mut lock = state.borrow_mut();
    let Some(gesture) = lock.two_finger.take() else {
        return;
    };
    let modifiers = lock.modifiers;
    let centroid = gesture.last_centroid;
    if gesture.pointer_id == pointer_id {
        // The second finger lifted; the primary stays cancelled.
    } else if let Some(primary) = lock.primary_touch.as_mut() {
        // The primary lifted; the second finger carries on as a cancelled
        // primary so its own lift is accounted for.
        primary.pointer_id = gesture.pointer_id;
        primary.mode = TouchMode::Cancelled;
        primary.last = position;
    }
    drop(lock);
    send_event(
        state,
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position: centroid,
            delta: ScrollDelta::Pixels(Point::default()),
            modifiers,
            touch_phase: TouchPhase::Ended,
        }),
    );
    send_event(
        state,
        PlatformInput::Pinch(PinchEvent {
            position: centroid,
            delta: 1.0,
            scale: 1.0,
            modifiers,
            phase: TouchPhase::Ended,
        }),
    );
}

/// Momentum scrolling after a fling.
fn start_momentum(
    state: &Rc<RefCell<AndroidWindowState>>,
    primary: &PrimaryTouch,
    position: Point<Pixels>,
    modifiers: Modifiers,
) {
    let now = Instant::now();
    let Some((then, then_position)) = primary
        .samples
        .iter()
        .find(|(at, _)| now.duration_since(*at) <= Duration::from_millis(120))
        .copied()
    else {
        return;
    };
    let elapsed = now.duration_since(then).as_secs_f32();
    if elapsed <= 0.0 {
        return;
    }
    let velocity = point(
        (position.x - then_position.x).0 / elapsed,
        (position.y - then_position.y).0 / elapsed,
    );
    if velocity.x.abs() < MOMENTUM_MIN_VELOCITY && velocity.y.abs() < MOMENTUM_MIN_VELOCITY {
        return;
    }
    state.borrow_mut().momentum = Some(Momentum {
        velocity,
        position,
        modifiers,
        last_tick: now,
    });
}

fn tick_momentum(state: &Rc<RefCell<AndroidWindowState>>) {
    let mut lock = state.borrow_mut();
    let Some(momentum) = lock.momentum.as_mut() else {
        return;
    };
    let now = Instant::now();
    let dt = now
        .duration_since(momentum.last_tick)
        .as_secs_f32()
        .min(0.1);
    momentum.last_tick = now;
    let delta = point(px(momentum.velocity.x * dt), px(momentum.velocity.y * dt));
    let decay = MOMENTUM_DECELERATION.powf(dt * 1000.0);
    momentum.velocity = point(momentum.velocity.x * decay, momentum.velocity.y * decay);
    let position = momentum.position;
    let modifiers = momentum.modifiers;
    let finished = momentum.velocity.x.abs() < MOMENTUM_MIN_VELOCITY
        && momentum.velocity.y.abs() < MOMENTUM_MIN_VELOCITY;
    if finished {
        lock.momentum = None;
    }
    drop(lock);
    send_event(
        state,
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Pixels(delta),
            modifiers,
            touch_phase: if finished {
                TouchPhase::Ended
            } else {
                TouchPhase::Moved
            },
        }),
    );
    if finished {
        lift_hover(state);
    }
}

/// A finger held still past the long-press time is a right click.
fn tick_long_press(state: &Rc<RefCell<AndroidWindowState>>) {
    let mut lock = state.borrow_mut();
    let Some(primary) = lock.primary_touch.as_mut() else {
        return;
    };
    if primary.mode != TouchMode::Undecided
        || primary.pointer
        || primary.down_at.elapsed() < LONG_PRESS
        || distance(primary.start, primary.last) >= TOUCH_SLOP
    {
        return;
    }
    let button = primary.button;
    let position = primary.last;
    primary.mode = TouchMode::LongPressed;
    let modifiers = lock.modifiers;
    drop(lock);
    cancel_press(state, button, TouchMode::Undecided);
    send_event(
        state,
        PlatformInput::MouseDown(MouseDownEvent {
            button: MouseButton::Right,
            position,
            modifiers,
            click_count: 1,
            first_mouse: false,
            pressure: 1.0,
        }),
    );
    send_event(
        state,
        PlatformInput::MouseUp(MouseUpEvent {
            button: MouseButton::Right,
            position,
            modifiers,
            click_count: 1,
            pressure: 1.0,
        }),
    );
}

/// Runs once a frame: the long-press timer and momentum scrolling.
pub(super) fn tick_touch_timers(state: &Rc<RefCell<AndroidWindowState>>) {
    tick_long_press(state);
    tick_momentum(state);
}

/// Keys. What gpui leaves unhandled is reported as such, so the system
/// gets its default (the back key finishes the activity, volume keys
/// change the volume). A printable key no binding took is typed into the
/// focused text field, as the desktop backends do.
fn handle_key(
    state: &Rc<RefCell<AndroidWindowState>>,
    app: &AndroidApp,
    event: &android_activity::input::KeyEvent<'_>,
) -> InputStatus {
    let meta = event.meta_state();
    let modifiers = modifiers_from_meta_state(meta);
    let capslock = capslock_from_meta_state(meta);
    match event.action() {
        KeyAction::Down => {
            update_modifiers(state, modifiers, capslock);
            if is_modifier_key(event.key_code()) {
                return InputStatus::Handled;
            }
            let Some(keystroke) = keystroke_from_key_event(app, event) else {
                return InputStatus::Unhandled;
            };
            let key_char = keystroke.key_char.clone();
            let handled = send_event(
                state,
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke,
                    is_held: event.repeat_count() > 0,
                }),
            );
            if handled {
                return InputStatus::Handled;
            }
            // Unbound and printable: it is text for the focused field, as
            // the desktop backends insert it.
            let Some(text) = key_char else {
                return InputStatus::Unhandled;
            };
            let handler = state.borrow_mut().input_handler.take();
            let Some(mut handler) = handler else {
                return InputStatus::Unhandled;
            };
            handler.replace_text_in_range(None, &text);
            state.borrow_mut().input_handler.get_or_insert(handler);
            InputStatus::Handled
        }
        KeyAction::Up => {
            update_modifiers(state, modifiers, capslock);
            if is_modifier_key(event.key_code()) {
                return InputStatus::Handled;
            }
            let Some(keystroke) = keystroke_from_key_event(app, event) else {
                return InputStatus::Unhandled;
            };
            if send_event(state, PlatformInput::KeyUp(KeyUpEvent { keystroke })) {
                InputStatus::Handled
            } else {
                InputStatus::Unhandled
            }
        }
        _ => InputStatus::Unhandled,
    }
}
