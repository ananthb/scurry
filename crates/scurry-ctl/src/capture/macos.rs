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

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    EventField,
};
use scurry_proto::{button, kind, MouseState};

use crate::ipc::DaemonState;
use crate::transport::Dongle;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

extern "C" {
    /// Decouples the hardware mouse from the on-screen cursor, so the local
    /// cursor stays put while deltas keep arriving. Returns a CGError; 0 is
    /// success. We check it -- assuming success is how the cursor kept moving
    /// while the pointer was supposedly on another machine.
    fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
    fn CGWarpMouseCursorPosition(point: CGPoint) -> i32;
    fn CGEventTapEnable(port: *mut core::ffi::c_void, enable: bool);
}

/// The tap's mach port, so the callback can re-enable itself.
///
/// Needed because the closure is built before the tap exists, so it cannot
/// capture it. Set once, immediately after creation.
static TAP_PORT: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(core::ptr::null_mut());

/// Where the local cursor is parked while input belongs to another machine.
///
/// Set to wherever the cursor actually was at handoff, not to the middle of the
/// display: the pointer left at a screen edge and should still be there when it
/// comes back. Warping it to the centre made it visibly jump on every crossing.
static PARK_X: AtomicI32 = AtomicI32::new(0);
static PARK_Y: AtomicI32 = AtomicI32::new(0);

/// Last cursor position seen while input was still local, which is what the
/// park position is taken from.
static LAST_X: AtomicI32 = AtomicI32::new(0);
static LAST_Y: AtomicI32 = AtomicI32::new(0);

/// Logged once per transition rather than per event, so a failing call is
/// visible without flooding.
fn report(call: &str, err: i32) {
    if err != 0 {
        eprintln!("warning: {call} failed with CGError {err}");
    }
}

/// Put the cursor back under the user's control. Safe to call repeatedly.
///
/// Must run on every exit path, including signals: leaving the cursor hidden
/// and decoupled would strand the user with a Mac that appears to have no
/// pointer at all, recoverable only by logging out.
extern "C" fn restore_cursor() {
    unsafe {
        CGAssociateMouseAndMouseCursorPosition(1);
        CGDisplayShowCursor(CGMainDisplayID());
    }
}

extern "C" fn on_signal(_sig: i32) {
    restore_cursor();
    // _exit, not exit: async-signal-safe, and destructors would race here.
    unsafe { libc::_exit(130) };
}

fn install_cleanup() {
    unsafe {
        libc::atexit(restore_cursor);
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGHUP, on_signal as *const () as usize);
    }
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
        // Belt and braces. Decoupling alone proved not to hold in practice, so
        // also pin the cursor to where it was parked. Warping is authoritative:
        // whatever moved it, this puts it back.
        CGWarpMouseCursorPosition(CGPoint {
            x: PARK_X.load(Ordering::Relaxed) as f64,
            y: PARK_Y.load(Ordering::Relaxed) as f64,
        });
    }
}

fn set_remote(remote: bool) {
    if REMOTE.swap(remote, Ordering::Relaxed) == remote {
        return;
    }
    unsafe {
        let display = CGMainDisplayID();
        if remote {
            // Park where the cursor already is -- at the edge it left from.
            PARK_X.store(LAST_X.load(Ordering::Relaxed), Ordering::Relaxed);
            PARK_Y.store(LAST_Y.load(Ordering::Relaxed), Ordering::Relaxed);

            report("CGAssociateMouseAndMouseCursorPosition(0)",
                   CGAssociateMouseAndMouseCursorPosition(0));
            report("CGDisplayHideCursor", CGDisplayHideCursor(display));
            // Hide as well as freeze: a stationary visible cursor reads as a
            // hung Mac rather than as input having gone elsewhere.
        } else {
            report("CGAssociateMouseAndMouseCursorPosition(1)",
                   CGAssociateMouseAndMouseCursorPosition(1));
            report("CGDisplayShowCursor", CGDisplayShowCursor(display));
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

/// Held button state. An atomic rather than living beside the link, so the
/// callback does not need the connection lock just to update a bitmask.
static BUTTONS: AtomicU8 = AtomicU8::new(0);

/// Opaque handle keeping capture alive. Dropping it stops input capture.
///
/// Aliased so callers do not need core-graphics in their own dependency list
/// just to name the thing they are holding.
pub type CaptureHandle = CGEventTap<'static>;

/// Install capture into the *current* run loop and return without blocking.
///
/// winit's event loop is a CFRunLoop, so the tray can host the event tap
/// directly instead of needing a separate process for it. That is what lets the
/// whole app be one binary the user drags to Applications: no daemon to
/// install, no service manager, no port contention, because there is only one
/// process and it owns the port.
///
/// The returned tap must be kept alive; dropping it stops capture.
pub fn install(dongle: Arc<Mutex<Dongle>>, state: Arc<DaemonState>) -> Result<CaptureHandle> {
    build(dongle, state)
}

/// Capture local input and forward it to the dongle, blocking forever.
///
/// For headless use, where there is no other run loop to join.
pub fn run(dongle: Arc<Mutex<Dongle>>, state: Arc<DaemonState>) -> Result<()> {
    let _tap = build(dongle, state)?;
    eprintln!("capturing; move the pointer off a screen edge to hand over");
    CFRunLoop::run_current();
    Ok(())
}

fn build(dongle: Arc<Mutex<Dongle>>, state: Arc<DaemonState>) -> Result<CaptureHandle> {
    install_cleanup();

    // The dongle owns the layout, so it must tell us where the pointer went;
    // we cannot work it out. A reader thread applies those announcements and
    // surfaces the firmware's log output, which shares this pipe.
    let reader = {
        let guard = dongle.lock().map_err(|_| anyhow!("dongle lock poisoned"))?;
        guard.try_clone()?
    };
    let focus_state = Arc::clone(&state);
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut on_log = |line: &str| eprintln!("[dongle] {line}");
        loop {
            match reader.recv(Duration::from_millis(200), &mut on_log) {
                Ok(Some(msg)) if msg.kind == kind::FOCUS => {
                    if let Some(&node) = msg.payload.first() {
                        FOCUS.store(node, Ordering::Relaxed);
                        focus_state.focus.store(node, Ordering::Relaxed);
                        set_remote(node != 0);
                        eprintln!("focus -> node {node}");
                    }
                }
                // Anything that is not a focus announcement is somebody's
                // reply; hand it to whoever is waiting.
                Ok(Some(msg)) => focus_state.deliver(msg.kind, msg.payload),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("dongle read error: {e}");
                    return;
                }
            }
        }
    });

    let tap_link = Arc::clone(&dongle);

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
                // Actually re-enable it. Logging alone left the tap dead: every
                // subsequent event passed through untouched, so input stopped
                // reaching the target and started acting on this machine again.
                // That is what the intermittent jank and the stray scrolling
                // were.
                let port = TAP_PORT.load(Ordering::Relaxed);
                if !port.is_null() {
                    unsafe { CGEventTapEnable(port, true) };
                    eprintln!("event tap was disabled by the system; re-enabled");
                } else {
                    eprintln!("event tap disabled and no port to re-enable it with");
                }
                return Some(event.clone());
            }

            if !REMOTE.load(Ordering::Relaxed) {
                let loc = event.location();
                LAST_X.store(loc.x as i32, Ordering::Relaxed);
                LAST_Y.store(loc.y as i32, Ordering::Relaxed);
            }

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
                            BUTTONS.fetch_or(bit, Ordering::Relaxed);
                        } else {
                            BUTTONS.fetch_and(!bit, Ordering::Relaxed);
                        }
                    }
                }
            }
            state.buttons = BUTTONS.load(Ordering::Relaxed);

            // Held only for the write. The control socket wants this lock too,
            // and blocking a tray query behind pointer motion would make the
            // settings window feel stuck.
            if let Ok(mut link) = tap_link.lock() {
                let _ = link.send(kind::MOUSE, &state.encode());
            }

            // The invariant: exactly one machine consumes any given event.
            // Passing it through locally while the dongle is routing it to a
            // target is what makes two pointers move at once, which must never
            // happen. FOCUS is the only thing that keeps the two ends in
            // agreement, so a disagreement here means it was lost.
            let remote = REMOTE.load(Ordering::Relaxed);
            debug_assert_eq!(
                remote,
                FOCUS.load(Ordering::Relaxed) != 0,
                "REMOTE and FOCUS disagree: both pointers would move"
            );
            if remote {
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
        TAP_PORT.store(
            tap.mach_port.as_concrete_TypeRef() as *mut core::ffi::c_void,
            Ordering::Relaxed,
        );
        let source = tap
            .mach_port
            .create_runloop_source(0)
            .map_err(|_| anyhow!("creating run loop source for the event tap"))?;
        CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
        tap.enable();
    }
    Ok(tap)
}

/// Whether macOS has granted this process Accessibility rights.
///
/// `prompt` shows the system's own dialog, which offers to open the right pane
/// of System Settings. Far better than printing instructions and hoping: the
/// OS asks in its own words, and the user is one click from the switch.
pub fn accessibility_trusted(prompt: bool) -> bool {
    use core_foundation::base::{CFTypeRef, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: CFTypeRef) -> bool;
        static kAXTrustedCheckOptionPrompt: core_foundation::string::CFStringRef;
    }
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let value = core_foundation::boolean::CFBoolean::from(prompt);
        let opts = CFDictionary::from_CFType_pairs(&[(key, value)]);
        AXIsProcessTrustedWithOptions(opts.as_CFTypeRef())
    }
}
