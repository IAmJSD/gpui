//! Touch, gestures and menus on iOS and iPadOS.
//!
//! Run it in the simulator with `examples/ios/run-simulator.sh mobile`
//! (see `docs/ios.md`). It also runs on the desktop, where the same code
//! gets a real mouse. What to try:
//!
//! * drag the list to scroll it, and fling it for momentum;
//! * pinch the square to scale it;
//! * long-press the banner for a native context menu (an action sheet on
//!   iPhone, a popover on iPad);
//! * on an iPad with a keyboard, hold Command for the app menu's shortcuts,
//!   or open the menu bar;
//! * rotate the device: the content stays inside the safe area.

use gpui::{
    App, Application, Context, FocusHandle, KeyBinding, Menu, MenuItem, MouseButton,
    MouseDownEvent, PinchEvent, Window, WindowOptions, actions, div, prelude::*, px, rgb,
};

actions!(mobile, [Greet, Reset, Quit]);

struct Mobile {
    /// Keeps the root focused so menu actions and shortcuts reach it.
    focus_handle: FocusHandle,
    scale: f32,
    status: String,
}

impl Mobile {
    fn context_menu_items() -> Vec<MenuItem> {
        vec![
            MenuItem::action("Say hello", Greet),
            MenuItem::action("Reset the square", Reset),
            MenuItem::separator(),
            MenuItem::submenu(Menu {
                name: "More".into(),
                items: vec![MenuItem::action("Quit", Quit)],
            }),
        ]
    }
}

impl Render for Mobile {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let insets = window.safe_area_insets();
        let scale = self.scale;

        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &Greet, _, cx| {
                this.status = "Hello from the menu!".into();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Reset, _, cx| {
                this.scale = 1.0;
                this.status = "Square reset".into();
                cx.notify();
            }))
            .size_full()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .pt(insets.top)
            .pb(insets.bottom)
            .pl(insets.left)
            .pr(insets.right)
            .flex()
            .flex_col()
            .child(
                div()
                    .p_4()
                    .bg(rgb(0x313244))
                    .text_lg()
                    .child("Long-press here for a context menu")
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            if !window.show_context_menu(event.position, Self::context_menu_items())
                            {
                                this.status = "No native context menu on this platform".into();
                                cx.notify();
                            }
                        }),
                    ),
            )
            .child(div().px_4().py_2().text_sm().child(self.status.clone()))
            .child(
                div()
                    .h(px(220.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_pinch(cx.listener(|this, event: &PinchEvent, _, cx| {
                        this.scale = (this.scale * event.delta).clamp(0.25, 4.0);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size(px(100. * scale))
                            .rounded_lg()
                            .bg(rgb(0xf38ba8))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(0x1e1e2e))
                            .child(format!("{scale:.2}x")),
                    ),
            )
            .child(
                div()
                    .id("list")
                    .flex_1()
                    .overflow_y_scroll()
                    .border_t_1()
                    .border_color(rgb(0x45475a))
                    .children((1..=100).map(|row| {
                        div()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(rgb(0x313244))
                            .child(format!("Row {row}"))
                    })),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("cmd-g", Greet, None),
            KeyBinding::new("cmd-r", Reset, None),
            KeyBinding::new("cmd-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        // On macOS this is the menu bar; on iPadOS it backs the menu bar and
        // the Command-key shortcut overlay; on iPhone it has no surface.
        cx.set_menus(vec![
            Menu {
                name: "Mobile".into(),
                items: vec![MenuItem::action("Quit", Quit)],
            },
            Menu {
                name: "Demo".into(),
                items: vec![
                    MenuItem::action("Say hello", Greet),
                    MenuItem::action("Reset the square", Reset),
                ],
            },
        ]);
        cx.open_window(WindowOptions::default(), |window, cx| {
            let view = cx.new(|cx| Mobile {
                focus_handle: cx.focus_handle(),
                scale: 1.0,
                status: "Ready".into(),
            });
            window.focus(&view.read(cx).focus_handle);
            view
        })
        .unwrap();
        cx.activate(true);
    });
}
