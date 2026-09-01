//! The link to the dongle.
//!
//! Two of them now. The cable is the original and remains the one that always
//! works; the wireless link is experimental and exists so the dongle can sit
//! between the machines instead of hanging off this one.
//!
//! Both are the same protocol over the same framing, so everything above this
//! module -- the control socket, the status poller, the event tap -- holds a
//! [`Dongle`] and never learns which kind it got.

pub mod ble;
pub mod serial;

use anyhow::Result;
use scurry_proto::{Header, HEADER_LEN, MAGIC, VERSION};
use std::time::Duration;

/// One decoded message from the dongle.
#[derive(Debug, Clone)]
pub struct Message {
    pub kind: u8,
    pub payload: Vec<u8>,
}

/// Reassembles frames out of a byte stream.
///
/// Neither transport delivers messages. The cable interleaves the firmware's
/// log output with the protocol and lands in whatever chunks the USB reads
/// happen to be; a BLE notification is bounded by the negotiated MTU, which no
/// config payload fits inside. So both sides of the wire run this same
/// resync-on-magic loop -- the dongle has its own copy for exactly the same
/// reason.
#[derive(Debug, Default)]
pub struct Framer {
    /// Bytes read but not yet consumed, so a partial message survives a read.
    buf: Vec<u8>,
    /// Log text seen so far without a terminating newline. The dongle's output
    /// arrives in whatever chunks the reads land in, so a line routinely
    /// straddles two of them and would otherwise be printed in fragments.
    log_partial: String,
}

impl Framer {
    pub fn with_capacity(n: usize) -> Self {
        Self { buf: Vec::with_capacity(n), log_partial: String::new() }
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pull the next complete message out, handing anything that is not one to
    /// `on_log` rather than dropping it silently -- on the cable that text is
    /// the only window into the firmware while the port is held open.
    pub fn take(&mut self, on_log: &mut dyn FnMut(&str)) -> Option<Message> {
        let mut skipped = 0usize;
        loop {
            if self.buf.len() - skipped < HEADER_LEN {
                break;
            }
            let head = &self.buf[skipped..];
            if head[0] != MAGIC || head[1] != VERSION {
                skipped += 1;
                continue;
            }
            match Header::decode(head) {
                Ok(h) => {
                    let total = HEADER_LEN + h.len as usize;
                    if self.buf.len() - skipped < total {
                        break; // payload still arriving
                    }
                    let start = skipped + HEADER_LEN;
                    let payload = self.buf[start..start + h.len as usize].to_vec();
                    self.flush_log(skipped, on_log);
                    // Drain the skipped log bytes AND the message. Draining
                    // only `total` left the skipped prefix in place and ate an
                    // equal number of bytes off the message instead, so the
                    // stream stayed misaligned from the first message that
                    // happened to follow log output -- which is most of them.
                    self.buf.drain(..skipped + total);
                    return Some(Message { kind: h.kind, payload });
                }
                Err(_) => {
                    skipped += 1;
                }
            }
        }
        // Keep the last few bytes: a header may be split across reads.
        if skipped > 0 {
            self.flush_log(skipped, on_log);
            self.buf.drain(..skipped);
        }
        None
    }

    fn flush_log(&mut self, upto: usize, on_log: &mut dyn FnMut(&str)) {
        if upto == 0 {
            return;
        }
        self.log_partial
            .push_str(&String::from_utf8_lossy(&self.buf[..upto]));

        // Emit only complete lines; hold anything after the last newline until
        // the rest of it arrives.
        while let Some(nl) = self.log_partial.find('\n') {
            let line: String = self.log_partial.drain(..=nl).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if !line.is_empty() {
                on_log(line);
            }
        }
    }
}

/// A link to the dongle, over whichever transport found one.
///
/// An enum rather than a boxed trait because of how reattaching works: a live
/// link is swapped *inside* the mutex its consumers already hold, so the socket
/// thread, the poller and the event tap pick up a new device without being
/// respawned. That wants a concrete sized type.
pub enum Dongle {
    Serial(serial::Serial),
    Wireless(ble::Wireless),
}

impl Dongle {
    /// Espressif's USB Serial/JTAG appears as usbmodem on macOS, ttyACM on Linux.
    pub fn autodetect() -> Result<String> {
        serial::Serial::autodetect()
    }

    /// Open by path, e.g. `/dev/cu.usbmodem2101`.
    pub fn open(path: &str) -> Result<Self> {
        Ok(Dongle::Serial(serial::Serial::open(path)?))
    }

    /// Find a dongle over the air and connect to its control service.
    ///
    /// Slower than opening a port by an order of magnitude, because it has to
    /// scan: expect seconds, not milliseconds.
    pub fn open_wireless(timeout: Duration) -> Result<Self> {
        Ok(Dongle::Wireless(ble::Wireless::connect(timeout)?))
    }

    /// A second handle on the same link, for the thread that only reads.
    pub fn try_clone(&self) -> Result<Self> {
        Ok(match self {
            Dongle::Serial(s) => Dongle::Serial(s.try_clone()?),
            Dongle::Wireless(w) => Dongle::Wireless(w.try_clone()),
        })
    }

    pub fn send(&mut self, kind: u8, payload: &[u8]) -> Result<()> {
        match self {
            Dongle::Serial(s) => s.send(kind, payload),
            Dongle::Wireless(w) => w.send(kind, payload),
        }
    }

    /// Read until a message arrives or `timeout` elapses.
    pub fn recv(
        &mut self,
        timeout: Duration,
        on_log: &mut dyn FnMut(&str),
    ) -> Result<Option<Message>> {
        match self {
            Dongle::Serial(s) => s.recv(timeout, on_log),
            Dongle::Wireless(w) => w.recv(timeout),
        }
    }

    /// How to describe this link to a person.
    pub fn describe(&self) -> String {
        match self {
            Dongle::Serial(s) => s.path().to_string(),
            Dongle::Wireless(w) => format!("{} (wireless)", w.name()),
        }
    }

    pub fn is_wireless(&self) -> bool {
        matches!(self, Dongle::Wireless(_))
    }
}

/// Build a frame. Shared so the two transports cannot drift in how they encode
/// one, which would show up only as garbage on the far side.
pub(crate) fn encode(kind: u8, seq: u16, payload: &[u8]) -> Vec<u8> {
    let header = Header::encode(kind, seq, payload.len() as u16);
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(payload);
    out
}
