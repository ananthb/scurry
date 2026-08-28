//! Mouse capture on macOS, via a CoreGraphics event tap.
//!
//! # Permission
//!
//! A tap that can *modify* events needs Accessibility permission (System
//! Settings -> Privacy & Security -> Accessibility). Without it
//! `CGEventTapCreate` returns null. The error path says so, because the failure
//! is otherwise inscrutable.
//!
//! # Deltas, not cursor position
//!
//! We read `MOUSE_EVENT_DELTA_X/Y`, which is device motion. That is what makes
//! edge handoff work: once the cursor is pinned against the edge of the display
//! it stops moving and position-based capture would report nothing, but the
//! deltas keep coming because the mouse is still physically moving.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    EventField,
};
use scurry_proto::{button, kind, MouseState};

use crate::transport::Dongle;

extern "C" {
    /// Decouples the hardware mouse from the on-screen cursor, so the local
    /// cursor stays put while deltas keep arriving.
    fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
}

/// Node currently holding the pointer, as last announced by the dongle. 0 is
/// this machine.
static FOCUS: AtomicU8 = AtomicU8::new(0);
/// Mirrors `FOCUS != 0`, so the tap callback can decide without a load-and-compare.
static REMOTE: AtomicBool = AtomicBool::new(false);

/// Pin the local cursor while the pointer belongs to another machine.
///
/// Re-asserted on *every* remote event, not just at handoff. macOS re-associates
/// the cursor on its own -- on focus changes and assorted system events -- so a
/// one-shot call quietly stops holding and the local cursor starts tracking
/// again. Re-asserting is cheap and is the only version that stays put.
fn hold_cursor() {
    unsafe {
        CGAssociateMouseAndMouseCursorPosition(0);
    }
}

fn set_remote(remote: bool) {
    if REMOTE.swap(remote, Ordering::Relaxed) == remote {
        return;
    }
    unsafe {
        if remote {
            hold_cursor();
            // Hide it too. Decoupling stops the cursor moving but leaves it on
            // screen, which reads as a frozen Mac rather than as input having
            // gone elsewhere.
            CGDisplayHideCursor(CGMainDisplayID());
        } else {
            CGAssociateMouseAndMouseCursorPosition(1);
            CGDisplayShowCursor(CGMainDisplayID());
        }
    }
}

fn button_bit(event_type: CGEventType, event: &CGEvent) -> Option<u8> {
    Some(match event_type {
        CGEventType::LeftMouseDown | CGEventType::LeftMouseUp => button::LEFT,
        CGEventType::RightMouseDown | CGEventType::RightMouseUp => button::RIGHT,
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            match event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
                2 => button::MIDDLE,
                3 => button::BACK,
                4 => button::FORWARD,
                _ => return None,
            }
        }
        _ => return None,
    })
}

fn is_press(event_type: CGEventType) -> bool {
    matches!(
        event_type,
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown
    )
}

fn clamp_i8(v: i64) -> i8 {
    v.clamp(i8::MIN as i64, i8::MAX as i64) as i8
}

fn clamp_i16(v: i64) -> i16 {
    v.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

/// Capture local input and forward it to the dongle, forever.
pub fn run(dongle: Dongle) -> Result<()> {
    // The dongle owns the layout, so it must tell us where the pointer went;
    // we cannot work it out. A reader thread applies those announcements and
    // surfaces the firmware's log output, which shares this pipe.
    let reader = dongle.try_clone()?;
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut on_log = |line: &str| eprintln!("[dongle] {line}");
        loop {
            match reader.recv(Duration::from_millis(200), &mut on_log) {
                Ok(Some(msg)) if msg.kind == kind::FOCUS => {
                    if let Some(&node) = msg.payload.first() {
                        FOCUS.store(node, Ordering::Relaxed);
                        set_remote(node != 0);
                        eprintln!("focus -> node {node}");
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("dongle read error: {e}");
                    return;
                }
            }
        }
    });

    let shared = Arc::new(Mutex::new((dongle, 0u8))); // (link, button state)
    let tap_state = Arc::clone(&shared);

    let events = vec![
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::ScrollWheel,
    ];

    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        // Default, not ListenOnly: we must be able to swallow events once the
        // pointer is on another machine, or remote movement would drag the
        // local cursor along with it.
        CGEventTapOptions::Default,
        events,
        move |_proxy, event_type, event| {
            // macOS disables a tap that responds too slowly and silently stops
            // delivering events until it is re-enabled. Handling this is not
            // optional: without it capture dies under load and looks like a hang.
            if matches!(
                event_type,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                eprintln!("event tap disabled by the system; re-enabling");
                return Some(event.clone());
            }

            let mut guard = match tap_state.lock() {
                Ok(g) => g,
                Err(_) => return Some(event.clone()),
            };
            let (dongle, buttons) = &mut *guard;

            let mut state = MouseState::default();
            match event_type {
                CGEventType::ScrollWheel => {
                    state.wheel = clamp_i8(
                        event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1),
                    );
                    state.pan = clamp_i8(
                        event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2),
                    );
                }
                _ => {
                    state.dx =
                        clamp_i16(event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X));
                    state.dy =
                        clamp_i16(event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y));
                    if let Some(bit) = button_bit(event_type, event) {
                        if is_press(event_type) {
                            *buttons |= bit;
                        } else {
                            *buttons &= !bit;
                        }
                    }
                }
            }
            state.buttons = *buttons;

            let _ = dongle.send(kind::MOUSE, &state.encode());
            drop(guard);

            if REMOTE.load(Ordering::Relaxed) {
                hold_cursor(); // re-assert; see hold_cursor()
                None // swallow: this input belongs to another machine
            } else {
                Some(event.clone())
            }
        },
    )
    .map_err(|_| {
        anyhow!(
            "could not create the event tap.\n\
             This almost always means Accessibility permission is missing:\n\
             System Settings -> Privacy & Security -> Accessibility, and add the\n\
             binary (or your terminal, when running under cargo)."
        )
    })?;

    unsafe {
        let source = tap
            .mach_port
            .create_runloop_source(0)
            .map_err(|_| anyhow!("creating run loop source for the event tap"))?;
        CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
        tap.enable();
        eprintln!("capturing; move the pointer off a screen edge to hand over");
        CFRunLoop::run_current();
    }
    Ok(())
}
