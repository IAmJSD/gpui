//! Pinch/magnify gesture dispatch.
//!
//! Platform backends report only the incremental `delta`; `Window` turns that
//! into the cumulative `scale`. These tests cover that accumulation and the
//! routing of `PinchEvent` to an `on_pinch` handler, which is as far as this
//! can be checked without a trackpad.

use gpui::{
    AppContext, Context, InteractiveElement, IntoElement, PinchEvent, Render, Styled,
    TestAppContext, TouchPhase, Window, div, point, px, size,
};
use std::{cell::RefCell, rc::Rc};

/// Records every pinch the element sees, as (delta, scale, phase).
type Log = Rc<RefCell<Vec<(f32, f32, TouchPhase)>>>;

struct PinchView(Log);

impl Render for PinchView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let log = self.0.clone();
        div().size_full().on_pinch(move |event: &PinchEvent, _, _| {
            log.borrow_mut()
                .push((event.delta, event.scale, event.phase));
        })
    }
}

fn pinch(delta: f32, phase: TouchPhase) -> PinchEvent {
    PinchEvent {
        position: point(px(50.), px(50.)),
        delta,
        // Whatever a backend puts here is overwritten by the window.
        scale: 1.0,
        phase,
        ..Default::default()
    }
}

#[gpui::test]
fn test_pinch_reaches_handler_with_accumulated_scale(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let log: Log = Default::default();

    let view_log = log.clone();
    cx.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, cx| {
        cx.new(|_| PinchView(view_log))
    });

    cx.simulate_event(pinch(1.0, TouchPhase::Started));
    cx.simulate_event(pinch(1.5, TouchPhase::Moved));
    cx.simulate_event(pinch(2.0, TouchPhase::Moved));
    cx.simulate_event(pinch(1.0, TouchPhase::Ended));

    let seen = log.borrow().clone();
    assert_eq!(
        seen,
        vec![
            (1.0, 1.0, TouchPhase::Started),
            (1.5, 1.5, TouchPhase::Moved),
            // 1.5 * 2.0: scale is cumulative across the gesture.
            (2.0, 3.0, TouchPhase::Moved),
            (1.0, 3.0, TouchPhase::Ended),
        ]
    );
}

#[gpui::test]
fn test_pinch_scale_resets_between_gestures(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let log: Log = Default::default();

    let view_log = log.clone();
    cx.draw(point(px(0.), px(0.)), size(px(100.), px(100.)), |_, cx| {
        cx.new(|_| PinchView(view_log))
    });

    cx.simulate_event(pinch(1.0, TouchPhase::Started));
    cx.simulate_event(pinch(4.0, TouchPhase::Moved));
    cx.simulate_event(pinch(1.0, TouchPhase::Ended));
    // A second gesture must not inherit the first one's scale.
    cx.simulate_event(pinch(1.0, TouchPhase::Started));
    cx.simulate_event(pinch(0.5, TouchPhase::Moved));

    assert_eq!(log.borrow().last().unwrap().1, 0.5);
}

#[gpui::test]
fn test_pinch_outside_the_element_is_not_delivered(cx: &mut TestAppContext) {
    let cx = cx.add_empty_window();
    let log: Log = Default::default();

    let view_log = log.clone();
    // Element occupies the top-left 20x20 of the window.
    cx.draw(point(px(0.), px(0.)), size(px(20.), px(20.)), |_, cx| {
        cx.new(|_| PinchView(view_log))
    });

    // ...but the gesture centroid is at (50, 50).
    cx.simulate_event(pinch(1.5, TouchPhase::Moved));

    assert!(log.borrow().is_empty());
}
