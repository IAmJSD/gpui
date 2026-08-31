use super::{WebDisplay, WebGpuContext, WebGpuRenderer};
use crate::{
    AnyWindowHandle, Bounds, Capslock, ClipboardItem, DevicePixels, DispatchEventResult, GpuSpecs,
    KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection, PinchEvent,
    Pixels, PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow,
    Point, PromptButton, PromptLevel, RequestFrameOptions, Scene, ScrollDelta, ScrollWheelEvent,
    Size, TouchPhase, WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowParams,
    point, px, size,
};
use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use wasm_bindgen::{JsCast, prelude::Closure};
use web_sys::{
    CompositionEvent, HtmlCanvasElement, HtmlInputElement, KeyboardEvent, PointerEvent, WheelEvent,
};
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

/// A ctrl+wheel pinch gesture ends this long after its last wheel event
/// (the browser gives no explicit end for synthesized pinch-wheels).
const PINCH_END_DELAY_MS: i32 = 150;

/// How long to wait for the browser's `paste` event after a paste keystroke
/// before giving up and dispatching the keystroke without fresh clipboard
/// contents.
const PASTE_EVENT_DELAY_MS: i32 = 100;

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

#[derive(Default)]
struct PinchState {
    /// A synthesized ctrl+wheel pinch is in progress.
    wheel_active: bool,
    /// Invalidates the pending end-of-gesture timeout when another wheel
    /// event arrives (timeouts cannot be cancelled without keeping their
    /// closures; letting stale ones fire and no-op is simpler).
    wheel_generation: u64,
    /// A Safari GestureEvent sequence is in progress (takes precedence over
    /// the ctrl+wheel path).
    gesture_active: bool,
    /// GestureEvent reports cumulative scale; gpui wants per-event deltas.
    gesture_previous_scale: f32,
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
    /// Identifies this window's canvas in its `data-raw-handle` attribute
    /// and raw window handle.
    handle_id: u32,
    /// An invisible focused `<input>`: composition (IME, dead keys) only
    /// happens on editable elements, and key events bubble from it to the
    /// window-level listeners. `update_ime_position` moves it so the IME
    /// popup appears near the caret.
    ime_input: HtmlInputElement,
    /// The platform's clipboard mirror; the `paste` listener refreshes it
    /// with real (external) clipboard contents before gpui acts on a paste
    /// keystroke.
    clipboard: Rc<RefCell<Option<ClipboardItem>>>,
    state: RefCell<WebWindowState>,
    pinch: RefCell<PinchState>,
    /// A paste keystroke being held back until the `paste` event delivers
    /// the clipboard contents (or the fallback timeout fires).
    pending_paste_keystroke: RefCell<Option<KeyDownEvent>>,
    /// True between `compositionstart` and `compositionend`. The hidden
    /// input's value is the IME's scratch space while it is set, and gpui's
    /// to clear once it is not.
    composing: Cell<bool>,
    callbacks: Callbacks,
    active: Cell<bool>,
    hovered: Cell<bool>,
    /// The DOM event listener closures; dropping them detaches nothing, but
    /// they must stay alive for as long as the listeners can fire.
    listeners: RefCell<Vec<Box<dyn Any>>>,
    /// Set right after construction; lets methods hand a `Weak` of this
    /// window to timeout closures without threading the `Rc` around (which,
    /// captured in a stored listener, would leak the window through a cycle).
    weak_self: RefCell<Weak<WebWindowInner>>,
}

pub(crate) struct WebWindow(Rc<WebWindowInner>);

impl WebWindow {
    pub(crate) fn new(
        gpu: &Arc<WebGpuContext>,
        clipboard: Rc<RefCell<Option<ClipboardItem>>>,
        _handle: AnyWindowHandle,
        _params: WindowParams,
    ) -> Result<Self> {
        let document = web_sys::window()
            .context("no global `window`")?
            .document()
            .context("no `document`")?;

        // A `data-raw-handle` attribute means the canvas is already claimed
        // by an earlier window; each window needs its own canvas.
        let canvas = match document
            .get_element_by_id(CANVAS_ELEMENT_ID)
            .and_then(|element| element.dyn_into::<HtmlCanvasElement>().ok())
            .filter(|canvas| !canvas.has_attribute("data-raw-handle"))
        {
            Some(canvas) => canvas,
            None => {
                let canvas = document
                    .create_element("canvas")
                    .ok()
                    .and_then(|element| element.dyn_into::<HtmlCanvasElement>().ok())
                    .context("failed to create a canvas element")?;
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
        let handle_id = next_handle_id();
        canvas
            .set_attribute("data-raw-handle", &handle_id.to_string())
            .ok();

        let ime_input = document
            .create_element("input")
            .ok()
            .and_then(|element| element.dyn_into::<HtmlInputElement>().ok())
            .context("failed to create the IME input element")?;
        {
            let style = ime_input.style();
            style.set_property("position", "fixed").ok();
            style.set_property("left", "0").ok();
            style.set_property("top", "0").ok();
            style.set_property("width", "1px").ok();
            style.set_property("height", "1px").ok();
            style.set_property("opacity", "0").ok();
            style.set_property("border", "none").ok();
            style.set_property("padding", "0").ok();
            style.set_property("outline", "none").ok();
        }
        ime_input.set_attribute("autocomplete", "off").ok();
        ime_input.set_attribute("autocapitalize", "off").ok();
        ime_input.set_attribute("spellcheck", "false").ok();
        document
            .body()
            .context("document has no body")?
            .append_child(&ime_input)
            .ok()
            .context("failed to append the IME input to the document body")?;

        let device_size = device_size_for(&canvas);
        canvas.set_width(device_size.width.0 as u32);
        canvas.set_height(device_size.height.0 as u32);

        let renderer = WebGpuRenderer::new(gpu, &canvas, device_size)?;

        let window = Self(Rc::new(WebWindowInner {
            canvas,
            handle_id,
            ime_input,
            clipboard,
            state: RefCell::new(WebWindowState {
                renderer,
                input_handler: None,
                mouse_position: Point::default(),
                modifiers: Modifiers::default(),
                capslock: Capslock::default(),
                click: None,
            }),
            pinch: RefCell::new(PinchState::default()),
            pending_paste_keystroke: RefCell::new(None),
            composing: Cell::new(false),
            callbacks: Callbacks::default(),
            // Set by the `focus` listener once the hidden input below is
            // focused; with several windows on a page only one of them is
            // active at a time, so the page's own focus is not the answer.
            active: Cell::new(false),
            hovered: Cell::new(false),
            listeners: RefCell::new(Vec::new()),
            weak_self: RefCell::new(Weak::new()),
        }));
        *window.0.weak_self.borrow_mut() = Rc::downgrade(&window.0);
        start_frame_loop(Rc::downgrade(&window.0));
        setup_event_listeners(&window.0);
        // Keyboard events are listened for on the hidden input, so the newest
        // window takes the keyboard the moment it opens -- without this the
        // page would need a click before it could be typed into.
        window.0.ime_input.focus().ok();
        Ok(window)
    }
}

fn next_handle_id() -> u32 {
    std::thread_local! {
        static NEXT_HANDLE_ID: Cell<u32> = const { Cell::new(1) };
    }
    NEXT_HANDLE_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    })
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

    /// Runs `f` with the input handler taken out of the window state, so the
    /// reentrant window update inside the handler cannot hit a borrowed
    /// RefCell.
    fn with_input_handler(&self, f: impl FnOnce(&mut PlatformInputHandler)) {
        let handler = self.state.borrow_mut().input_handler.take();
        if let Some(mut handler) = handler {
            f(&mut handler);
            let mut state = self.state.borrow_mut();
            if state.input_handler.is_none() {
                state.input_handler = Some(handler);
            }
        }
    }

    /// Moves the hidden input to `origin` (window coordinates, which are the
    /// viewport's because the canvas fills it).
    fn place_ime_input(&self, origin: Point<Pixels>) {
        let style = self.ime_input.style();
        style
            .set_property("left", &format!("{}px", origin.x.0))
            .ok();
        style.set_property("top", &format!("{}px", origin.y.0)).ok();
    }

    /// Parks the hidden input at the caret so the browser draws the IME
    /// candidate window there. `update_ime_position` only fires when an
    /// application asks gpui to invalidate the character coordinates, but the
    /// browser needs the element in the right place for every composition, so
    /// the backend asks the input handler itself.
    fn place_ime_input_at_caret(&self) {
        let mut origin = None;
        self.with_input_handler(|handler| {
            if let Some(selection) = handler.selected_text_range(true) {
                let caret = if selection.reversed {
                    selection.range.start
                } else {
                    selection.range.end
                };
                origin = handler
                    .bounds_for_range(caret..caret)
                    .map(|bounds| bounds.origin);
            }
        });
        if let Some(origin) = origin {
            self.place_ime_input(origin);
        }
    }

    fn dispatch_pinch(&self, position: Point<Pixels>, modifiers: Modifiers, phase: TouchPhase) {
        self.dispatch_pinch_delta(position, modifiers, phase, 1.0);
    }

    fn dispatch_pinch_delta(
        &self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        phase: TouchPhase,
        delta: f32,
    ) {
        self.dispatch_input(PlatformInput::Pinch(PinchEvent {
            position,
            delta,
            // Accumulated by `Window` from the deltas.
            scale: 1.0,
            modifiers,
            phase,
        }));
    }

    /// A synthesized ctrl+wheel pinch has no end event; each wheel event
    /// (re)schedules this timeout, and only the newest generation acts.
    fn schedule_wheel_pinch_end(&self, position: Point<Pixels>, modifiers: Modifiers) {
        let generation = self.pinch.borrow().wheel_generation;
        let weak = self.weak_self.borrow().clone();
        let closure = Closure::once_into_js(move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            {
                let mut pinch = inner.pinch.borrow_mut();
                if !pinch.wheel_active || pinch.wheel_generation != generation {
                    return;
                }
                pinch.wheel_active = false;
            }
            inner.dispatch_pinch(position, modifiers, TouchPhase::Ended);
        });
        if let Some(window) = web_sys::window() {
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    closure.unchecked_ref(),
                    PINCH_END_DELAY_MS,
                )
                .ok();
        }
    }

    /// A paste keystroke waits for the `paste` event; if none arrives (no
    /// clipboard permission, remapped binding, ...) the keystroke must still
    /// reach the application.
    fn schedule_paste_fallback(&self) {
        let weak = self.weak_self.borrow().clone();
        let closure = Closure::once_into_js(move || {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let pending = inner.pending_paste_keystroke.borrow_mut().take();
            if let Some(event) = pending {
                inner.dispatch_input(PlatformInput::KeyDown(event));
            }
        });
        if let Some(window) = web_sys::window() {
            window
                .set_timeout_with_callback_and_timeout_and_arguments_0(
                    closure.unchecked_ref(),
                    PASTE_EVENT_DELAY_MS,
                )
                .ok();
        }
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

/// Safari's GestureEvent is nonstandard, so its properties come through
/// reflection.
fn gesture_event_scale(event: &web_sys::Event) -> f32 {
    js_sys::Reflect::get(event.as_ref(), &"scale".into())
        .ok()
        .and_then(|value| value.as_f64())
        .unwrap_or(1.0) as f32
}

fn gesture_event_position(inner: &WebWindowInner, event: &web_sys::Event) -> Point<Pixels> {
    let coordinate = |name: &str| {
        js_sys::Reflect::get(event.as_ref(), &name.into())
            .ok()
            .and_then(|value| value.as_f64())
    };
    match (coordinate("clientX"), coordinate("clientY")) {
        (Some(x), Some(y)) => {
            let rect = inner.canvas.get_bounding_client_rect();
            point(px((x - rect.left()) as f32), px((y - rect.top()) as f32))
        }
        _ => inner.state.borrow().mouse_position,
    }
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
    let modifiers = keyboard_event_modifiers(event);

    // Option/AltGr characters (macOS option-a -> "å", AltGr+2 -> "€", which
    // browsers report as ctrl+alt): the DOM `key` is the transformed
    // character, and `code` names the physical key. When those disagree, the
    // keystroke follows the macOS convention -- `key` is the unmodified key so
    // bindings like alt-a still match, and `key_char` carries the character
    // the layout produced. `with_simulated_ime` never fills key_char for
    // alt/ctrl chords, so a plain chord (its `key` and `code` agree) stays a
    // chord.
    if modifiers.alt && !modifiers.platform && dom_key.chars().count() == 1 {
        if let Some(unmodified) = unmodified_key_from_code(&event.code()) {
            if unmodified != key {
                return Keystroke {
                    modifiers,
                    key: unmodified,
                    key_char: Some(dom_key),
                };
            }
        }
    }

    Keystroke {
        modifiers,
        key,
        key_char: None,
    }
    // Derives key_char ("a" -> "a", shift-a -> "A", enter -> "\n"; nothing
    // for ctrl/cmd chords) the same way simulated keystrokes do.
    .with_simulated_ime()
}

/// The key an unmodified press of this physical key would produce, for the
/// codes where that is knowable ("KeyA".."KeyZ", "Digit0".."Digit9").
fn unmodified_key_from_code(code: &str) -> Option<String> {
    if let Some(letter) = code.strip_prefix("Key") {
        if letter.len() == 1 && letter.chars().all(|c| c.is_ascii_uppercase()) {
            return Some(letter.to_ascii_lowercase());
        }
    }
    if let Some(digit) = code.strip_prefix("Digit") {
        if digit.len() == 1 && digit.chars().all(|c| c.is_ascii_digit()) {
            return Some(digit.to_string());
        }
    }
    None
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
        // Keyboard input, and IME composition in particular, targets the
        // hidden input, which is where this window's key listeners live.
        // Suppressing the pointer event's default action is what makes the
        // focus stick: the browser would otherwise move it to the body, which
        // owns no listeners, and swallow every subsequent keystroke.
        event.prevent_default();
        inner.ime_input.focus().ok();
        inner.dispatch_input(PlatformInput::MouseDown(MouseDownEvent {
            button,
            position,
            modifiers: mouse_event_modifiers(&event),
            click_count,
            first_mouse,
            pressure: pointer_pressure(&event),
        }));
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
    {
        add_listener::<WheelEvent>(inner, canvas, "wheel", Some(false), move |inner, event| {
            let position = mouse_event_position(&event);
            let modifiers = mouse_event_modifiers(&event);

            // Browsers synthesize ctrl+wheel for trackpad pinches (and a real
            // ctrl+wheel conventionally means zoom as well). Safari reports
            // pinches through GestureEvents instead, which take precedence.
            if event.ctrl_key() && !inner.pinch.borrow().gesture_active {
                let started = {
                    let mut pinch = inner.pinch.borrow_mut();
                    pinch.wheel_generation += 1;
                    !std::mem::replace(&mut pinch.wheel_active, true)
                };
                if started {
                    inner.dispatch_pinch(position, modifiers, TouchPhase::Started);
                }
                let dy_pixels = match event.delta_mode() {
                    WheelEvent::DOM_DELTA_PIXEL => event.delta_y() as f32,
                    // Pinch-wheels are pixel-mode in practice; a line is
                    // roughly a text line's worth.
                    _ => event.delta_y() as f32 * 16.0,
                };
                // The conventional web mapping: scale multiplier per event,
                // exponential in the wheel delta, >1 when pinching out.
                let delta = (-dy_pixels / 100.0).exp();
                inner.dispatch_pinch_delta(position, modifiers, TouchPhase::Moved, delta);
                inner.schedule_wheel_pinch_end(position, modifiers);
                // Always consumed: the browser would otherwise zoom the page.
                event.prevent_default();
                return;
            }

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
                modifiers,
                touch_phase: TouchPhase::Moved,
            }));
            event.prevent_default();
        });
    }

    // Safari's nonstandard pinch events; web-sys has no bindings, so `scale`
    // and the position come through js_sys::Reflect. Chrome and Firefox never
    // fire these.
    add_listener::<web_sys::Event>(inner, canvas, "gesturestart", None, |inner, event| {
        {
            let mut pinch = inner.pinch.borrow_mut();
            pinch.gesture_active = true;
            pinch.gesture_previous_scale = 1.0;
        }
        let position = gesture_event_position(inner, &event);
        let modifiers = inner.state.borrow().modifiers;
        inner.dispatch_pinch(position, modifiers, TouchPhase::Started);
        // Stop Safari from zooming the page.
        event.prevent_default();
    });

    add_listener::<web_sys::Event>(inner, canvas, "gesturechange", None, |inner, event| {
        let scale = gesture_event_scale(&event);
        let delta = {
            let mut pinch = inner.pinch.borrow_mut();
            if !pinch.gesture_active || pinch.gesture_previous_scale <= 0.0 {
                return;
            }
            let delta = scale / pinch.gesture_previous_scale;
            pinch.gesture_previous_scale = scale;
            delta
        };
        let position = gesture_event_position(inner, &event);
        let modifiers = inner.state.borrow().modifiers;
        inner.dispatch_pinch_delta(position, modifiers, TouchPhase::Moved, delta);
        event.prevent_default();
    });

    add_listener::<web_sys::Event>(inner, canvas, "gestureend", None, |inner, event| {
        inner.pinch.borrow_mut().gesture_active = false;
        let position = gesture_event_position(inner, &event);
        let modifiers = inner.state.borrow().modifiers;
        inner.dispatch_pinch(position, modifiers, TouchPhase::Ended);
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

    // Every keyboard-related listener goes on this window's own hidden input
    // rather than on the page: focus lives there, key events start there, and
    // a page with several gpui windows must not deliver one keystroke to all
    // of them.
    let ime_target: &web_sys::EventTarget = inner.ime_input.as_ref();

    add_listener::<KeyboardEvent>(inner, ime_target, "keydown", None, |inner, event| {
        // During IME composition the composition events carry the text; the
        // interleaved synthetic key events must not dispatch. `isComposing`
        // is false on the keydown that *starts* a composition, which is what
        // the keyCode 229 sentinel is for. A dead key ("Dead") starts a
        // composition of its own.
        if event.is_composing() || event.key_code() == 229 || event.key() == "Dead" {
            return;
        }
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
        let keystroke = keystroke_for(&event);

        // A paste keystroke is held back so the browser's `paste` event --
        // the one place external clipboard contents are synchronously
        // readable -- can refresh the clipboard mirror first. Not
        // preventDefault'ed: that would suppress the paste event itself.
        if (modifiers.platform || modifiers.control)
            && !modifiers.alt
            && keystroke.key == "v"
            && !event.repeat()
        {
            *inner.pending_paste_keystroke.borrow_mut() = Some(KeyDownEvent {
                keystroke,
                is_held: false,
            });
            inner.schedule_paste_fallback();
            return;
        }

        let result = inner.dispatch_input(PlatformInput::KeyDown(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: event.repeat(),
        }));
        if result.default_prevented {
            event.prevent_default();
        }
        // Tab is a gpui navigation key on every platform; the browser's own
        // response to it would move the DOM focus off the hidden input and
        // take the keyboard away from the window.
        if keystroke.key == "tab" {
            event.prevent_default();
        }
        // gpui did not bind the key, so it is text: hand the character to the
        // window's input handler, the way every other backend does. Anything
        // beyond shift is a chord, not typing -- except an Option/AltGr
        // character, whose key_char only exists because the layout transformed
        // the key (see keystroke_for).
        let is_layout_transformed = keystroke.modifiers.alt && !keystroke.modifiers.platform;
        if result.propagate
            && (keystroke.modifiers.is_subset_of(&Modifiers::shift()) || is_layout_transformed)
            && let Some(key_char) = keystroke.key_char.as_ref()
        {
            inner.with_input_handler(|handler| {
                handler.replace_text_in_range(None, key_char);
            });
        }
    });

    add_listener::<KeyboardEvent>(inner, ime_target, "keyup", None, |inner, event| {
        if event.is_composing() || event.key_code() == 229 || event.key() == "Dead" {
            return;
        }
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

    add_listener::<web_sys::ClipboardEvent>(inner, ime_target, "paste", None, |inner, event| {
        // Within this event the external clipboard is synchronously
        // readable; refresh the mirror before letting gpui act.
        if let Some(data) = event.clipboard_data() {
            if let Ok(text) = data.get_data("text/plain") {
                if !text.is_empty() {
                    *inner.clipboard.borrow_mut() = Some(ClipboardItem::new_string(text));
                }
            }
        }
        let pending = inner.pending_paste_keystroke.borrow_mut().take();
        if let Some(key_down) = pending {
            inner.dispatch_input(PlatformInput::KeyDown(key_down));
            event.prevent_default();
        }
    });

    add_listener::<CompositionEvent>(inner, ime_target, "compositionstart", None, |inner, _| {
        inner.composing.set(true);
        inner.place_ime_input_at_caret();
    });

    add_listener::<CompositionEvent>(inner, ime_target, "compositionupdate", None, |inner, event| {
        let text = event.data().unwrap_or_default();
        inner.with_input_handler(|handler| {
            handler.replace_and_mark_text_in_range(None, &text, None);
        });
        inner.place_ime_input_at_caret();
    });

    add_listener::<CompositionEvent>(inner, ime_target, "compositionend", None, |inner, event| {
        inner.composing.set(false);
        let text = event.data().unwrap_or_default();
        inner.with_input_handler(|handler| {
            if text.is_empty() {
                handler.unmark_text();
            } else {
                handler.replace_text_in_range(None, &text);
            }
        });
        // The composed text also landed in the hidden input; it must not
        // accumulate.
        inner.ime_input.set_value("");
    });

    // Outside composition the hidden input is not a text field, just the
    // event target gpui borrows to see key and IME events: anything the
    // browser types into it is a duplicate of what gpui already holds, and
    // letting it pile up would grow without bound and give the IME a bogus
    // idea of the surrounding text.
    add_listener::<web_sys::Event>(inner, ime_target, "input", None, |inner, _event| {
        if !inner.composing.get() {
            inner.ime_input.set_value("");
        }
    });

    add_listener::<web_sys::Event>(inner, ime_target, "focus", None, |inner, _event| {
        inner.active.set(true);
        let mut callback = inner.callbacks.active_status_change.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(true);
        }
        replace_if_empty(&inner.callbacks.active_status_change, callback);
    });

    add_listener::<web_sys::Event>(inner, ime_target, "blur", None, |inner, _event| {
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
        let raw = raw_window_handle::RawWindowHandle::Web(raw_window_handle::WebWindowHandle::new(
            self.0.handle_id,
        ));
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
        self.0.ime_input.focus().ok();
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

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        // The IME popup anchors to the hidden input, so park it at the caret.
        self.0.place_ime_input(bounds.origin);
    }
}
