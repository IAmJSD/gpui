use super::{WebDisplay, WebGpuContext, WebGpuRenderer};
use crate::{
    AnyWindowHandle, Bounds, Capslock, DevicePixels, DispatchEventResult, GpuSpecs, Modifiers,
    Pixels, PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow,
    Point, PromptButton, PromptLevel, RequestFrameOptions, Scene, Size, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowParams, px, size,
};
use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use raw_window_handle as rwh;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use wasm_bindgen::{JsCast as _, prelude::Closure};
use web_sys::HtmlCanvasElement;

/// If an element with this id exists and is a `<canvas>`, the window renders
/// into it; otherwise a full-viewport canvas is created and appended to the
/// document body.
const CANVAS_ELEMENT_ID: &str = "gpui";

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

struct WebWindowState {
    renderer: WebGpuRenderer,
    input_handler: Option<PlatformInputHandler>,
}

pub(crate) struct WebWindowInner {
    canvas: HtmlCanvasElement,
    state: RefCell<WebWindowState>,
    callbacks: Callbacks,
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
            }),
            callbacks: Callbacks::default(),
        }));
        start_frame_loop(Rc::downgrade(&window.0));
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
}

/// Put a taken callback back unless a reentrant call installed a new one
/// while it was out.
fn replace_if_empty<T>(slot: &RefCell<Option<T>>, value: Option<T>) {
    let mut slot = slot.borrow_mut();
    if slot.is_none() {
        *slot = value;
    }
}

impl rwh::HasWindowHandle for WebWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let raw = rwh::RawWindowHandle::Web(rwh::WebWindowHandle::new(1));
        Ok(unsafe { rwh::WindowHandle::borrow_raw(raw) })
    }
}

impl rwh::HasDisplayHandle for WebWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let raw = rwh::RawDisplayHandle::Web(rwh::WebDisplayHandle::new());
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(raw) })
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
        WindowAppearance::Light
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(WebDisplay))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        Point::default()
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
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
        web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.has_focus().ok())
            .unwrap_or(false)
    }

    fn is_hovered(&self) -> bool {
        false
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
