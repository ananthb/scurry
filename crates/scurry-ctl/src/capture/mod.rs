//! Reading local input, per platform, and the thread that reads the dongle back.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use scurry_proto::kind;

use crate::ipc::DaemonState;
use crate::transport::Dongle;

pub mod keymap;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub use macos::{accessibility_trusted, install, run, CaptureHandle};

#[cfg(not(target_os = "macos"))]
pub fn run(
    _dongle: std::sync::Arc<std::sync::Mutex<crate::transport::Dongle>>,
    _state: std::sync::Arc<crate::ipc::DaemonState>,
) -> anyhow::Result<()> {
    anyhow::bail!("input capture is only implemented for macOS so far")
}

/// Applied to every FOCUS announcement. On macOS this parks and hides the local
/// cursor; elsewhere there is nothing to park, so only [`DaemonState::focus`]
/// moves.
#[cfg(target_os = "macos")]
fn apply_focus(node: u8) {
    macos::set_focus(node);
}

#[cfg(not(target_os = "macos"))]
fn apply_focus(_node: u8) {}

/// True while a thread is draining the dongle.
static READER_RUNNING: AtomicBool = AtomicBool::new(false);

/// Asks the reader to come home.
///
/// The reader's only exit used to be a read failure, which is right for a
/// device that vanished and wrong for a link being deliberately replaced --
/// swapping the `Dongle` inside the mutex does not reach the reader, because it
/// holds its own clone. Without this, switching from the radio to the cable
/// would leave the reader on the radio while everything else talked over the
/// cable.
static READER_STOP: AtomicBool = AtomicBool::new(false);

/// Set when a reader stops because the link failed, and cleared by
/// [`take_link_failure`].
///
/// This is how the app finds out the dongle was unplugged, and it is the only
/// signal worth using. The reader is already sitting in `poll()` on the port, so
/// it learns within one 20ms port timeout; serialport turns the POLLHUP a
/// vanished device produces into `BrokenPipe`, which is not `TimedOut` and so is
/// not swallowed by [`Dongle::recv`]'s timeout arm. Every alternative is worse:
/// the status poller only notices after a two-second request times out,
/// `Dongle::autodetect()` cannot tell a replugged device from the original
/// because the path stays `/dev/cu.usbmodem2101` across the re-enumeration, and
/// a periodic health-check write would contend for the link lock with pointer
/// motion, which is the one lock on the hot path.
static LINK_FAILED: AtomicBool = AtomicBool::new(false);

/// Whether the link has failed since this was last asked, clearing the flag.
///
/// Latched rather than level-triggered so that "no reader has ever run" -- the
/// state while macOS has not granted Accessibility, and before any dongle is
/// plugged in -- is not mistaken for "the device went away".
pub fn take_link_failure() -> bool {
    LINK_FAILED.swap(false, Ordering::SeqCst)
}

/// Start the thread that applies the dongle's announcements, if one is not
/// already running.
///
/// The dongle owns the layout, so it must tell us where the pointer went; we
/// cannot work it out. This thread applies those announcements and surfaces the
/// firmware's log output, which shares the same pipe. It also answers config and
/// status queries, by handing anything that is not a FOCUS message to whoever is
/// waiting for it -- so it has to run whether or not input capture does.
///
/// Idempotent, and that is the point. Exactly one reader may own the serial fd.
/// `install()` is retried until Accessibility is granted, and the reader used to
/// be spawned on every attempt: within a minute dozens of them were calling
/// `read()` on the same fd with each frame's bytes split between them, which
/// corrupted FOCUS announcements and stole the replies to config and status
/// queries.
/// Stop the reader and wait for it to let go of its handle.
///
/// Bounded, because a reader wedged in a driver call must not wedge the caller
/// too -- the menu is drawn on that thread. It waits a little over one read
/// timeout, which is all a healthy reader needs.
pub fn stop_reader() {
    if !READER_RUNNING.load(Ordering::SeqCst) {
        return;
    }
    READER_STOP.store(true, Ordering::SeqCst);
    let deadline = std::time::Instant::now() + Duration::from_millis(600);
    while READER_RUNNING.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    READER_STOP.store(false, Ordering::SeqCst);
}

pub fn watch(dongle: &Arc<Mutex<Dongle>>, state: &Arc<DaemonState>) -> Result<()> {
    if READER_RUNNING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let cloned = {
        let guard = dongle.lock().map_err(|_| anyhow!("dongle lock poisoned"))?;
        guard.try_clone()
    };
    let mut reader = match cloned {
        Ok(r) => r,
        Err(e) => {
            READER_RUNNING.store(false, Ordering::SeqCst);
            return Err(e);
        }
    };

    let state = Arc::clone(state);
    std::thread::spawn(move || {
        let mut on_log = |line: &str| eprintln!("[dongle] {line}");
        let mut asked_to_stop = false;
        loop {
            if READER_STOP.load(Ordering::SeqCst) {
                asked_to_stop = true;
                break;
            }
            match reader.recv(Duration::from_millis(200), &mut on_log) {
                Ok(Some(msg)) if msg.kind == kind::FOCUS => {
                    if let Some(&node) = msg.payload.first() {
                        apply_focus(node);
                        state.focus.store(node, Ordering::Relaxed);
                    }
                }
                // Anything that is not a focus announcement is somebody's
                // reply; hand it to whoever is waiting.
                Ok(Some(msg)) => state.deliver(msg.kind, msg.payload),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("dongle read error: {e}");
                    break;
                }
            }
        }

        // Released before the failure is announced, and not after. Whoever acts
        // on the announcement immediately asks for a new reader, and if this
        // still read as running that request would be dropped on the floor --
        // leaving the reattached dongle with nobody reading it.
        READER_RUNNING.store(false, Ordering::SeqCst);

        // Bring the pointer home before announcing anything. An unplug while it
        // was on another machine would otherwise leave FOCUS stuck non-zero for
        // good: the cursor stays hidden, every event is swallowed, and the Mac
        // looks frozen with no pointer and no way to click anything.
        apply_focus(0);
        state.focus.store(0, Ordering::Relaxed);

        // Only a link that broke is a failure. One that was asked to stop is
        // about to be replaced by the caller, and announcing a failure would
        // send it round the reattach path it is already in the middle of.
        if !asked_to_stop {
            LINK_FAILED.store(true, Ordering::SeqCst);
        }
    });
    Ok(())
}

/// The rectangle the local cursor lives in, in the same units the layout uses
/// for the local screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Bounds {
    fn span(self, axis: Axis) -> (i32, i32) {
        match axis {
            Axis::X => (self.x, self.x + self.width - 1),
            Axis::Y => (self.y, self.y + self.height - 1),
        }
    }
}

#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
}

/// How far to advance the dongle's model of the pointer for one event, while
/// the pointer is still on this machine.
///
/// The obvious input is the raw HID delta, and that is what this used to send.
/// It is wrong: the visible cursor moves by the device count *after pointer
/// acceleration*, so on a 1600-DPI mouse an inch of travel is ~1600 counts but
/// maybe 800 points of cursor movement. The dongle's dead-reckoned pointer ran
/// ahead of the cursor and handed off to the next machine while the cursor was
/// still in the middle of the display.
///
/// So while the pointer is local we send how far the *cursor* actually moved,
/// and the crossing fires exactly when the cursor reaches the edge.
///
/// The device delta is still needed for the one case the screen delta cannot
/// describe: once the cursor is pinned against the edge of the display it stops
/// moving no matter how far the mouse is pushed, and a screen delta of zero
/// would mean the pointer could never leave this machine at all. When the
/// cursor is against the boundary and the device is still moving outward, fall
/// back to the device count on that axis.
///
/// A stationary cursor anywhere *else* means acceleration rounded a small
/// movement away to nothing, and sending the device count there is exactly the
/// race this function exists to stop. Send zero.
///
/// Each axis is decided on its own, so sliding along the top edge still tracks
/// the cursor horizontally while pushing off it vertically.
///
/// This keeps the two pointers moving *together*; it does not make them start
/// in the same place. The dongle puts its virtual pointer at the middle of the
/// local screen when the layout is loaded, and the cursor is wherever the user
/// left it, so there can be a fixed offset between them until the first
/// crossing -- pushing into an edge feeds device counts until the model reaches
/// it too, and coming back lands both at the same edge. Unlike the
/// acceleration error, that offset is bounded by the screen and does not grow
/// with every movement.
pub fn local_delta(
    prev: (i32, i32),
    now: (i32, i32),
    device: (i32, i32),
    bounds: Bounds,
) -> (i32, i32) {
    (
        axis_delta(now.0 - prev.0, device.0, now.0, bounds.span(Axis::X)),
        axis_delta(now.1 - prev.1, device.1, now.1, bounds.span(Axis::Y)),
    )
}

/// How near the boundary still counts as against it.
///
/// The cursor position arrives as a float and is truncated, and exactly where
/// macOS stops the cursor relative to the display rectangle is not something to
/// bet handoff on: being one point out would mean the pinned case never fires
/// and the pointer could never leave this machine at all. One point of slack
/// costs at most one event's worth of over-reported motion.
const EDGE_SLACK: i32 = 1;

fn axis_delta(screen: i32, device: i32, pos: i32, (min, max): (i32, i32)) -> i32 {
    if screen != 0 {
        return screen;
    }
    let pinned = (device < 0 && pos <= min + EDGE_SLACK) || (device > 0 && pos >= max - EDGE_SLACK);
    if pinned {
        device
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1512x982 Mac display at the origin, matching the local screen in the
    /// shipped layout.
    const MAC: Bounds = Bounds {
        x: 0,
        y: 0,
        width: 1512,
        height: 982,
    };

    #[test]
    fn mid_screen_motion_follows_the_visible_cursor() {
        // 40 device counts that moved the cursor 20 points must advance the
        // dongle's model by 20, or it reaches the edge twice as fast as the
        // cursor does -- the pointer jumping to the next machine while the
        // cursor is still mid-screen.
        assert_eq!(local_delta((700, 500), (720, 500), (40, 0), MAC), (20, 0));
    }

    #[test]
    fn a_cursor_pinned_at_the_edge_still_hands_off() {
        // The cursor cannot move past x=1511, so the screen delta is zero
        // forever. Without the device-count fallback the pointer could never
        // leave this machine.
        assert_eq!(local_delta((1511, 500), (1511, 500), (30, 0), MAC), (30, 0));
        assert_eq!(local_delta((0, 500), (0, 500), (-30, 0), MAC), (-30, 0));
        assert_eq!(local_delta((700, 0), (700, 0), (0, -30), MAC), (0, -30));
        assert_eq!(local_delta((700, 981), (700, 981), (0, 30), MAC), (0, 30));

        // And a point short of it, since where exactly macOS stops the cursor
        // is not worth betting handoff on.
        assert_eq!(local_delta((1510, 500), (1510, 500), (30, 0), MAC), (30, 0));
    }

    #[test]
    fn being_at_an_edge_while_moving_inward_is_not_pinned() {
        // Sitting on the left edge and pushing right is ordinary motion. Taking
        // the device count here would over-report every movement that starts
        // from a screen edge.
        assert_eq!(local_delta((0, 500), (0, 500), (12, 0), MAC), (0, 0));
    }

    #[test]
    fn acceleration_rounding_away_a_tiny_move_sends_nothing() {
        // A slow nudge the OS rounded to no cursor movement. Sending the device
        // count would let a series of them walk the model off the screen while
        // the cursor had not moved at all.
        assert_eq!(local_delta((700, 500), (700, 500), (1, 1), MAC), (0, 0));
    }

    #[test]
    fn axes_are_decided_independently() {
        // Sliding along the top edge: y is pinned and pushing up, x is moving
        // normally. Each axis must pick its own source.
        assert_eq!(local_delta((700, 0), (740, 0), (90, -20), MAC), (40, -20));
    }

    #[test]
    fn a_stationary_mouse_reports_nothing() {
        assert_eq!(local_delta((1511, 981), (1511, 981), (0, 0), MAC), (0, 0));
    }

    #[test]
    fn deceleration_is_reported_too() {
        // The other half of acceleration: a fast flick moves the cursor further
        // than the device counts. The cursor is what the user sees, so it wins.
        assert_eq!(local_delta((100, 500), (400, 500), (150, 0), MAC), (300, 0));
    }
}
