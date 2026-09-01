//! The wireless link, as a BLE central.
//!
//! Experimental. The dongle grew a control service beside its HID one; this
//! connects to it and speaks the same framed protocol the cable speaks.
//!
//! # Why a thread with its own runtime
//!
//! btleplug is async and the rest of this program is not. Rather than colour
//! everything above with `async`, the whole BLE conversation lives on one
//! thread running a single-threaded tokio runtime, and the two sides exchange
//! bytes through channels. That also keeps CoreBluetooth's callbacks off the
//! event tap's thread, which matters: a tap that responds slowly gets disabled
//! by macOS.
//!
//! # Discovery is by name
//!
//! Not by service UUID. The advertisement is already full -- flags, TX power,
//! the 128-bit HID UUID and the name come to more than the 31 bytes an
//! advertisement holds -- so the control service's UUID is not in it. And
//! CoreBluetooth never discloses a peer's address, so that is not available to
//! match on either: every device on macOS reports 00:00:00:00:00:00.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Manager, Peripheral};
use futures::StreamExt;
use scurry_proto::{kind, Header, MouseState, HEADER_LEN, MAX_PAYLOAD};
use uuid::Uuid;

use super::{encode, Framer, Message};

/// 73637572-7279-4c49-4e4b-0000000000xx -- "scurry" and "LINK" in ASCII. Must
/// match the characteristics the firmware publishes.
///
/// The service's own UUID is not here: finding both characteristics is the
/// check that matters, and it gives a better answer when they are missing.
pub const CONTROL: Uuid = Uuid::from_u128(0x73637572_7279_4c49_4e4b_000000000002);
pub const EVENT: Uuid = Uuid::from_u128(0x73637572_7279_4c49_4e4b_000000000003);

/// Advertised name prefix. The firmware appends a per-board id.
const NAME_PREFIX: &str = "Scurry";

/// How much of a frame goes in one GATT write.
///
/// Deliberately small. A write cannot exceed the negotiated MTU less three, the
/// MTU is decided by the host, and btleplug does not expose what was agreed --
/// so the only number guaranteed to work is the one that fits the smallest MTU
/// the spec allows. The hot path is unaffected either way, because a mouse or
/// key frame is sixteen bytes and fits in one write regardless; only a config
/// push is split, and that happens once when somebody presses Save.
const WRITE_CHUNK: usize = 20;

#[derive(Default)]
struct Inbox {
    queue: Mutex<VecDeque<Message>>,
    arrived: Condvar,
}

struct Shared {
    inbox: Inbox,
    /// Cleared when the BLE task exits for any reason. Read by `recv`, so a
    /// dropped link surfaces as a read error -- which is exactly what the tray
    /// already watches for to trigger a reattach.
    alive: AtomicBool,
    name: Mutex<String>,
}

pub struct Wireless {
    shared: Arc<Shared>,
    writes: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    seq: u16,
}

impl Wireless {
    /// Scan, connect, subscribe. Blocks until the link is usable or fails.
    pub fn connect(timeout: Duration) -> Result<Self> {
        let shared = Arc::new(Shared {
            inbox: Inbox::default(),
            alive: AtomicBool::new(false),
            name: Mutex::new(String::new()),
        });
        let (writes, write_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<String, String>>();

        let task_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("scurry-ble".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("starting the BLE runtime: {e}")));
                        return;
                    }
                };
                rt.block_on(run(timeout, ready_tx, write_rx, Arc::clone(&task_shared)));
                // Whatever ended it -- a disconnect, a failed write, the link
                // going out of range -- callers must find out by failing rather
                // than by blocking forever.
                task_shared.alive.store(false, Ordering::Relaxed);
                task_shared.inbox.arrived.notify_all();
            })
            .map_err(|e| anyhow!("spawning the BLE thread: {e}"))?;

        // The scan alone takes seconds, so wait generously past it before
        // deciding nothing is there.
        match ready_rx.recv_timeout(timeout + Duration::from_secs(10)) {
            Ok(Ok(name)) => {
                *shared.name.lock().unwrap() = name;
                shared.alive.store(true, Ordering::Relaxed);
                Ok(Self { shared, writes, seq: 0 })
            }
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => bail!("the BLE thread did not report back"),
        }
    }

    /// A second handle on the same link, for the thread that only reads.
    ///
    /// Both handles share one inbox. That is sound because exactly one reader
    /// exists -- if a second appeared they would steal messages from each other,
    /// which is the bug the daemon's reply table exists to prevent one layer up.
    pub fn try_clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            writes: self.writes.clone(),
            seq: 0,
        }
    }

    pub fn name(&self) -> String {
        self.shared.name.lock().map(|n| n.clone()).unwrap_or_default()
    }

    pub fn send(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_PAYLOAD {
            bail!("payload of {} bytes exceeds the dongle's buffer", payload.len());
        }
        if !self.shared.alive.load(Ordering::Relaxed) {
            bail!("the wireless link is down");
        }
        let frame = encode(kind, self.seq, payload);
        self.seq = self.seq.wrapping_add(1);
        self.writes
            .send(frame)
            .map_err(|_| anyhow!("the wireless link is down"))
    }

    pub fn recv(&mut self, timeout: Duration) -> Result<Option<Message>> {
        let deadline = Instant::now() + timeout;
        let mut queue = self
            .shared
            .inbox
            .queue
            .lock()
            .map_err(|_| anyhow!("wireless inbox poisoned"))?;
        loop {
            if let Some(msg) = queue.pop_front() {
                return Ok(Some(msg));
            }
            if !self.shared.alive.load(Ordering::Relaxed) {
                bail!("the wireless link dropped");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let (q, _) = self
                .shared
                .inbox
                .arrived
                .wait_timeout(queue, remaining)
                .map_err(|_| anyhow!("wireless inbox poisoned"))?;
            queue = q;
        }
    }
}

async fn run(
    scan_for: Duration,
    ready: std::sync::mpsc::Sender<Result<String, String>>,
    mut writes: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    shared: Arc<Shared>,
) {
    let (peripheral, name) = match find(scan_for).await {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return;
        }
    };

    let chars = peripheral.characteristics();
    let (Some(control), Some(event)) = (
        chars.iter().find(|c| c.uuid == CONTROL).cloned(),
        chars.iter().find(|c| c.uuid == EVENT).cloned(),
    ) else {
        let _ = ready.send(Err(
            "the dongle has no control service. Its firmware predates the wireless link, \
             or macOS is serving a cached service list -- forget the device in Bluetooth \
             settings and try again."
                .into(),
        ));
        return;
    };

    if let Err(e) = peripheral.subscribe(&event).await {
        let _ = ready.send(Err(format!("subscribing to the dongle's events: {e}")));
        return;
    }
    let mut notifications = match peripheral.notifications().await {
        Ok(n) => n,
        Err(e) => {
            let _ = ready.send(Err(format!("opening the notification stream: {e}")));
            return;
        }
    };

    let _ = ready.send(Ok(name));

    let mut framer = Framer::with_capacity(1024);
    let mut on_log = |line: &str| eprintln!("[dongle] {line}");

    loop {
        tokio::select! {
            // Frames from the dongle.
            notif = notifications.next() => {
                let Some(notif) = notif else { break };
                if notif.uuid != EVENT {
                    continue;
                }
                framer.push(&notif.value);
                let mut woke = false;
                while let Some(msg) = framer.take(&mut on_log) {
                    if let Ok(mut q) = shared.inbox.queue.lock() {
                        q.push_back(msg);
                        woke = true;
                    }
                }
                if woke {
                    shared.inbox.arrived.notify_all();
                }
            }

            // Frames for the dongle.
            frame = writes.recv() => {
                let Some(frame) = frame else { break };
                // Take everything else that is already waiting, so a burst of
                // motion collapses instead of queueing. Without this the radio
                // falls behind a 125Hz pointer stream and never catches up:
                // the queue is unbounded, so the lag would grow without limit
                // rather than settling at some steady offset.
                let mut batch = vec![frame];
                while let Ok(more) = writes.try_recv() {
                    batch.push(more);
                }
                for frame in coalesce(batch) {
                // Pointer and key traffic goes unacknowledged: an ack per mouse
                // report would double the radio events for no benefit, since a
                // dropped report is repaired by the next one. Control messages
                // are rare and worth confirming.
                let hot = frame.get(2).is_some_and(|k| *k < 0x10);
                let wt = if hot { WriteType::WithoutResponse } else { WriteType::WithResponse };
                for chunk in frame.chunks(WRITE_CHUNK) {
                    if let Err(e) = peripheral.write(&control, chunk, wt).await {
                        eprintln!("wireless write failed: {e}");
                        return;
                    }
                }
                }
            }
        }
    }
    let _ = peripheral.disconnect().await;
}

/// Collapse a burst of queued frames into the fewest that mean the same thing.
///
/// Only runs of consecutive mouse updates are merged, and only with each other.
/// The protocol is what makes that lossless: motion is a delta, so summing a
/// run lands the pointer in the same place, and buttons are absolute state, so
/// the last one in the run is the truth. Everything else -- keys, config,
/// anything unrecognised -- passes through untouched and in order, because
/// merging a run of mouse frames across a key frame would reorder a click
/// against a keystroke.
///
/// The merged frame carries the last sequence number of its run. The dongle
/// drops anything that is not newer than what it has seen, so carrying the
/// first would make the very next frame look like a straggler.
fn coalesce(frames: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(frames.len());
    for frame in frames {
        let parsed = Header::decode(&frame)
            .ok()
            .filter(|h| h.kind == kind::MOUSE)
            .and_then(|h| MouseState::decode(&frame[HEADER_LEN..]).map(|m| (h.seq, m)));

        let Some((seq, next)) = parsed else {
            out.push(frame);
            continue;
        };

        // Merge only into an immediately preceding mouse frame.
        let prev = out
            .last()
            .and_then(|f| Header::decode(f).ok().filter(|h| h.kind == kind::MOUSE))
            .and_then(|_| MouseState::decode(&out.last().unwrap()[HEADER_LEN..]));

        match prev {
            Some(prev) => {
                let merged = MouseState {
                    buttons: next.buttons,
                    dx: prev.dx.saturating_add(next.dx),
                    dy: prev.dy.saturating_add(next.dy),
                    wheel: prev.wheel.saturating_add(next.wheel),
                    pan: prev.pan.saturating_add(next.pan),
                };
                *out.last_mut().unwrap() = encode(kind::MOUSE, seq, &merged.encode());
            }
            None => out.push(frame),
        }
    }
    out
}

/// Scan until a dongle turns up, or `scan_for` elapses.
async fn find(scan_for: Duration) -> Result<(Peripheral, String)> {
    let manager = Manager::new().await.map_err(|e| anyhow!("starting Bluetooth: {e}"))?;
    let adapter = manager
        .adapters()
        .await
        .map_err(|e| anyhow!("listing Bluetooth adapters: {e}"))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("this machine has no Bluetooth adapter"))?;

    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| anyhow!("scanning: {e}"))?;

    let deadline = Instant::now() + scan_for;
    let found = loop {
        if let Some(hit) = first_scurry(&adapter).await {
            break Some(hit);
        }
        if Instant::now() >= deadline {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    };
    let _ = adapter.stop_scan().await;

    let (peripheral, name) = found.ok_or_else(|| {
        anyhow!("no dongle answered in {scan_for:?} (is it powered, and in range?)")
    })?;

    if !peripheral.is_connected().await.unwrap_or(false) {
        peripheral
            .connect()
            .await
            .map_err(|e| anyhow!("connecting to {name}: {e}"))?;
    }
    peripheral
        .discover_services()
        .await
        .map_err(|e| anyhow!("discovering services on {name}: {e}"))?;
    Ok((peripheral, name))
}

async fn first_scurry(adapter: &btleplug::platform::Adapter) -> Option<(Peripheral, String)> {
    for p in adapter.peripherals().await.ok()? {
        let Ok(Some(props)) = p.properties().await else { continue };
        let Some(name) = props.local_name else { continue };
        // macOS decorates a bonded HID device's name, so match loosely rather
        // than on a prefix: it presents ours as "HID [Scurry 629X]".
        if name.contains(NAME_PREFIX) {
            return Some((p, name));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use scurry_proto::button;

    fn mouse(seq: u16, buttons: u8, dx: i16, dy: i16) -> Vec<u8> {
        let m = MouseState { buttons, dx, dy, wheel: 0, pan: 0 };
        encode(kind::MOUSE, seq, &m.encode())
    }

    fn parse(frame: &[u8]) -> (u16, MouseState) {
        let h = Header::decode(frame).unwrap();
        (h.seq, MouseState::decode(&frame[HEADER_LEN..]).unwrap())
    }

    #[test]
    fn motion_sums_and_buttons_take_the_latest() {
        // The whole point: three reports the radio could not keep up with
        // become one that puts the pointer in the same place.
        let out = coalesce(vec![
            mouse(1, 0, 5, 0),
            mouse(2, button::LEFT, 7, -3),
            mouse(3, button::LEFT, 1, 1),
        ]);
        assert_eq!(out.len(), 1);
        let (seq, m) = parse(&out[0]);
        assert_eq!((m.dx, m.dy), (13, -2));
        assert_eq!(m.buttons, button::LEFT, "buttons are absolute; the last one wins");
        assert_eq!(seq, 3, "carrying the first would make the next frame a straggler");
    }

    #[test]
    fn a_key_frame_splits_a_run() {
        // Merging across one would reorder a click against a keystroke.
        let keys = encode(kind::KEY, 9, &[0u8; 8]);
        let out = coalesce(vec![mouse(1, 0, 1, 0), keys.clone(), mouse(3, 0, 2, 0)]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1], keys);
    }

    #[test]
    fn control_frames_pass_through_untouched() {
        let cfg = encode(kind::SET_CONFIG, 4, &[1, 2, 3]);
        let out = coalesce(vec![cfg.clone()]);
        assert_eq!(out, vec![cfg]);
    }

    #[test]
    fn a_single_frame_is_unchanged() {
        let one = mouse(7, button::RIGHT, -4, 9);
        assert_eq!(coalesce(vec![one.clone()]), vec![one]);
    }

    #[test]
    fn extreme_bursts_do_not_wrap() {
        // Saturating, not wrapping: a summed run that overflowed would send the
        // pointer hard in the opposite direction.
        let out = coalesce(vec![mouse(1, 0, i16::MAX, 0), mouse(2, 0, 100, 0)]);
        let (_, m) = parse(&out[0]);
        assert_eq!(m.dx, i16::MAX);
    }
}
