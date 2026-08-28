//! C ABI over the layout engine, so the dongle routes with the same code the
//! controller is tested against rather than a hand-written reimplementation.
//!
//! Everything here is a thin shim. The logic lives in `scurry-layout`, which is
//! `no_std`, allocation-free and covered by tests that run on the host.

#![no_std]

use core::sync::atomic::{AtomicBool, Ordering};

use scurry_layout::{Layout, Motion, Screen};
use scurry_proto::{ScreenWire, MAX_SCREENS, SCREEN_WIRE_LEN};

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
    }

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
        let w = ScreenWire::with_name(s.node, s.name(), s.x, s.y, s.width, s.height);
        w.encode_into(&mut buf[1 + i * SCREEN_WIRE_LEN..]);
    }
    need as i32
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
