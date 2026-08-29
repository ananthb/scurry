//! C ABI over the layout engine, so the dongle routes with the same code the
//! controller is tested against rather than a hand-written reimplementation.
//!
//! Everything here is a thin shim. The logic lives in `scurry-layout`, which is
//! `no_std`, allocation-free and covered by tests that run on the host.

#![no_std]

use core::sync::atomic::{AtomicBool, Ordering};

use scurry_layout::{Layout, Motion, Screen};
use scurry_proto::{mouse_flag, InputProfile, ScreenWire, MAX_SCREENS, SCREEN_WIRE_LEN};

/// Result of feeding motion in, as seen from C.
#[repr(C)]
pub struct ScurryRoute {
    /// 1 when the pointer changed screens.
    pub crossed: u8,
    /// Node being left. Only meaningful when `crossed`.
    pub from: u8,
    /// Node now holding the pointer. 0 means the controller's own screen, so
    /// the dongle should transmit nothing.
    pub to: u8,
    /// Arrival edge, encoding [`scurry_proto::Edge`]. Only when `crossed`.
    pub edge: u8,
    /// Position along that edge. Only when `crossed`.
    pub ratio: u16,
    /// Pointer position within `to`, normalised to 0..=32767.
    ///
    /// The dongle sends this as an absolute HID coordinate rather than a
    /// relative nudge. Relative motion requires dead reckoning -- assuming our
    /// model of the remote pointer matches reality -- and that assumption is
    /// false the moment anything else moves the pointer, or as soon as the
    /// target applies its own pointer acceleration to our deltas. Absolute
    /// coordinates have no such failure mode.
    pub abs_x: u16,
    pub abs_y: u16,
}

/// Normalise a coordinate within a span to 0..=32767, the HID logical range.
fn normalise(offset: i32, span: i32) -> u16 {
    if span <= 1 {
        return 0;
    }
    let clamped = offset.clamp(0, span - 1) as i64;
    ((clamped * 32767) / (span - 1) as i64) as u16
}

/// The single layout. The firmware is single-threaded over this: only the
/// reader task touches it, so a plain static is sufficient and an allocator
/// would buy nothing.
static mut LAYOUT: Option<Layout> = None;
static CONFIGURED: AtomicBool = AtomicBool::new(false);

/// Which node each Bluetooth address belongs to, from the stored layout.
///
/// Kept beside the layout rather than inside it: the layout is about geometry,
/// and which physical machine sits at a screen is a separate question it does
/// not need to answer.
static mut PINS: [([u8; 6], u8); MAX_SCREENS] = [([0u8; 6], 0); MAX_SCREENS];
static mut PIN_COUNT: usize = 0;

/// Input profile per node id, indexed directly by node so a lookup is an array
/// read on the hot path rather than a search.
static mut PROFILES: [InputProfile; MAX_SCREENS] = [InputProfile::identity(); MAX_SCREENS];

fn edge_to_wire(e: scurry_proto::Edge) -> u8 {
    match e {
        scurry_proto::Edge::Left => 0,
        scurry_proto::Edge::Right => 1,
        scurry_proto::Edge::Top => 2,
        scurry_proto::Edge::Bottom => 3,
    }
}

/// Install a layout from a CONFIG payload: one count byte then that many
/// screens.
///
/// Returns 0 on success, or a negative code. Rejecting here rather than at the
/// caller means an invalid layout can never be committed to storage.
///
/// # Safety
/// `data` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn scurry_layout_load(data: *const u8, len: usize) -> i32 {
    if data.is_null() || len < 1 {
        return -1;
    }
    let buf = core::slice::from_raw_parts(data, len);
    let count = buf[0] as usize;
    if count == 0 || count > MAX_SCREENS {
        return -2;
    }
    if buf.len() < 1 + count * SCREEN_WIRE_LEN {
        return -3;
    }

    let mut screens = [Screen::new(0, "", 0, 0, 1, 1); MAX_SCREENS];
    // Reset first: a node dropped from the layout must not keep the profile it
    // had under the previous one.
    PROFILES = [InputProfile::identity(); MAX_SCREENS];
    let mut pins = 0usize;
    for i in 0..count {
        let off = 1 + i * SCREEN_WIRE_LEN;
        let w = match ScreenWire::decode(&buf[off..]) {
            Some(w) => w,
            None => return -4,
        };
        if w.width <= 0 || w.height <= 0 {
            return -5;
        }
        screens[i] = Screen::new(w.node, w.name_str(), w.x, w.y, w.width, w.height);
        if (w.node as usize) < MAX_SCREENS {
            PROFILES[w.node as usize] = w.profile;
        }
        if w.is_pinned() {
            PINS[pins] = (w.bda, w.node);
            pins += 1;
        }
    }
    PIN_COUNT = pins;

    match Layout::new(&screens[..count]) {
        Ok(l) => {
            LAYOUT = Some(l);
            CONFIGURED.store(true, Ordering::Relaxed);
            0
        }
        Err(_) => -6,
    }
}

/// Serialise the current layout back into a CONFIG payload.
///
/// Returns the number of bytes written, or a negative code.
///
/// # Safety
/// `out` must point to `cap` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn scurry_layout_save(out: *mut u8, cap: usize) -> i32 {
    if out.is_null() {
        return -1;
    }
    let layout = match &*core::ptr::addr_of!(LAYOUT) {
        Some(l) => l,
        None => return -2,
    };
    let screens = layout.screens();
    let need = 1 + screens.len() * SCREEN_WIRE_LEN;
    if cap < need {
        return -3;
    }
    let buf = core::slice::from_raw_parts_mut(out, cap);
    buf[0] = screens.len() as u8;
    for (i, s) in screens.iter().enumerate() {
        // Carry the pin back out, or a read-modify-write through the settings
        // pane would silently unpin every screen.
        let mut bda = [0u8; 6];
        let pins = &*core::ptr::addr_of!(PINS);
        for (pinned, node) in pins.iter().take(PIN_COUNT) {
            if *node == s.node {
                bda = *pinned;
            }
        }
        let mut w = ScreenWire::pinned(s.node, s.name(), s.x, s.y, s.width, s.height, bda);
        // Carry the profile back out, or reading and rewriting the config
        // through the settings pane would quietly reset every translation.
        if (s.node as usize) < MAX_SCREENS {
            w.profile = (*core::ptr::addr_of!(PROFILES))[s.node as usize];
        }
        w.encode_into(&mut buf[1 + i * SCREEN_WIRE_LEN..]);
    }
    need as i32
}

/// A mouse report after the target's profile has been applied.
#[repr(C)]
pub struct ScurryMouse {
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
    pub wheel: i8,
    pub pan: i8,
}

fn profile_for(node: u8) -> InputProfile {
    unsafe {
        let profiles = &*core::ptr::addr_of!(PROFILES);
        if (node as usize) < MAX_SCREENS {
            profiles[node as usize]
        } else {
            InputProfile::identity()
        }
    }
}

/// Translate a mouse report for a target: pointer scaling, axis inversion,
/// scroll direction, button swapping.
///
/// Done here rather than in C so the arithmetic stays in the crate the host
/// tests cover.
///
/// # Safety
/// `out` must point to a writable `ScurryMouse`.
#[no_mangle]
pub unsafe extern "C" fn scurry_map_mouse(
    node: u8,
    buttons: u8,
    dx: i16,
    dy: i16,
    wheel: i8,
    pan: i8,
    out: *mut ScurryMouse,
) -> i32 {
    if out.is_null() {
        return -1;
    }
    let p = profile_for(node);
    let (dx, dy) = p.map_motion(dx, dy);

    let mut buttons = buttons;
    if p.mouse_flags & mouse_flag::SWAP_BUTTONS != 0 {
        let left = buttons & scurry_proto::button::LEFT != 0;
        let right = buttons & scurry_proto::button::RIGHT != 0;
        buttons &= !(scurry_proto::button::LEFT | scurry_proto::button::RIGHT);
        if left {
            buttons |= scurry_proto::button::RIGHT;
        }
        if right {
            buttons |= scurry_proto::button::LEFT;
        }
    }

    let invert = p.mouse_flags & mouse_flag::NATURAL_SCROLL != 0;
    let (wheel, pan) = if invert {
        (wheel.saturating_neg(), pan.saturating_neg())
    } else {
        (wheel, pan)
    };

    core::ptr::write(out, ScurryMouse { buttons, dx, dy, wheel, pan });
    0
}

/// Translate a host modifier byte into the one this target expects.
///
/// This is what makes a Mac usable against a PC-style target: the host sends
/// Cmd where Linux and ChromeOS want Ctrl.
#[no_mangle]
pub extern "C" fn scurry_map_modifiers(node: u8, host: u8) -> u8 {
    profile_for(node).map_modifiers(host)
}

/// The node pinned to this Bluetooth address, or -1 if none is.
///
/// Lets the dongle give a reconnecting machine the same node id every time,
/// instead of whichever slot happened to be free. Without it, two screens
/// silently swap whenever the connection race resolves the other way.
///
/// # Safety
/// `bda` must point to six readable bytes.
#[no_mangle]
pub unsafe extern "C" fn scurry_layout_node_for_address(bda: *const u8) -> i32 {
    if bda.is_null() {
        return -1;
    }
    let addr = core::slice::from_raw_parts(bda, 6);
    let pins = &*core::ptr::addr_of!(PINS);
    for (pinned, node) in pins.iter().take(PIN_COUNT) {
        if pinned == addr {
            return *node as i32;
        }
    }
    -1
}

/// Declare which nodes can currently receive input, as a bitmask where bit N
/// means node N. Node 0 is always available regardless.
///
/// Without this the pointer crosses onto machines that are not connected, which
/// presents as the pointer sticking to a screen edge and going dead.
///
/// # Safety
/// Safe to call at any time; takes effect on the next advance.
#[no_mangle]
pub extern "C" fn scurry_layout_set_available(mask: u32) {
    unsafe {
        if let Some(l) = &mut *core::ptr::addr_of_mut!(LAYOUT) {
            l.set_available(mask);
        }
    }
}

/// True once a layout has been installed. Until then the dongle has nowhere to
/// route input and should drop it rather than guess.
#[no_mangle]
pub extern "C" fn scurry_layout_ready() -> u8 {
    u8::from(CONFIGURED.load(Ordering::Relaxed))
}

/// Feed relative motion in and learn where it landed.
///
/// # Safety
/// `out` must point to a writable `ScurryRoute`.
#[no_mangle]
pub unsafe extern "C" fn scurry_layout_advance(dx: i32, dy: i32, out: *mut ScurryRoute) -> i32 {
    if out.is_null() {
        return -1;
    }
    let layout = match &mut *core::ptr::addr_of_mut!(LAYOUT) {
        Some(l) => l,
        None => return -2,
    };
    let motion = layout.advance(dx, dy);

    // Position is read after the move, from the screen now holding the pointer,
    // so the normalisation uses the destination's dimensions.
    let active = layout.active();
    let (px, py) = layout.position();
    let abs_x = normalise(px - active.x, active.width);
    let abs_y = normalise(py - active.y, active.height);

    let r = match motion {
        Motion::Stayed { node, .. } => ScurryRoute {
            crossed: 0,
            from: node,
            to: node,
            edge: 0,
            ratio: 0,
            abs_x,
            abs_y,
        },
        Motion::Crossed { from, to, edge, ratio, .. } => ScurryRoute {
            crossed: 1,
            from,
            to,
            edge: edge_to_wire(edge),
            ratio,
            abs_x,
            abs_y,
        },
    };
    core::ptr::write(out, r);
    0
}

/// panic = abort in the profile, so this only has to be divergent. Nothing in
/// the layout engine panics on valid input; a panic here means a bug, and
/// halting is preferable to continuing with a corrupt pointer position.
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
