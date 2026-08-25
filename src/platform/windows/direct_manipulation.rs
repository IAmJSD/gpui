//! Precision-touchpad pinch and pan, via Direct Manipulation.
//!
//! `WM_GESTURE` covers touchscreens only. A precision touchpad never produces
//! one: its contacts go to the pointer input stack, and a window that does
//! nothing with them gets the legacy fallback -- `WM_MOUSEWHEEL` for a
//! two-finger pan and Ctrl+`WM_MOUSEWHEEL` for a pinch. Direct Manipulation is
//! the only interface that hands over the real gesture.
//!
//! Taking it is all-or-nothing. `DM_POINTERHITTEST` arrives before the gesture
//! has been classified, so a window that claims the contact to get pinches
//! claims the pans as well and stops receiving `WM_MOUSEWHEEL` for them. This
//! module therefore replaces *both*: the content transform's scale becomes a
//! `PinchEvent` and its translation a pixel-precise `ScrollWheelEvent`.
//!
//! That is a lot to put in the path of ordinary scrolling on the word of a
//! cross-compile, so it is off unless `GPUI_ENABLE_DIRECT_MANIPULATION` is set
//! to `1` or `true`. See `UPSTREAM.md`.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use ::util::ResultExt;
use anyhow::Context as _;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::DirectManipulation::*;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, KillTimer, SetTimer};
use windows::core::implement;

use crate::platform::windows::{WindowsWindowInner, current_modifiers, logical_point};
use crate::{
    PinchEvent, Pixels, PlatformInput, Point, ScrollDelta, ScrollWheelEvent, TouchPhase, point, px,
};

pub(crate) const ENABLE_DIRECT_MANIPULATION: &str = "GPUI_ENABLE_DIRECT_MANIPULATION";

/// `WM_TIMER` id for pumping the update manager. Distinct from
/// `SIZE_MOVE_LOOP_TIMER_ID` in `events.rs`.
pub(crate) const DIRECT_MANIPULATION_TIMER_ID: usize = 2;

/// Roughly 60Hz. A `MANUALUPDATE` viewport only advances -- and only reports
/// anything -- when `Update` is called, so this is the gesture's sample rate.
const UPDATE_INTERVAL_MS: u32 = 16;

/// Everything this fork wants from a manipulation: the interaction itself, a
/// scale, and a translation, each with its inertia so a flick keeps going.
const CONFIGURATION: DIRECTMANIPULATION_CONFIGURATION = DIRECTMANIPULATION_CONFIGURATION(
    DIRECTMANIPULATION_CONFIGURATION_INTERACTION.0
        | DIRECTMANIPULATION_CONFIGURATION_SCALING.0
        | DIRECTMANIPULATION_CONFIGURATION_SCALING_INERTIA.0
        | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_X.0
        | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_Y.0
        | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_INERTIA.0,
);

/// A scale within this much of the previous one is hand tremor rather than a
/// pinch, and would otherwise start a gesture on every two-finger pan.
const SCALE_EPSILON: f32 = 0.0001;

pub(crate) struct DirectManipulation {
    hwnd: HWND,
    manager: IDirectManipulationManager,
    update_manager: IDirectManipulationUpdateManager,
    viewport: IDirectManipulationViewport,
    event_handler_cookie: u32,
}

impl DirectManipulation {
    /// Returns `None` when the opt-in is unset, or when any part of the setup
    /// fails -- in which case the window keeps the legacy Ctrl+scroll
    /// behaviour, which is a working fallback rather than a broken window.
    pub(crate) fn new(hwnd: HWND, window: Weak<WindowsWindowInner>) -> Option<Self> {
        if !std::env::var(ENABLE_DIRECT_MANIPULATION)
            .is_ok_and(|value| value == "true" || value == "1")
        {
            return None;
        }

        unsafe {
            let manager: IDirectManipulationManager =
                CoCreateInstance(&DirectManipulationManager, None, CLSCTX_INPROC_SERVER)
                    .context("creating the Direct Manipulation manager")
                    .log_err()?;
            let update_manager: IDirectManipulationUpdateManager = manager
                .GetUpdateManager()
                .context("getting the Direct Manipulation update manager")
                .log_err()?;
            let viewport: IDirectManipulationViewport = manager
                .CreateViewport(None, hwnd)
                .context("creating the Direct Manipulation viewport")
                .log_err()?;

            viewport.ActivateConfiguration(CONFIGURATION).log_err()?;
            // Nothing here is composited by Direct Manipulation -- the
            // transform is only ever read back and turned into events -- so it
            // must not drive itself off the compositor clock.
            viewport
                .SetViewportOptions(DIRECTMANIPULATION_VIEWPORT_OPTIONS_MANUALUPDATE)
                .log_err()?;

            let handler: IDirectManipulationViewportEventHandler =
                ViewportEventHandler::new(window).into();
            let event_handler_cookie = viewport
                .AddEventHandler(Some(hwnd), &handler)
                .context("registering the Direct Manipulation event handler")
                .log_err()?;

            manager.Activate(hwnd).log_err()?;
            viewport.Enable().log_err()?;

            let this = Self {
                hwnd,
                manager,
                update_manager,
                viewport,
                event_handler_cookie,
            };
            this.set_viewport_size(client_rect(hwnd));
            Some(this)
        }
    }

    /// Claims a contact that landed on this window, which is what makes Direct
    /// Manipulation report the gesture instead of the system synthesising a
    /// wheel message from it.
    pub(crate) fn set_contact(&self, pointer_id: u32) {
        unsafe {
            if self.viewport.SetContact(pointer_id).log_err().is_none() {
                return;
            }
            // The viewport advances only while this is ticking, so the tick
            // has to start before the manipulation does.
            SetTimer(
                Some(self.hwnd),
                DIRECT_MANIPULATION_TIMER_ID,
                UPDATE_INTERVAL_MS,
                None,
            );
        }
    }

    /// Pumps one frame. Returns `false` once the viewport has come to rest and
    /// the timer is no longer needed.
    pub(crate) fn update(&self) -> bool {
        unsafe {
            self.update_manager.Update(None).log_err();
            // Trust the status over the status-changed callback: a contact
            // that turns out not to be a manipulation at all never produces a
            // transition, and the timer would tick forever waiting for one.
            !matches!(
                self.viewport.GetStatus(),
                Ok(DIRECTMANIPULATION_READY)
                    | Ok(DIRECTMANIPULATION_ENABLED)
                    | Ok(DIRECTMANIPULATION_DISABLED)
                    | Err(_)
            )
        }
    }

    pub(crate) fn stop_updating(&self) {
        // Not logged: this is also the teardown path, where there is usually
        // no timer left to kill and failing is the ordinary case.
        unsafe { _ = KillTimer(Some(self.hwnd), DIRECT_MANIPULATION_TIMER_ID) };
    }

    pub(crate) fn set_viewport_size(&self, rect: RECT) {
        unsafe { self.viewport.SetViewportRect(&rect).log_err() };
    }
}

impl Drop for DirectManipulation {
    fn drop(&mut self) {
        self.stop_updating();
        unsafe {
            self.viewport
                .RemoveEventHandler(self.event_handler_cookie)
                .log_err();
            // `Abandon` rather than `Disable`: the viewport is going away with
            // the window, and this releases it without waiting for whatever
            // manipulation might still be in flight.
            self.viewport.Abandon().log_err();
            self.manager.Deactivate(self.hwnd).log_err();
        }
    }
}

fn client_rect(hwnd: HWND) -> RECT {
    let mut rect = RECT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rect).log_err() };
    rect
}

/// The anchor for both event kinds. A precision touchpad leaves the cursor
/// where it is for the duration of a gesture, and Direct Manipulation reports
/// no centroid of its own, so the cursor is the best answer available.
fn cursor_client_position(hwnd: HWND, scale_factor: f32) -> Point<Pixels> {
    let mut cursor = POINT::default();
    unsafe {
        GetCursorPos(&mut cursor).log_err();
        ScreenToClient(hwnd, &mut cursor).ok().log_err();
    }
    logical_point(cursor.x as f32, cursor.y as f32, scale_factor)
}

/// The part of a manipulation already turned into events, so that Direct
/// Manipulation's cumulative transform can be reported as the per-event deltas
/// `PinchEvent` and `ScrollWheelEvent` want.
struct Manipulation {
    scale: f32,
    translation: Point<f32>,
    /// Whether a `TouchPhase::Started` pinch has gone out for this
    /// manipulation, and so whether an `Ended` is owed when it finishes.
    pinching: bool,
}

impl Default for Manipulation {
    fn default() -> Self {
        Self {
            scale: 1.0,
            translation: Point::default(),
            pinching: false,
        }
    }
}

#[implement(IDirectManipulationViewportEventHandler)]
struct ViewportEventHandler {
    window: Weak<WindowsWindowInner>,
    manipulation: RefCell<Manipulation>,
}

impl ViewportEventHandler {
    fn new(window: Weak<WindowsWindowInner>) -> Self {
        Self {
            window,
            manipulation: RefCell::new(Manipulation::default()),
        }
    }

    fn emit(&self, window: &Rc<WindowsWindowInner>, input: PlatformInput) {
        let mut lock = window.state.borrow_mut();
        let Some(mut func) = lock.callbacks.input.take() else {
            return;
        };
        drop(lock);
        func(input);
        window.state.borrow_mut().callbacks.input = Some(func);
    }
}

#[allow(non_snake_case)]
impl IDirectManipulationViewportEventHandler_Impl for ViewportEventHandler_Impl {
    fn OnViewportStatusChanged(
        &self,
        viewport: windows::core::Ref<'_, IDirectManipulationViewport>,
        current: DIRECTMANIPULATION_STATUS,
        _previous: DIRECTMANIPULATION_STATUS,
    ) -> windows::core::Result<()> {
        if current == DIRECTMANIPULATION_RUNNING {
            *self.manipulation.borrow_mut() = Manipulation::default();
            return Ok(());
        }
        if current != DIRECTMANIPULATION_READY {
            return Ok(());
        }

        let Some(window) = self.window.upgrade() else {
            return Ok(());
        };

        // Coming to rest. Close out a pinch if one was open...
        if std::mem::take(&mut self.manipulation.borrow_mut().pinching) {
            let scale_factor = window.state.borrow().scale_factor;
            self.emit(
                &window,
                PlatformInput::Pinch(PinchEvent {
                    position: cursor_client_position(window.hwnd(), scale_factor),
                    delta: 1.0,
                    // Accumulated by `Window`.
                    scale: 1.0,
                    modifiers: current_modifiers(),
                    phase: TouchPhase::Ended,
                }),
            );
        }

        // ...and put the content transform back to identity, so the next
        // manipulation is measured from 1.0 rather than from wherever this one
        // left off.
        if let Some(viewport) = viewport.as_ref() {
            let rect = client_rect(window.hwnd());
            unsafe {
                viewport
                    .ZoomToRect(0.0, 0.0, rect.right as f32, rect.bottom as f32, false)
                    .log_err()
            };
        }
        Ok(())
    }

    fn OnViewportUpdated(
        &self,
        _viewport: windows::core::Ref<'_, IDirectManipulationViewport>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnContentUpdated(
        &self,
        _viewport: windows::core::Ref<'_, IDirectManipulationViewport>,
        content: windows::core::Ref<'_, IDirectManipulationContent>,
    ) -> windows::core::Result<()> {
        let content = content.ok()?;
        let Some(window) = self.window.upgrade() else {
            return Ok(());
        };

        // `[m11, m12, m21, m22, dx, dy]`. Direct Manipulation only ever
        // produces a uniform scale and a translation, so `m11` is the scale
        // and the rest of the linear part can be ignored.
        let mut transform = [0.0f32; 6];
        unsafe { content.GetContentTransform(&mut transform)? };
        let scale = transform[0];
        let translation = point(transform[4], transform[5]);

        let scale_factor = window.state.borrow().scale_factor;
        let position = cursor_client_position(window.hwnd(), scale_factor);
        let modifiers = current_modifiers();

        let mut manipulation = self.manipulation.borrow_mut();

        let translation_delta = translation - manipulation.translation;
        manipulation.translation = translation;

        // Guard the divisor so a degenerate zero scale cannot poison the
        // caller's zoom with a zero or infinite delta.
        let pinch = (scale.is_finite() && scale > 0.0)
            .then(|| scale / manipulation.scale)
            .filter(|delta| (delta - 1.0).abs() > SCALE_EPSILON)
            .map(|delta| {
                manipulation.scale = scale;
                let phase = if manipulation.pinching {
                    TouchPhase::Moved
                } else {
                    manipulation.pinching = true;
                    TouchPhase::Started
                };
                // The first event of a gesture reports no change, as every
                // other backend's does.
                match phase {
                    TouchPhase::Started => (1.0, phase),
                    _ => (delta, phase),
                }
            });

        drop(manipulation);

        if translation_delta.x != 0.0 || translation_delta.y != 0.0 {
            self.emit(
                &window,
                PlatformInput::ScrollWheel(ScrollWheelEvent {
                    position,
                    delta: ScrollDelta::Pixels(point(
                        px(translation_delta.x / scale_factor),
                        px(translation_delta.y / scale_factor),
                    )),
                    modifiers,
                    touch_phase: TouchPhase::Moved,
                }),
            );
        }

        if let Some((delta, phase)) = pinch {
            self.emit(
                &window,
                PlatformInput::Pinch(PinchEvent {
                    position,
                    delta,
                    // Accumulated by `Window`.
                    scale: 1.0,
                    modifiers,
                    phase,
                }),
            );
        }

        Ok(())
    }
}
