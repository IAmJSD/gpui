//! A gpui window on iOS: a `UIWindow` whose root view controller shows a
//! single `GPUIView`, a `UIView` that hosts the Metal layer, receives
//! touches, presses and gestures, and speaks `UITextInput` to the system
//! keyboard.
//!
//! # Touch synthesis
//!
//! gpui's input model is a mouse. A finger becomes one:
//!
//! * a tap is a left button press and release;
//! * a drag past the slop distance becomes a scroll (the pending left press
//!   is cancelled with a release far outside the window, the way browsers
//!   cancel a pointer when they take over a scroll) and gets momentum when
//!   the finger lifts;
//! * a long press is a right click, or over a focused text field the system
//!   edit menu; moving the finger after a long press drags with the left
//!   button held, which is how iOS itself starts drags;
//! * two fingers pinching are a `PinchEvent`;
//! * an Apple Pencil is a left button with pressure and never scrolls;
//! * an iPad pointer (trackpad or mouse) is a real mouse: its touches
//!   click and drag, hover is `MouseMove`, two-finger scroll is a
//!   `ScrollWheel`, and the secondary button is a right click.

use super::{
    BoolExt, IosDisplay, NSPoint, NSRange, NSRect, NSSize, NSStringExt, UIEdgeInsets,
    events::{
        capslock_from_flags, event_modifier_flags, keystroke_from_ui_key, modifiers_from_flags,
    },
    id, nil, ns_string,
    platform::{os_action_for_selector, platform},
    renderer,
};
use crate::{
    AnyWindowHandle, Bounds, Capslock, DispatchEventResult, Edges, ForegroundExecutor, GpuSpecs,
    KeyDownEvent, KeyUpEvent, Keystroke, MenuItem, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent, PinchEvent, Pixels,
    PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow, Point,
    PromptButton, PromptLevel, RequestFrameOptions, ScrollDelta, ScrollWheelEvent, Size,
    TouchPhase, WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowKind, WindowParams, appearance_from_style, point, px,
};
use block::ConcreteBlock;
use ctor::ctor;
use futures::channel::oneshot;
use objc::{
    class,
    declare::ClassDecl,
    msg_send,
    runtime::{BOOL, Class, NO, Object, Protocol, Sel, YES},
    sel, sel_impl,
};
use parking_lot::Mutex;
use raw_window_handle as rwh;
use std::{
    collections::VecDeque,
    ffi::c_void,
    ops::Range,
    ptr::{self, NonNull},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

const WINDOW_STATE_IVAR: &str = "windowState";
const INPUT_DELEGATE_IVAR: &str = "gpuiInputDelegate";
const TOKENIZER_IVAR: &str = "gpuiTokenizer";
const OFFSET_IVAR: &str = "gpuiOffset";
const START_IVAR: &str = "gpuiStart";
const END_IVAR: &str = "gpuiEnd";

static mut WINDOW_CLASS: *const Class = ptr::null();
static mut VIEW_CLASS: *const Class = ptr::null();
static mut TEXT_POSITION_CLASS: *const Class = ptr::null();
static mut TEXT_RANGE_CLASS: *const Class = ptr::null();

/// `UIWindowLevel`s.
const UI_WINDOW_LEVEL_NORMAL: f64 = 0.0;
const UI_WINDOW_LEVEL_FLOATING: f64 = 1.0;
const UI_WINDOW_LEVEL_POPUP: f64 = 1000.0;

/// `UITouchType`.
const UI_TOUCH_TYPE_PENCIL: isize = 2;
const UI_TOUCH_TYPE_INDIRECT_POINTER: isize = 3;
/// `UIEventButtonMaskSecondary`.
const UI_EVENT_BUTTON_MASK_SECONDARY: usize = 1 << 1;

/// `UIGestureRecognizerState`.
const GESTURE_BEGAN: isize = 1;
const GESTURE_CHANGED: isize = 2;
const GESTURE_ENDED: isize = 3;
const GESTURE_CANCELLED: isize = 4;

/// `UITextLayoutDirection`.
const TEXT_DIRECTION_RIGHT: isize = 0;
const TEXT_DIRECTION_LEFT: isize = 1;
const TEXT_DIRECTION_UP: isize = 2;
const TEXT_DIRECTION_DOWN: isize = 3;

/// How far a finger may travel before a press turns into a scroll.
const TOUCH_SLOP: f32 = 8.0;
/// Taps closer in time and space than this count as a multi-click.
const MULTI_TAP_INTERVAL: Duration = Duration::from_millis(350);
const MULTI_TAP_DISTANCE: f32 = 24.0;
/// Deceleration of a flung scroll, per millisecond (UIScrollView's normal
/// rate), and the speed below which momentum stops.
const MOMENTUM_DECELERATION: f32 = 0.998;
const MOMENTUM_MIN_VELOCITY: f32 = 20.0;
/// A release this far outside the window cancels a press without a click.
const CANCEL_POSITION: Point<Pixels> = Point {
    x: Pixels(-100_000.0),
    y: Pixels(-100_000.0),
};

#[ctor]
unsafe fn build_classes() {
    unsafe {
        WINDOW_CLASS = {
            let mut decl = ClassDecl::new("GPUIWindow", class!(UIWindow)).unwrap();
            decl.add_ivar::<*mut c_void>(WINDOW_STATE_IVAR);
            decl.add_method(sel!(dealloc), dealloc_window as extern "C" fn(&Object, Sel));
            decl.add_method(
                sel!(becomeKeyWindow),
                become_key_window as extern "C" fn(&Object, Sel),
            );
            decl.add_method(
                sel!(resignKeyWindow),
                resign_key_window as extern "C" fn(&Object, Sel),
            );
            // With no text field focused nothing is first responder, and
            // hardware keys land on the window.
            decl.add_method(
                sel!(pressesBegan:withEvent:),
                window_presses_began as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(pressesEnded:withEvent:),
                window_presses_ended as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(pressesCancelled:withEvent:),
                window_presses_ended as extern "C" fn(&Object, Sel, id, id),
            );
            // UIKit turns cmd-z/x/c/v/a and cmd-shift-z into these before
            // any press is delivered, so they must be caught here to reach
            // gpui at all.
            decl.add_method(
                sel!(canPerformAction:withSender:),
                can_perform_action as extern "C" fn(&Object, Sel, Sel, id) -> BOOL,
            );
            for selector in standard_edit_selectors() {
                decl.add_method(
                    selector,
                    perform_edit_action as extern "C" fn(&Object, Sel, id),
                );
            }
            decl.add_method(
                sel!(keyCommands),
                reserved_key_commands as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(handleGPUIReservedShortcut:),
                handle_reserved_shortcut as extern "C" fn(&Object, Sel, id),
            );
            decl.register()
        };

        VIEW_CLASS = {
            let mut decl = ClassDecl::new("GPUIView", class!(UIView)).unwrap();
            decl.add_ivar::<*mut c_void>(WINDOW_STATE_IVAR);
            decl.add_ivar::<id>(INPUT_DELEGATE_IVAR);
            decl.add_ivar::<id>(TOKENIZER_IVAR);
            decl.add_method(sel!(dealloc), dealloc_view as extern "C" fn(&Object, Sel));

            decl.add_method(
                sel!(layoutSubviews),
                layout_subviews as extern "C" fn(&Object, Sel),
            );
            decl.add_method(
                sel!(safeAreaInsetsDidChange),
                safe_area_insets_did_change as extern "C" fn(&Object, Sel),
            );
            decl.add_method(
                sel!(traitCollectionDidChange:),
                trait_collection_did_change as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(displayLinkStep:),
                display_link_step as extern "C" fn(&Object, Sel, id),
            );

            decl.add_method(
                sel!(touchesBegan:withEvent:),
                touches_began as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(touchesMoved:withEvent:),
                touches_moved as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(touchesEnded:withEvent:),
                touches_ended as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(touchesCancelled:withEvent:),
                touches_cancelled as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(handlePinch:),
                handle_pinch as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(handleLongPress:),
                handle_long_press as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(handlePointerScroll:),
                handle_pointer_scroll as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(handleTwoFingerPan:),
                handle_two_finger_pan as extern "C" fn(&Object, Sel, id),
            );
            // UIGestureRecognizerDelegate: pinch and two-finger pan are the
            // same two fingers and must both fire.
            decl.add_method(
                sel!(gestureRecognizer:shouldRecognizeSimultaneouslyWithGestureRecognizer:),
                recognize_simultaneously as extern "C" fn(&Object, Sel, id, id) -> BOOL,
            );
            decl.add_method(
                sel!(handleHover:),
                handle_hover as extern "C" fn(&Object, Sel, id),
            );

            decl.add_method(
                sel!(pressesBegan:withEvent:),
                presses_began as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(pressesEnded:withEvent:),
                presses_ended as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(pressesCancelled:withEvent:),
                presses_ended as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(canBecomeFirstResponder),
                yes as extern "C" fn(&Object, Sel) -> BOOL,
            );

            decl.add_method(
                sel!(keyboardWillChangeFrame:),
                keyboard_will_change_frame as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(applicationDidEnterBackground:),
                application_did_enter_background as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(applicationWillEnterForeground:),
                application_will_enter_foreground as extern "C" fn(&Object, Sel, id),
            );

            // Standard edit actions: the system edit menu's buttons, and
            // the keyboard shortcuts UIKit reserves for them.
            decl.add_method(
                sel!(canPerformAction:withSender:),
                can_perform_action as extern "C" fn(&Object, Sel, Sel, id) -> BOOL,
            );
            for selector in standard_edit_selectors() {
                decl.add_method(
                    selector,
                    perform_edit_action as extern "C" fn(&Object, Sel, id),
                );
            }
            decl.add_method(
                sel!(keyCommands),
                reserved_key_commands as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(handleGPUIReservedShortcut:),
                handle_reserved_shortcut as extern "C" fn(&Object, Sel, id),
            );

            // UIKeyInput
            decl.add_method(sel!(hasText), yes as extern "C" fn(&Object, Sel) -> BOOL);
            decl.add_method(
                sel!(insertText:),
                insert_text as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(deleteBackward),
                delete_backward as extern "C" fn(&Object, Sel),
            );

            // UITextInput
            decl.add_method(
                sel!(textInRange:),
                text_in_range as extern "C" fn(&Object, Sel, id) -> id,
            );
            decl.add_method(
                sel!(replaceRange:withText:),
                replace_range as extern "C" fn(&Object, Sel, id, id),
            );
            decl.add_method(
                sel!(selectedTextRange),
                selected_text_range as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(setSelectedTextRange:),
                set_selected_text_range as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(markedTextRange),
                marked_text_range as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(markedTextStyle),
                marked_text_style as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(setMarkedTextStyle:),
                set_marked_text_style as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(setMarkedText:selectedRange:),
                set_marked_text as extern "C" fn(&Object, Sel, id, NSRange),
            );
            decl.add_method(sel!(unmarkText), unmark_text as extern "C" fn(&Object, Sel));
            decl.add_method(
                sel!(beginningOfDocument),
                beginning_of_document as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(endOfDocument),
                end_of_document as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(textRangeFromPosition:toPosition:),
                text_range_from_position as extern "C" fn(&Object, Sel, id, id) -> id,
            );
            decl.add_method(
                sel!(positionFromPosition:offset:),
                position_from_position as extern "C" fn(&Object, Sel, id, isize) -> id,
            );
            decl.add_method(
                sel!(positionFromPosition:inDirection:offset:),
                position_from_position_in_direction
                    as extern "C" fn(&Object, Sel, id, isize, isize) -> id,
            );
            decl.add_method(
                sel!(comparePosition:toPosition:),
                compare_position as extern "C" fn(&Object, Sel, id, id) -> isize,
            );
            decl.add_method(
                sel!(offsetFromPosition:toPosition:),
                offset_from_position as extern "C" fn(&Object, Sel, id, id) -> isize,
            );
            decl.add_method(
                sel!(inputDelegate),
                input_delegate as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(setInputDelegate:),
                set_input_delegate as extern "C" fn(&Object, Sel, id),
            );
            decl.add_method(
                sel!(tokenizer),
                tokenizer as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(
                sel!(positionWithinRange:farthestInDirection:),
                position_within_range as extern "C" fn(&Object, Sel, id, isize) -> id,
            );
            decl.add_method(
                sel!(characterRangeByExtendingPosition:inDirection:),
                character_range_by_extending as extern "C" fn(&Object, Sel, id, isize) -> id,
            );
            decl.add_method(
                sel!(baseWritingDirectionForPosition:inDirection:),
                base_writing_direction as extern "C" fn(&Object, Sel, id, isize) -> isize,
            );
            decl.add_method(
                sel!(setBaseWritingDirection:forRange:),
                set_base_writing_direction as extern "C" fn(&Object, Sel, isize, id),
            );
            decl.add_method(
                sel!(firstRectForRange:),
                first_rect_for_range as extern "C" fn(&Object, Sel, id) -> NSRect,
            );
            decl.add_method(
                sel!(caretRectForPosition:),
                caret_rect_for_position as extern "C" fn(&Object, Sel, id) -> NSRect,
            );
            decl.add_method(
                sel!(selectionRectsForRange:),
                selection_rects_for_range as extern "C" fn(&Object, Sel, id) -> id,
            );
            decl.add_method(
                sel!(closestPositionToPoint:),
                closest_position_to_point as extern "C" fn(&Object, Sel, NSPoint) -> id,
            );
            decl.add_method(
                sel!(closestPositionToPoint:withinRange:),
                closest_position_to_point_within_range
                    as extern "C" fn(&Object, Sel, NSPoint, id) -> id,
            );
            decl.add_method(
                sel!(characterRangeAtPoint:),
                character_range_at_point as extern "C" fn(&Object, Sel, NSPoint) -> id,
            );
            decl.add_method(
                sel!(textInputView),
                text_input_view as extern "C" fn(&Object, Sel) -> id,
            );
            // UITextInputTraits: no autocorrection by default, gpui text
            // fields are not prose.
            decl.add_method(
                sel!(autocorrectionType),
                autocorrection_type as extern "C" fn(&Object, Sel) -> isize,
            );
            decl.add_method(
                sel!(keyboardAppearance),
                keyboard_appearance as extern "C" fn(&Object, Sel) -> isize,
            );

            if let Some(protocol) = Protocol::get("UITextInput") {
                decl.add_protocol(protocol);
            }
            if let Some(protocol) = Protocol::get("UIGestureRecognizerDelegate") {
                decl.add_protocol(protocol);
            }
            decl.register()
        };

        TEXT_POSITION_CLASS = {
            let mut decl = ClassDecl::new("GPUITextPosition", class!(UITextPosition)).unwrap();
            decl.add_ivar::<isize>(OFFSET_IVAR);
            decl.register()
        };

        TEXT_RANGE_CLASS = {
            let mut decl = ClassDecl::new("GPUITextRange", class!(UITextRange)).unwrap();
            decl.add_ivar::<isize>(START_IVAR);
            decl.add_ivar::<isize>(END_IVAR);
            decl.add_method(
                sel!(start),
                range_start as extern "C" fn(&Object, Sel) -> id,
            );
            decl.add_method(sel!(end), range_end as extern "C" fn(&Object, Sel) -> id);
            decl.add_method(
                sel!(isEmpty),
                range_is_empty as extern "C" fn(&Object, Sel) -> BOOL,
            );
            decl.register()
        };
    }
}

/// What the primary finger has turned into.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum TouchMode {
    /// Pressed, not yet moved far enough to tell a tap from a scroll.
    Undecided,
    /// Scrolling; the pending press has been cancelled.
    Scroll,
    /// Dragging with the button held: pointers, pencils, and fingers that
    /// moved after a long press.
    Drag,
    /// Long-pressed and still; a move starts a drag, a lift does nothing.
    LongPressed,
    /// Taken over by a gesture or the edit menu; ignore until it lifts.
    Cancelled,
}

struct PrimaryTouch {
    touch: id,
    button: MouseButton,
    mode: TouchMode,
    start: Point<Pixels>,
    last: Point<Pixels>,
    /// Recent positions, for the fling velocity.
    samples: VecDeque<(Instant, Point<Pixels>)>,
}

struct Momentum {
    velocity: Point<f32>,
    position: Point<Pixels>,
    modifiers: Modifiers,
    last_tick: Instant,
}

struct IosWindowState {
    handle: AnyWindowHandle,
    executor: ForegroundExecutor,
    native_window: id,
    view_controller: id,
    native_view: NonNull<Object>,
    display_link: id,
    renderer: renderer::Renderer,
    kind: WindowKind,
    request_frame_callback: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    event_callback: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    activate_callback: Option<Box<dyn FnMut(bool)>>,
    hover_callback: Option<Box<dyn FnMut(bool)>>,
    resize_callback: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved_callback: Option<Box<dyn FnMut()>>,
    should_close_callback: Option<Box<dyn FnMut() -> bool>>,
    close_callback: Option<Box<dyn FnOnce()>>,
    appearance_changed_callback: Option<Box<dyn FnMut()>>,
    hit_test_window_control_callback: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
    input_handler: Option<PlatformInputHandler>,
    /// Whether the system keyboard is wanted; mirrors `input_handler` at the
    /// end of each frame and decides whether the view is first responder.
    keyboard_wanted: bool,
    primary_touch: Option<PrimaryTouch>,
    momentum: Option<Momentum>,
    last_tap: Option<(Instant, Point<Pixels>, usize)>,
    mouse_position: Point<Pixels>,
    modifiers: Modifiers,
    capslock: Capslock,
    /// The pinch scale reported at the previous gesture step.
    pinch_last_scale: f32,
    /// How much of the view the software keyboard covers, in points.
    keyboard_overlap: Pixels,
    /// The last `UIPress` seen, by identity and timestamp. UIKit delivers
    /// a press a second time when a priority key command matches it, and
    /// the second copy must not become a second keystroke.
    last_press: (usize, f64),
    last_appearance: WindowAppearance,
    title: String,
    /// Actions of the native context menu currently shown, if any.
    context_menu_actions: Vec<Box<dyn crate::Action>>,
}

unsafe impl Send for IosWindowState {}

impl IosWindowState {
    fn content_size(&self) -> Size<Pixels> {
        unsafe {
            let bounds: NSRect = msg_send![self.native_view.as_ptr(), bounds];
            bounds.into()
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        unsafe {
            let frame: NSRect = msg_send![self.native_window, frame];
            Bounds::new(
                point(px(frame.origin.x as f32), px(frame.origin.y as f32)),
                frame.size.into(),
            )
        }
    }

    fn scale_factor(&self) -> f32 {
        unsafe {
            let screen: id = msg_send![self.native_window, screen];
            if screen.is_null() {
                IosDisplay::scale_factor()
            } else {
                let scale: f64 = msg_send![screen, scale];
                scale as f32
            }
        }
    }

    fn safe_area_insets(&self) -> Edges<Pixels> {
        unsafe {
            let insets: UIEdgeInsets = msg_send![self.native_view.as_ptr(), safeAreaInsets];
            Edges {
                top: px(insets.top as f32),
                right: px(insets.right as f32),
                bottom: px(insets.bottom as f32).max(self.keyboard_overlap),
                left: px(insets.left as f32),
            }
        }
    }

    fn appearance(&self) -> WindowAppearance {
        unsafe {
            let traits: id = msg_send![self.native_view.as_ptr(), traitCollection];
            let style: isize = msg_send![traits, userInterfaceStyle];
            appearance_from_style(style)
        }
    }

    fn update_drawable_size(&mut self) {
        let scale_factor = self.scale_factor();
        let size = self.content_size();
        unsafe {
            let layer = self.renderer.layer_ptr() as id;
            let _: () = msg_send![layer, setContentsScale: scale_factor as f64];
            let bounds: NSRect = msg_send![self.native_view.as_ptr(), bounds];
            let _: () = msg_send![layer, setFrame: bounds];
        }
        self.renderer
            .update_drawable_size(size.to_device_pixels(scale_factor));
    }

    fn set_display_link_paused(&self, paused: bool) {
        if !self.display_link.is_null() {
            unsafe {
                let _: () = msg_send![self.display_link, setPaused: paused.to_objc()];
            }
        }
    }
}

pub(crate) struct IosWindow(Arc<Mutex<IosWindowState>>);

impl IosWindow {
    pub(crate) fn open(
        handle: AnyWindowHandle,
        params: WindowParams,
        executor: ForegroundExecutor,
        renderer_context: renderer::Context,
    ) -> Self {
        unsafe {
            let screen_bounds = IosDisplay.bounds();
            let (frame, level) = match params.kind {
                WindowKind::Normal => (screen_bounds, UI_WINDOW_LEVEL_NORMAL),
                WindowKind::Floating => (screen_bounds, UI_WINDOW_LEVEL_FLOATING),
                // A popup keeps the bounds it asked for, above everything.
                WindowKind::PopUp => (params.bounds, UI_WINDOW_LEVEL_POPUP),
            };
            let frame_rect = NSRect::new(
                NSPoint::new(frame.origin.x.0 as f64, frame.origin.y.0 as f64),
                NSSize::new(frame.size.width.0 as f64, frame.size.height.0 as f64),
            );

            let native_window: id = msg_send![WINDOW_CLASS, alloc];
            let native_window: id = msg_send![native_window, initWithFrame: frame_rect];
            let _: () = msg_send![native_window, setWindowLevel: level];

            let view_controller: id = msg_send![class!(UIViewController), new];
            let native_view: id = msg_send![VIEW_CLASS, alloc];
            let view_bounds = NSRect::new(NSPoint::new(0., 0.), frame_rect.size);
            let native_view: id = msg_send![native_view, initWithFrame: view_bounds];
            let _: () = msg_send![native_view, setMultipleTouchEnabled: YES];
            let _: () = msg_send![native_view, setOpaque: YES];
            let _: () = msg_send![view_controller, setView: native_view];
            let _: () = msg_send![native_window, setRootViewController: view_controller];

            let renderer = renderer::new_renderer(
                renderer_context,
                native_window as *mut _,
                native_view as *mut _,
                frame.size.map(|pixels| pixels.0),
                false,
            );
            let view_layer: id = msg_send![native_view, layer];
            let _: () = msg_send![view_layer, addSublayer: renderer.layer_ptr() as id];

            let window = Self(Arc::new(Mutex::new(IosWindowState {
                handle,
                executor,
                native_window,
                view_controller,
                native_view: NonNull::new_unchecked(native_view),
                display_link: nil,
                renderer,
                kind: params.kind,
                request_frame_callback: None,
                event_callback: None,
                activate_callback: None,
                hover_callback: None,
                resize_callback: None,
                moved_callback: None,
                should_close_callback: None,
                close_callback: None,
                appearance_changed_callback: None,
                hit_test_window_control_callback: None,
                input_handler: None,
                keyboard_wanted: false,
                primary_touch: None,
                momentum: None,
                last_tap: None,
                mouse_position: Point::default(),
                modifiers: Modifiers::default(),
                capslock: Capslock::default(),
                pinch_last_scale: 1.0,
                keyboard_overlap: px(0.),
                last_press: (0, 0.0),
                last_appearance: WindowAppearance::Light,
                title: String::new(),
                context_menu_actions: Vec::new(),
            })));

            (*native_window).set_ivar(
                WINDOW_STATE_IVAR,
                Arc::into_raw(window.0.clone()) as *const c_void,
            );
            (*native_view).set_ivar(
                WINDOW_STATE_IVAR,
                Arc::into_raw(window.0.clone()) as *const c_void,
            );

            {
                let mut lock = window.0.lock();
                lock.last_appearance = lock.appearance();
                lock.update_drawable_size();
            }

            add_gesture_recognizers(native_view);
            observe_notifications(native_view);

            let display_link: id = msg_send![
                class!(CADisplayLink),
                displayLinkWithTarget: native_view
                selector: sel!(displayLinkStep:)
            ];
            let run_loop: id = msg_send![class!(NSRunLoop), mainRunLoop];
            let _: () =
                msg_send![display_link, addToRunLoop: run_loop forMode: NSRunLoopCommonModes];
            let _: () = msg_send![display_link, retain];
            window.0.lock().display_link = display_link;

            if let Some(title) = params
                .titlebar
                .as_ref()
                .and_then(|titlebar| titlebar.title.as_ref())
            {
                window.0.lock().title = title.to_string();
            }

            window
        }
    }

    pub(super) fn native_window(&self) -> id {
        self.0.lock().native_window
    }

    pub(super) fn active_window() -> Option<AnyWindowHandle> {
        Self::all_windows()
            .into_iter()
            .find(|(window, _)| unsafe {
                let is_key: BOOL = msg_send![*window, isKeyWindow];
                is_key == YES
            })
            .map(|(_, handle)| handle)
    }

    pub(super) fn ordered_windows() -> Vec<AnyWindowHandle> {
        let mut windows = Self::all_windows();
        // Front to back, as the macOS `orderedWindows` reports them.
        windows.reverse();
        windows.into_iter().map(|(_, handle)| handle).collect()
    }

    /// Every gpui window in every connected scene, back to front.
    fn all_windows() -> Vec<(id, AnyWindowHandle)> {
        let mut result = Vec::new();
        unsafe {
            let app: id = msg_send![class!(UIApplication), sharedApplication];
            let scenes: id = msg_send![app, connectedScenes];
            let scenes: id = msg_send![scenes, allObjects];
            let scene_count: usize = msg_send![scenes, count];
            for i in 0..scene_count {
                let scene: id = msg_send![scenes, objectAtIndex: i];
                let responds: BOOL = msg_send![scene, respondsToSelector: sel!(windows)];
                if responds == NO {
                    continue;
                }
                let windows: id = msg_send![scene, windows];
                let count: usize = msg_send![windows, count];
                for j in 0..count {
                    let window: id = msg_send![windows, objectAtIndex: j];
                    let is_ours: BOOL = msg_send![window, isKindOfClass: WINDOW_CLASS];
                    if is_ours == YES {
                        let handle = get_window_state(&*window).lock().handle;
                        result.push((window, handle));
                    }
                }
            }
        }
        result
    }
}

impl Drop for IosWindow {
    fn drop(&mut self) {
        let mut this = self.0.lock();
        this.renderer.destroy();
        let window = this.native_window;
        let view = this.native_view.as_ptr();
        let display_link = this.display_link;
        this.display_link = nil;
        this.input_handler.take();
        this.executor
            .spawn(async move {
                unsafe {
                    if !display_link.is_null() {
                        let _: () = msg_send![display_link, invalidate];
                        let _: () = msg_send![display_link, release];
                    }
                    let center: id = msg_send![class!(NSNotificationCenter), defaultCenter];
                    let _: () = msg_send![center, removeObserver: view];
                    let _: () = msg_send![window, setHidden: YES];
                    let _: () = msg_send![window, setRootViewController: nil];
                    let _: () = msg_send![window, release];
                }
            })
            .detach();
    }
}

impl PlatformWindow for IosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.lock().bounds()
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Windowed(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.0.lock().content_size()
    }

    fn resize(&mut self, size: Size<Pixels>) {
        // Only popups own their frame; everything else fills the scene.
        let lock = self.0.lock();
        if lock.kind != WindowKind::PopUp {
            return;
        }
        unsafe {
            let mut frame: NSRect = msg_send![lock.native_window, frame];
            frame.size = NSSize::new(size.width.0 as f64, size.height.0 as f64);
            let _: () = msg_send![lock.native_window, setFrame: frame];
        }
    }

    fn scale_factor(&self) -> f32 {
        self.0.lock().scale_factor()
    }

    fn appearance(&self) -> WindowAppearance {
        self.0.lock().appearance()
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.lock().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.0.lock().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.0.lock().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.lock().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.lock().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        msg: &str,
        detail: Option<&str>,
        answers: &[PromptButton],
    ) -> Option<oneshot::Receiver<usize>> {
        let (done_tx, done_rx) = oneshot::channel();
        let done_tx = Rc::new(std::cell::RefCell::new(Some(done_tx)));
        let lock = self.0.lock();
        unsafe {
            let alert: id = msg_send![
                class!(UIAlertController),
                alertControllerWithTitle: ns_string(msg)
                message: detail.map_or(nil, |detail| ns_string(detail))
                preferredStyle: 1isize
            ];
            for (index, answer) in answers.iter().enumerate() {
                let style: isize = if answer.is_cancel() { 1 } else { 0 };
                let done_tx = done_tx.clone();
                let handler = ConcreteBlock::new(move |_action: id| {
                    if let Some(tx) = done_tx.borrow_mut().take() {
                        tx.send(index).ok();
                    }
                });
                let handler = handler.copy();
                let action: id = msg_send![
                    class!(UIAlertAction),
                    actionWithTitle: ns_string(answer.label())
                    style: style
                    handler: &*handler
                ];
                let _: () = msg_send![alert, addAction: action];
            }
            let controller = topmost(lock.view_controller);
            let _: () =
                msg_send![controller, presentViewController: alert animated: YES completion: nil];
        }
        Some(done_rx)
    }

    fn activate(&self) {
        let lock = self.0.lock();
        unsafe {
            let _: () = msg_send![lock.native_window, makeKeyAndVisible];
        }
    }

    fn is_active(&self) -> bool {
        let lock = self.0.lock();
        unsafe {
            let is_key: BOOL = msg_send![lock.native_window, isKeyWindow];
            is_key == YES
        }
    }

    fn is_hovered(&self) -> bool {
        self.is_active()
    }

    fn set_title(&mut self, title: &str) {
        let mut lock = self.0.lock();
        lock.title = title.to_string();
        unsafe {
            // Shown by the iPad app switcher and Stage Manager.
            let scene: id = msg_send![lock.native_window, windowScene];
            if !scene.is_null() {
                let _: () = msg_send![scene, setTitle: ns_string(title)];
            }
        }
    }

    fn get_title(&self) -> String {
        self.0.lock().title.clone()
    }

    fn set_background_appearance(&self, background_appearance: WindowBackgroundAppearance) {
        let lock = self.0.lock();
        let opaque = matches!(background_appearance, WindowBackgroundAppearance::Opaque);
        unsafe {
            let color: id = if opaque {
                msg_send![class!(UIColor), blackColor]
            } else {
                msg_send![class!(UIColor), clearColor]
            };
            let _: () = msg_send![lock.native_window, setBackgroundColor: color];
            let _: () = msg_send![lock.native_view.as_ptr(), setOpaque: opaque.to_objc()];
            let _: () = msg_send![lock.renderer.layer_ptr() as id, setOpaque: opaque.to_objc()];
        }
    }

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        false
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.lock().request_frame_callback = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.lock().event_callback = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.lock().activate_callback = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.lock().hover_callback = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.lock().resize_callback = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().moved_callback = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.lock().should_close_callback = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.0.lock().hit_test_window_control_callback = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.lock().close_callback = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.lock().appearance_changed_callback = Some(callback);
    }

    fn draw(&self, scene: &crate::Scene) {
        let mut lock = self.0.lock();
        lock.renderer.draw(scene);

        // The frame is the one moment the input handler reliably reflects
        // focus: it is present exactly when a text element has it. The view
        // is first responder (which is what shows the keyboard) exactly
        // then; the change is made on the next turn, off this stack,
        // because UIKit reads the input handler back when focus moves.
        let wants_keyboard = lock.input_handler.is_some();
        if wants_keyboard != lock.keyboard_wanted {
            lock.keyboard_wanted = wants_keyboard;
            let view = lock.native_view.as_ptr();
            let executor = lock.executor.clone();
            drop(lock);
            executor
                .spawn(async move {
                    unsafe {
                        let _: BOOL = if wants_keyboard {
                            msg_send![view, becomeFirstResponder]
                        } else {
                            msg_send![view, resignFirstResponder]
                        };
                    }
                })
                .detach();
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.0.lock().renderer.sprite_atlas().clone()
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        None
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // UIKit asks for caret and selection rects itself.
    }

    fn safe_area_insets(&self) -> Edges<Pixels> {
        self.0.lock().safe_area_insets()
    }

    fn claim_touch_drag(&self) {
        claim_touch_drag(&self.0);
    }

    fn show_context_menu(&self, position: Point<Pixels>, items: Vec<MenuItem>) -> bool {
        let mut actions: Vec<Box<dyn crate::Action>> = Vec::new();
        let mut entries: Vec<(String, usize)> = Vec::new();
        flatten_context_menu(items, "", &mut actions, &mut entries);
        if entries.is_empty() {
            return false;
        }

        let mut lock = self.0.lock();
        lock.context_menu_actions = actions;
        let view = lock.native_view.as_ptr();
        let controller = unsafe { topmost(lock.view_controller) };
        let state = self.0.clone();
        drop(lock);

        unsafe {
            let sheet: id = msg_send![
                class!(UIAlertController),
                alertControllerWithTitle: nil
                message: nil
                preferredStyle: 0isize
            ];
            for (title, index) in entries {
                let state = state.clone();
                let handler = ConcreteBlock::new(move |_action: id| {
                    let action = state
                        .lock()
                        .context_menu_actions
                        .get(index)
                        .map(|action| action.boxed_clone());
                    if let (Some(action), Some(platform)) = (action, platform()) {
                        platform.dispatch_menu_action(action.as_ref());
                    }
                });
                let handler = handler.copy();
                let action: id = msg_send![
                    class!(UIAlertAction),
                    actionWithTitle: ns_string(&title)
                    style: 0isize
                    handler: &*handler
                ];
                let _: () = msg_send![sheet, addAction: action];
            }
            let cancel: id = msg_send![
                class!(UIAlertAction),
                actionWithTitle: ns_string("Cancel")
                style: 1isize
                handler: nil
            ];
            let _: () = msg_send![sheet, addAction: cancel];

            // On iPad an action sheet is a popover and needs an anchor.
            let popover: id = msg_send![sheet, popoverPresentationController];
            if !popover.is_null() {
                let _: () = msg_send![popover, setSourceView: view];
                let anchor = NSRect::new(
                    NSPoint::new(position.x.0 as f64, position.y.0 as f64),
                    NSSize::new(1., 1.),
                );
                let _: () = msg_send![popover, setSourceRect: anchor];
            }
            let _: () =
                msg_send![controller, presentViewController: sheet animated: YES completion: nil];
        }
        true
    }
}

impl rwh::HasWindowHandle for IosWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let lock = self.0.lock();
        let view = NonNull::new(lock.native_view.as_ptr() as *mut c_void).unwrap();
        let mut handle = rwh::UiKitWindowHandle::new(view);
        handle.ui_view_controller = NonNull::new(lock.view_controller as *mut c_void);
        unsafe { Ok(rwh::WindowHandle::borrow_raw(handle.into())) }
    }
}

impl rwh::HasDisplayHandle for IosWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        unsafe {
            Ok(rwh::DisplayHandle::borrow_raw(
                rwh::UiKitDisplayHandle::new().into(),
            ))
        }
    }
}

/// Flattens a menu tree into sheet rows: submenus become "Menu ▸ Item".
fn flatten_context_menu(
    items: Vec<MenuItem>,
    prefix: &str,
    actions: &mut Vec<Box<dyn crate::Action>>,
    entries: &mut Vec<(String, usize)>,
) {
    for item in items {
        match item {
            MenuItem::Action { name, action, .. } => {
                entries.push((format!("{prefix}{name}"), actions.len()));
                actions.push(action);
            }
            MenuItem::Submenu(menu) => {
                let prefix = format!("{prefix}{} ▸ ", menu.name);
                flatten_context_menu(menu.items, &prefix, actions, entries);
            }
            MenuItem::Separator | MenuItem::SystemMenu(_) => {}
        }
    }
}

unsafe fn topmost(mut controller: id) -> id {
    unsafe {
        loop {
            let presented: id = msg_send![controller, presentedViewController];
            if presented.is_null() {
                return controller;
            }
            controller = presented;
        }
    }
}

unsafe fn add_gesture_recognizers(view: id) {
    unsafe {
        let pinch: id = msg_send![class!(UIPinchGestureRecognizer), alloc];
        let pinch: id = msg_send![pinch, initWithTarget: view action: sel!(handlePinch:)];
        let _: () = msg_send![pinch, setCancelsTouchesInView: NO];
        let _: () = msg_send![pinch, setDelaysTouchesBegan: NO];
        let _: () = msg_send![view, addGestureRecognizer: pinch];
        let _: () = msg_send![pinch, release];

        let long_press: id = msg_send![class!(UILongPressGestureRecognizer), alloc];
        let long_press: id =
            msg_send![long_press, initWithTarget: view action: sel!(handleLongPress:)];
        let _: () = msg_send![long_press, setMinimumPressDuration: 0.5f64];
        let _: () = msg_send![long_press, setAllowableMovement: TOUCH_SLOP as f64];
        let _: () = msg_send![long_press, setCancelsTouchesInView: NO];
        let _: () = msg_send![long_press, setDelaysTouchesBegan: NO];
        let _: () = msg_send![view, addGestureRecognizer: long_press];
        let _: () = msg_send![long_press, release];

        // iPad pointer: two-finger scroll and mouse wheels arrive as scroll
        // events, which only a pan recognizer opted into them receives.
        let indirect_pointer: id =
            msg_send![class!(NSNumber), numberWithInteger: UI_TOUCH_TYPE_INDIRECT_POINTER];
        let pointer_types: id = msg_send![class!(NSArray), arrayWithObject: indirect_pointer];
        let pan: id = msg_send![class!(UIPanGestureRecognizer), alloc];
        let pan: id = msg_send![pan, initWithTarget: view action: sel!(handlePointerScroll:)];
        let _: () = msg_send![pan, setAllowedScrollTypesMask: 3usize];
        let _: () = msg_send![pan, setAllowedTouchTypes: pointer_types];
        let _: () = msg_send![pan, setCancelsTouchesInView: NO];
        let _: () = msg_send![view, addGestureRecognizer: pan];
        let _: () = msg_send![pan, release];

        // Two fingers dragging together scroll, whatever one finger does
        // over the same element (a canvas that claims single-finger drags
        // for painting still pans with two). Recognised alongside the
        // pinch, which is the same two fingers.
        let two_finger_pan: id = msg_send![class!(UIPanGestureRecognizer), alloc];
        let two_finger_pan: id =
            msg_send![two_finger_pan, initWithTarget: view action: sel!(handleTwoFingerPan:)];
        let _: () = msg_send![two_finger_pan, setMinimumNumberOfTouches: 2usize];
        let _: () = msg_send![two_finger_pan, setMaximumNumberOfTouches: 2usize];
        let _: () = msg_send![two_finger_pan, setCancelsTouchesInView: NO];
        let _: () = msg_send![two_finger_pan, setDelegate: view];
        let _: () = msg_send![pinch, setDelegate: view];
        let _: () = msg_send![view, addGestureRecognizer: two_finger_pan];
        let _: () = msg_send![two_finger_pan, release];

        if let Some(hover_class) = Class::get("UIHoverGestureRecognizer") {
            let hover: id = msg_send![hover_class, alloc];
            let hover: id = msg_send![hover, initWithTarget: view action: sel!(handleHover:)];
            let _: () = msg_send![view, addGestureRecognizer: hover];
            let _: () = msg_send![hover, release];
        }

        if let Some(edit_menu_class) = Class::get("UIEditMenuInteraction") {
            let interaction: id = msg_send![edit_menu_class, alloc];
            let interaction: id = msg_send![interaction, initWithDelegate: nil];
            let _: () = msg_send![view, addInteraction: interaction];
            let _: () = msg_send![interaction, release];
        }
    }
}

unsafe fn observe_notifications(view: id) {
    unsafe {
        let center: id = msg_send![class!(NSNotificationCenter), defaultCenter];
        for (name, selector) in [
            (
                UIKeyboardWillChangeFrameNotification,
                sel!(keyboardWillChangeFrame:),
            ),
            (
                UIApplicationDidEnterBackgroundNotification,
                sel!(applicationDidEnterBackground:),
            ),
            (
                UIApplicationWillEnterForegroundNotification,
                sel!(applicationWillEnterForeground:),
            ),
        ] {
            let _: () =
                msg_send![center, addObserver: view selector: selector name: name object: nil];
        }
    }
}

unsafe fn get_window_state(object: &Object) -> Arc<Mutex<IosWindowState>> {
    unsafe {
        let raw: *mut c_void = *object.get_ivar(WINDOW_STATE_IVAR);
        let rc1 = Arc::from_raw(raw as *mut Mutex<IosWindowState>);
        let rc2 = rc1.clone();
        std::mem::forget(rc1);
        rc2
    }
}

unsafe fn drop_window_state(object: &Object) {
    unsafe {
        let raw: *mut c_void = *object.get_ivar(WINDOW_STATE_IVAR);
        if !raw.is_null() {
            Arc::from_raw(raw as *mut Mutex<IosWindowState>);
        }
    }
}

extern "C" fn yes(_: &Object, _: Sel) -> BOOL {
    YES
}

extern "C" fn dealloc_window(this: &Object, _: Sel) {
    unsafe {
        drop_window_state(this);
        let _: () = msg_send![super(this, class!(UIWindow)), dealloc];
    }
}

extern "C" fn dealloc_view(this: &Object, _: Sel) {
    unsafe {
        drop_window_state(this);
        let tokenizer: id = *this.get_ivar(TOKENIZER_IVAR);
        if !tokenizer.is_null() {
            let _: () = msg_send![tokenizer, release];
        }
        let _: () = msg_send![super(this, class!(UIView)), dealloc];
    }
}

/// Runs the window's input callback with `event`; `true` when handled.
fn send_event(state: &Mutex<IosWindowState>, event: PlatformInput) -> bool {
    let callback = state.lock().event_callback.take();
    if let Some(mut callback) = callback {
        let result = callback(event);
        state.lock().event_callback = Some(callback);
        !result.propagate
    } else {
        false
    }
}

fn with_input_handler<F, R>(this: &Object, f: F) -> Option<R>
where
    F: FnOnce(&mut PlatformInputHandler) -> R,
{
    let state = unsafe { get_window_state(this) };
    let mut lock = state.lock();
    if let Some(mut input_handler) = lock.input_handler.take() {
        drop(lock);
        let result = f(&mut input_handler);
        state.lock().input_handler = Some(input_handler);
        Some(result)
    } else {
        None
    }
}

fn notify_resize(state: &Arc<Mutex<IosWindowState>>) {
    let mut lock = state.lock();
    if let Some(mut callback) = lock.resize_callback.take() {
        let content_size = lock.content_size();
        let scale_factor = lock.scale_factor();
        drop(lock);
        callback(content_size, scale_factor);
        state.lock().resize_callback = Some(callback);
    }
}

/// `UIWindow` key status.
extern "C" fn become_key_window(this: &Object, _: Sel) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let _: () = msg_send![super(this, class!(UIWindow)), becomeKeyWindow];
    }
    let mut lock = state.lock();
    if let Some(mut callback) = lock.activate_callback.take() {
        drop(lock);
        callback(true);
        state.lock().activate_callback = Some(callback);
    }
}

extern "C" fn resign_key_window(this: &Object, _: Sel) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let _: () = msg_send![super(this, class!(UIWindow)), resignKeyWindow];
    }
    let mut lock = state.lock();
    if let Some(mut callback) = lock.activate_callback.take() {
        drop(lock);
        callback(false);
        state.lock().activate_callback = Some(callback);
    }
}

/// `UIView` layout and traits.
extern "C" fn layout_subviews(this: &Object, _: Sel) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let _: () = msg_send![super(this, class!(UIView)), layoutSubviews];
    }
    {
        let mut lock = state.lock();
        lock.update_drawable_size();
    }
    notify_resize(&state);
}

extern "C" fn safe_area_insets_did_change(this: &Object, _: Sel) {
    let state = unsafe { get_window_state(this) };
    notify_resize(&state);
}

extern "C" fn trait_collection_did_change(this: &Object, _: Sel, previous: id) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let _: () = msg_send![super(this, class!(UIView)), traitCollectionDidChange: previous];
    }
    let mut lock = state.lock();
    let appearance = lock.appearance();
    if appearance != lock.last_appearance {
        lock.last_appearance = appearance;
        if let Some(mut callback) = lock.appearance_changed_callback.take() {
            drop(lock);
            callback();
            state.lock().appearance_changed_callback = Some(callback);
        }
    }
}

extern "C" fn display_link_step(this: &Object, _: Sel, _link: id) {
    let state = unsafe { get_window_state(this) };
    tick_momentum(&state);
    let mut lock = state.lock();
    if let Some(mut callback) = lock.request_frame_callback.take() {
        drop(lock);
        callback(Default::default());
        state.lock().request_frame_callback = Some(callback);
    }
}

extern "C" fn application_did_enter_background(this: &Object, _: Sel, _: id) {
    // Metal work submitted from the background terminates the app.
    let state = unsafe { get_window_state(this) };
    state.lock().set_display_link_paused(true);
}

extern "C" fn application_will_enter_foreground(this: &Object, _: Sel, _: id) {
    let state = unsafe { get_window_state(this) };
    state.lock().set_display_link_paused(false);
}

extern "C" fn keyboard_will_change_frame(this: &Object, _: Sel, notification: id) {
    let state = unsafe { get_window_state(this) };
    let overlap = unsafe {
        let info: id = msg_send![notification, userInfo];
        let value: id = msg_send![info, objectForKey: UIKeyboardFrameEndUserInfoKey];
        if value.is_null() {
            return;
        }
        let keyboard_frame: NSRect = msg_send![value, CGRectValue];
        // The keyboard frame is in screen coordinates.
        let window: id = msg_send![this, window];
        let screen: id = if window.is_null() {
            msg_send![class!(UIScreen), mainScreen]
        } else {
            msg_send![window, screen]
        };
        let space: id = msg_send![screen, coordinateSpace];
        let local: NSRect = msg_send![this, convertRect: keyboard_frame fromCoordinateSpace: space];
        let bounds: NSRect = msg_send![this, bounds];
        let bottom = bounds.origin.y + bounds.size.height;
        px((bottom - local.origin.y).clamp(0.0, bounds.size.height) as f32)
    };
    {
        let mut lock = state.lock();
        if lock.keyboard_overlap == overlap {
            return;
        }
        lock.keyboard_overlap = overlap;
    }
    notify_resize(&state);
}

/// Touches.
unsafe fn touch_position(this: &Object, touch: id) -> Point<Pixels> {
    unsafe {
        let location: NSPoint = msg_send![touch, locationInView: this];
        location.into()
    }
}

unsafe fn touch_pressure(touch: id) -> f32 {
    unsafe {
        let kind: isize = msg_send![touch, type];
        if kind != UI_TOUCH_TYPE_PENCIL {
            return 1.0;
        }
        let force: f64 = msg_send![touch, force];
        let max: f64 = msg_send![touch, maximumPossibleForce];
        if max > 0.0 && force > 0.0 {
            (force / max).clamp(0.0, 1.0) as f32
        } else {
            1.0
        }
    }
}

fn update_modifiers(state: &Arc<Mutex<IosWindowState>>, flags: usize) {
    let modifiers = modifiers_from_flags(flags);
    let capslock = capslock_from_flags(flags);
    let mut lock = state.lock();
    if lock.modifiers == modifiers && lock.capslock == capslock {
        return;
    }
    lock.modifiers = modifiers;
    lock.capslock = capslock;
    drop(lock);
    send_event(
        state,
        PlatformInput::ModifiersChanged(ModifiersChangedEvent {
            modifiers,
            capslock,
        }),
    );
}

extern "C" fn touches_began(this: &Object, _: Sel, touches: id, event: id) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        update_modifiers(&state, event_modifier_flags(event));
        let touches: id = msg_send![touches, allObjects];
        let count: usize = msg_send![touches, count];
        for i in 0..count {
            let touch: id = msg_send![touches, objectAtIndex: i];
            let mut lock = state.lock();
            if lock.primary_touch.is_some() {
                // A second finger: the pinch recognizer deals with it.
                continue;
            }
            lock.momentum = None;
            let position = touch_position(this, touch);
            let kind: isize = msg_send![touch, type];
            let is_pointer = kind == UI_TOUCH_TYPE_INDIRECT_POINTER;
            let button_mask: usize = if event.is_null() {
                0
            } else {
                msg_send![event, buttonMask]
            };
            let button = if is_pointer && button_mask & UI_EVENT_BUTTON_MASK_SECONDARY != 0 {
                MouseButton::Right
            } else {
                MouseButton::Left
            };
            let mode = if is_pointer || kind == UI_TOUCH_TYPE_PENCIL {
                TouchMode::Drag
            } else {
                TouchMode::Undecided
            };

            let now = Instant::now();
            let click_count = match lock.last_tap {
                Some((at, at_position, count))
                    if now.duration_since(at) < MULTI_TAP_INTERVAL
                        && distance(at_position, position) < MULTI_TAP_DISTANCE =>
                {
                    count + 1
                }
                _ => 1,
            };
            lock.last_tap = Some((now, position, click_count));
            lock.mouse_position = position;
            let modifiers = lock.modifiers;
            let mut samples = VecDeque::new();
            samples.push_back((now, position));
            lock.primary_touch = Some(PrimaryTouch {
                touch,
                button,
                mode,
                start: position,
                last: position,
                samples,
            });
            drop(lock);

            send_event(
                &state,
                PlatformInput::MouseDown(MouseDownEvent {
                    button,
                    position,
                    modifiers,
                    click_count,
                    first_mouse: false,
                    pressure: touch_pressure(touch),
                }),
            );
        }
    }
}

fn distance(a: Point<Pixels>, b: Point<Pixels>) -> f32 {
    let dx = (a.x - b.x).0;
    let dy = (a.y - b.y).0;
    (dx * dx + dy * dy).sqrt()
}

/// Releases a press that is not going to be a click. A press still
/// undecided is released far outside the window, so nothing under it
/// sees a click (as browsers cancel a pointer when a scroll takes it); a
/// drag in progress is released where the finger last was, which is what
/// a mouse would report and what a drag handler expects.
fn cancel_press(state: &Arc<Mutex<IosWindowState>>, button: MouseButton, mode: TouchMode) {
    let lock = state.lock();
    let modifiers = lock.modifiers;
    let position = match mode {
        TouchMode::Drag => lock.mouse_position,
        _ => CANCEL_POSITION,
    };
    drop(lock);
    send_event(
        state,
        PlatformInput::MouseUp(MouseUpEvent {
            button,
            position,
            modifiers,
            click_count: 1,
            pressure: 1.0,
        }),
    );
}

/// Turns the press being dispatched into a drag, so the finger will not
/// scroll. Only a press that has not moved yet can still be claimed.
fn claim_touch_drag(state: &Arc<Mutex<IosWindowState>>) {
    let mut lock = state.lock();
    if let Some(primary) = lock.primary_touch.as_mut() {
        if primary.mode == TouchMode::Undecided {
            primary.mode = TouchMode::Drag;
        }
    }
}

/// Cancels the primary touch's press when a two-finger gesture takes
/// over; returns without sending anything if there is none to cancel.
fn cancel_primary_for_gesture(state: &Arc<Mutex<IosWindowState>>) {
    let mut lock = state.lock();
    lock.momentum = None;
    let cancel = lock.primary_touch.as_mut().and_then(|primary| {
        let button = primary.button;
        let mode = primary.mode;
        let needs_cancel = matches!(
            mode,
            TouchMode::Undecided | TouchMode::Drag | TouchMode::LongPressed
        );
        primary.mode = TouchMode::Cancelled;
        needs_cancel.then_some((button, mode))
    });
    drop(lock);
    if let Some((button, mode)) = cancel {
        cancel_press(state, button, mode);
    }
}

extern "C" fn touches_moved(this: &Object, _: Sel, touches: id, event: id) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        update_modifiers(&state, event_modifier_flags(event));
        let touches: id = msg_send![touches, allObjects];
        let count: usize = msg_send![touches, count];
        for i in 0..count {
            let touch: id = msg_send![touches, objectAtIndex: i];
            let position = touch_position(this, touch);
            let pressure = touch_pressure(touch);
            let mut lock = state.lock();
            let Some((last, start, button, mode)) =
                lock.primary_touch.as_mut().and_then(|primary| {
                    if primary.touch != touch {
                        return None;
                    }
                    let now = Instant::now();
                    primary.samples.push_back((now, position));
                    while primary.samples.len() > 8 {
                        primary.samples.pop_front();
                    }
                    let last = primary.last;
                    primary.last = position;
                    Some((last, primary.start, primary.button, primary.mode))
                })
            else {
                continue;
            };
            lock.mouse_position = position;
            let modifiers = lock.modifiers;
            let set_mode = |lock: &mut IosWindowState, mode: TouchMode| {
                if let Some(primary) = lock.primary_touch.as_mut() {
                    primary.mode = mode;
                }
            };

            match mode {
                TouchMode::Undecided => {
                    if distance(start, position) < TOUCH_SLOP {
                        continue;
                    }
                    set_mode(&mut lock, TouchMode::Scroll);
                    drop(lock);
                    cancel_press(&state, button, TouchMode::Undecided);
                    send_event(
                        &state,
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(position - start),
                            modifiers,
                            touch_phase: TouchPhase::Started,
                        }),
                    );
                }
                TouchMode::Scroll => {
                    drop(lock);
                    send_event(
                        &state,
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(position - last),
                            modifiers,
                            touch_phase: TouchPhase::Moved,
                        }),
                    );
                }
                TouchMode::LongPressed => {
                    if distance(start, position) < TOUCH_SLOP {
                        continue;
                    }
                    set_mode(&mut lock, TouchMode::Drag);
                    drop(lock);
                    send_event(
                        &state,
                        PlatformInput::MouseDown(MouseDownEvent {
                            button,
                            position: start,
                            modifiers,
                            click_count: 1,
                            first_mouse: false,
                            pressure,
                        }),
                    );
                    send_event(
                        &state,
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position,
                            pressed_button: Some(button),
                            modifiers,
                            pressure,
                        }),
                    );
                }
                TouchMode::Drag => {
                    drop(lock);
                    send_event(
                        &state,
                        PlatformInput::MouseMove(MouseMoveEvent {
                            position,
                            pressed_button: Some(button),
                            modifiers,
                            pressure,
                        }),
                    );
                }
                TouchMode::Cancelled => {}
            }
        }
    }
}

fn finish_touch(this: &Object, touches: id, cancelled: bool) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let touches: id = msg_send![touches, allObjects];
        let count: usize = msg_send![touches, count];
        for i in 0..count {
            let touch: id = msg_send![touches, objectAtIndex: i];
            let mut lock = state.lock();
            let Some(primary) = lock.primary_touch.as_ref() else {
                continue;
            };
            if primary.touch != touch {
                continue;
            }
            let primary = lock.primary_touch.take().unwrap();
            let position = touch_position(this, touch);
            let modifiers = lock.modifiers;
            lock.mouse_position = position;
            let click_count = lock.last_tap.map_or(1, |(_, _, count)| count);
            drop(lock);

            match primary.mode {
                TouchMode::Undecided if cancelled => {
                    cancel_press(&state, primary.button, TouchMode::Undecided)
                }
                TouchMode::Undecided | TouchMode::Drag => {
                    send_event(
                        &state,
                        PlatformInput::MouseUp(MouseUpEvent {
                            button: primary.button,
                            position,
                            modifiers,
                            click_count,
                            pressure: 1.0,
                        }),
                    );
                    if primary.mode == TouchMode::Drag {
                        // Pointer and pencil ups are not taps.
                        state.lock().last_tap = None;
                    }
                }
                TouchMode::Scroll => {
                    send_event(
                        &state,
                        PlatformInput::ScrollWheel(ScrollWheelEvent {
                            position,
                            delta: ScrollDelta::Pixels(Point::default()),
                            modifiers,
                            touch_phase: TouchPhase::Ended,
                        }),
                    );
                    state.lock().last_tap = None;
                    if !cancelled {
                        start_momentum(&state, &primary, position, modifiers);
                    }
                }
                TouchMode::LongPressed | TouchMode::Cancelled => {
                    state.lock().last_tap = None;
                }
            }
        }
    }
}

extern "C" fn touches_ended(this: &Object, _: Sel, touches: id, event: id) {
    let state = unsafe { get_window_state(this) };
    update_modifiers(&state, unsafe { event_modifier_flags(event) });
    finish_touch(this, touches, false);
}

extern "C" fn touches_cancelled(this: &Object, _: Sel, touches: id, _event: id) {
    finish_touch(this, touches, true);
}

/// Momentum scrolling after a fling.
fn start_momentum(
    state: &Arc<Mutex<IosWindowState>>,
    primary: &PrimaryTouch,
    position: Point<Pixels>,
    modifiers: Modifiers,
) {
    let now = Instant::now();
    let Some((then, then_position)) = primary
        .samples
        .iter()
        .find(|(at, _)| now.duration_since(*at) <= Duration::from_millis(120))
        .copied()
    else {
        return;
    };
    let elapsed = now.duration_since(then).as_secs_f32();
    if elapsed <= 0.0 {
        return;
    }
    let velocity = point(
        (position.x - then_position.x).0 / elapsed,
        (position.y - then_position.y).0 / elapsed,
    );
    if velocity.x.abs() < MOMENTUM_MIN_VELOCITY && velocity.y.abs() < MOMENTUM_MIN_VELOCITY {
        return;
    }
    state.lock().momentum = Some(Momentum {
        velocity,
        position,
        modifiers,
        last_tick: now,
    });
}

fn tick_momentum(state: &Arc<Mutex<IosWindowState>>) {
    let mut lock = state.lock();
    let Some(momentum) = lock.momentum.as_mut() else {
        return;
    };
    let now = Instant::now();
    let dt = now
        .duration_since(momentum.last_tick)
        .as_secs_f32()
        .min(0.1);
    momentum.last_tick = now;
    let delta = point(px(momentum.velocity.x * dt), px(momentum.velocity.y * dt));
    let decay = MOMENTUM_DECELERATION.powf(dt * 1000.0);
    momentum.velocity = point(momentum.velocity.x * decay, momentum.velocity.y * decay);
    let position = momentum.position;
    let modifiers = momentum.modifiers;
    let finished = momentum.velocity.x.abs() < MOMENTUM_MIN_VELOCITY
        && momentum.velocity.y.abs() < MOMENTUM_MIN_VELOCITY;
    if finished {
        lock.momentum = None;
    }
    drop(lock);
    send_event(
        state,
        PlatformInput::ScrollWheel(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Pixels(delta),
            modifiers,
            touch_phase: if finished {
                TouchPhase::Ended
            } else {
                TouchPhase::Moved
            },
        }),
    );
}

/// Gesture recognizers.
extern "C" fn handle_pinch(this: &Object, _: Sel, recognizer: id) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let gesture_state: isize = msg_send![recognizer, state];
        let scale: f64 = msg_send![recognizer, scale];
        let scale = scale as f32;
        let location: NSPoint = msg_send![recognizer, locationInView: this];
        let position: Point<Pixels> = location.into();
        let flags: usize = msg_send![recognizer, modifierFlags];
        update_modifiers(&state, flags);

        let phase = match gesture_state {
            GESTURE_BEGAN => {
                state.lock().pinch_last_scale = scale;
                // The fingers are pinching, not pressing: cancel the press.
                cancel_primary_for_gesture(&state);
                TouchPhase::Started
            }
            GESTURE_CHANGED => TouchPhase::Moved,
            GESTURE_ENDED | GESTURE_CANCELLED => TouchPhase::Ended,
            _ => return,
        };
        let mut lock = state.lock();
        let delta = match phase {
            TouchPhase::Moved if lock.pinch_last_scale > 0.0 => scale / lock.pinch_last_scale,
            _ => 1.0,
        };
        lock.pinch_last_scale = scale;
        let modifiers = lock.modifiers;
        drop(lock);
        send_event(
            &state,
            PlatformInput::Pinch(PinchEvent {
                position,
                delta,
                scale: 1.0,
                modifiers,
                phase,
            }),
        );
    }
}

extern "C" fn handle_long_press(this: &Object, _: Sel, recognizer: id) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let gesture_state: isize = msg_send![recognizer, state];
        if gesture_state != GESTURE_BEGAN {
            return;
        }
        let location: NSPoint = msg_send![recognizer, locationInView: this];
        let position: Point<Pixels> = location.into();

        let mut lock = state.lock();
        let Some((button, mode)) = lock
            .primary_touch
            .as_ref()
            .map(|primary| (primary.button, primary.mode))
        else {
            return;
        };
        if mode != TouchMode::Undecided {
            return;
        }
        let has_text_focus = lock.input_handler.is_some();
        let modifiers = lock.modifiers;

        if has_text_focus && present_edit_menu(this, position) {
            if let Some(primary) = lock.primary_touch.as_mut() {
                primary.mode = TouchMode::Cancelled;
            }
            drop(lock);
            cancel_press(&state, button, TouchMode::Undecided);
            return;
        }

        if let Some(primary) = lock.primary_touch.as_mut() {
            primary.mode = TouchMode::LongPressed;
        }
        drop(lock);
        cancel_press(&state, button, TouchMode::Undecided);
        send_event(
            &state,
            PlatformInput::MouseDown(MouseDownEvent {
                button: MouseButton::Right,
                position,
                modifiers,
                click_count: 1,
                first_mouse: false,
                pressure: 1.0,
            }),
        );
        send_event(
            &state,
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Right,
                position,
                modifiers,
                click_count: 1,
                pressure: 1.0,
            }),
        );
    }
}

/// Shows the system edit menu (cut/copy/paste/select all) at `position`,
/// if the app registered any of those actions. iOS 16 and later.
unsafe fn present_edit_menu(view: &Object, position: Point<Pixels>) -> bool {
    unsafe {
        let Some(platform) = platform() else {
            return false;
        };
        let any_available = [
            crate::OsAction::Cut,
            crate::OsAction::Copy,
            crate::OsAction::Paste,
            crate::OsAction::SelectAll,
        ]
        .into_iter()
        .any(|action| platform.can_perform_os_action(action));
        if !any_available {
            return false;
        }
        let Some(configuration_class) = Class::get("UIEditMenuConfiguration") else {
            return false;
        };
        let interactions: id = msg_send![view, interactions];
        let count: usize = msg_send![interactions, count];
        for i in 0..count {
            let interaction: id = msg_send![interactions, objectAtIndex: i];
            let is_edit_menu: BOOL =
                msg_send![interaction, isKindOfClass: class!(UIEditMenuInteraction)];
            if is_edit_menu == NO {
                continue;
            }
            let source = NSPoint::new(position.x.0 as f64, position.y.0 as f64);
            let configuration: id = msg_send![configuration_class, configurationWithIdentifier: nil sourcePoint: source];
            let _: () = msg_send![interaction, presentEditMenuWithConfiguration: configuration];
            return true;
        }
        false
    }
}

extern "C" fn handle_pointer_scroll(this: &Object, _: Sel, recognizer: id) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let gesture_state: isize = msg_send![recognizer, state];
        let location: NSPoint = msg_send![recognizer, locationInView: this];
        let position: Point<Pixels> = location.into();
        let translation: NSPoint = msg_send![recognizer, translationInView: this];
        let _: () = msg_send![recognizer, setTranslation: NSPoint::default() inView: this];
        let phase = match gesture_state {
            GESTURE_BEGAN => TouchPhase::Started,
            GESTURE_CHANGED => TouchPhase::Moved,
            GESTURE_ENDED | GESTURE_CANCELLED => TouchPhase::Ended,
            _ => return,
        };
        let mut lock = state.lock();
        lock.momentum = None;
        lock.mouse_position = position;
        let modifiers = lock.modifiers;
        drop(lock);
        send_event(
            &state,
            PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(point(
                    px(translation.x as f32),
                    px(translation.y as f32),
                )),
                modifiers,
                touch_phase: phase,
            }),
        );
    }
}

extern "C" fn recognize_simultaneously(_: &Object, _: Sel, _: id, _: id) -> BOOL {
    YES
}

extern "C" fn handle_two_finger_pan(this: &Object, _: Sel, recognizer: id) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let gesture_state: isize = msg_send![recognizer, state];
        let location: NSPoint = msg_send![recognizer, locationInView: this];
        let position: Point<Pixels> = location.into();
        let translation: NSPoint = msg_send![recognizer, translationInView: this];
        let _: () = msg_send![recognizer, setTranslation: NSPoint::default() inView: this];
        let phase = match gesture_state {
            GESTURE_BEGAN => {
                cancel_primary_for_gesture(&state);
                TouchPhase::Started
            }
            GESTURE_CHANGED => TouchPhase::Moved,
            GESTURE_ENDED | GESTURE_CANCELLED => TouchPhase::Ended,
            _ => return,
        };
        let mut lock = state.lock();
        lock.mouse_position = position;
        let modifiers = lock.modifiers;
        drop(lock);
        send_event(
            &state,
            PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta: ScrollDelta::Pixels(point(
                    px(translation.x as f32),
                    px(translation.y as f32),
                )),
                modifiers,
                touch_phase: phase,
            }),
        );
    }
}

extern "C" fn handle_hover(this: &Object, _: Sel, recognizer: id) {
    let state = unsafe { get_window_state(this) };
    unsafe {
        let gesture_state: isize = msg_send![recognizer, state];
        let location: NSPoint = msg_send![recognizer, locationInView: this];
        let position: Point<Pixels> = location.into();
        let mut lock = state.lock();
        let modifiers = lock.modifiers;
        let pressed_button = lock
            .primary_touch
            .as_ref()
            .filter(|primary| primary.mode == TouchMode::Drag)
            .map(|primary| primary.button);
        lock.mouse_position = position;
        drop(lock);
        match gesture_state {
            GESTURE_BEGAN | GESTURE_CHANGED => {
                if pressed_button.is_some() {
                    // The touch handlers report drags.
                    return;
                }
                send_event(
                    &state,
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        pressed_button: None,
                        modifiers,
                        pressure: 1.0,
                    }),
                );
            }
            GESTURE_ENDED | GESTURE_CANCELLED => {
                send_event(
                    &state,
                    PlatformInput::MouseExited(MouseExitEvent {
                        position,
                        pressed_button,
                        modifiers,
                    }),
                );
            }
            _ => {}
        }
    }
}

/// Hardware keyboard. Presses arrive on the view while it is first
/// responder (a text field is focused) and on the window otherwise; both
/// feed the same handler and fall back to `super` for what gpui leaves
/// unhandled, which is what lets UIKit turn a press into text
/// (`insertText:`, `deleteBackward`) for the text field.
fn presses_began_impl(state: &Arc<Mutex<IosWindowState>>, presses: id) -> bool {
    unsafe {
        let presses_array: id = msg_send![presses, allObjects];
        let count: usize = msg_send![presses_array, count];
        let mut unhandled = false;
        for i in 0..count {
            let press: id = msg_send![presses_array, objectAtIndex: i];
            let key: id = msg_send![press, key];
            if key.is_null() {
                unhandled = true;
                continue;
            }
            let timestamp: f64 = msg_send![press, timestamp];
            {
                let mut lock = state.lock();
                if lock.last_press == (press as usize, timestamp) {
                    continue;
                }
                lock.last_press = (press as usize, timestamp);
            }
            let flags: usize = msg_send![key, modifierFlags];
            update_modifiers(state, flags);
            let Some(keystroke) = keystroke_from_ui_key(key) else {
                continue;
            };
            let handled = send_event(
                state,
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke,
                    is_held: false,
                }),
            );
            if !handled {
                unhandled = true;
            }
        }
        unhandled
    }
}

fn presses_ended_impl(state: &Arc<Mutex<IosWindowState>>, presses: id) -> bool {
    unsafe {
        let presses_array: id = msg_send![presses, allObjects];
        let count: usize = msg_send![presses_array, count];
        let mut unhandled = false;
        for i in 0..count {
            let press: id = msg_send![presses_array, objectAtIndex: i];
            let key: id = msg_send![press, key];
            if key.is_null() {
                unhandled = true;
                continue;
            }
            let flags: usize = msg_send![key, modifierFlags];
            let key_code: isize = msg_send![key, keyCode];
            if super::events::is_modifier_key(key_code) {
                // The flags still include the key being released.
                let released = match key_code {
                    0xE0 | 0xE4 => super::events::UI_KEY_MODIFIER_CONTROL,
                    0xE1 | 0xE5 => super::events::UI_KEY_MODIFIER_SHIFT,
                    0xE2 | 0xE6 => super::events::UI_KEY_MODIFIER_ALTERNATE,
                    0xE3 | 0xE7 => super::events::UI_KEY_MODIFIER_COMMAND,
                    _ => 0,
                };
                update_modifiers(state, flags & !released);
                continue;
            }
            update_modifiers(state, flags);
            let Some(keystroke) = keystroke_from_ui_key(key) else {
                continue;
            };
            let handled = send_event(state, PlatformInput::KeyUp(KeyUpEvent { keystroke }));
            if !handled {
                unhandled = true;
            }
        }
        unhandled
    }
}

extern "C" fn presses_began(this: &Object, _: Sel, presses: id, event: id) {
    let state = unsafe { get_window_state(this) };
    if presses_began_impl(&state, presses) {
        unsafe {
            let _: () =
                msg_send![super(this, class!(UIView)), pressesBegan: presses withEvent: event];
        }
    }
}

extern "C" fn presses_ended(this: &Object, _: Sel, presses: id, event: id) {
    let state = unsafe { get_window_state(this) };
    if presses_ended_impl(&state, presses) {
        unsafe {
            let _: () =
                msg_send![super(this, class!(UIView)), pressesEnded: presses withEvent: event];
        }
    }
}

/// Whether the view already saw the presses: they travel up the responder
/// chain to the window when it leaves them unhandled.
fn view_is_first_responder(state: &Arc<Mutex<IosWindowState>>) -> bool {
    let view = state.lock().native_view.as_ptr();
    unsafe {
        let is_first: BOOL = msg_send![view, isFirstResponder];
        is_first == YES
    }
}

extern "C" fn window_presses_began(this: &Object, _: Sel, presses: id, event: id) {
    let state = unsafe { get_window_state(this) };
    if view_is_first_responder(&state) || presses_began_impl(&state, presses) {
        unsafe {
            let _: () =
                msg_send![super(this, class!(UIWindow)), pressesBegan: presses withEvent: event];
        }
    }
}

extern "C" fn window_presses_ended(this: &Object, _: Sel, presses: id, event: id) {
    let state = unsafe { get_window_state(this) };
    if view_is_first_responder(&state) || presses_ended_impl(&state, presses) {
        unsafe {
            let _: () =
                msg_send![super(this, class!(UIWindow)), pressesEnded: presses withEvent: event];
        }
    }
}

/// The `UIResponderStandardEditActions` UIKit drives itself: from the
/// system edit menu, and from the shortcuts it reserves for them, which
/// never arrive as presses. Each is answered with the app's registered
/// `OsAction` when there is one, and otherwise as the keystroke a desktop
/// would have delivered, so the keymap gets its turn.
fn standard_edit_selectors() -> [Sel; 6] {
    [
        sel!(cut:),
        sel!(copy:),
        sel!(paste:),
        sel!(selectAll:),
        sel!(undo:),
        sel!(redo:),
    ]
}

fn keystroke_for_edit_selector(selector: Sel) -> Option<Keystroke> {
    let (key, shift) = if selector == sel!(cut:) {
        ("x", false)
    } else if selector == sel!(copy:) {
        ("c", false)
    } else if selector == sel!(paste:) {
        ("v", false)
    } else if selector == sel!(selectAll:) {
        ("a", false)
    } else if selector == sel!(undo:) {
        ("z", false)
    } else if selector == sel!(redo:) {
        ("z", true)
    } else {
        return None;
    };
    Some(Keystroke {
        modifiers: Modifiers {
            platform: true,
            shift,
            ..Modifiers::default()
        },
        key: key.to_string(),
        key_char: None,
    })
}

/// The shortcuts UIKit reserves for its standard edit actions never reach
/// `pressesBegan:`; without a text view to act on, UIKit drops them. These
/// `UIKeyCommand`s, which ask for priority over that system behaviour,
/// claim them back and replay them as the keystrokes they are.
const RESERVED_SHORTCUTS: [(&str, bool); 6] = [
    ("z", false),
    ("z", true),
    ("x", false),
    ("c", false),
    ("v", false),
    ("a", false),
];

extern "C" fn reserved_key_commands(_this: &Object, _: Sel) -> id {
    unsafe {
        let commands: id = msg_send![class!(NSMutableArray), array];
        for (key, shift) in RESERVED_SHORTCUTS {
            let mut flags = super::events::UI_KEY_MODIFIER_COMMAND;
            if shift {
                flags |= super::events::UI_KEY_MODIFIER_SHIFT;
            }
            let command: id = msg_send![
                class!(UIKeyCommand),
                keyCommandWithInput: ns_string(key)
                modifierFlags: flags
                action: sel!(handleGPUIReservedShortcut:)
            ];
            let responds: BOOL =
                msg_send![command, respondsToSelector: sel!(setWantsPriorityOverSystemBehavior:)];
            if responds == YES {
                let _: () = msg_send![command, setWantsPriorityOverSystemBehavior: YES];
            }
            let _: () = msg_send![commands, addObject: command];
        }
        commands
    }
}

extern "C" fn handle_reserved_shortcut(this: &Object, _: Sel, command: id) {
    let (key, flags) = unsafe {
        let input: id = msg_send![command, input];
        let flags: usize = msg_send![command, modifierFlags];
        (input.to_str().to_string(), flags)
    };
    let keystroke = Keystroke {
        modifiers: modifiers_from_flags(flags),
        key,
        key_char: None,
    };
    let state = unsafe { get_window_state(this) };
    send_event(
        &state,
        PlatformInput::KeyDown(KeyDownEvent {
            keystroke,
            is_held: false,
        }),
    );
}

extern "C" fn can_perform_action(_this: &Object, _: Sel, action: Sel, _sender: id) -> BOOL {
    standard_edit_selectors().contains(&action).to_objc()
}

extern "C" fn perform_edit_action(this: &Object, selector: Sel, _sender: id) {
    if let (Some(platform), Some(os_action)) = (platform(), os_action_for_selector(selector)) {
        if platform.os_action(os_action).is_some() {
            platform.perform_os_action(os_action);
            return;
        }
    }
    let Some(keystroke) = keystroke_for_edit_selector(selector) else {
        return;
    };
    let state = unsafe { get_window_state(this) };
    send_event(
        &state,
        PlatformInput::KeyDown(KeyDownEvent {
            keystroke,
            is_held: false,
        }),
    );
}

/// UIKeyInput. Enter, tab and backspace are offered to gpui as keystrokes
/// first, since text fields usually bind them; only unbound ones become
/// text edits.
fn dispatch_key_or_insert(this: &Object, key: &str, key_char: Option<&str>, insert: Option<&str>) {
    let state = unsafe { get_window_state(this) };
    let modifiers = state.lock().modifiers;
    let handled = send_event(
        &state,
        PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                modifiers,
                key: key.to_string(),
                key_char: key_char.map(str::to_string),
            },
            is_held: false,
        }),
    );
    if handled {
        return;
    }
    if let Some(text) = insert {
        with_input_handler(this, |handler| handler.replace_text_in_range(None, text));
    }
}

extern "C" fn insert_text(this: &Object, _: Sel, text: id) {
    let text = unsafe { text.to_str().to_string() };
    match text.as_str() {
        "\n" | "\r" => dispatch_key_or_insert(this, "enter", Some("\n"), Some("\n")),
        "\t" => dispatch_key_or_insert(this, "tab", Some("\t"), Some("\t")),
        _ => {
            with_input_handler(this, |handler| handler.replace_text_in_range(None, &text));
        }
    }
}

extern "C" fn delete_backward(this: &Object, _: Sel) {
    let state = unsafe { get_window_state(this) };
    let modifiers = state.lock().modifiers;
    let handled = send_event(
        &state,
        PlatformInput::KeyDown(KeyDownEvent {
            keystroke: Keystroke {
                modifiers,
                key: "backspace".to_string(),
                key_char: None,
            },
            is_held: false,
        }),
    );
    if handled {
        return;
    }
    with_input_handler(this, |handler| {
        let Some(selection) = handler.selected_text_range(false) else {
            return;
        };
        let range = if selection.range.is_empty() {
            selection.range.start.saturating_sub(1)..selection.range.start
        } else {
            selection.range
        };
        handler.replace_text_in_range(Some(range), "");
    });
}

/// UITextInput positions and ranges are UTF-16 offsets, as gpui's input
/// handler expects.
unsafe fn make_position(offset: usize) -> id {
    unsafe {
        let position: id = msg_send![TEXT_POSITION_CLASS, new];
        (*position).set_ivar(OFFSET_IVAR, offset as isize);
        msg_send![position, autorelease]
    }
}

unsafe fn make_range(range: Range<usize>) -> id {
    unsafe {
        let text_range: id = msg_send![TEXT_RANGE_CLASS, new];
        (*text_range).set_ivar(START_IVAR, range.start as isize);
        (*text_range).set_ivar(END_IVAR, range.end as isize);
        msg_send![text_range, autorelease]
    }
}

unsafe fn position_offset(position: id) -> Option<usize> {
    unsafe {
        if position.is_null() {
            return None;
        }
        let is_ours: BOOL = msg_send![position, isKindOfClass: TEXT_POSITION_CLASS];
        if is_ours == NO {
            return None;
        }
        let offset: isize = *(*position).get_ivar(OFFSET_IVAR);
        Some(offset.max(0) as usize)
    }
}

unsafe fn range_bounds(range: id) -> Option<Range<usize>> {
    unsafe {
        if range.is_null() {
            return None;
        }
        let is_ours: BOOL = msg_send![range, isKindOfClass: TEXT_RANGE_CLASS];
        if is_ours == NO {
            return None;
        }
        let start: isize = *(*range).get_ivar(START_IVAR);
        let end: isize = *(*range).get_ivar(END_IVAR);
        let start = start.max(0) as usize;
        let end = (end.max(0) as usize).max(start);
        Some(start..end)
    }
}

extern "C" fn range_start(this: &Object, _: Sel) -> id {
    unsafe {
        let start: isize = *this.get_ivar(START_IVAR);
        make_position(start.max(0) as usize)
    }
}

extern "C" fn range_end(this: &Object, _: Sel) -> id {
    unsafe {
        let end: isize = *this.get_ivar(END_IVAR);
        make_position(end.max(0) as usize)
    }
}

extern "C" fn range_is_empty(this: &Object, _: Sel) -> BOOL {
    unsafe {
        let start: isize = *this.get_ivar(START_IVAR);
        let end: isize = *this.get_ivar(END_IVAR);
        (start >= end).to_objc()
    }
}

/// The input handler has no length query, so the document end is found by
/// reading past the selection: the text that comes back ends where the
/// document does.
const DOCUMENT_PROBE: usize = 4096;

fn document_end(this: &Object) -> usize {
    with_input_handler(this, |handler| {
        let anchor = handler
            .selected_text_range(false)
            .map_or(0, |selection| selection.range.end)
            .max(handler.marked_text_range().map_or(0, |range| range.end));
        let mut adjusted = None;
        match handler.text_for_range(anchor..anchor + DOCUMENT_PROBE, &mut adjusted) {
            Some(text) => anchor + text.encode_utf16().count(),
            None => anchor,
        }
    })
    .unwrap_or(0)
}

extern "C" fn text_in_range(this: &Object, _: Sel, range: id) -> id {
    unsafe {
        let Some(range) = range_bounds(range) else {
            return ns_string("");
        };
        let text = with_input_handler(this, |handler| {
            let mut adjusted = None;
            handler.text_for_range(range, &mut adjusted)
        })
        .flatten()
        .unwrap_or_default();
        ns_string(&text)
    }
}

extern "C" fn replace_range(this: &Object, _: Sel, range: id, text: id) {
    unsafe {
        let Some(range) = range_bounds(range) else {
            return;
        };
        let text = text.to_str().to_string();
        with_input_handler(this, |handler| {
            handler.replace_text_in_range(Some(range), &text)
        });
    }
}

extern "C" fn selected_text_range(this: &Object, _: Sel) -> id {
    unsafe {
        match with_input_handler(this, |handler| handler.selected_text_range(false)).flatten() {
            Some(selection) => make_range(selection.range),
            None => nil,
        }
    }
}

extern "C" fn set_selected_text_range(_this: &Object, _: Sel, _range: id) {
    // gpui's input handler exposes no way to move the selection from the
    // platform; the keyboard's cursor gestures are not supported.
}

extern "C" fn marked_text_range(this: &Object, _: Sel) -> id {
    unsafe {
        match with_input_handler(this, |handler| handler.marked_text_range()).flatten() {
            Some(range) => make_range(range),
            None => nil,
        }
    }
}

extern "C" fn marked_text_style(_this: &Object, _: Sel) -> id {
    nil
}

extern "C" fn set_marked_text_style(_this: &Object, _: Sel, _style: id) {}

extern "C" fn set_marked_text(this: &Object, _: Sel, text: id, selected_range: NSRange) {
    unsafe {
        let text = text.to_str().to_string();
        let selected_range = selected_range.to_range();
        with_input_handler(this, |handler| {
            handler.replace_and_mark_text_in_range(None, &text, selected_range)
        });
    }
}

extern "C" fn unmark_text(this: &Object, _: Sel) {
    with_input_handler(this, |handler| handler.unmark_text());
}

extern "C" fn beginning_of_document(_this: &Object, _: Sel) -> id {
    unsafe { make_position(0) }
}

extern "C" fn end_of_document(this: &Object, _: Sel) -> id {
    unsafe { make_position(document_end(this)) }
}

extern "C" fn text_range_from_position(this: &Object, _: Sel, from: id, to: id) -> id {
    unsafe {
        let (Some(from), Some(to)) = (position_offset(from), position_offset(to)) else {
            return nil;
        };
        let _ = this;
        make_range(from.min(to)..from.max(to))
    }
}

extern "C" fn position_from_position(this: &Object, _: Sel, position: id, offset: isize) -> id {
    unsafe {
        let Some(base) = position_offset(position) else {
            return nil;
        };
        let target = base as isize + offset;
        if target < 0 {
            return nil;
        }
        let target = target as usize;
        if offset > 0 && target > document_end(this) {
            return nil;
        }
        make_position(target)
    }
}

extern "C" fn position_from_position_in_direction(
    this: &Object,
    sel: Sel,
    position: id,
    direction: isize,
    offset: isize,
) -> id {
    let signed = match direction {
        TEXT_DIRECTION_RIGHT | TEXT_DIRECTION_DOWN => offset,
        TEXT_DIRECTION_LEFT | TEXT_DIRECTION_UP => -offset,
        _ => 0,
    };
    position_from_position(this, sel, position, signed)
}

extern "C" fn compare_position(_this: &Object, _: Sel, a: id, b: id) -> isize {
    unsafe {
        match (position_offset(a), position_offset(b)) {
            (Some(a), Some(b)) => match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            },
            _ => 0,
        }
    }
}

extern "C" fn offset_from_position(_this: &Object, _: Sel, from: id, to: id) -> isize {
    unsafe {
        match (position_offset(from), position_offset(to)) {
            (Some(from), Some(to)) => to as isize - from as isize,
            _ => 0,
        }
    }
}

extern "C" fn input_delegate(this: &Object, _: Sel) -> id {
    unsafe { *this.get_ivar(INPUT_DELEGATE_IVAR) }
}

extern "C" fn set_input_delegate(this: &Object, _: Sel, delegate: id) {
    unsafe {
        let this = this as *const Object as *mut Object;
        (*this).set_ivar(INPUT_DELEGATE_IVAR, delegate);
    }
}

extern "C" fn tokenizer(this: &Object, _: Sel) -> id {
    unsafe {
        let mut tokenizer: id = *this.get_ivar(TOKENIZER_IVAR);
        if tokenizer.is_null() {
            tokenizer = msg_send![class!(UITextInputStringTokenizer), alloc];
            tokenizer = msg_send![tokenizer, initWithTextInput: this];
            let this = this as *const Object as *mut Object;
            (*this).set_ivar(TOKENIZER_IVAR, tokenizer);
        }
        tokenizer
    }
}

extern "C" fn position_within_range(_this: &Object, _: Sel, range: id, direction: isize) -> id {
    unsafe {
        let Some(range) = range_bounds(range) else {
            return nil;
        };
        match direction {
            TEXT_DIRECTION_RIGHT | TEXT_DIRECTION_DOWN => make_position(range.end),
            _ => make_position(range.start),
        }
    }
}

extern "C" fn character_range_by_extending(
    this: &Object,
    _: Sel,
    position: id,
    direction: isize,
) -> id {
    unsafe {
        let Some(offset) = position_offset(position) else {
            return nil;
        };
        match direction {
            TEXT_DIRECTION_RIGHT | TEXT_DIRECTION_DOWN => {
                let end = (offset + 1).min(document_end(this).max(offset));
                make_range(offset..end)
            }
            _ => make_range(offset.saturating_sub(1)..offset),
        }
    }
}

extern "C" fn base_writing_direction(
    _this: &Object,
    _: Sel,
    _position: id,
    _direction: isize,
) -> isize {
    // NSWritingDirectionNatural
    -1
}

extern "C" fn set_base_writing_direction(_this: &Object, _: Sel, _direction: isize, _range: id) {}

fn rect_for_range(this: &Object, range: Range<usize>) -> NSRect {
    with_input_handler(this, |handler| handler.bounds_for_range(range))
        .flatten()
        .map_or(NSRect::default(), |bounds| {
            NSRect::new(
                NSPoint::new(bounds.origin.x.0 as f64, bounds.origin.y.0 as f64),
                NSSize::new(bounds.size.width.0 as f64, bounds.size.height.0 as f64),
            )
        })
}

extern "C" fn first_rect_for_range(this: &Object, _: Sel, range: id) -> NSRect {
    unsafe {
        match range_bounds(range) {
            Some(range) => rect_for_range(this, range),
            None => NSRect::default(),
        }
    }
}

extern "C" fn caret_rect_for_position(this: &Object, _: Sel, position: id) -> NSRect {
    unsafe {
        let Some(offset) = position_offset(position) else {
            return NSRect::default();
        };
        let mut rect = rect_for_range(this, offset..offset);
        if rect.size.width < 2.0 {
            rect.size.width = 2.0;
        }
        rect
    }
}

extern "C" fn selection_rects_for_range(_this: &Object, _: Sel, _range: id) -> id {
    unsafe { msg_send![class!(NSArray), array] }
}

fn position_for_point(this: &Object, location: NSPoint) -> usize {
    let position: Point<Pixels> = location.into();
    with_input_handler(this, |handler| {
        handler
            .character_index_for_point(position)
            .or_else(|| handler.selected_text_range(false).map(|s| s.range.start))
    })
    .flatten()
    .unwrap_or(0)
}

extern "C" fn closest_position_to_point(this: &Object, _: Sel, location: NSPoint) -> id {
    unsafe { make_position(position_for_point(this, location)) }
}

extern "C" fn closest_position_to_point_within_range(
    this: &Object,
    _: Sel,
    location: NSPoint,
    range: id,
) -> id {
    unsafe {
        let offset = position_for_point(this, location);
        let offset = match range_bounds(range) {
            Some(range) => offset.clamp(range.start, range.end),
            None => offset,
        };
        make_position(offset)
    }
}

extern "C" fn character_range_at_point(this: &Object, _: Sel, location: NSPoint) -> id {
    unsafe {
        let offset = position_for_point(this, location);
        make_range(offset..offset)
    }
}

extern "C" fn text_input_view(this: &Object, _: Sel) -> id {
    this as *const Object as id
}

extern "C" fn autocorrection_type(_this: &Object, _: Sel) -> isize {
    // UITextAutocorrectionTypeNo
    1
}

extern "C" fn keyboard_appearance(this: &Object, _: Sel) -> isize {
    let state = unsafe { get_window_state(this) };
    // UIKeyboardAppearanceDark / Light
    match state.lock().appearance() {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => 1,
        WindowAppearance::Light | WindowAppearance::VibrantLight => 2,
    }
}

#[link(name = "UIKit", kind = "framework")]
unsafe extern "C" {
    static UIKeyboardWillChangeFrameNotification: id;
    static UIKeyboardFrameEndUserInfoKey: id;
    static UIApplicationDidEnterBackgroundNotification: id;
    static UIApplicationWillEnterForegroundNotification: id;
}

#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSRunLoopCommonModes: id;
}
