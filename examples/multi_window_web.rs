//! Two gpui windows in one browser page. On the web a window is a canvas, so
//! this checks that a second `open_window` gets a second canvas of its own --
//! with its own `data-raw-handle`, its own WebGPU surface and its own frame
//! loop -- rather than the two windows fighting over one.
//!
//! The canvases are stacked in creation order, so the second window covers the
//! first; each draws its own label and counts its own clicks, and the DOM can
//! be inspected for the two `<canvas data-raw-handle>` elements. See
//! `docs/web.md` for how to build and serve it.

use gpui::{
    App, Application, Bounds, Context, FocusHandle, Focusable, KeyDownEvent, MouseButton, Rgba,
    SharedString, Window, WindowBounds, WindowKind, WindowOptions, div, point, prelude::*, px, rgb,
    size,
};

struct WindowContents {
    label: SharedString,
    background: Rgba,
    clicks: usize,
    keys: usize,
    focus_handle: FocusHandle,
}

impl Focusable for WindowContents {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for WindowContents {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self.label.clone();
        div()
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                // Each window logs the keys it sees, so a key delivered to
                // more than one window is obvious.
                log::info!("{label}: key {}", event.keystroke.unparse());
                this.keys += 1;
                cx.notify();
            }))
            .flex()
            .flex_col()
            .gap_4()
            .size_full()
            .items_center()
            .justify_center()
            .bg(self.background)
            .text_color(rgb(0xeceff4))
            .child(div().text_xl().child(self.label.clone()))
            .child(
                div()
                    .id("click-me")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x4c566a))
                    .cursor_pointer()
                    .child(format!("clicks: {} keys: {}", self.clicks, self.keys))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.clicks += 1;
                            cx.notify();
                        }),
                    ),
            )
            // A hairline inset border makes each canvas's extent visible.
            .child(div().w(px(320.)).h(px(1.)).bg(rgb(0xeceff4)))
    }
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    {
        console_error_panic_hook::set_once();
        console_log::init().ok();
    }
    #[cfg(not(target_arch = "wasm32"))]
    env_logger::init();

    Application::new().run(|cx: &mut App| {
        #[cfg(target_arch = "wasm32")]
        cx.text_system()
            .add_fonts(vec![
                include_bytes!("fonts/IBMPlexSans-Regular.ttf").as_slice().into(),
            ])
            .unwrap();

        for (index, (label, background)) in [
            ("window one (canvas #1)", rgb(0x2e3440)),
            ("window two (canvas #2)", rgb(0x3b4252)),
        ]
        .into_iter()
        .enumerate()
        {
            log::info!("opening window {}", index + 1);
            let window = cx
                .open_window(WindowOptions::default(), |_, cx| {
                    cx.new(|cx| WindowContents {
                        label: label.into(),
                        background,
                        clicks: 0,
                        keys: 0,
                        focus_handle: cx.focus_handle(),
                    })
                })
                .unwrap();
            window
                .update(cx, |view, window, cx| {
                    window.focus(&view.focus_handle(cx));
                })
                .unwrap();
        }

        // A positioned popup window: on the web this becomes a canvas at the
        // requested bounds, above the full-viewport windows.
        cx.open_window(
            WindowOptions {
                kind: WindowKind::PopUp,
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(40.0), px(40.0)),
                    size: size(px(320.0), px(120.0)),
                })),
                ..Default::default()
            },
            |_, cx| {
                cx.new(|cx| WindowContents {
                    label: "popup (canvas #3, 320\u{d7}120 at 40,40)".into(),
                    background: rgb(0x4c566a),
                    clicks: 0,
                    keys: 0,
                    focus_handle: cx.focus_handle(),
                })
            },
        )
        .unwrap();
    });
}
