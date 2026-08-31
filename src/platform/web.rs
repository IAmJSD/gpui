//! Web (wasm32) platform backend.
//!
//! This is currently a compile-only stub: it satisfies the `Platform` trait so
//! that gpui builds for `wasm32-unknown-unknown`, but it cannot yet open
//! windows or dispatch tasks. Browser integration (canvas windows, DOM input,
//! a microtask-based dispatcher, and a WebGPU renderer) lands in later stages.

mod dispatcher;
mod platform;

pub(crate) use dispatcher::*;
pub(crate) use platform::*;

pub(crate) type PlatformScreenCaptureFrame = ();
