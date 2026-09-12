# gpui on Android

gpui builds for `aarch64-linux-android` (devices and the emulator on Apple
Silicon) and `x86_64-linux-android` (the emulator on Intel hosts). The app
is Android's own `NativeActivity`: it loads the app as a shared library and
calls `android_main`, which `gpui::android_main!(main)` defines for you
next to an ordinary `main`. No Java is compiled; the activity's lifecycle
and input reach Rust through the `android-activity` glue, and the few
services that only Java has are called through JNI. The renderer is the
blade Vulkan renderer shared with Linux, and text is the cosmic-text stack
shared with Linux and the web, with the system's Roboto and Noto fonts.
The Android-specific code lives in `src/platform/android`.

## Status

Verified in the Android 15 emulator (API 35, Pixel 7 profile, SwiftShader
Vulkan through gfxstream, Android Emulator 37.1) with `examples/mobile.rs`
and `examples/input.rs`: rendering and text, safe-area insets, a drag
scrolling a list and a fast swipe flinging it, a long press reaching the
right-click handler, a two-finger spread and pinch scaling the square by
exactly the finger-distance ratio, a hardware `ctrl-r` chord and plain and
shifted letters arriving as the expected keystrokes, the software keyboard
appearing when the text field is focused and going away when focus leaves,
typing and Backspace landing in the field, select-all/copy/paste going
through the system clipboard, rotation to landscape and back relaying out
under the new insets, a dark-mode switch, the activity being covered by
another app and coming back with its surface recreated, Back finishing
the activity when nothing binds `escape`, and a relaunch entering
`android_main` again in the same process. No physical device has run it
yet: a real Vulkan driver, a real input method (Gboard's key-event
fallback), a stylus and a mouse are implemented against the documented
behaviour but unverified, as are `open_url` and the keystore-backed
credentials.

Working:

- The full gpui programming model. The window is the activity: it fills
  the screen, `is_maximized` is true and `resize`/`minimize`/`zoom` do
  nothing. An activity shows one window; a second `open_window` succeeds
  but that window is never shown (its frames are never requested), with a
  warning in the log.
- Rendering through the shared blade renderer on Vulkan 1.1+ (the same
  device requirements as the Linux backend: timeline semaphores, dynamic
  rendering, descriptor indexing). The surface comes and goes with the
  activity: when it leaves the foreground the renderer is dropped and
  frames stop; the sprite atlas (glyphs, images) is kept, so coming back
  is cheap. `gpu_specs` reports the Vulkan device.
- Text through cosmic-text, loading `/system/fonts` (and `/product/fonts`,
  `/data/fonts`) at startup; `.SystemUIFont` is Roboto, and `add_fonts`
  works as everywhere.
- **Touch**, synthesised into gpui's mouse model (see below).
- **Keyboards**, hardware and software: key events become
  `KeyDown`/`KeyUp` with the macOS `key`/`key_char` conventions, with the
  characters looked up in the input device's `KeyCharacterMap`. A
  printable key no binding takes is typed into the focused text field.
  What gpui leaves unhandled goes back to the system, so Back (delivered
  as `escape`) still leaves the app when nothing binds it, and the volume
  keys work. gpui's `cmd` modifier is the Meta key, which Android reserves
  for its own shortcuts (Meta+G opens Gmail, and no Meta chord reaches the
  app at all), so bind `ctrl` for Android as for Linux.
- **The software keyboard** appears while a gpui text element has focus
  (that is when a window has an input handler) and goes away when focus
  leaves. It types through key events: `NativeActivity` offers the input
  method no `InputConnection`, so the keyboard falls back to sending a
  key event per character. Letters, digits, punctuation, Enter, Backspace
  and Space arrive that way; suggestions, autocorrect and IME composition
  (which need an `InputConnection`) do not.
- **Safe areas**: `Window::safe_area_insets()` reports the status bar, the
  navigation bar or gesture area, a display cutout and, while it is up,
  the software keyboard, from `WindowInsets` (Android 11+). The window is
  drawn edge to edge, so pad the root element by these insets. Changes
  trigger a relayout; the insets are re-read a few times a second, since
  the keyboard comes and goes without a native event.
- A rotation, a dark-mode switch or a density change arrive as a resize or
  an appearance change rather than restarting the app (the example
  manifest's `configChanges` says so; copy it). Dark mode follows the
  configuration's night mode.
- `Application::run` returns when the activity is destroyed, after the
  quit callbacks; `App::quit` finishes the activity.
- The clipboard (text only: an image needs a content provider the app
  would have to declare in Java), `open_url` (an `ACTION_VIEW` intent),
  the URL or file the app was launched to open through `App::on_open_urls`
  (from the launch intent; a later intent to a running app is not seen),
  and credentials: `write_credentials` encrypts with an AES key generated
  in the Android keystore (which never leaves it) and stores the result
  in the app's private data directory.
- Panics and `println!`/`eprintln!` show up in logcat (tags `gpui-stdout`
  and `gpui-stderr`): Android connects a process's stdio to nothing, so
  gpui pipes them through.
- `Application::headless()` runs the executor without an activity, for a
  test binary run from a shell.

Not available:

- File pickers (`prompt_for_paths`, `prompt_for_new_path`), native prompts
  (`prompt` returns `None`, so gpui draws its own when the app has a prompt
  builder) and native context menus (`show_context_menu` returns `false`,
  so the app draws its own): each needs an activity result or a Java
  listener, which a `NativeActivity` has no way to receive. `reveal_path`,
  `open_with_system` (a `file://` URI cannot be handed to another app),
  `restart`, `hide`, `activate(ignoring_other_apps)`, cursor styles, app
  and dock menus (`set_menus` keeps the menus for `get_menus` and the
  keymap bindings still fire), screen capture, auxiliary executables, and
  `register_url_scheme` (declare an intent filter in the manifest).
- A second window, as above.

## Touch model

gpui's input is a mouse, so a finger becomes one, exactly as on iOS:

| Gesture | gpui sees |
| --- | --- |
| Tap | Left `MouseDown` then `MouseUp` at the point; quick repeats raise `click_count` |
| Drag past 8dp | The press is cancelled (a `MouseUp` far outside the window, as browsers cancel a pointer when they take a scroll) and `ScrollWheelEvent`s follow the finger; a fling keeps scrolling with momentum |
| Long press (0.5s) | Right `MouseDown`/`MouseUp` at the point; moving the finger afterwards drags with the left button held, which is how Android starts drags |
| Two-finger drag | `ScrollWheelEvent`s from the fingers' midpoint, whatever one finger does over the same element; any press in progress is cancelled |
| Two-finger pinch | `PinchEvent` with `Started`/`Moved`/`Ended` phases, recognised alongside the two-finger drag |
| Stylus | Left button with `pressure`; its barrel button is the right button; never scrolls |
| Mouse | A real mouse: clicks and drags with its own buttons, `MouseMove` on hover, `ScrollWheel` in lines for the wheel |

An element whose mouse-down handler calls `Window::claim_touch_drag()`
keeps that finger as a drag: it gets `MouseMove`s with the button held and
never a scroll, and a long press on it is a press, not a right click. That
is what a canvas that paints with the mouse wants; lists and panels leave
the default alone. A finger hovers nothing once it lifts, so when the last
touch ends (and any fling it started has stopped) the mouse is moved off
the window, and hover styles clear.

The 8dp slop, 0.5s hold and the deceleration rate are constants at the top
of `src/platform/android/window.rs`.

## Building and running

Requirements: the Android SDK with `platform-tools`, `build-tools`, a
`platforms;android-35` (any recent API works; the manifest targets 35 and
requires 30), an NDK, and, to run without a device, `emulator` and a
system image. From Homebrew:

```sh
brew install --cask android-commandlinetools temurin
sdkmanager --install "platform-tools" "build-tools;35.0.0" "platforms;android-35" \
    "ndk;27.2.12479018" "emulator" "system-images;android-35;google_apis;arm64-v8a"
rustup target add aarch64-linux-android
```

`cargo check --target aarch64-linux-android` needs the NDK's clang as the
target's C compiler and linker, because the `android-activity` glue
compiles one C file; the environment variables are in `UPSTREAM.md`
("Checking the other platforms"). `examples/android/run-emulator.sh` sets
them from `ANDROID_HOME`/`ANDROID_NDK_HOME` (or the usual SDK locations),
builds an example, wraps it in an APK, and runs it on the attached device
or emulator, booting one if there is none, then follows its log:

```sh
examples/android/run-emulator.sh mobile              # touch and gestures
examples/android/run-emulator.sh input               # the software keyboard
examples/android/run-emulator.sh hello_world --release
examples/android/run-emulator.sh mobile --headless   # an emulator with no window
```

`rustc` cannot build one crate as both an executable and a shared
library, so each example that runs on Android has a second target,
`<example>_android`, whose source is a one-line wrapper around the
example (`examples/android/<example>.rs`, `crate-type = ["cdylib"]` in
`Cargo.toml`); add such a pair for a new example. An application does the
same: build it as a `cdylib` (with `gpui::android_main!(main)` somewhere
in it), put the library under `lib/<abi>/` in an APK whose manifest names
`android.app.NativeActivity` with `android.app.lib_name` set to the
library's name (see `examples/android/AndroidManifest.xml`; keep its
`configChanges`), and sign it. Gradle and Android Studio can do the same
with a `jniLibs` directory. `Application::run` must be called from
`android_main`, which is what the macro arranges, and returns when the
activity is destroyed.

The script's emulator boots with `-feature Vulkan -gpu swiftshader_indirect`,
that is, with a software Vulkan device on the host. That device is Vulkan
1.3 but advertises none of the promoted extensions blade asks for by
name, which is why `blade-graphics` is vendored with a small change
(`vendor/README.md`). A real device needs a Vulkan 1.1 driver with the
extensions listed above, which every device from about 2019 on has.

Driving the app from a terminal: `adb shell input swipe`/`tap`/`text`/
`keyevent` and `adb exec-out screencap -p` cover single-finger input, the
software keyboard's key events, hardware keys and evidence. A swipe under
about 100ms delivers a down and an up with no move between; the backend
treats a lift far from its landing point as a scroll, so such a swipe
flings. Two fingers need `adb root` and `sendevent` on the touchscreen's
`/dev/input/event*` (multitouch protocol B), and the emulator's screen has
a pressure axis, so each contact must report `ABS_MT_PRESSURE` above zero
or Android takes it for a hover. `adb logcat gpui-stderr:V '*:S'` shows the
app's panics.

## Differences from the desktop backends

- `is_window_hovered` equals `is_window_active`, as on macOS and iOS.
- The clipboard is the system clipboard, readable only while the app is in
  the foreground (Android's rule).
- There is no keyboard-layout API; `keyboard_layout` reports the
  configuration's language and `keyboard_mapper` is the identity mapper.
- Frames are paced by a timer at the display's refresh rate while a
  surface exists and the activity is resumed, and not at all otherwise;
  the executor still runs in the background.
