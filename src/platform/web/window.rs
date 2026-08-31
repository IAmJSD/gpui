use super::{WebDisplay, WebGpuContext, WebGpuRenderer};
use crate::{
    AnyWindowHandle, Bounds, Capslock, DevicePixels, DispatchEventResult, GpuSpecs, KeyDownEvent,
    KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, Pixels, PlatformAtlas,
    PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point, PromptButton,
    PromptLevel, RequestFrameOptions, Scene, ScrollDelta, ScrollWheelEvent, Size,
    TouchPhase, WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowParams, point,
    px, size,
};
use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use wasm_bindgen::{JsCast, prelude::Closure};
use web_sys::{HtmlCanvasElement, KeyboardEvent, PointerEvent, WheelEvent};
use web_time::Instant;

/// If an element with this id exists and is a `<canvas>`, the window renders
/// into it; otherwise a full-viewport canvas is created and appended to the
/// document body.
const CANVAS_ELEMENT_ID: &str = "gpui";

/// Consecutive clicks within this interval and [`DOUBLE_CLICK_DISTANCE`] of
/// each other increment the click count, like the X11 backend's own counting
/// (the DOM's `detail` counter is not available on pointer events).
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const DOUBLE_CLICK_DISTANCE: Pixels = px(4.0);

/// Kept in sync with SCROLL_LINES in the Linux backends.
const SCROLL_LINES: f32 = 3.0;

#[derive(Default)]
struct Callbacks {
    request_frame: RefCell<Option<Box<dyn FnMut(RequestFrameOptions)>>>,
    #[allow(clippy::type_complexity)]
    input: RefCell<Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>>,
    active_status_change: RefCell<Option<Box<dyn FnMut(bool)>>>,
    hover_status_change: RefCell<Option<Box<dyn FnMut(bool)>>>,
    resize: RefCell<Option<Box<dyn FnMut(Size<Pixels>, f32)>>>,
    moved: RefCell<Option<Box<dyn FnMut()>>>,
    should_close: RefCell<Option<Box<dyn FnMut() -> bool>>>,
    close: RefCell<Option<Box<dyn FnOnce()>>>,
    appearance_changed: RefCell<Option<Box<dyn FnMut()>>>,
}

struct ClickState {
    last_click: Instant,
    last_position: Point<Pixels>,
    button: MouseButton,
    count: usize,
}

struct WebWindowState {
    renderer: WebGpuRenderer,
    input_handler: Option<PlatformInputHandler>,
    mouse_position: Point<Pixels>,
    modifiers: Modifiers,
    capslock: Capslock,
    click: Option<ClickState>,
}

pub(crate) struct WebWindowInner {
    canvas: HtmlCanvasElement,
    state: RefCell<WebWindowState>,
    callbacks: Callbacks,
    active: Cell<bool>,
    hovered: Cell<bool>,
    /// The DOM event listener closures; dropping them detaches nothing, but
    /// they must stay alive for as long as the listeners can fire.
    listeners: RefCell<Vec<Box<dyn Any>>>,
}

pub(crate) struct WebWindow(Rc<WebWindowInner>);

impl WebWindow {
    pub(crate) fn new(
        gpu: &Arc<WebGpuContext>,
        _handle: AnyWindowHandle,
        _params: WindowParams,
    ) -> Result<Self> {
        let document = web_sys::window()
            .context("no global `window`")?
            .document()
            .context("no `document`")?;

        let canvas = match document
            .get_element_by_id(CANVAS_ELEMENT_ID)
            .and_then(|element| element.dyn_into::<HtmlCanvasElement>().ok())
        {
            Some(canvas) => canvas,
            None => {
                let canvas = document
                    .create_element("canvas")
                    .ok()
                    .and_then(|element| element.dyn_into::<HtmlCanvasElement>().ok())
                    .context("failed to create a canvas element")?;
                canvas.set_id(CANVAS_ELEMENT_ID);
                let style = canvas.style();
                style.set_property("position", "fixed").ok();
                style.set_property("inset", "0").ok();
                style.set_property("width", "100vw").ok();
                style.set_property("height", "100vh").ok();
                document
                    .body()
                    .context("document has no body")?
                    .append_child(&canvas)
                    .ok()
                    .context("failed to append the canvas to the document body")?;
                canvas
            }
        };

        // raw-window-handle's convention for addressing canvases.
        canvas.set_attribute("data-raw-handle", "1").ok();

        let device_size = device_size_for(&canvas);
        canvas.set_width(device_size.width.0 as u32);
        canvas.set_height(device_size.height.0 as u32);

        let renderer = WebGpuRenderer::new(gpu, &canvas, device_size)?;

        let window = Self(Rc::new(WebWindowInner {
            canvas,
            state: RefCell::new(WebWindowState {
                renderer,
                input_handler: None,
                mouse_position: Point::default(),
                modifiers: Modifiers::default(),
                capslock: Capslock::default(),
                click: None,
            }),
            callbacks: Callbacks::default(),
            active: Cell::new(
                document
                    .has_focus()
                    .unwrap_or(false),
            ),
            hovered: Cell::new(false),
            listeners: RefCell::new(Vec::new()),
        }));
        start_frame_loop(Rc::downgrade(&window.0));
        setup_event_listeners(&window.0);
        Ok(window)
    }
}

fn scale_factor() -> f32 {
    web_sys::window()
        .map(|window| window.device_pixel_ratio() as f32)
        .unwrap_or(1.0)
        .max(0.1)
}

fn css_size(canvas: &HtmlCanvasElement) -> Size<Pixels> {
    size(
        px(canvas.client_width().max(0) as f32),
        px(canvas.client_height().max(0) as f32),
    )
}

fn device_size_for(canvas: &HtmlCanvasElement) -> Size<DevicePixels> {
    let scale = scale_factor();
    let css = css_size(canvas);
    Size {
        width: DevicePixels((css.width.0 * scale).round().max(1.0) as i32),
        height: DevicePixels((css.height.0 * scale).round().max(1.0) as i32),
    }
}

fn prefers_dark_appearance() -> bool {
    web_sys::window()
        .and_then(|window| window.match_media("(prefers-color-scheme: dark)").ok())
        .flatten()
        .is_some_and(|query| query.matches())
}

/// Drives the window from `requestAnimationFrame`: applies any size or scale
/// change, then asks gpui for a frame. The closure keeps only a weak
/// reference, so dropping the window ends the loop.
fn start_frame_loop(inner: Weak<WebWindowInner>) {
    let closure: Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>> = Rc::new(RefCell::new(None));
    let closure_for_body = closure.clone();
    *closure.borrow_mut() = Some(Closure::new(move |_timestamp: f64| {
        let Some(inner) = inner.upgrade() else {
            // Window is gone; drop the closure to break the Rc cycle.
            closure_for_body.borrow_mut().take();
            return;
        };
        inner.on_animation_frame();
        if let Some(closure) = closure_for_body.borrow().as_ref() {
            request_animation_frame(closure);
        }
    }));
    request_animation_frame(closure.borrow().as_ref().unwrap());
}

fn request_animation_frame(closure: &Closure<dyn FnMut(f64)>) {
    web_sys::window()
        .expect("no global `window`")
        .request_animation_frame(closure.as_ref().unchecked_ref())
        .expect("requestAnimationFrame failed");
}

impl WebWindowInner {
    fn on_animation_frame(&self) {
        let device_size = device_size_for(&self.canvas);
        if device_size.width.0 as u32 != self.canvas.width()
            || device_size.height.0 as u32 != self.canvas.height()
        {
            self.canvas.set_width(device_size.width.0 as u32);
            self.canvas.set_height(device_size.height.0 as u32);
            self.state
                .borrow_mut()
                .renderer
                .update_drawable_size(device_size);
            let mut resize = self.callbacks.resize.borrow_mut().take();
            if let Some(callback) = resize.as_mut() {
                callback(css_size(&self.canvas), scale_factor());
            }
            replace_if_empty(&self.callbacks.resize, resize);
        }

        let mut request_frame = self.callbacks.request_frame.borrow_mut().take();
        if let Some(callback) = request_frame.as_mut() {
            callback(RequestFrameOptions::default());
        }
        replace_if_empty(&self.callbacks.request_frame, request_frame);
    }

    fn dispatch_input(&self, input: PlatformInput) -> DispatchEventResult {
        let mut callback = self.callbacks.input.borrow_mut().take();
        let result = if let Some(callback) = callback.as_mut() {
            callback(input)
        } else {
            DispatchEventResult::default()
        };
        replace_if_empty(&self.callbacks.input, callback);
        result
    }

    /// Increments or restarts the click chain and returns the click count for
    /// a press at `position`.
    fn click_count(&self, button: MouseButton, position: Point<Pixels>) -> usize {
        let mut state = self.state.borrow_mut();
        let now = Instant::now();
        let count = match &state.click {
            Some(click)
                if click.button == button
                    && now.duration_since(click.last_click) <= DOUBLE_CLICK_INTERVAL
                    && (click.last_position.x - position.x).abs() <= DOUBLE_CLICK_DISTANCE
                    && (click.last_position.y - position.y).abs() <= DOUBLE_CLICK_DISTANCE =>
            {
                click.count + 1
            }
            _ => 1,
        };
        state.click = Some(ClickState {
            last_click: now,
            last_position: position,
            button,
            count,
        });
        count
    }
}

/// Put a taken callback back unless a reentrant call installed a new one
/// while it was out.
fn replace_if_empty<T>(slot: &RefCell<Option<T>>, value: Option<T>) {
    let mut slot = slot.borrow_mut();
    if slot.is_none() {
        *slot = value;
    }
}

// --- DOM input translation --- //

fn mouse_button(button: i16) -> Option<MouseButton> {
    match button {
        0 => Some(MouseButton::Left),
        1 => Some(MouseButton::Middle),
        2 => Some(MouseButton::Right),
        3 => Some(MouseButton::Navigate(NavigationDirection::Back)),
        4 => Some(MouseButton::Navigate(NavigationDirection::Forward)),
        _ => None,
    }
}

/// The highest-priority button held down, from the `buttons` bitmask.
fn pressed_button(buttons: u16) -> Option<MouseButton> {
    if buttons & 1 != 0 {
        Some(MouseButton::Left)
    } else if buttons & 2 != 0 {
        Some(MouseButton::Right)
    } else if buttons & 4 != 0 {
        Some(MouseButton::Middle)
    } else if buttons & 8 != 0 {
        Some(MouseButton::Navigate(NavigationDirection::Back))
    } else if buttons & 16 != 0 {
        Some(MouseButton::Navigate(NavigationDirection::Forward))
    } else {
        None
    }
}

fn mouse_event_position(event: &web_sys::MouseEvent) -> Point<Pixels> {
    point(px(event.offset_x() as f32), px(event.offset_y() as f32))
}

fn mouse_event_modifiers(event: &web_sys::MouseEvent) -> Modifiers {
    Modifiers {
        control: event.ctrl_key(),
        alt: event.alt_key(),
        shift: event.shift_key(),
        platform: event.meta_key(),
        function: false,
    }
}

fn keyboard_event_modifiers(event: &KeyboardEvent) -> Modifiers {
    Modifiers {
        control: event.ctrl_key(),
        alt: event.alt_key(),
        shift: event.shift_key(),
        platform: event.meta_key(),
        function: false,
    }
}

/// 1.0 for mice (the DOM reports 0.5 for any held mouse button), the real
/// pressure for pens.
fn pointer_pressure(event: &PointerEvent) -> f32 {
    if event.pointer_type() == "pen" {
        event.pressure()
    } else {
        1.0
    }
}

fn is_modifier_key(key: &str) -> bool {
    matches!(key, "Shift" | "Control" | "Alt" | "Meta" | "CapsLock")
}

/// Translates a DOM `KeyboardEvent.key` value into gpui's keystroke, matching
/// the naming the desktop backends produce ("enter", "left", lowercase
/// letters with shift as a modifier, ...).
fn keystroke_for(event: &KeyboardEvent) -> Keystroke {
    let dom_key = event.key();
    let key = match dom_key.as_str() {
        " " => "space".to_string(),
        "Enter" => "enter".to_string(),
        "Tab" => "tab".to_string(),
        "Escape" => "escape".to_string(),
        "Backspace" => "backspace".to_string(),
        "Delete" => "delete".to_string(),
        "ArrowLeft" => "left".to_string(),
        "ArrowRight" => "right".to_string(),
        "ArrowUp" => "up".to_string(),
        "ArrowDown" => "down".to_string(),
        "PageUp" => "pageup".to_string(),
        "PageDown" => "pagedown".to_string(),
        "Home" => "home".to_string(),
        "End" => "end".to_string(),
        "Insert" => "insert".to_string(),
        "ContextMenu" => "menu".to_string(),
        key if key.chars().count() == 1 => key.to_lowercase(),
        // Remaining named keys ("F1".."F35", "PrintScreen", ...) match gpui's
        // names when lowercased, or fall through harmlessly unbound.
        key => key.to_lowercase(),
    };
    Keystroke {
        modifiers: keyboard_event_modifiers(event),
        key,
        key_char: None,
    }
    // Derives key_char ("a" -> "a", shift-a -> "A", enter -> "\n"; nothing
    // for ctrl/cmd chords) the same way simulated keystrokes do.
    .with_simulated_ime()
}

/// Registers a DOM event listener on `target` and keeps the closure alive in
/// the window.
fn add_listener<E: JsCast + 'static>(
    inner: &Rc<WebWindowInner>,
    target: &web_sys::EventTarget,
    event_name: &str,
    passive: Option<bool>,
    handler: impl Fn(&WebWindowInner, E) + 'static,
) {
    let weak = Rc::downgrade(inner);
    let closure = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let Ok(event) = event.dyn_into::<E>() else {
            return;
        };
        handler(&inner, event);
    });
    let result = match passive {
        Some(passive) => {
            let options = web_sys::AddEventListenerOptions::new();
            options.set_passive(passive);
            target.add_event_listener_with_callback_and_add_event_listener_options(
                event_name,
                closure.as_ref().unchecked_ref(),
                &options,
            )
        }
        None => {
            target.add_event_listener_with_callback(event_name, closure.as_ref().unchecked_ref())
        }
    };
    if result.is_err() {
        log::error!("failed to add a DOM listener for {event_name:?}");
    }
    inner.listeners.borrow_mut().push(Box::new(closure));
}

fn setup_event_listeners(inner: &Rc<WebWindowInner>) {
    let canvas: &web_sys::EventTarget = inner.canvas.as_ref();

    add_listener::<PointerEvent>(inner, canvas, "pointerdown", None, |inner, event| {
        let Some(button) = mouse_button(event.button()) else {
            return;
        };
        let position = mouse_event_position(&event);
        let first_mouse = !inner.active.get();
        let click_count = inner.click_count(button, position);
        {
            let mut state = inner.state.borrow_mut();
            state.mouse_position = position;
            state.modifiers = mouse_event_modifiers(&event);
        }
        // Keep receiving pointermove/pointerup while dragging outside the
        // canvas.
        inner.canvas.set_pointer_capture(event.pointer_id()).ok();
        let result = inner.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
            button,
            position,
            modifiers: mouse_event_modifiers(&event),
            click_count,
            first_mouse,
            pressure: pointer_pressure(&event),
        }));
        if result.default_prevented {
            event.prevent_default();
        }
    });

    add_listener::<PointerEvent>(inner, canvas, "pointerup", None, |inner, event| {
        let Some(button) = mouse_button(event.button()) else {
            return;
        };
        let position = mouse_event_position(&event);
        let click_count = inner
            .state
            .borrow()
            .click
            .as_ref()
            .map_or(1, |click| click.count);
        let result = inner.dispatch_input(PlatformInput::MouseUp(MouseUpEvent {
            button,
            position,
            modifiers: mouse_event_modifiers(&event),
            click_count,
            pressure: pointer_pressure(&event),
        }));
        if result.default_prevented {
            event.prevent_default();
        }
    });

    add_listener::<PointerEvent>(inner, canvas, "pointermove", None, |inner, event| {
        let position = mouse_event_position(&event);
        inner.state.borrow_mut().mouse_position = position;
        inner.dispatch_input(PlatformInput::MouseMove(MouseMoveEvent {
            position,
            pressed_button: pressed_button(event.buttons()),
            modifiers: mouse_event_modifiers(&event),
            pressure: pointer_pressure(&event),
        }));
    });

    add_listener::<PointerEvent>(inner, canvas, "pointerenter", None, |inner, _event| {
        inner.hovered.set(true);
        let mut callback = inner.callbacks.hover_status_change.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(true);
        }
        replace_if_empty(&inner.callbacks.hover_status_change, callback);
    });

    add_listener::<PointerEvent>(inner, canvas, "pointerleave", None, |inner, event| {
        inner.hovered.set(false);
        let mut callback = inner.callbacks.hover_status_change.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(false);
        }
        replace_if_empty(&inner.callbacks.hover_status_change, callback);
        inner.dispatch_input(PlatformInput::MouseExited(MouseExitEvent {
            position: mouse_event_position(&event),
            pressed_button: pressed_button(event.buttons()),
            modifiers: mouse_event_modifiers(&event),
        }));
    });

    // Must be non-passive to be able to stop the page from scrolling.
    add_listener::<WheelEvent>(inner, canvas, "wheel", Some(false), |inner, event| {
        let position = mouse_event_position(&event);
        // The DOM's positive-delta direction is the opposite of gpui's.
        let (dx, dy) = (-event.delta_x() as f32, -event.delta_y() as f32);
        let delta = match event.delta_mode() {
            WheelEvent::DOM_DELTA_PIXEL => ScrollDelta::Pixels(point(px(dx), px(dy))),
            WheelEvent::DOM_DELTA_LINE => ScrollDelta::Lines(point(dx, dy)),
            // A page is approximated as one wheel notch's worth of lines.
            _ => ScrollDelta::Lines(point(dx * SCROLL_LINES, dy * SCROLL_LINES)),
        };
        inner.dispatch_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
            position,
            delta,
            modifiers: mouse_event_modifiers(&event),
            touch_phase: TouchPhase::Moved,
        }));
        event.prevent_default();
    });

    add_listener::<web_sys::MouseEvent>(inner, canvas, "contextmenu", None, |_inner, event| {
        // Right-click is delivered through pointerdown/up; the browser's own
        // context menu would shadow gpui's.
        event.prevent_default();
    });

    let Some(window) = web_sys::window() else {
        return;
    };
    let window_target: &web_sys::EventTarget = window.as_ref();

    add_listener::<KeyboardEvent>(inner, window_target, "keydown", None, |inner, event| {
        let modifiers = keyboard_event_modifiers(&event);
        let capslock = Capslock {
            on: event.get_modifier_state("CapsLock"),
        };
        if is_modifier_key(&event.key()) {
            {
                let mut state = inner.state.borrow_mut();
                state.modifiers = modifiers;
                state.capslock = capslock;
            }
            inner.dispatch_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock,
            }));
            return;
        }
        inner.state.borrow_mut().modifiers = modifiers;
        let result = inner.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: keystroke_for(&event),
            is_held: event.repeat(),
        }));
        if result.default_prevented {
            event.prevent_default();
        }
    });

    add_listener::<KeyboardEvent>(inner, window_target, "keyup", None, |inner, event| {
        let modifiers = keyboard_event_modifiers(&event);
        let capslock = Capslock {
            on: event.get_modifier_state("CapsLock"),
        };
        if is_modifier_key(&event.key()) {
            {
                let mut state = inner.state.borrow_mut();
                state.modifiers = modifiers;
                state.capslock = capslock;
            }
            inner.dispatch_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock,
            }));
            return;
        }
        let result = inner.dispatch_input(PlatformInput::KeyUp(KeyUpEvent {
            keystroke: keystroke_for(&event),
        }));
        if result.default_prevented {
            event.prevent_default();
        }
    });

    add_listener::<web_sys::Event>(inner, window_target, "focus", None, |inner, _event| {
        inner.active.set(true);
        let mut callback = inner.callbacks.active_status_change.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(true);
        }
        replace_if_empty(&inner.callbacks.active_status_change, callback);
    });

    add_listener::<web_sys::Event>(inner, window_target, "blur", None, |inner, _event| {
        inner.active.set(false);
        let mut callback = inner.callbacks.active_status_change.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(false);
        }
        replace_if_empty(&inner.callbacks.active_status_change, callback);
    });

    if let Ok(Some(query)) = window.match_media("(prefers-color-scheme: dark)") {
        let target: &web_sys::EventTarget = query.as_ref();
        add_listener::<web_sys::Event>(inner, target, "change", None, |inner, _event| {
            let mut callback = inner.callbacks.appearance_changed.borrow_mut().take();
            if let Some(callback) = callback.as_mut() {
                callback();
            }
            replace_if_empty(&inner.callbacks.appearance_changed, callback);
        });
    }
}

impl raw_window_handle::HasWindowHandle for WebWindow {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let raw =
            raw_window_handle::RawWindowHandle::Web(raw_window_handle::WebWindowHandle::new(1));
        Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

impl raw_window_handle::HasDisplayHandle for WebWindow {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        let raw =
            raw_window_handle::RawDisplayHandle::Web(raw_window_handle::WebDisplayHandle::new());
        Ok(unsafe { raw_window_handle::DisplayHandle::borrow_raw(raw) })
    }
}

impl PlatformWindow for WebWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: Point::default(),
            size: css_size(&self.0.canvas),
        }
    }

    fn is_maximized(&self) -> bool {
        false
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        css_size(&self.0.canvas)
    }

    fn resize(&mut self, _size: Size<Pixels>) {
        // The canvas is laid out by the page; programmatic window resizing
        // does not apply on the web.
    }

    fn scale_factor(&self) -> f32 {
        scale_factor()
    }

    fn appearance(&self) -> WindowAppearance {
        if prefers_dark_appearance() {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        }
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(WebDisplay))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.0.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.0.state.borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        // Returning None makes gpui render its own prompt.
        None
    }

    fn activate(&self) {
        self.0.canvas.focus().ok();
    }

    fn is_active(&self) -> bool {
        self.0.active.get()
    }

    fn is_hovered(&self) -> bool {
        self.0.hovered.get()
    }

    fn set_title(&mut self, title: &str) {
        if let Some(document) = web_sys::window().and_then(|window| window.document()) {
            document.set_title(title);
        }
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {}

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        false
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.0.callbacks.request_frame.borrow_mut() = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        *self.0.callbacks.input.borrow_mut() = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.0.callbacks.active_status_change.borrow_mut() = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.0.callbacks.hover_status_change.borrow_mut() = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        *self.0.callbacks.resize.borrow_mut() = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        *self.0.callbacks.moved.borrow_mut() = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.0.callbacks.should_close.borrow_mut() = Some(callback);
    }

    fn on_hit_test_window_control(
        &self,
        _callback: Box<dyn FnMut() -> Option<crate::WindowControlArea>>,
    ) {
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        *self.0.callbacks.close.borrow_mut() = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        *self.0.callbacks.appearance_changed.borrow_mut() = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        self.0.state.borrow_mut().renderer.draw(scene);
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.state.borrow().renderer.sprite_atlas()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        Some(self.0.state.borrow().renderer.gpu_specs())
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}
}
