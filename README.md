> **This is a fork.** Upstream GPUI is developed by Zed Industries at
> [zed-industries/zed](https://github.com/zed-industries/zed); this repository
> adds pinch/magnify gestures and stylus pressure on top of the published
> 0.2.2 release.
> See [`UPSTREAM.md`](UPSTREAM.md) for provenance and
> [Pinch gestures](#pinch-gestures) for the addition.

# Welcome to GPUI!

GPUI is a hybrid immediate and retained mode, GPU accelerated, UI framework
for Rust, designed to support a wide variety of applications.

## Getting Started

GPUI is still in active development as we work on the Zed code editor, and is still pre-1.0. There will often be breaking changes between versions. You'll also need to use the latest version of stable Rust and be on macOS or Linux. Add the following to your `Cargo.toml`:

```toml
gpui = { version = "*" }
```

 - [Ownership and data flow](src/_ownership_and_data_flow.rs)

Everything in GPUI starts with an `Application`. You can create one with `Application::new()`, and kick off your application by passing a callback to `Application::run()`. Inside this callback, you can create a new window with `App::open_window()`, and register your first root view. See [gpui.rs](https://www.gpui.rs/) for a complete example.

### Dependencies

GPUI has various system dependencies that it needs in order to work.

#### macOS

On macOS, GPUI uses Metal for rendering. In order to use Metal, you need to do the following:

- Install [Xcode](https://apps.apple.com/us/app/xcode/id497799835?mt=12) from the macOS App Store, or from the [Apple Developer](https://developer.apple.com/download/all/) website. Note this requires a developer account.

> Ensure you launch Xcode after installing, and install the macOS components, which is the default option.

- Install [Xcode command line tools](https://developer.apple.com/xcode/resources/)

  ```sh
  xcode-select --install
  ```

- Ensure that the Xcode command line tools are using your newly installed copy of Xcode:

  ```sh
  sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
  ```

## The Big Picture

GPUI offers three different [registers](<https://en.wikipedia.org/wiki/Register_(sociolinguistics)>) depending on your needs:

- State management and communication with `Entity`'s. Whenever you need to store application state that communicates between different parts of your application, you'll want to use GPUI's entities. Entities are owned by GPUI and are only accessible through an owned smart pointer similar to an `Rc`. See the `app::context` module for more information.

- High level, declarative UI with views. All UI in GPUI starts with a view. A view is simply an `Entity` that can be rendered, by implementing the `Render` trait. At the start of each frame, GPUI will call this render method on the root view of a given window. Views build a tree of `elements`, lay them out and style them with a tailwind-style API, and then give them to GPUI to turn into pixels. See the `div` element for an all purpose swiss-army knife of rendering.

- Low level, imperative UI with Elements. Elements are the building blocks of UI in GPUI, and they provide a nice wrapper around an imperative API that provides as much flexibility and control as you need. Elements have total control over how they and their child elements are rendered and can be used for making efficient views into large lists, implement custom layouting for a code editor, and anything else you can think of. See the `element` module for more information.

Each of these registers has one or more corresponding contexts that can be accessed from all GPUI services. This context is your main interface to GPUI, and is used extensively throughout the framework.

## Other Resources

In addition to the systems above, GPUI provides a range of smaller services that are useful for building complex applications:

- Actions are user-defined structs that are used for converting keystrokes into logical operations in your UI. Use this for implementing keyboard shortcuts, such as cmd-q. See the `action` module for more information.

- Platform services, such as `quit the app` or `open a URL` are available as methods on the `app::App`.

- An async executor that is integrated with the platform's event loop. See the `executor` module for more information.,

- The `[gpui::test]` macro provides a convenient way to write tests for your GPUI applications. Tests also have their own kind of context, a `TestAppContext` which provides ways of simulating common platform input. See `app::test_context` and `test` modules for more details.

Currently, the best way to learn about these APIs is to read the Zed source code, ask us about it at a fireside hack, or drop a question in the [Zed Discord](https://zed.dev/community-links). We're working on improving the documentation, creating more examples, and will be publishing more guides to GPUI on our [blog](https://zed.dev/blog).


## Running in the browser

This fork compiles to `wasm32-unknown-unknown` and runs gpui applications in
the browser: windows are canvases rendered through WebGPU, text is shaped by
the same cosmic-text stack as Linux (bring your own fonts -- browsers expose
none), and DOM events become gpui input, including pinch gestures, IME
composition and clipboard paste. Applications keep the normal gpui
programming model; the platform differences that leak through (a
non-blocking `Application::run`, no `BackgroundExecutor::block`) are small
and documented. See [docs/web.md](docs/web.md) for the status table, build
steps, and three browser examples (`hello_web`, `input_web`,
`multi_window_web`).

## Running on iOS and iPadOS

This fork also builds for `aarch64-apple-ios` and the Simulator targets.
UIKit hosts the app, the Metal renderer and CoreText text system are shared
with macOS, and touch is mapped onto gpui's mouse model: taps click, drags
scroll with momentum, a long press is a right click (or the system edit
menu over a text field), pinches are `PinchEvent`s, and iPad pointers are
real mice. The software keyboard drives gpui text fields through
`UITextInput`; `Window::safe_area_insets()` reports what system UI covers.
App menus set with `App::set_menus` become the iPadOS menu bar and
hardware-keyboard shortcuts, and `Window::show_context_menu` shows a native
action sheet or popover. See [docs/ios.md](docs/ios.md) for the status,
build steps and `examples/ios/run-simulator.sh`, which runs any example in
the Simulator (`examples/mobile.rs` exercises the touch and menu paths).

## Running on Android

This fork also builds for `aarch64-linux-android` (and `x86_64`), as a
shared library loaded by Android's own `NativeActivity`, so an app needs no
Java: `gpui::android_main!(main)` next to an ordinary `main` is the whole
entry point. Rendering is the blade Vulkan renderer shared with Linux, text
the cosmic-text stack with the system's Roboto and Noto fonts, and touch is
mapped onto gpui's mouse model as on iOS: taps click, drags scroll with
momentum, a long press is a right click, two fingers pan and pinch
(`PinchEvent`), a stylus draws with pressure, and a mouse is a mouse. The
software keyboard appears while a gpui text field has focus and types into
it; `Window::safe_area_insets()` reports the status bar, navigation bar,
display cutout and keyboard. See [docs/android.md](docs/android.md) for the
status, build steps and `examples/android/run-emulator.sh`, which builds
any example into an APK and runs it on a device or an emulator it boots.

## Pinch gestures

This fork adds a `PinchEvent` alongside `ScrollWheelEvent`, so trackpad
pinch-to-zoom can be handled directly instead of being approximated with
modifier+scroll:

```rust
div().on_pinch(|event: &PinchEvent, _window, _cx| {
    // `delta` is the multiplicative change since the previous event of this
    // gesture, so this is all a zoomable view needs:
    zoom *= event.delta;

    // `scale` is the cumulative change since the gesture began (1.0 at
    // `TouchPhase::Started`), for views that would rather snapshot their
    // starting zoom and multiply once.
    // `position` is the gesture centroid, for zooming about the fingers.
});
```

Platforms report magnification in different terms — macOS sends a per-event
increment, Wayland and X11 an absolute scale relative to the start of the
gesture, Windows an absolute finger distance — so all of them are normalised
into `delta`/`scale` before dispatch. Gestures arrive as a
`Started` event, zero or more `Moved` events, and an `Ended` event; `delta` is
`1.0` for the first and last of those. Like scroll events, pinches are routed
to the element under the centroid.

### Platform support

| Platform | Status | Mechanism |
| --- | --- | --- |
| macOS | Supported | `magnifyWithEvent:` / `NSEventTypeMagnify` |
| iOS/iPadOS | Supported | `UIPinchGestureRecognizer`; trackpad pinches on iPad arrive the same way |
| Android | Supported | Two touch pointers, tracked by the backend; a two-finger drag scrolls at the same time |
| Linux/Wayland | Supported | `zwp_pointer_gestures_v1` pinch, when the compositor advertises it |
| Linux/X11 | Supported | XI 2.4 gesture events (xorg-server 21.1+, libinput); older servers deliver nothing |
| Windows | Touchscreen; touchpad opt-in | `WM_GESTURE` / `GID_ZOOM`; precision touchpads need Direct Manipulation, see below |

Pre-21.1 X11 servers cannot deliver a pinch at all, and on Windows a
precision touchpad delivers one only with the opt-in below, so applications
should keep a modifier+scroll zoom path as a fallback rather than relying on
`on_pinch` alone. (On Windows a Ctrl+scroll fallback is precisely the form
touchpad pinches otherwise arrive in.)

### Precision touchpads on Windows

`WM_GESTURE` covers touchscreens only. A precision touchpad's contacts go to
the pointer input stack instead, and a window that does nothing with them
gets the legacy fallback: `WM_MOUSEWHEEL` for a two-finger pan, Ctrl+
`WM_MOUSEWHEEL` for a pinch. Direct Manipulation is the only interface that
hands over the real gesture.

Claiming it is all-or-nothing. `DM_POINTERHITTEST` arrives before the gesture
has been classified, so a window that takes the contact to get pinches takes
the pans with it and stops receiving `WM_MOUSEWHEEL` for them. This fork
therefore replaces both: the manipulation's scale becomes a `PinchEvent` and
its translation a pixel-precise `ScrollWheelEvent`, with inertia on each.

Because that puts new code in the path of ordinary scrolling, it is off by
default. Set `GPUI_ENABLE_DIRECT_MANIPULATION` to `1` or `true` to turn it
on; anything else, or a failure to set Direct Manipulation up, leaves the
window on the Ctrl+scroll fallback. It has not been exercised on real
hardware -- see [`UPSTREAM.md`](UPSTREAM.md).


## Stylus pressure

`MouseDownEvent`, `MouseUpEvent` and `MouseMoveEvent` carry a `pressure`
field, 0.0..=1.0. It is **1.0** for an ordinary mouse and on platforms
whose tablet input is not wired up, so a caller can multiply by it
unconditionally -- a brush that scaled its opacity by pressure would
otherwise paint nothing at all on a mouse.

| Platform | Status | Mechanism |
| --- | --- | --- |
| macOS | Supported | `NSEvent.pressure` |
| Linux/Wayland | Supported | `zwp_tablet_v2`, when the compositor advertises it |
| Linux/X11 | Supported | XInput2 "Abs Pressure" valuator |
| Windows | Supported | `WM_POINTER` pen pressure, carried onto the synthesised mouse messages |

Two platforms cannot tell "no tablet" from "zero pressure" and report full
pressure for both: AppKit reports 0 for an ordinary mouse click, which is
indistinguishable from a stylus barely touching the tablet, so anything at
or below zero is reported as full pressure, and Windows does the same for a
zero reading, which is what a hovering pen reports. X11 identifies tablets
by their pressure valuator and Wayland by the tool's advertised pressure
capability, so on both a hovering stylus reports its true (zero) pressure
and only devices without the axis report 1.0.

Wayland delivers tablet input on its own protocol rather than through
`wl_pointer`, and a compositor stops emulating pointer events for a tool as
soon as a client binds that protocol, so this fork synthesises the mouse
event stream from it: the tip is the left button, the two barrel buttons are
the right and middle buttons (matching what the X11 wacom driver and Windows
pen input report), and a tool that leaves proximity produces a mouse exit.
Tablet pads -- the ring of express keys on the tablet body -- are not mapped
to anything. A tool gets its own cursor, so `set_cursor_style` reaches it
only when the compositor supports `cursor-shape-v1`; otherwise the
compositor's default cursor stays under the stylus.

On Windows the pen system gestures (press-and-hold for right-click, tap
feedback, flicks) are disabled on GPUI windows, since they delay or swallow
the pen events a drawing surface needs to receive immediately.
