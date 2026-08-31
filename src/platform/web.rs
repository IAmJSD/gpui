//! Web (wasm32) platform backend.
//!
//! Runs gpui inside a browser page. The JS event loop is the platform event
//! loop: tasks are dispatched as microtasks and timeouts, each window is an
//! `HtmlCanvasElement` driven by `requestAnimationFrame`, and scenes render
//! through WebGPU. `Platform::run` does not block -- it performs the async
//! GPU setup and then invokes the launch callback, after which the
//! application lives in the callbacks it registered.
//!
//! Not yet implemented: DOM input events, text (the `NoopTextSystem` is
//! used), clipboard, and dark-mode appearance tracking.

mod atlas;
mod dispatcher;
mod display;
mod platform;
mod renderer;
mod window;

pub(crate) use atlas::*;
pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use platform::*;
pub(crate) use renderer::*;
pub(crate) use window::*;

pub(crate) type PlatformScreenCaptureFrame = ();
