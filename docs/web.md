# gpui on the web

gpui compiles to `wasm32-unknown-unknown` and renders into an
`HtmlCanvasElement` through **WebGPU**. The browser's JS event loop is the
platform event loop: tasks run as microtasks and timeouts, and frames are
driven by `requestAnimationFrame`.

## Status

Working:

- The full gpui programming model: entities, layout (taffy), styling,
  animations, `Application::run` / `open_window`.
- Rendering of quads, borders, shadows, underlines, paths, sprites and text
  via WebGPU (a port of the blade renderer; see
  `src/platform/web/renderer.rs`).
- **Text**, through the same cosmic-text stack as Linux. The browser
  exposes no system fonts, so the font database starts empty: add fonts
  with `cx.text_system().add_fonts(...)` at startup. gpui's default
  `.SystemUIFont` resolves to "IBM Plex Sans" on this backend; the
  `hello_web` example embeds it (`examples/fonts/`, SIL OFL 1.1).
- **Input**: pointer events (with pen pressure and manual multi-click
  counting), the scroll wheel (pixel and line deltas), keyboard events
  (modifier tracking, capslock, browser default suppressed exactly when
  gpui marks an event handled), hover and focus tracking. Right-click
  reaches gpui; the browser context menu is suppressed.
- Dark mode: `prefers-color-scheme` is reflected in `window_appearance` and
  appearance-change callbacks.
- Cursor styles (CSS cursors) and `open_url` (new tab; subject to the
  popup blocker outside user gestures).
- Clipboard, with a caveat: the browser clipboard is async and
  permission-gated while gpui's `read_from_clipboard` is synchronous, so
  reads are served from a mirror of what the application last wrote.
  Copying to other apps works for text (best effort); pasting content
  copied *outside* the application does not reach gpui yet.
- A single window, backed by a canvas. If the page contains
  `<canvas id="gpui">` it is used; otherwise a full-viewport canvas is
  appended to `<body>`. Device-pixel-ratio changes and canvas resizes are
  picked up every frame.

Not yet implemented:

- IME composition (dead keys and CJK input methods do not compose;
  plain typing works via key events).
- Multiple windows, file drag-and-drop, pinch gestures.
- `BackgroundExecutor::block` cannot work on the web (there is no second
  thread to make progress while the caller waits) and will panic if reached.
- The `test-support` feature does not build on wasm.

## Requirements

- A browser with WebGPU (Chrome/Edge 113+, Safari 18+, Firefox 141+).
- `rustup target add wasm32-unknown-unknown`
- `cargo install wasm-bindgen-cli --version <version>` where `<version>`
  matches the `wasm-bindgen` entry in `Cargo.lock`.

## Building the example

```sh
cargo build --target wasm32-unknown-unknown --example hello_web --release
wasm-bindgen --target web --out-dir examples/web/pkg \
    target/wasm32-unknown-unknown/release/examples/hello_web.wasm
```

Serve `examples/web/` from any static file server (WebGPU requires a secure
context: `localhost` or https):

```sh
python3 -m http.server -d examples/web 8000
# open http://localhost:8000
```

## Application structure on the web

`Platform::run` does not block. It performs the asynchronous WebGPU setup
(adapter and device requests return promises) and then invokes your
`Application::run` callback; after the callback returns, the application
lives on in the callbacks it registered (rendering, timers, spawned tasks).
Consequences:

- `open_window` must be called from (or after) the run callback, never
  before `run`.
- `main` returns almost immediately; that is normal.
- Use `cx.spawn` / executors for async work. Both executors run on the JS
  event loop; `BackgroundExecutor::spawn` still requires `Send` futures but
  they execute on the single browser thread.

## Downstream crates

`[patch.crates-io]` is only honored in the top-level manifest of a build, so
a project that depends on gpui and targets wasm needs to copy the two patch
entries (for `gpui_util` and `gpui_http_client`) from this repository's
`Cargo.toml` into its own workspace manifest, along with the
`getrandom_backend = "wasm_js"` rustflag from `.cargo/config.toml`.
