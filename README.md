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
| Linux/Wayland | Supported | `zwp_pointer_gestures_v1` pinch, when the compositor advertises it |
| Linux/X11 | Supported | XI 2.4 gesture events (xorg-server 21.1+, libinput); older servers deliver nothing |
| Windows | Touchscreen only | `WM_GESTURE` / `GID_ZOOM`. Precision-touchpad pinches arrive as Ctrl+scroll instead; real touchpad pinch would need Direct Manipulation |

Windows touchpads and pre-21.1 X11 servers cannot deliver a pinch, so
applications should keep a modifier+scroll zoom path as a fallback rather
than relying on `on_pinch` alone. (On Windows a Ctrl+scroll fallback is
precisely the form touchpad pinches arrive in.)


## Stylus pressure

`MouseDownEvent`, `MouseUpEvent` and `MouseMoveEvent` carry a `pressure`
field, 0.0..=1.0. It is **1.0** for an ordinary mouse and on platforms
whose tablet input is not wired up, so a caller can multiply by it
unconditionally -- a brush that scaled its opacity by pressure would
otherwise paint nothing at all on a mouse.

| Platform | Status | Mechanism |
| --- | --- | --- |
| macOS | Supported | `NSEvent.pressure` |
| Linux/Wayland | Not implemented | Would need `zwp_tablet_v2` |
| Linux/X11 | Supported | XInput2 "Abs Pressure" valuator |
| Windows | Supported | `WM_POINTER` pen pressure, carried onto the synthesised mouse messages |

Two platforms cannot tell "no tablet" from "zero pressure" and report full
pressure for both: AppKit reports 0 for an ordinary mouse click, which is
indistinguishable from a stylus barely touching the tablet, so anything at
or below zero is reported as full pressure, and Windows does the same for a
zero reading, which is what a hovering pen reports. X11 identifies tablets
by their pressure valuator, so there a hovering stylus reports its true
(zero) pressure and only devices without the valuator report 1.0.

On Windows the pen system gestures (press-and-hold for right-click, tap
feedback, flicks) are disabled on GPUI windows, since they delay or swallow
the pen events a drawing surface needs to receive immediately.
