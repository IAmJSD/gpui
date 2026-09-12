//! Android platform backend.
//!
//! The application is a `NativeActivity`, reached through the
//! `android-activity` glue: `Platform::run` owns the `android_main` thread's
//! event loop, polling the activity's looper for lifecycle commands, input
//! and wake-ups from the executor. Rendering goes through the blade
//! (Vulkan) renderer onto the activity's `ANativeWindow`; text through the
//! cosmic-text stack shared with Linux and the web. Touch is synthesised
//! into gpui's mouse model as on iOS (see `window.rs`), and the platform
//! services that only exist in Java (the clipboard, window insets, intents,
//! the keystore) go through JNI. See `docs/android.md`.

mod dispatcher;
mod display;
mod events;
mod jni;
mod platform;
mod window;

pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use platform::*;
pub(crate) use window::*;

/// Screen capture is not available on Android.
pub(crate) type PlatformScreenCaptureFrame = ();

/// The `android_main` entry point support, re-exported as `gpui::android`.
pub mod entry {
    use parking_lot::RwLock;
    use std::sync::OnceLock;

    pub use android_activity::{self, AndroidApp};

    static APP: RwLock<Option<AndroidApp>> = RwLock::new(None);

    /// The activity handle the app was started with.
    ///
    /// Available from `main` (see [`main`]) on; `None` when the process is
    /// not an Android activity, or before it has started.
    pub fn app() -> Option<AndroidApp> {
        APP.read().clone()
    }

    /// Runs `main` as the body of `android_main`, which is where an Android
    /// app starts. `gpui::android_main!` expands to a call to this; it stores
    /// the activity handle for the platform, sends the process's stdout and
    /// stderr to logcat (so `println!`, `eprintln!` and panic messages can be
    /// read with `adb logcat`), then calls `main`, which is expected to call
    /// `Application::run`.
    pub fn main(app: AndroidApp, main: impl FnOnce()) {
        static LOGCAT: OnceLock<()> = OnceLock::new();
        LOGCAT.get_or_init(super::logcat::forward_stdio);
        *APP.write() = Some(app);
        main();
        *APP.write() = None;
    }
}

/// Forwards the process's stdout and stderr to logcat. Android connects
/// them to `/dev/null`; without this a panic message goes nowhere.
mod logcat {
    use std::{
        ffi::{CStr, CString},
        io::{BufRead, BufReader},
        os::fd::FromRawFd,
    };

    /// `ANDROID_LOG_INFO` and `ANDROID_LOG_ERROR`.
    const ANDROID_LOG_INFO: i32 = 4;
    const ANDROID_LOG_ERROR: i32 = 6;

    unsafe extern "C" {
        fn __android_log_write(
            priority: i32,
            tag: *const std::ffi::c_char,
            text: *const std::ffi::c_char,
        ) -> i32;
    }

    pub(super) fn forward_stdio() {
        forward(libc::STDOUT_FILENO, ANDROID_LOG_INFO, c"gpui-stdout");
        forward(libc::STDERR_FILENO, ANDROID_LOG_ERROR, c"gpui-stderr");
    }

    fn forward(fd: i32, priority: i32, tag: &'static CStr) {
        let mut pipe = [0i32; 2];
        unsafe {
            if libc::pipe(pipe.as_mut_ptr()) != 0 {
                return;
            }
            libc::dup2(pipe[1], fd);
            libc::close(pipe[1]);
        }
        let read_end = pipe[0];
        std::thread::Builder::new()
            .name(format!("logcat-{fd}"))
            .spawn(move || {
                let file = unsafe { std::fs::File::from_raw_fd(read_end) };
                for line in BufReader::new(file).lines().map_while(Result::ok) {
                    if let Ok(text) = CString::new(line) {
                        unsafe {
                            __android_log_write(priority, tag.as_ptr(), text.as_ptr());
                        }
                    }
                }
            })
            .ok();
    }
}
