use crate::{PlatformDispatcher, TaskLabel};
use async_task::Runnable;
use std::time::Duration;

/// Stub dispatcher for the web backend.
///
/// The browser has a single thread and no blocking primitives, so every queue
/// must ultimately be driven by the JS event loop (microtasks and
/// `setTimeout`). That wiring requires wasm-bindgen glue and arrives with the
/// runtime stage of the web backend; until then, scheduling panics.
pub(crate) struct WebDispatcher;

impl PlatformDispatcher for WebDispatcher {
    fn is_main_thread(&self) -> bool {
        true
    }

    fn dispatch(&self, _runnable: Runnable, _label: Option<TaskLabel>) {
        unimplemented!("task dispatch is not implemented yet on the web backend");
    }

    fn dispatch_on_main_thread(&self, _runnable: Runnable) {
        unimplemented!("task dispatch is not implemented yet on the web backend");
    }

    fn dispatch_after(&self, _duration: Duration, _runnable: Runnable) {
        unimplemented!("task dispatch is not implemented yet on the web backend");
    }
}
