//! Mouse capture on macOS, via a CoreGraphics event tap.
//!
//! # Permission
//!
//! Creating an event tap that can *modify* events needs Accessibility
//! permission (System Settings -> Privacy & Security -> Accessibility). Without
//! it `CGEventTapCreate` returns null and we cannot capture anything. The error
//! path says so explicitly, because the failure is otherwise inscrutable.
//!
//! # Why deltas rather than cursor position
//!
//! We read `EventField::MOUSE_EVENT_DELTA_X/Y`, which is device motion, not cursor
//! position. That distinction is what makes edge handoff work at all: once the
//! pointer is pinned against the edge of the display, the cursor stops moving
//! and position-based capture would report nothing, but the deltas keep coming
//! because the mouse is still physically moving.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    EventField,
};
use scurry_proto::{button, MouseState, Payload};

use crate::layout::{Layout, Motion};
use crate::transport::SerialTransport;

extern "C" {
    /// Decouples the hardware mouse from the on-screen cursor. While the
    /// pointer is on a remote screen we stop the local cursor from moving, but
    /// keep receiving deltas.
    fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
}

struct Shared {
    layout: Layout,
    transport: SerialTransport,
    buttons: u8,
}

/// Set while the pointer is on a remote screen, so the tap callback can decide
/// whether to swallow the event without taking the lock first.
static REMOTE: AtomicBool = AtomicBool::new(false);

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

/// Capture local mouse input and route it across `layout` forever.
pub fn run(layout: Layout, transport: SerialTransport) -> Result<()> {
    let shared = Mutex::new(Shared { layout, transport, buttons: 0 });

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
        // pointer is on another machine, or every remote movement would also
        // move the local cursor.
        CGEventTapOptions::Default,
        events,
        move |_proxy, event_type, event| {
            // macOS disables a tap that takes too long to respond, and silently
            // stops delivering events until it is re-enabled. Handling this is
            // not optional: without it capture dies under load and looks like a
            // hang.
            if matches!(
                event_type,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                log_reenable();
                return Some(event.clone());
            }

            let mut s = match shared.lock() {
                Ok(s) => s,
                Err(_) => return Some(event.clone()),
            };

            let mut wheel = 0i8;
            let mut pan = 0i8;
            let (mut dx, mut dy) = (0i32, 0i32);

            match event_type {
                CGEventType::ScrollWheel => {
                    wheel = clamp_i8(event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1));
                    pan = clamp_i8(event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2));
                }
                _ => {
                    dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as i32;
                    dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as i32;
                    if let Some(bit) = button_bit(event_type, event) {
                        if is_press(event_type) {
                            s.buttons |= bit;
                        } else {
                            s.buttons &= !bit;
                        }
                    }
                }
            }

            let motion = s.layout.advance(dx, dy);
            let buttons = s.buttons;

            match motion {
                Motion::Crossed { from, to, edge, ratio, .. } => {
                    // Order matters. Release on the machine being left before
                    // announcing arrival, or a drag across the boundary strands
                    // a held button on the departed machine.
                    if from != crate::layout::Screen::LOCAL {
                        let _ = s.transport.send_to(from, Payload::Leave);
                    }
                    if to != crate::layout::Screen::LOCAL {
                        let _ = s.transport.send_to(to, Payload::Enter { edge, ratio });
                    }
                    set_remote(to != crate::layout::Screen::LOCAL);
                }
                Motion::Stayed { node, .. } => {
                    if node != crate::layout::Screen::LOCAL {
                        let st = MouseState {
                            buttons,
                            dx: clamp_i16(dx),
                            dy: clamp_i16(dy),
                            wheel,
                            pan,
                        };
                        let _ = s.transport.send_to(node, Payload::Mouse(st));
                    }
                }
            }

            if REMOTE.load(Ordering::Relaxed) {
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

fn set_remote(remote: bool) {
    let was = REMOTE.swap(remote, Ordering::Relaxed);
    if was != remote {
        // Decouple the cursor while remote so it stops travelling on this
        // display, while the device keeps producing deltas for us to forward.
        unsafe { CGAssociateMouseAndMouseCursorPosition(i32::from(!remote)) };
    }
}

fn log_reenable() {
    eprintln!("event tap was disabled by the system; re-enabling");
}

fn clamp_i8(v: i64) -> i8 {
    v.clamp(i8::MIN as i64, i8::MAX as i64) as i8
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}
