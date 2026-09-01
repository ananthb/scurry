//! Mouse capture on macOS, via a CoreGraphics event tap.
//!
//! # Permission
//!
//! A tap that can *modify* events needs Accessibility permission (System
//! Settings -> Privacy & Security -> Accessibility). Without it
//! `CGEventTapCreate` returns null. The error path says so, because the failure
//! is otherwise inscrutable.
//!
//! # Why the tap is built by hand
//!
//! `CGEventTap::new` in core-graphics 0.24 takes a `Vec<CGEventType>`, and that
//! enum has no variants for the trackpad gesture types (29 Gesture, 30 Magnify,
//! 31 Swipe, 32 Rotate, 33 BeginGesture, 34 EndGesture/Pressure, 35
//! SmartMagnify). They were therefore absent from the mask, never delivered to
//! the callback, and never swallowed: pinch-zoom, rotate, two-finger swipe and
//! force-click all acted on the Mac while the pointer was supposed to be on
//! another machine. The wrapper cannot be made to carry them either -- it hands
//! the raw C event type to the closure as a `CGEventType`, so a value of 30
//! would be an invalid discriminant. So we call `CGEventTapCreate` ourselves
//! with a hand-built mask, and the callback matches on raw `u32`s and never
//! transmutes one into the enum.
//!
//! # What gets sent as motion
//!
//! While the pointer is on this machine we send how far the *visible cursor*
//! moved; while it is on another machine we send raw device counts. See
//! [`super::local_delta`] for why, and for the one case where a local event
//! still has to fall back to device counts.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Once};

use anyhow::{anyhow, Result};
use core_foundation::base::TCFType;
use core_foundation::mach_port::{CFMachPort, CFMachPortRef};
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop, CFRunLoopSource};
use core_graphics::event::{
    CGEventFlags, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, EventField,
};
use scurry_proto::{button, kind, modifier, KeyState, MouseState};

use crate::capture::{keymap, local_delta, Bounds};
use crate::ipc::DaemonState;
use crate::transport::Dongle;

/// An opaque `CGEventRef`. Deliberately not core-graphics' `CGEvent`: the
/// callback is handed a borrowed reference it does not own, and a wrapper whose
/// `Drop` released it would over-release the system's.
type CGEventRef = *mut c_void;

/// The event types we ask for, and match on, as raw numbers.
///
/// The gesture range has no counterpart in core-graphics' `CGEventType`, so the
/// whole set is spelled out here rather than half in the enum and half not.
mod ev {
    pub const LEFT_MOUSE_DOWN: u32 = 1;
    pub const LEFT_MOUSE_UP: u32 = 2;
    pub const RIGHT_MOUSE_DOWN: u32 = 3;
    pub const RIGHT_MOUSE_UP: u32 = 4;
    pub const MOUSE_MOVED: u32 = 5;
    pub const LEFT_MOUSE_DRAGGED: u32 = 6;
    pub const RIGHT_MOUSE_DRAGGED: u32 = 7;
    pub const KEY_DOWN: u32 = 10;
    pub const KEY_UP: u32 = 11;
    pub const FLAGS_CHANGED: u32 = 12;
    pub const SCROLL_WHEEL: u32 = 22;
    pub const OTHER_MOUSE_DOWN: u32 = 25;
    pub const OTHER_MOUSE_UP: u32 = 26;
    pub const OTHER_MOUSE_DRAGGED: u32 = 27;

    /// Trackpad gestures, in NSEvent numbering: Gesture, Magnify, Swipe,
    /// Rotate, BeginGesture, EndGesture -- which current systems reuse as
    /// Pressure, and which is what force-click arrives as -- and SmartMagnify.
    /// Contiguous, so the mask and the callback both treat them as one range.
    pub const GESTURE_FIRST: u32 = 29;
    pub const GESTURE_LAST: u32 = 35;

    pub const TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    pub const TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;

    /// Everything the tap asks the system for, gestures aside. The two
    /// TapDisabled types are deliberately absent: they arrive whatever the mask
    /// says.
    pub const WANTED: &[u32] = &[
        MOUSE_MOVED,
        LEFT_MOUSE_DRAGGED,
        RIGHT_MOUSE_DRAGGED,
        OTHER_MOUSE_DRAGGED,
        LEFT_MOUSE_DOWN,
        LEFT_MOUSE_UP,
        RIGHT_MOUSE_DOWN,
        RIGHT_MOUSE_UP,
        OTHER_MOUSE_DOWN,
        OTHER_MOUSE_UP,
        SCROLL_WHEEL,
        KEY_DOWN,
        KEY_UP,
        // Modifier presses arrive as FlagsChanged, not KeyDown, so without this
        // a bare Cmd or Shift would never reach the target.
        FLAGS_CHANGED,
    ];
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

/// The tap callback, as CoreGraphics calls it. The event type is a `u32`
/// because the values that arrive are not limited to the ones core-graphics'
/// enum names.
type TapCallback = unsafe extern "C" fn(
    proxy: *mut c_void,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

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
    fn CGDisplayBounds(display: u32) -> CGRect;

    fn CGEventTapCreate(
        tap: CGEventTapLocation,
        place: CGEventTapPlacement,
        options: CGEventTapOptions,
        events_of_interest: u64,
        callback: TapCallback,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(port: *mut c_void, enable: bool);

    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
}

fn field(event: CGEventRef, f: u32) -> i64 {
    unsafe { CGEventGetIntegerValueField(event, f) }
}

fn location(event: CGEventRef) -> CGPoint {
    unsafe { CGEventGetLocation(event) }
}

fn flags(event: CGEventRef) -> CGEventFlags {
    CGEventFlags::from_bits_truncate(unsafe { CGEventGetFlags(event) })
}

/// The tap's mach port, so the callback can re-enable itself.
///
/// Needed because the callback is registered before the tap exists, so it
/// cannot be handed the port. Set once, immediately after creation.
static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(core::ptr::null_mut());

/// Where the local cursor is parked while input belongs to another machine.
///
/// Set to wherever the cursor actually was at handoff, not to the middle of the
/// display: the pointer left at a screen edge and should still be there when it
/// comes back. Warping it to the centre made it visibly jump on every crossing.
static PARK_X: AtomicI32 = AtomicI32::new(0);
static PARK_Y: AtomicI32 = AtomicI32::new(0);

/// Where the cursor was at the previous local event. Both the park position and
/// the screen-space motion delta are measured from here.
static LAST_X: AtomicI32 = AtomicI32::new(0);
static LAST_Y: AtomicI32 = AtomicI32::new(0);

/// The main display's rectangle in points, cached so the hot path costs four
/// atomic loads rather than a CoreGraphics call per event.
///
/// This is the rectangle the cursor is considered pinned against, and it is in
/// the same units as the local screen in the layout: `CGEventGetLocation` is in
/// points for the main display, and the 1512x982 in `scurry.toml` is this Mac's
/// point size, not its pixel size.
///
/// With a second physical display attached the cursor can walk outside this
/// rectangle -- macOS's global coordinate space spans every display -- and
/// handoff would fire at the main display's edge rather than at the far edge of
/// the desktop. The layout already models this Mac as exactly one screen, so
/// that limitation is not new here; fixing it is a layout question rather than
/// a capture one.
static BOUNDS_X: AtomicI32 = AtomicI32::new(0);
static BOUNDS_Y: AtomicI32 = AtomicI32::new(0);
static BOUNDS_W: AtomicI32 = AtomicI32::new(1);
static BOUNDS_H: AtomicI32 = AtomicI32::new(1);

fn refresh_display_bounds() {
    let r = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    BOUNDS_X.store(r.origin.x as i32, Ordering::Relaxed);
    BOUNDS_Y.store(r.origin.y as i32, Ordering::Relaxed);
    BOUNDS_W.store((r.size.width as i32).max(1), Ordering::Relaxed);
    BOUNDS_H.store((r.size.height as i32).max(1), Ordering::Relaxed);
}

fn display_bounds() -> Bounds {
    Bounds {
        x: BOUNDS_X.load(Ordering::Relaxed),
        y: BOUNDS_Y.load(Ordering::Relaxed),
        width: BOUNDS_W.load(Ordering::Relaxed),
        height: BOUNDS_H.load(Ordering::Relaxed),
    }
}

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

/// Registered once per process. `install()` is retried until Accessibility is
/// granted, and each attempt used to push another copy of `restore_cursor` onto
/// the atexit list.
static CLEANUP: Once = Once::new();

fn install_cleanup() {
    CLEANUP.call_once(|| unsafe {
        libc::atexit(restore_cursor);
        libc::signal(libc::SIGINT, on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, on_signal as *const () as usize);
        libc::signal(libc::SIGHUP, on_signal as *const () as usize);
    });
}

/// Node currently holding the pointer, as last announced by the dongle. 0 is
/// this machine.
///
/// This is the *only* record of where the pointer is. There used to be a second
/// `REMOTE` bool mirroring `FOCUS != 0`, written from the same place and read by
/// the tap, with a `debug_assert_eq!` between the two that compiled to nothing
/// in release. Two statics that must agree is something to get wrong; one
/// static and a comparison is not, so the invariant is structural now rather
/// than asserted.
static FOCUS: AtomicU8 = AtomicU8::new(0);

/// True while the pointer belongs to another machine.
fn is_remote() -> bool {
    FOCUS.load(Ordering::Relaxed) != 0
}

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

/// Apply a FOCUS announcement from the dongle.
///
/// The firmware re-announces the current node periodically rather than only when
/// it changes, so a lost announcement self-heals instead of leaving this side
/// stale forever -- and a stale value meant the tap passed input through to this
/// Mac *and* forwarded it, which is what made both pointers move and clicks land
/// in two places. Repeats are therefore the common case, arriving at pointer
/// rate: everything past the swap runs only on a real change, or the log line
/// alone would be a flood.
pub(super) fn set_focus(node: u8) {
    let previous = FOCUS.swap(node, Ordering::Relaxed);
    if previous == node {
        return;
    }
    eprintln!("focus -> node {node}");
    if (previous != 0) == (node != 0) {
        // Moving between two remote machines. The cursor is already parked and
        // hidden, and re-doing either would make it flicker on every crossing.
        return;
    }

    unsafe {
        let display = CGMainDisplayID();
        if node != 0 {
            // Park where the cursor already is -- at the edge it left from.
            PARK_X.store(LAST_X.load(Ordering::Relaxed), Ordering::Relaxed);
            PARK_Y.store(LAST_Y.load(Ordering::Relaxed), Ordering::Relaxed);

            report(
                "CGAssociateMouseAndMouseCursorPosition(0)",
                CGAssociateMouseAndMouseCursorPosition(0),
            );
            report("CGDisplayHideCursor", CGDisplayHideCursor(display));
            // Hide as well as freeze: a stationary visible cursor reads as a
            // hung Mac rather than as input having gone elsewhere.
        } else {
            // `hold_cursor` warps the cursor, and a warp generates motion events
            // of its own. Those can still be in flight when focus comes back
            // here, so resume from the park point: the first local delta is then
            // zero rather than the size of the warp, and the warp's own motion
            // can never reach the dongle.
            LAST_X.store(PARK_X.load(Ordering::Relaxed), Ordering::Relaxed);
            LAST_Y.store(PARK_Y.load(Ordering::Relaxed), Ordering::Relaxed);
            // A display may have been rearranged or replaced while input was
            // elsewhere, and the edge test depends on its size.
            refresh_display_bounds();

            report(
                "CGAssociateMouseAndMouseCursorPosition(1)",
                CGAssociateMouseAndMouseCursorPosition(1),
            );
            report("CGDisplayShowCursor", CGDisplayShowCursor(display));
        }
    }
}

fn button_bit(etype: u32, event: CGEventRef) -> Option<u8> {
    Some(match etype {
        ev::LEFT_MOUSE_DOWN | ev::LEFT_MOUSE_UP => button::LEFT,
        ev::RIGHT_MOUSE_DOWN | ev::RIGHT_MOUSE_UP => button::RIGHT,
        ev::OTHER_MOUSE_DOWN | ev::OTHER_MOUSE_UP => {
            match field(event, EventField::MOUSE_EVENT_BUTTON_NUMBER) {
                2 => button::MIDDLE,
                3 => button::BACK,
                4 => button::FORWARD,
                _ => return None,
            }
        }
        _ => return None,
    })
}

fn is_press(etype: u32) -> bool {
    matches!(
        etype,
        ev::LEFT_MOUSE_DOWN | ev::RIGHT_MOUSE_DOWN | ev::OTHER_MOUSE_DOWN
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

/// Keys currently held, as HID usage codes.
///
/// Tracked here rather than derived per event because HID reports the whole set
/// every time: a report naming only the newest key would release everything
/// else the moment a second key went down.
static HELD: Mutex<[u8; 6]> = Mutex::new([0u8; 6]);

/// Translate macOS's modifier flags into the HID modifier byte.
///
/// The flags do not distinguish left from right, so everything maps to the left
/// variant. Targets treat them identically for shortcuts, and the alternative
/// -- tracking FlagsChanged keycodes to tell the sides apart -- buys nothing a
/// user would notice.
fn modifiers_from(flags: CGEventFlags) -> u8 {
    use core_graphics::event::CGEventFlags as F;
    let mut m = 0u8;
    if flags.contains(F::CGEventFlagShift) {
        m |= modifier::LSHIFT;
    }
    if flags.contains(F::CGEventFlagControl) {
        m |= modifier::LCTRL;
    }
    if flags.contains(F::CGEventFlagAlternate) {
        m |= modifier::LALT;
    }
    if flags.contains(F::CGEventFlagCommand) {
        m |= modifier::LGUI;
    }
    m
}

/// Record a key going down or coming up. Returns false if nothing changed, so
/// autorepeat does not flood the link with identical reports.
fn track_key(usage: u8, pressed: bool) -> bool {
    let Ok(mut held) = HELD.lock() else {
        return false;
    };
    if pressed {
        if held.contains(&usage) {
            return false; // already down: this is autorepeat
        }
        if let Some(slot) = held.iter_mut().find(|k| **k == 0) {
            *slot = usage;
            return true;
        }
        // Six keys is what the boot protocol report carries. Beyond that the
        // press is dropped rather than evicting a key that is genuinely held.
        false
    } else if let Some(slot) = held.iter_mut().find(|k| **k == usage) {
        *slot = 0;
        true
    } else {
        false
    }
}

fn current_keys() -> [u8; 6] {
    HELD.lock().map(|h| *h).unwrap_or([0u8; 6])
}

/// What the callback needs, behind the `user_info` pointer the tap carries.
struct Tap {
    link: Arc<Mutex<Dongle>>,
}

/// Opaque handle keeping capture alive. Dropping it stops input capture.
pub struct CaptureHandle {
    port: CFMachPort,
    source: CFRunLoopSource,
    runloop: CFRunLoop,
    /// The callback's context. Owned here rather than leaked, and freed only
    /// after the run loop source has been removed, so no callback can still be
    /// looking at it.
    ctx: *mut Tap,
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        unsafe {
            CGEventTapEnable(self.port.as_concrete_TypeRef() as *mut c_void, false);
            self.runloop
                .remove_source(&self.source, kCFRunLoopCommonModes);
            let _ = TAP_PORT.compare_exchange(
                self.port.as_concrete_TypeRef() as *mut c_void,
                core::ptr::null_mut(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
            drop(Box::from_raw(self.ctx));
        }
        // Nothing is going to re-assert the hold once capture is gone. Leaving
        // the cursor decoupled and hidden would look like the Mac had lost its
        // pointer entirely.
        restore_cursor();
    }
}

/// Install capture into the *current* run loop and return without blocking.
///
/// winit's event loop is a CFRunLoop, so the tray can host the event tap
/// directly instead of needing a separate process for it. That is what lets the
/// whole app be one binary the user drags to Applications: no daemon to
/// install, no service manager, no port contention, because there is only one
/// process and it owns the port.
///
/// The returned handle must be kept alive; dropping it stops capture.
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
    refresh_display_bounds();

    // The tap is created first. This used to start the reader thread before the
    // tap and return early when the tap could not be made -- the ordinary state
    // until the user grants Accessibility, retried every couple of seconds -- so
    // every failed attempt left another reader behind on its own dup of the
    // serial fd. `watch` is idempotent now, but the order still means a failed
    // install starts nothing that was not already running.
    let tap = create_tap(Arc::clone(&dongle))?;
    super::watch(&dongle, &state)?;
    Ok(tap)
}

fn create_tap(link: Arc<Mutex<Dongle>>) -> Result<CaptureHandle> {
    // Gestures go in as a range rather than named one by one: the point is that
    // every type in it reaches the callback so it can be swallowed, not that we
    // do anything different with each.
    let mask: u64 = ev::WANTED
        .iter()
        .copied()
        .chain(ev::GESTURE_FIRST..=ev::GESTURE_LAST)
        .fold(0u64, |m, t| m | (1u64 << t));
    let ctx = Box::into_raw(Box::new(Tap { link }));

    let port = unsafe {
        CGEventTapCreate(
            CGEventTapLocation::HID,
            CGEventTapPlacement::HeadInsertEventTap,
            // Default, not ListenOnly: we must be able to swallow events once
            // the pointer is on another machine, or remote movement would drag
            // the local cursor along with it.
            CGEventTapOptions::Default,
            mask,
            on_event,
            ctx as *mut c_void,
        )
    };
    if port.is_null() {
        unsafe { drop(Box::from_raw(ctx)) };
        return Err(anyhow!(
            "could not create the event tap.\n\
             This almost always means Accessibility permission is missing:\n\
             System Settings -> Privacy & Security -> Accessibility, and add the\n\
             binary (or your terminal, when running under cargo)."
        ));
    }

    let port = unsafe { CFMachPort::wrap_under_create_rule(port) };
    let Ok(source) = port.create_runloop_source(0) else {
        unsafe { drop(Box::from_raw(ctx)) };
        return Err(anyhow!("creating run loop source for the event tap"));
    };

    let runloop = CFRunLoop::get_current();
    unsafe {
        TAP_PORT.store(port.as_concrete_TypeRef() as *mut c_void, Ordering::Relaxed);
        runloop.add_source(&source, kCFRunLoopCommonModes);
        CGEventTapEnable(port.as_concrete_TypeRef() as *mut c_void, true);
    }
    Ok(CaptureHandle {
        port,
        source,
        runloop,
        ctx,
    })
}

/// # Safety
///
/// Called only by CoreGraphics, with the `user_info` pointer given to
/// `CGEventTapCreate`. That pointer stays valid for as long as the run loop
/// source is registered, and dropping the [`CaptureHandle`] removes the source
/// before freeing it.
unsafe extern "C" fn on_event(
    _proxy: *mut c_void,
    etype: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let tap = &*(user_info as *const Tap);
    if handle(etype, event, tap) {
        event
    } else {
        // Null swallows it: the event never reaches the rest of the system.
        core::ptr::null_mut()
    }
}

/// Returns true to pass the event through to this machine, false to swallow it.
fn handle(etype: u32, event: CGEventRef, tap: &Tap) -> bool {
    // macOS disables a tap that responds too slowly and silently stops
    // delivering events until it is re-enabled. Handling this is not optional:
    // without it capture dies under load and looks like a hang.
    if etype == ev::TAP_DISABLED_BY_TIMEOUT || etype == ev::TAP_DISABLED_BY_USER_INPUT {
        // Actually re-enable it. Logging alone left the tap dead: every
        // subsequent event passed through untouched, so input stopped reaching
        // the target and started acting on this machine again. That is what the
        // intermittent jank and the stray scrolling were.
        let port = TAP_PORT.load(Ordering::Relaxed);
        if !port.is_null() {
            unsafe { CGEventTapEnable(port, true) };
            eprintln!("event tap was disabled by the system; re-enabled");
        } else {
            eprintln!("event tap disabled and no port to re-enable it with");
        }
        return true;
    }

    // The invariant: exactly one machine consumes any given event. Passing one
    // through locally while the dongle is routing it to a target is what makes
    // two pointers move at once and clicks land in both places.
    let remote = is_remote();

    // Keyboard is a separate message and returns early: it shares only the
    // swallow decision with the pointer path.
    if matches!(etype, ev::KEY_DOWN | ev::KEY_UP | ev::FLAGS_CHANGED) {
        if !remote {
            // Forget anything we thought was held, so returning from a remote
            // screen does not leave a phantom key down there.
            if let Ok(mut held) = HELD.lock() {
                *held = [0u8; 6];
            }
            return true;
        }
        let vk = field(event, EventField::KEYBOARD_EVENT_KEYCODE);
        let changed = match (etype, keymap::hid_usage(vk)) {
            (ev::KEY_DOWN, Some(u)) => track_key(u, true),
            (ev::KEY_UP, Some(u)) => track_key(u, false),
            // FlagsChanged carries no keycode worth sending; the modifier byte
            // below already reflects it.
            (ev::FLAGS_CHANGED, _) => true,
            _ => false,
        };
        if changed {
            let ks = KeyState {
                modifiers: modifiers_from(flags(event)),
                keys: current_keys(),
            };
            if let Ok(mut l) = tap.link.lock() {
                let _ = l.send(kind::KEY, &ks.encode());
            }
        }
        hold_cursor();
        return false; // typing belongs to the other machine
    }

    // Trackpad gestures: pinch-zoom, rotate, two-finger swipe, force-click.
    // The dongle is a BLE HID *mouse* with no gesture channel, so there is
    // nothing meaningful to forward and nothing is sent. They are swallowed all
    // the same, because acting on the Mac while the user is looking at another
    // machine's screen is the bug -- and that is exactly what they did for as
    // long as they were missing from the tap's mask.
    if (ev::GESTURE_FIRST..=ev::GESTURE_LAST).contains(&etype) {
        if remote {
            hold_cursor(); // re-assert; see hold_cursor()
            return false;
        }
        return true;
    }

    let mut state = MouseState::default();
    if etype == ev::SCROLL_WHEEL {
        state.wheel = clamp_i8(field(event, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1));
        state.pan = clamp_i8(field(event, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2));
    } else {
        let device = (
            clamp_i16(field(event, EventField::MOUSE_EVENT_DELTA_X)) as i32,
            clamp_i16(field(event, EventField::MOUSE_EVENT_DELTA_Y)) as i32,
        );
        let (dx, dy) = if remote {
            // The cursor is decoupled and pinned by hold_cursor(), so where it
            // is says nothing about how far the mouse moved. Device counts are
            // all there is, and they are what the target's own pointer
            // acceleration expects anyway.
            device
        } else {
            let loc = location(event);
            let now = (loc.x as i32, loc.y as i32);
            let prev = (
                LAST_X.load(Ordering::Relaxed),
                LAST_Y.load(Ordering::Relaxed),
            );
            // Stored before the send: the dongle answers a crossing with FOCUS,
            // and the park position is read from here the moment it does.
            LAST_X.store(now.0, Ordering::Relaxed);
            LAST_Y.store(now.1, Ordering::Relaxed);
            local_delta(prev, now, device, display_bounds())
        };
        state.dx = clamp_i16(dx as i64);
        state.dy = clamp_i16(dy as i64);

        if let Some(bit) = button_bit(etype, event) {
            if is_press(etype) {
                BUTTONS.fetch_or(bit, Ordering::Relaxed);
            } else {
                BUTTONS.fetch_and(!bit, Ordering::Relaxed);
            }
        }
    }
    state.buttons = BUTTONS.load(Ordering::Relaxed);

    // Held only for the write. The control socket wants this lock too, and
    // blocking a tray query behind pointer motion would make the settings
    // window feel stuck.
    if let Ok(mut l) = tap.link.lock() {
        let _ = l.send(kind::MOUSE, &state.encode());
    }

    if remote {
        hold_cursor(); // re-assert; see hold_cursor()
        false // swallow: this input belongs to another machine
    } else {
        true
    }
}

/// Whether macOS has granted this process Accessibility rights.
///
/// `prompt` shows the system's own dialog, which offers to open the right pane
/// of System Settings. Far better than printing instructions and hoping: the
/// OS asks in its own words, and the user is one click from the switch.
pub fn accessibility_trusted(prompt: bool) -> bool {
    use core_foundation::base::CFTypeRef;
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
