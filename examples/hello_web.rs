//! A gpui application that runs in the browser. See `docs/web.md` for how to
//! build and serve it.
//!
//! The browser exposes no system fonts, so the example embeds IBM Plex Sans
//! (`examples/fonts/`, SIL OFL 1.1) -- the same family gpui's default
//! `.SystemUIFont` resolves to on this backend.

use gpui::{
    Animation, AnimationExt as _, App, Application, Context, MouseButton, Window, WindowOptions,
    black, div, prelude::*, px, rgb,
};
use std::time::Duration;

struct HelloWeb {
    clicks: usize,
}

impl Render for HelloWeb {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .size_full()
            .justify_center()
            .items_center()
            .bg(rgb(0x2e3440))
            .text_color(rgb(0xeceff4))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_8()
                    .bg(rgb(0x3b4252))
                    .rounded_xl()
                    .shadow_lg()
                    .items_center()
                    .child(div().text_xl().child("Hello from gpui on the web"))
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(square(0xbf616a))
                            .child(square(0xebcb8b))
                            .child(square(0xa3be8c))
                            .child(square(0x81a1c1)),
                    )
                    .child(
                        div()
                            .id("click-me")
                            .px_4()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(0x5e81ac))
                            .cursor_pointer()
                            .child(if self.clicks == 0 {
                                "Click me".to_string()
                            } else {
                                format!("Clicked {} times", self.clicks)
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _event, _window, cx| {
                                    this.clicks += 1;
                                    cx.notify();
                                }),
                            ),
                    )
                    .child(
                        div()
                            .w(px(224.0))
                            .h(px(8.0))
                            .rounded_full()
                            .bg(rgb(0x4c566a))
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(rgb(0x88c0d0))
                                    .with_animation(
                                        "progress",
                                        Animation::new(Duration::from_secs(2)).repeat(),
                                        |this, delta| this.w(px(224.0 * delta)),
                                    ),
                            ),
                    ),
            )
    }
}

fn square(color: u32) -> impl IntoElement {
    div()
        .size_12()
        .bg(rgb(color))
        .rounded_md()
        .border_2()
        .border_color(black())
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    {
        console_error_panic_hook::set_once();
        console_log::init().ok();
    }

    Application::new().run(|cx: &mut App| {
        cx.text_system()
            .add_fonts(vec![
                include_bytes!("fonts/IBMPlexSans-Regular.ttf")
                    .as_slice()
                    .into(),
            ])
            .unwrap();
        cx.open_window(WindowOptions::default(), |_, cx| {
            cx.new(|_| HelloWeb { clicks: 0 })
        })
        .unwrap();
    });
}
