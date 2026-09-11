# gpui on iOS and iPadOS

gpui builds for `aarch64-apple-ios` (devices) and `aarch64-apple-ios-sim` /
`x86_64-apple-ios` (the Simulator). UIKit owns the process: `Application::run`
calls `UIApplicationMain` and never returns, as on macOS. Each gpui window is
a `UIWindow` with one Metal-backed view; frames are paced by a
`CADisplayLink`. The Metal renderer, the CoreText text system and the
libdispatch executor are the macOS ones, included from `src/platform/mac`
by path, so the two Apple backends share one renderer and one text stack.
The UIKit-specific code lives in `src/platform/ios`.

## Status

Verified in the iOS 26.3 Simulator (iPhone 17 Pro and iPad Pro 13-inch,
Xcode 26.3) with `examples/hello_world.rs` and `examples/mobile.rs`:
rendering and text, the scene lifecycle, safe-area insets, a long press
opening a native context menu and its item dispatching a gpui action, a
fling scrolling a list with momentum, a hardware-keyboard `cmd-g` reaching
a gpui key binding, the app menu being built through `UIMenuBuilder` with
its `UIKeyCommand`s validated, and, with `examples/input.rs`, typing from
the hardware keyboard and from the software keyboard (including its
predictive bar, which reads the field through `UITextInput`, and its
Backspace key) landing in a gpui text field. No physical device has run it yet, and
the Simulator cannot fake a pinch or a real input method, so the pinch
gesture, the software keyboard's IME composition and autocorrect, the
document picker, the keychain, and the iPad pointer paths are implemented
against the documented UIKit behaviour but unverified.

Working:

- The full gpui programming model. Windows fill the scene; on iPhone that
  is the screen, on iPad it is whatever Stage Manager or Split View gives
  the app. `WindowKind::PopUp` windows keep the bounds they asked for and
  float above the main window at alert level.
- Rendering through the shared Metal renderer (CPU buffers are shared
  rather than managed on iOS). Video surfaces (`Window::paint_surface`)
  stay macOS-only; the core-video crate's dependency chain links
  OpenGL.framework, which iOS lacks.
- Text through CoreText, with the system fonts and `add_fonts`.
- **Touch**, synthesised into gpui's mouse model (see below).
- **Hardware keyboards**: `UIPress` events become `KeyDown`/`KeyUp` with
  the macOS `key`/`key_char` conventions, and modifier state is tracked.
  Keys gpui leaves unhandled fall through to UIKit, which turns them into
  text for the focused field.
- **The software keyboard and `UITextInput`**: the view becomes first
  responder exactly while a gpui text element has focus (that is what
  raises and dismisses the keyboard) and implements `UITextInput`, so
  marked text, replacements and the caret rect map onto gpui's
  `InputHandler`. Return, Tab and Backspace are offered to gpui as
  keystrokes first, since text fields usually bind them.
- **Safe areas**: `Window::safe_area_insets()` reports the status bar,
  display cutouts, the home indicator and, while it is up, the software
  keyboard. Changes trigger a relayout. Pad the root element by these
  insets; the window itself extends edge to edge.
- **App menus**: `App::set_menus` publishes the menu through
  `UIMenuBuilder`. On iPadOS this is the menu bar and the Command-key
  shortcut overlay of a hardware keyboard; on iPhone it has no surface but
  the key equivalents still work with a keyboard. The first menu is treated
  as the application menu, as on macOS: its items join the system's
  About/Settings/Quit group. Key equivalents come from the keymap the same
  way the macOS menu's do. Separators become inline groups; the Services
  menu is dropped. `App::set_dock_menu` becomes the app icon's home-screen
  quick actions.
- **Context menus**: `Window::show_context_menu(position, items)` presents
  a native menu, an action sheet on iPhone and a popover anchored at the
  position on iPad. Submenus flatten to "Menu ▸ Item" rows. The chosen
  item's action is dispatched like an app-menu action. It returns `false`
  on platforms with no native menu (all the desktops), so a caller can draw
  its own there.
- **The edit menu**: with a text field focused, a long press presents the
  system edit menu (`UIEditMenuInteraction`, iOS 16+) when the app has
  registered `OsAction::Cut`/`Copy`/`Paste`/`SelectAll` menu items, and
  those actions are what the menu's buttons dispatch.
- Pasteboard (text with gpui's metadata, and images), `open_url`,
  `prompt` (an alert), the document picker for `prompt_for_paths` and
  `prompt_for_new_path` (the latter exports an empty placeholder file and
  reports where it landed), `open_with_system` (the share sheet), the
  keychain for credentials, dark mode through the trait collection, and
  the app being paused while backgrounded (Metal work in the background
  terminates an iOS app).

Not available:

- `restart`, `hide`, `activate(ignoring_other_apps)`, cursor styles, screen
  capture, auxiliary executables, `register_url_scheme` (declare
  `CFBundleURLTypes` in Info.plist instead), and moving the selection from
  the keyboard's cursor gestures (gpui's `InputHandler` cannot set a
  selection).

## Touch model

gpui's input is a mouse, so a finger becomes one:

| Gesture | gpui sees |
| --- | --- |
| Tap | Left `MouseDown` then `MouseUp` at the point; quick repeats raise `click_count` |
| Drag past 8pt | The press is cancelled (a `MouseUp` far outside the window, as browsers cancel a pointer when they take a scroll) and `ScrollWheelEvent`s follow the finger; a fling keeps scrolling with UIScrollView's deceleration |
| Long press (0.5s) | Right `MouseDown`/`MouseUp` at the point, or the system edit menu over a focused text field; moving the finger afterwards drags with the left button held, which is how iOS starts drags |
| Two-finger pinch | `PinchEvent` with `Started`/`Moved`/`Ended` phases; any press in progress is cancelled |
| Apple Pencil | Left button with `pressure`; never scrolls |
| iPad pointer | A real mouse: clicks and drags, `MouseMove` on hover, `ScrollWheel` for two-finger scrolling and mouse wheels, secondary button as right click |

The 8pt slop, 0.5s hold and the deceleration rate are constants at the top
of `src/platform/ios/window.rs`.

## Building and running

Requirements: a macOS host with Xcode and its iOS platform installed, and
the Rust targets:

```sh
rustup target add aarch64-apple-ios-sim   # Apple Silicon Simulator
rustup target add x86_64-apple-ios        # Intel Simulator
rustup target add aarch64-apple-ios       # devices
```

The build script compiles `shaders.metal` with `xcrun -sdk iphonesimulator`
or `iphoneos` (chosen by the target's ABI) and generates the libdispatch
bindings against that SDK, so a plain `cargo build --target ...` works:

```sh
cargo check --target aarch64-apple-ios-sim
cargo check --target aarch64-apple-ios
```

An iOS executable needs an `.app` bundle with an `Info.plist` to run.
`examples/ios/run-simulator.sh` builds an example, wraps it with
`examples/ios/Info.plist`, boots a Simulator if none is running, installs
and launches it:

```sh
examples/ios/run-simulator.sh mobile                       # touch, gestures, menus
examples/ios/run-simulator.sh input                        # the software keyboard
examples/ios/run-simulator.sh hello_world --device "iPad Pro 13-inch (M5)"
SIMULATOR_UDID=<udid> examples/ios/run-simulator.sh mobile --release
```

The example bundles are unsigned, which the Simulator accepts. A device or
App Store build needs signing, entitlements (the keychain needs
`keychain-access-groups`) and usually an Xcode project: build the
application as a `staticlib` crate whose exported entry point calls
`Application::run`, and link it from a one-file Objective-C or Swift
target. `Application::run` must be called on the main thread and never
returns; open windows from its callback.

The `Info.plist` template declares the scene manifest naming gpui's
`GPUISceneDelegate`; the app delegate also opts into scenes on its own, so
an app without the manifest still runs on today's SDKs. Declare a
`UILaunchScreen` (an empty dictionary will do) to get the full-screen
layout on iPhone.

## Differences from the desktop backends

- One window is visible on iPhone; extra `Normal` windows stack and the
  last one opened is key. `is_maximized` is always true, `is_fullscreen`
  false, `resize`/`minimize`/`zoom` do nothing except for popups.
- `Platform::quit` runs the quit callbacks and exits the process, which
  iOS does not expect apps to do; leave the user to close the app.
- `is_window_hovered` equals `is_window_active`, as on macOS.
- The pasteboard is `UIPasteboard.generalPasteboard`, so text written by
  gpui also lands in the system clipboard for other apps.
- There is no keyboard-layout API; `keyboard_layout` reports the current
  input mode's language and `keyboard_mapper` is the identity mapper.
