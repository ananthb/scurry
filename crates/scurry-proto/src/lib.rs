//! Wire format shared by the scurry controller, the dongle, and the nodes.
//!
//! # Why the frame looks like this
//!
//! ESP-NOW is an unreliable datagram transport: frames can be dropped or
//! reordered, and there is no retransmit. Two consequences shape the format.
//!
//! First, **button state is absolute, not a press/release delta**. If a release
//! were a separate event and its frame were dropped, the target would be left
//! with a button stuck down and no way to recover — the single worst failure
//! mode this protocol could have. Sending the full button bitmask in every
//! frame means the next frame to arrive always repairs the state.
//!
//! Motion, by contrast, *is* a delta. A dropped motion frame costs a few pixels
//! of pointer travel and self-corrects on the next movement, which is a far
//! cheaper failure than absolute coordinates going stale.
//!
//! Second, every frame carries a sequence number so a receiver can discard
//! reordered stragglers rather than apply motion backwards.
//!
//! # Layout
//!
//! Fixed 16 bytes, little-endian. ESP-NOW allows 250, so there is ample room to
//! grow, but a fixed size means no length negotiation and no allocation.
//!
//! ```text
//! 0       magic 0x53
//! 1       version
//! 2       kind
//! 3       node id
//! 4..6    seq (u16)
//! 6..16   payload, kind-specific
//! ```

#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]

/// Every frame starts with this byte. Cheap rejection of foreign traffic that
/// happens to land on the same ESP-NOW peer.
pub const MAGIC: u8 = 0x53;

/// Bumped on any incompatible layout change. A node refuses mismatched frames
/// rather than misinterpreting them.
pub const VERSION: u8 = 1;

/// Wire size of every frame, in bytes.
pub const FRAME_LEN: usize = 16;

/// Broadcast node id, addressed to every node.
pub const NODE_BROADCAST: u8 = 0xFF;

/// Mouse buttons, as bitmask positions matching the USB HID boot-protocol
/// mouse report, so a node can forward the low bits without remapping.
pub mod button {
    pub const LEFT: u8 = 1 << 0;
    pub const RIGHT: u8 = 1 << 1;
    pub const MIDDLE: u8 = 1 << 2;
    pub const BACK: u8 = 1 << 3;
    pub const FORWARD: u8 = 1 << 4;
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
    ///
    /// Leaving your right edge means arriving at the neighbour's left.
    pub fn opposite(self) -> Self {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    fn to_wire(self) -> u8 {
        match self {
            Edge::Left => 0,
            Edge::Right => 1,
            Edge::Top => 2,
            Edge::Bottom => 3,
        }
    }

    fn from_wire(b: u8) -> Option<Self> {
        Some(match b {
            0 => Edge::Left,
            1 => Edge::Right,
            2 => Edge::Top,
            3 => Edge::Bottom,
            _ => return None,
        })
    }
}

/// A relative mouse update. Buttons are absolute state; motion is a delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseState {
    /// Absolute button bitmask. See [`button`].
    pub buttons: u8,
    pub dx: i16,
    pub dy: i16,
    /// Vertical wheel, positive is away from the user.
    pub wheel: i8,
    /// Horizontal wheel, positive is rightward.
    pub pan: i8,
}

/// One decoded protocol frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// Pointer activity for whichever node currently holds focus.
    Mouse(MouseState),
    /// The pointer has entered this node's screen at `edge`, `ratio` of the way
    /// along it (0 = top/left corner, u16::MAX = bottom/right).
    Enter { edge: Edge, ratio: u16 },
    /// The pointer has left this node's screen.
    ///
    /// A node receiving this MUST release every held button. Otherwise a drag
    /// that crosses a screen boundary would strand a held button on the machine
    /// being departed.
    Leave,
    /// Liveness probe, echoed back as [`Payload::Pong`].
    Ping,
    Pong,
}

impl Payload {
    fn kind(&self) -> u8 {
        match self {
            Payload::Mouse(_) => 1,
            Payload::Enter { .. } => 2,
            Payload::Leave => 3,
            Payload::Ping => 4,
            Payload::Pong => 5,
        }
    }
}

/// A frame as it appears on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub node: u8,
    pub seq: u16,
    pub payload: Payload,
}

/// Why a byte slice could not be decoded into a [`Frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Slice was shorter than [`FRAME_LEN`].
    Truncated,
    /// First byte was not [`MAGIC`]; almost certainly not ours.
    BadMagic,
    /// Known framing, unknown protocol version.
    VersionMismatch { got: u8 },
    /// Version matched but the kind byte is not one we handle.
    UnknownKind(u8),
    /// Kind was valid but a field within the payload was not.
    MalformedPayload,
}

impl Frame {
    pub fn new(node: u8, seq: u16, payload: Payload) -> Self {
        Self { node, seq, payload }
    }

    /// Serialise into exactly [`FRAME_LEN`] bytes.
    pub fn encode(&self) -> [u8; FRAME_LEN] {
        let mut b = [0u8; FRAME_LEN];
        b[0] = MAGIC;
        b[1] = VERSION;
        b[2] = self.payload.kind();
        b[3] = self.node;
        b[4..6].copy_from_slice(&self.seq.to_le_bytes());

        match self.payload {
            Payload::Mouse(m) => {
                b[6] = m.buttons;
                b[8..10].copy_from_slice(&m.dx.to_le_bytes());
                b[10..12].copy_from_slice(&m.dy.to_le_bytes());
                b[12] = m.wheel as u8;
                b[13] = m.pan as u8;
            }
            Payload::Enter { edge, ratio } => {
                b[6] = edge.to_wire();
                b[8..10].copy_from_slice(&ratio.to_le_bytes());
            }
            Payload::Leave | Payload::Ping | Payload::Pong => {}
        }
        b
    }

    /// Parse a frame, rejecting anything not addressed to this protocol.
    pub fn decode(b: &[u8]) -> Result<Self, DecodeError> {
        if b.len() < FRAME_LEN {
            return Err(DecodeError::Truncated);
        }
        if b[0] != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        if b[1] != VERSION {
            return Err(DecodeError::VersionMismatch { got: b[1] });
        }
        let node = b[3];
        let seq = u16::from_le_bytes([b[4], b[5]]);

        let payload = match b[2] {
            1 => Payload::Mouse(MouseState {
                buttons: b[6],
                dx: i16::from_le_bytes([b[8], b[9]]),
                dy: i16::from_le_bytes([b[10], b[11]]),
                wheel: b[12] as i8,
                pan: b[13] as i8,
            }),
            2 => Payload::Enter {
                edge: Edge::from_wire(b[6]).ok_or(DecodeError::MalformedPayload)?,
                ratio: u16::from_le_bytes([b[8], b[9]]),
            },
            3 => Payload::Leave,
            4 => Payload::Ping,
            5 => Payload::Pong,
            k => return Err(DecodeError::UnknownKind(k)),
        };

        Ok(Frame { node, seq, payload })
    }
}

/// Tracks sequence numbers so a receiver can drop reordered stragglers.
///
/// Sequence numbers wrap at [`u16::MAX`], so "newer" is defined by signed
/// distance on the wrapped circle rather than by `>`. A frame more than half
/// the sequence space ahead is treated as old, which is the standard trick and
/// is correct as long as we never have 32768 frames in flight — at a 1kHz
/// report rate that would be 32 seconds of buffering.
#[derive(Debug, Default, Clone, Copy)]
pub struct SeqGate {
    last: Option<u16>,
}

impl SeqGate {
    pub fn new() -> Self {
        Self { last: None }
    }

    /// Returns true if `seq` is newer than everything seen so far, and records
    /// it. Returns false for duplicates and stragglers, which should be ignored.
    pub fn accept(&mut self, seq: u16) -> bool {
        match self.last {
            None => {
                self.last = Some(seq);
                true
            }
            Some(prev) => {
                if seq.wrapping_sub(prev) != 0 && (seq.wrapping_sub(prev) as i16) > 0 {
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

    fn roundtrip(p: Payload) {
        let f = Frame::new(7, 1234, p);
        let decoded = Frame::decode(&f.encode()).expect("decodes");
        assert_eq!(f, decoded);
    }

    #[test]
    fn every_payload_roundtrips() {
        roundtrip(Payload::Mouse(MouseState {
            buttons: button::LEFT | button::MIDDLE,
            dx: -1234,
            dy: 5678,
            wheel: -3,
            pan: 2,
        }));
        roundtrip(Payload::Enter { edge: Edge::Top, ratio: 40000 });
        roundtrip(Payload::Leave);
        roundtrip(Payload::Ping);
        roundtrip(Payload::Pong);
    }

    #[test]
    fn encodes_to_fixed_width() {
        assert_eq!(Frame::new(0, 0, Payload::Leave).encode().len(), FRAME_LEN);
    }

    #[test]
    fn negative_motion_survives_the_wire() {
        // i16/i8 sign handling through the byte layout is the easiest thing to
        // get wrong here, and it would show up as a pointer that only moves
        // down and right.
        let m = MouseState { buttons: 0, dx: i16::MIN, dy: -1, wheel: i8::MIN, pan: -1 };
        let f = Frame::new(0, 0, Payload::Mouse(m));
        match Frame::decode(&f.encode()).unwrap().payload {
            Payload::Mouse(got) => assert_eq!(got, m),
            other => panic!("wrong payload: {other:?}"),
        }
    }

    #[test]
    fn rejects_foreign_and_malformed_frames() {
        assert_eq!(Frame::decode(&[]), Err(DecodeError::Truncated));
        assert_eq!(Frame::decode(&[0u8; FRAME_LEN]), Err(DecodeError::BadMagic));

        let mut wrong_version = Frame::new(0, 0, Payload::Ping).encode();
        wrong_version[1] = 99;
        assert_eq!(
            Frame::decode(&wrong_version),
            Err(DecodeError::VersionMismatch { got: 99 })
        );

        let mut bad_kind = Frame::new(0, 0, Payload::Ping).encode();
        bad_kind[2] = 77;
        assert_eq!(Frame::decode(&bad_kind), Err(DecodeError::UnknownKind(77)));

        let mut bad_edge = Frame::new(0, 0, Payload::Enter { edge: Edge::Left, ratio: 0 }).encode();
        bad_edge[6] = 9;
        assert_eq!(Frame::decode(&bad_edge), Err(DecodeError::MalformedPayload));
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
        assert!(!g.accept(10), "duplicate must be dropped");
        assert!(!g.accept(9), "straggler must be dropped");
        assert!(g.accept(11));
    }

    #[test]
    fn seq_gate_survives_wraparound() {
        // The whole point of the signed-distance comparison: a naive `>` would
        // stall the pointer for 32k frames every time the counter wraps.
        let mut g = SeqGate::new();
        assert!(g.accept(u16::MAX - 1));
        assert!(g.accept(u16::MAX));
        assert!(g.accept(0), "wrap to zero is newer, not older");
        assert!(g.accept(1));
        assert!(!g.accept(u16::MAX), "pre-wrap value is now a straggler");
    }
}
