//! Wire format between the scurry controller and the dongle.
//!
//! # Shape
//!
//! An 8-byte header followed by a length-prefixed payload.
//!
//! ```text
//! 0       magic 0x53
//! 1       version
//! 2       kind
//! 3       flags (reserved)
//! 4..6    seq (u16)
//! 6..8    payload length (u16)
//! 8..     payload
//! ```
//!
//! A mouse update carries an 8-byte payload, so the hot path is still exactly
//! 16 bytes on the wire — the length prefix costs nothing there but lets config
//! messages be arbitrarily long.
//!
//! # The controller does not route
//!
//! The layout lives on the dongle, so the controller sends *raw* pointer motion
//! and has no opinion about which machine it lands on. That is what lets the
//! same firmware run standalone with no controller at all.
//!
//! # Buttons are absolute
//!
//! BLE does not retransmit. If a release were its own event and its frame were
//! dropped, a button would be stuck down on a machine the user is not sitting
//! at — the worst failure this protocol could have. Every update carries the
//! full button bitmask, so the next one repairs the state. Motion is a delta,
//! because a dropped motion frame costs a few pixels and self-corrects.

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

pub const MAGIC: u8 = 0x53;
pub const VERSION: u8 = 2;

/// Bytes before the payload.
pub const HEADER_LEN: usize = 8;

/// Largest payload we will accept. Bounds the dongle's reassembly buffer, which
/// has no allocator to grow one.
pub const MAX_PAYLOAD: usize = 256;

/// One local screen plus up to four bonded targets.
pub const MAX_SCREENS: usize = 5;

/// Screen names are fixed-width so neither side needs an allocator.
pub const NAME_LEN: usize = 16;

/// Bytes one screen occupies on the wire.
pub const SCREEN_WIRE_LEN: usize = 1 + NAME_LEN + 16;

pub mod button {
    pub const LEFT: u8 = 1 << 0;
    pub const RIGHT: u8 = 1 << 1;
    pub const MIDDLE: u8 = 1 << 2;
    pub const BACK: u8 = 1 << 3;
    pub const FORWARD: u8 = 1 << 4;
}

/// Message kinds. Control messages start at 0x10 so the hot path stays in the
/// low numbers and a reader can tell the classes apart at a glance.
pub mod kind {
    /// Controller -> dongle: raw pointer motion. The dongle routes it.
    pub const MOUSE: u8 = 0x01;
    pub const PING: u8 = 0x04;
    pub const PONG: u8 = 0x05;

    /// Controller -> dongle: send me the stored layout.
    pub const GET_CONFIG: u8 = 0x10;
    /// Either direction: a full layout.
    pub const CONFIG: u8 = 0x11;
    /// Controller -> dongle: replace the stored layout and persist it.
    pub const SET_CONFIG: u8 = 0x12;
    /// Controller -> dongle: report bonded targets and connection state.
    pub const GET_STATUS: u8 = 0x13;
    /// Dongle -> controller: bonded targets and connection state.
    pub const STATUS: u8 = 0x14;
    /// Dongle -> controller: result of a request.
    pub const ACK: u8 = 0x15;
}

/// Result codes carried by [`kind::ACK`].
pub mod ack {
    pub const OK: u8 = 0;
    pub const BAD_REQUEST: u8 = 1;
    pub const INVALID_LAYOUT: u8 = 2;
    pub const STORAGE_FAILED: u8 = 3;
}

/// Which screen edge a pointer crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// The edge a pointer arrives at, given the edge it departed from.
    pub fn opposite(self) -> Self {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// A relative pointer update. Buttons are absolute state; motion is a delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseState {
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
    pub wheel: i8,
    pub pan: i8,
}

impl MouseState {
    pub const WIRE_LEN: usize = 8;

    pub fn encode(&self) -> [u8; Self::WIRE_LEN] {
        let mut b = [0u8; Self::WIRE_LEN];
        b[0] = self.buttons;
        b[1..3].copy_from_slice(&self.dx.to_le_bytes());
        b[3..5].copy_from_slice(&self.dy.to_le_bytes());
        b[5] = self.wheel as u8;
        b[6] = self.pan as u8;
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < Self::WIRE_LEN {
            return None;
        }
        Some(Self {
            buttons: b[0],
            dx: i16::from_le_bytes([b[1], b[2]]),
            dy: i16::from_le_bytes([b[3], b[4]]),
            wheel: b[5] as i8,
            pan: b[6] as i8,
        })
    }
}

/// One screen, as it appears inside a [`kind::CONFIG`] payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenWire {
    pub node: u8,
    pub name: [u8; NAME_LEN],
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl ScreenWire {
    pub fn encode_into(&self, out: &mut [u8]) {
        out[0] = self.node;
        out[1..1 + NAME_LEN].copy_from_slice(&self.name);
        let base = 1 + NAME_LEN;
        out[base..base + 4].copy_from_slice(&self.x.to_le_bytes());
        out[base + 4..base + 8].copy_from_slice(&self.y.to_le_bytes());
        out[base + 8..base + 12].copy_from_slice(&self.width.to_le_bytes());
        out[base + 12..base + 16].copy_from_slice(&self.height.to_le_bytes());
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < SCREEN_WIRE_LEN {
            return None;
        }
        let mut name = [0u8; NAME_LEN];
        name.copy_from_slice(&b[1..1 + NAME_LEN]);
        let base = 1 + NAME_LEN;
        let rd = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        Some(Self {
            node: b[0],
            name,
            x: rd(base),
            y: rd(base + 4),
            width: rd(base + 8),
            height: rd(base + 12),
        })
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        core::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    pub fn with_name(node: u8, name: &str, x: i32, y: i32, width: i32, height: i32) -> Self {
        let mut buf = [0u8; NAME_LEN];
        let src = name.as_bytes();
        let n = core::cmp::min(src.len(), NAME_LEN);
        buf[..n].copy_from_slice(&src[..n]);
        Self { node, name: buf, x, y, width, height }
    }
}

/// One bonded target, as it appears inside a [`kind::STATUS`] payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotStatus {
    /// Connection slot, 0-based. Node ids are `slot + 1`.
    pub slot: u8,
    pub connected: bool,
    /// Peer Bluetooth address, all zero when never bonded.
    pub bda: [u8; 6],
}

impl SlotStatus {
    pub const WIRE_LEN: usize = 8;

    pub fn encode_into(&self, out: &mut [u8]) {
        out[0] = self.slot;
        out[1] = u8::from(self.connected);
        out[2..8].copy_from_slice(&self.bda);
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < Self::WIRE_LEN {
            return None;
        }
        let mut bda = [0u8; 6];
        bda.copy_from_slice(&b[2..8]);
        Some(Self { slot: b[0], connected: b[1] != 0, bda })
    }
}

/// A parsed message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub seq: u16,
    pub len: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Fewer than [`HEADER_LEN`] bytes available.
    Truncated,
    BadMagic,
    VersionMismatch { got: u8 },
    /// Payload length exceeds [`MAX_PAYLOAD`]; refuse rather than try to buffer.
    Oversized(u16),
}

impl Header {
    pub fn encode(kind: u8, seq: u16, len: u16) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = MAGIC;
        b[1] = VERSION;
        b[2] = kind;
        b[4..6].copy_from_slice(&seq.to_le_bytes());
        b[6..8].copy_from_slice(&len.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        if b.len() < HEADER_LEN {
            return Err(DecodeError::Truncated);
        }
        if b[0] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        if b[1] != VERSION {
            return Err(DecodeError::VersionMismatch { got: b[1] });
        }
        let len = u16::from_le_bytes([b[6], b[7]]);
        if len as usize > MAX_PAYLOAD {
            return Err(DecodeError::Oversized(len));
        }
        Ok(Header { kind: b[2], seq: u16::from_le_bytes([b[4], b[5]]), len })
    }
}

/// Tracks sequence numbers so a receiver can drop reordered stragglers.
///
/// Sequence numbers wrap at [`u16::MAX`], so "newer" is signed distance on the
/// wrapped circle rather than `>`. A naive comparison would stall the stream for
/// 32k messages every time the counter wrapped.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeqGate {
    last: Option<u16>,
}

impl SeqGate {
    pub fn new() -> Self {
        Self { last: None }
    }

    pub fn accept(&mut self, seq: u16) -> bool {
        match self.last {
            None => {
                self.last = Some(seq);
                true
            }
            Some(prev) => {
                if (seq.wrapping_sub(prev) as i16) > 0 {
                    self.last = Some(seq);
                    true
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_stays_sixteen_bytes_on_the_wire() {
        // The hot path must not have grown when the length prefix was added.
        assert_eq!(HEADER_LEN + MouseState::WIRE_LEN, 16);
    }

    #[test]
    fn header_roundtrips() {
        let h = Header::encode(kind::MOUSE, 1234, 8);
        let got = Header::decode(&h).unwrap();
        assert_eq!(got, Header { kind: kind::MOUSE, seq: 1234, len: 8 });
    }

    #[test]
    fn mouse_roundtrips_including_negatives() {
        // Sign handling through the byte layout is the easiest thing to get
        // wrong, and shows up as a pointer that only moves down and right.
        let m = MouseState {
            buttons: button::LEFT | button::MIDDLE,
            dx: i16::MIN,
            dy: -1,
            wheel: i8::MIN,
            pan: -1,
        };
        assert_eq!(MouseState::decode(&m.encode()).unwrap(), m);
    }

    #[test]
    fn screen_roundtrips() {
        let s = ScreenWire::with_name(1, "chromebook", -100, 262, 1920, 1080);
        let mut buf = [0u8; SCREEN_WIRE_LEN];
        s.encode_into(&mut buf);
        let got = ScreenWire::decode(&buf).unwrap();
        assert_eq!(got, s);
        assert_eq!(got.name_str(), "chromebook");
    }

    #[test]
    fn screen_name_truncates_without_panicking() {
        let s = ScreenWire::with_name(1, "a-name-far-longer-than-sixteen", 0, 0, 1, 1);
        assert_eq!(s.name_str().len(), NAME_LEN);
    }

    #[test]
    fn slot_status_roundtrips() {
        let s = SlotStatus { slot: 2, connected: true, bda: [1, 2, 3, 4, 5, 6] };
        let mut buf = [0u8; SlotStatus::WIRE_LEN];
        s.encode_into(&mut buf);
        assert_eq!(SlotStatus::decode(&buf).unwrap(), s);
    }

    #[test]
    fn rejects_foreign_and_oversized() {
        assert_eq!(Header::decode(&[]), Err(DecodeError::Truncated));
        assert_eq!(Header::decode(&[0u8; HEADER_LEN]), Err(DecodeError::BadMagic));

        let mut wrong = Header::encode(kind::PING, 0, 0);
        wrong[1] = 99;
        assert_eq!(Header::decode(&wrong), Err(DecodeError::VersionMismatch { got: 99 }));

        // A hostile or corrupt length must be refused, not buffered: the dongle
        // has no allocator and a fixed reassembly buffer.
        let big = Header::encode(kind::CONFIG, 0, (MAX_PAYLOAD + 1) as u16);
        assert_eq!(
            Header::decode(&big),
            Err(DecodeError::Oversized((MAX_PAYLOAD + 1) as u16))
        );
    }

    #[test]
    fn a_full_config_payload_fits() {
        // MAX_SCREENS screens plus the count byte must fit MAX_PAYLOAD, or the
        // largest legal config could never be sent.
        assert!(1 + MAX_SCREENS * SCREEN_WIRE_LEN <= MAX_PAYLOAD);
    }

    #[test]
    fn edges_pair_up() {
        for e in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            assert_eq!(e.opposite().opposite(), e);
            assert_ne!(e.opposite(), e);
        }
    }

    #[test]
    fn seq_gate_drops_duplicates_and_stragglers() {
        let mut g = SeqGate::new();
        assert!(g.accept(10));
        assert!(!g.accept(10));
        assert!(!g.accept(9));
        assert!(g.accept(11));
    }

    #[test]
    fn seq_gate_survives_wraparound() {
        let mut g = SeqGate::new();
        assert!(g.accept(u16::MAX - 1));
        assert!(g.accept(u16::MAX));
        assert!(g.accept(0), "wrap to zero is newer, not older");
        assert!(g.accept(1));
        assert!(!g.accept(u16::MAX), "pre-wrap value is now a straggler");
    }
}
