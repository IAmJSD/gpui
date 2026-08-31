use crate::{PlatformDispatcher, TaskLabel};
use async_task::Runnable;
use std::time::Duration;
use wasm_bindgen::{JsCast as _, prelude::Closure};

/// Dispatcher for the web backend.
///
/// The browser has exactly one thread, so "background" and "main thread" work
/// both run on the JS event loop: runnables are scheduled as microtasks
/// (matching the promptness gpui expects from its foreground queue) and
/// delayed work goes through `setTimeout`. `BackgroundExecutor::block` cannot
/// work in this model -- there is no other thread to make progress while the
/// caller parks -- and will panic in the parking layer if reached.
pub(crate) struct WebDispatcher;

fn schedule(runnable: Runnable) {
    let closure = Closure::once_into_js(move || runnable.run());
    web_sys::window()
        .expect("no global `window`; the web backend must run on the browser main thread")
        .queue_microtask(closure.unchecked_ref());
}

impl PlatformDispatcher for WebDispatcher {
    fn is_main_thread(&self) -> bool {
        true
    }

    fn dispatch(&self, runnable: Runnable, _label: Option<TaskLabel>) {
        schedule(runnable);
    }

    fn dispatch_on_main_thread(&self, runnable: Runnable) {
        schedule(runnable);
    }

    fn dispatch_after(&self, duration: Duration, runnable: Runnable) {
        let closure = Closure::once_into_js(move || runnable.run());
        web_sys::window()
            .expect("no global `window`; the web backend must run on the browser main thread")
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                closure.unchecked_ref(),
                duration.as_millis().try_into().unwrap_or(i32::MAX),
            )
            .expect("setTimeout failed");
    }
}
