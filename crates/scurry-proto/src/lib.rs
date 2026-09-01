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
pub const VERSION: u8 = 3;

/// Bytes before the payload.
pub const HEADER_LEN: usize = 8;

/// Largest payload we will accept. Bounds the dongle's reassembly buffer, which
/// has no allocator to grow one.
///
/// Raised from 256 when per-target input profiles were added: five screens at
/// 51 bytes plus a count byte is 256 exactly, and a format sitting precisely on
/// its own limit has no room for the next field.
pub const MAX_PAYLOAD: usize = 512;

/// One local screen plus up to four bonded targets.
pub const MAX_SCREENS: usize = 5;

/// Screen names are fixed-width so neither side needs an allocator.
pub const NAME_LEN: usize = 16;

/// Bytes one screen occupies on the wire: node, name, geometry, peer address,
/// input profile.
pub const SCREEN_WIRE_LEN: usize = 1 + NAME_LEN + 16 + 6 + InputProfile::WIRE_LEN;

/// Bytes the largest legal config occupies: a count byte, then every screen.
pub const MAX_CONFIG_PAYLOAD: usize = 1 + MAX_SCREENS * SCREEN_WIRE_LEN;

// It must fit in one payload, or the largest legal config could never be sent
// at all. Checked at compile time rather than in a test: a wire format that
// cannot carry its own maximum should fail the build, not wait for someone to
// run the suite.
const _: () = assert!(MAX_CONFIG_PAYLOAD <= MAX_PAYLOAD);

/// HID keyboard modifier bits, in the order the boot-protocol report uses them.
pub mod modifier {
    pub const LCTRL: u8 = 1 << 0;
    pub const LSHIFT: u8 = 1 << 1;
    pub const LALT: u8 = 1 << 2;
    pub const LGUI: u8 = 1 << 3;
    pub const RCTRL: u8 = 1 << 4;
    pub const RSHIFT: u8 = 1 << 5;
    pub const RALT: u8 = 1 << 6;
    pub const RGUI: u8 = 1 << 7;
}

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
    /// Controller -> dongle: keyboard state. Absolute, like mouse buttons: the
    /// full modifier byte and the full set of held keys, so a dropped message
    /// cannot leave a key stuck down on a machine nobody is looking at.
    pub const KEY: u8 = 0x02;
    pub const PING: u8 = 0x04;
    pub const PONG: u8 = 0x05;
    /// Dongle -> controller: the pointer now belongs to this node; payload is
    /// a single node id, 0 meaning the controller's own screen.
    ///
    /// Required, not informational. Since the dongle owns the layout, the
    /// controller cannot otherwise tell whether to swallow local input and hide
    /// its cursor -- it has no idea where the pointer went.
    pub const FOCUS: u8 = 0x06;

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

    /// Controller -> dongle: report the wireless control link's state.
    pub const GET_WIRELESS: u8 = 0x16;
    /// Dongle -> controller: see [`super::WirelessState`].
    pub const WIRELESS: u8 = 0x17;
    /// Controller -> dongle: open or close the pairing window, or forget the
    /// controller. Refused when it arrives over the wireless link itself --
    /// authorising a new controller is exactly the power an attacker would
    /// want, so it takes physical access.
    pub const SET_WIRELESS: u8 = 0x18;
}

/// Operations carried by [`kind::SET_WIRELESS`].
pub mod wireless_op {
    /// Forget the pinned controller and close any window.
    pub const FORGET: u8 = 0;
    /// Open the pairing window; second byte is how many seconds.
    pub const PAIR: u8 = 1;
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

/// Mouse behaviour flags, per target.
pub mod mouse_flag {
    pub const INVERT_X: u8 = 1 << 0;
    pub const INVERT_Y: u8 = 1 << 1;
    /// Reverse wheel direction, for a target whose own setting disagrees with
    /// the controller's.
    pub const NATURAL_SCROLL: u8 = 1 << 2;
    pub const SWAP_BUTTONS: u8 = 1 << 3;
}

/// How input is translated on its way to one target.
///
/// The dongle applies this, not the controller: it already owns routing and
/// knows which machine a report is bound for, so the controller stays a dumb
/// forwarder and the settings travel with the layout.
///
/// Defaults are the identity, so a layout that says nothing behaves exactly as
/// it did before profiles existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputProfile {
    /// Pointer speed as a percentage; 100 is unchanged.
    pub mouse_scale_pct: u16,
    /// See [`mouse_flag`].
    pub mouse_flags: u8,
    /// Indexed by host modifier bit, holding the target modifier mask to send
    /// in its place.
    ///
    /// This is what makes a Mac usable against anything else: the host sends
    /// Cmd (LGui) for copy and paste, while Linux and ChromeOS want Ctrl.
    /// Expressed as a full map rather than a swap flag because the useful cases
    /// are not symmetric -- swapping Cmd to Ctrl does not imply wanting Ctrl to
    /// become Cmd.
    pub mod_map: [u8; 8],
}

impl InputProfile {
    pub const WIRE_LEN: usize = 2 + 1 + 1 + 8;

    /// Pass everything through unchanged.
    pub const fn identity() -> Self {
        Self {
            mouse_scale_pct: 100,
            mouse_flags: 0,
            mod_map: [
                modifier::LCTRL,
                modifier::LSHIFT,
                modifier::LALT,
                modifier::LGUI,
                modifier::RCTRL,
                modifier::RSHIFT,
                modifier::RALT,
                modifier::RGUI,
            ],
        }
    }

    /// Swap Command and Control, for driving a PC-style target from a Mac.
    pub const fn swap_gui_ctrl() -> Self {
        let mut p = Self::identity();
        p.mod_map[3] = modifier::LCTRL; // host LGui  -> target LCtrl
        p.mod_map[0] = modifier::LGUI;  // host LCtrl -> target LGui
        p.mod_map[7] = modifier::RCTRL;
        p.mod_map[4] = modifier::RGUI;
        p
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::identity()
    }

    /// Translate a host modifier byte into the target's.
    pub fn map_modifiers(&self, host: u8) -> u8 {
        let mut out = 0u8;
        for (bit, target) in self.mod_map.iter().enumerate() {
            if host & (1 << bit) != 0 {
                out |= target;
            }
        }
        out
    }

    /// Apply pointer scaling and axis flips.
    pub fn map_motion(&self, dx: i16, dy: i16) -> (i16, i16) {
        let scale = |v: i16| -> i16 {
            let scaled = (v as i32 * self.mouse_scale_pct as i32) / 100;
            scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16
        };
        let mut x = scale(dx);
        let mut y = scale(dy);
        // saturating_neg, because negating i16::MIN would wrap to itself and
        // silently invert nothing on the fastest possible flick.
        if self.mouse_flags & mouse_flag::INVERT_X != 0 {
            x = x.saturating_neg();
        }
        if self.mouse_flags & mouse_flag::INVERT_Y != 0 {
            y = y.saturating_neg();
        }
        (x, y)
    }

    pub fn encode_into(&self, out: &mut [u8]) {
        out[0..2].copy_from_slice(&self.mouse_scale_pct.to_le_bytes());
        out[2] = self.mouse_flags;
        out[3] = 0; // reserved
        out[4..12].copy_from_slice(&self.mod_map);
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < Self::WIRE_LEN {
            return None;
        }
        let mut mod_map = [0u8; 8];
        mod_map.copy_from_slice(&b[4..12]);
        let mouse_scale_pct = u16::from_le_bytes([b[0], b[1]]);
        Some(Self {
            // A zero scale would freeze the pointer with no way to tell why, so
            // treat an unset or nonsensical value as unchanged.
            mouse_scale_pct: if mouse_scale_pct == 0 { 100 } else { mouse_scale_pct },
            mouse_flags: b[2],
            mod_map,
        })
    }
}

impl Default for InputProfile {
    fn default() -> Self {
        Self::identity()
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
    /// How input is translated for this target.
    pub profile: InputProfile,
    /// Bluetooth address of the machine this screen belongs to; all zero when
    /// unpinned.
    ///
    /// Node ids used to be connection slots, filled in whatever order machines
    /// happened to connect. After a dongle reboot the race could reverse two
    /// screens with no error anywhere -- push left and land on the machine that
    /// should have been on the right. Pinning to the peer address makes the
    /// layout mean the same thing every time.
    pub bda: [u8; 6],
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
        out[base + 16..base + 22].copy_from_slice(&self.bda);
        self.profile.encode_into(&mut out[base + 22..]);
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < SCREEN_WIRE_LEN {
            return None;
        }
        let mut name = [0u8; NAME_LEN];
        name.copy_from_slice(&b[1..1 + NAME_LEN]);
        let base = 1 + NAME_LEN;
        let rd = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let mut bda = [0u8; 6];
        bda.copy_from_slice(&b[base + 16..base + 22]);
        Some(Self {
            node: b[0],
            name,
            x: rd(base),
            y: rd(base + 4),
            width: rd(base + 8),
            height: rd(base + 12),
            bda,
            profile: InputProfile::decode(&b[base + 22..])?,
        })
    }

    pub fn name_str(&self) -> &str {
        let end = self.name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        core::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    pub fn with_name(node: u8, name: &str, x: i32, y: i32, width: i32, height: i32) -> Self {
        Self::pinned(node, name, x, y, width, height, [0u8; 6])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn pinned(
        node: u8,
        name: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        bda: [u8; 6],
    ) -> Self {
        let mut buf = [0u8; NAME_LEN];
        let src = name.as_bytes();
        let n = core::cmp::min(src.len(), NAME_LEN);
        buf[..n].copy_from_slice(&src[..n]);
        Self { node, name: buf, x, y, width, height, bda, profile: InputProfile::identity() }
    }

    /// True when this screen names a specific machine rather than whichever
    /// one happens to connect first.
    pub fn is_pinned(&self) -> bool {
        self.bda != [0u8; 6]
    }
}

/// Keyboard state: which modifiers and which keys are held, right now.
///
/// Absolute rather than press/release events, for the same reason mouse buttons
/// are: BLE does not retransmit, and a lost release would strand a key held
/// down on a machine the user is not looking at. Six keycodes is what the boot
/// protocol report carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyState {
    pub modifiers: u8,
    pub keys: [u8; 6],
}

impl KeyState {
    pub const WIRE_LEN: usize = 8;

    pub fn encode(&self) -> [u8; Self::WIRE_LEN] {
        let mut b = [0u8; Self::WIRE_LEN];
        b[0] = self.modifiers;
        b[2..8].copy_from_slice(&self.keys);
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < Self::WIRE_LEN {
            return None;
        }
        let mut keys = [0u8; 6];
        keys.copy_from_slice(&b[2..8]);
        Some(Self { modifiers: b[0], keys })
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

/// How many controllers the dongle will authorise at once.
///
/// More than one because a laptop is not the only thing that might drive this:
/// a phone should be able to take over without the laptop having to be
/// forgotten and re-paired to get it back. Exactly one drives at a time.
pub const MAX_CONTROLLERS: usize = 4;

/// The wireless control link's state, as it appears inside a
/// [`kind::WIRELESS`] payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WirelessState {
    /// A wireless controller currently has the wheel.
    pub wireless_driving: bool,
    /// The cable has it. Both false means nobody has sent input yet.
    ///
    /// Two flags rather than one, because "no wireless controller is driving"
    /// and "the cable is driving" are different states and the difference is
    /// what a person wants to see.
    pub cable_driving: bool,
    /// Which one is driving; all zero when none is.
    pub active: [u8; 6],
    /// Seconds left in the pairing window, 0 when closed.
    pub window_secs: u8,
    /// How many entries of `controllers` are meaningful.
    pub count: u8,
    /// Every authorised controller, driving or not.
    pub controllers: [[u8; 6]; MAX_CONTROLLERS],
}

impl Default for WirelessState {
    fn default() -> Self {
        Self {
            wireless_driving: false,
            cable_driving: false,
            active: [0u8; 6],
            window_secs: 0,
            count: 0,
            controllers: [[0u8; 6]; MAX_CONTROLLERS],
        }
    }
}

impl WirelessState {
    /// Bytes before the list of authorised controllers.
    pub const HEADER_LEN: usize = 9;

    pub fn count(&self) -> usize {
        core::cmp::min(self.count as usize, MAX_CONTROLLERS)
    }

    /// Every authorised controller, without the padding.
    pub fn controllers(&self) -> &[[u8; 6]] {
        &self.controllers[..self.count()]
    }

    /// True when this address is the one currently driving.
    pub fn is_active(&self, bda: &[u8; 6]) -> bool {
        self.active != [0u8; 6] && &self.active == bda
    }

    /// Returns how many bytes were written, or None if `out` is too small.
    pub fn encode_into(&self, out: &mut [u8]) -> Option<usize> {
        let n = self.count();
        let need = Self::HEADER_LEN + n * 6;
        if out.len() < need {
            return None;
        }
        out[..need].fill(0);
        out[0] = u8::from(self.wireless_driving) | (u8::from(self.cable_driving) << 1);
        out[1..7].copy_from_slice(&self.active);
        out[7] = self.window_secs;
        out[8] = n as u8;
        for (i, c) in self.controllers[..n].iter().enumerate() {
            out[Self::HEADER_LEN + i * 6..Self::HEADER_LEN + i * 6 + 6].copy_from_slice(c);
        }
        Some(need)
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < Self::HEADER_LEN {
            return None;
        }
        let mut active = [0u8; 6];
        active.copy_from_slice(&b[1..7]);
        // Clamped rather than trusted: a corrupt count must not make this read
        // past what actually arrived.
        let count = core::cmp::min(b[8] as usize, MAX_CONTROLLERS);
        let count = core::cmp::min(count, (b.len() - Self::HEADER_LEN) / 6);
        let mut controllers = [[0u8; 6]; MAX_CONTROLLERS];
        for (i, slot) in controllers.iter_mut().enumerate().take(count) {
            let off = Self::HEADER_LEN + i * 6;
            slot.copy_from_slice(&b[off..off + 6]);
        }
        Some(Self {
            wireless_driving: b[0] & 0x01 != 0,
            cable_driving: b[0] & 0x02 != 0,
            active,
            window_secs: b[7],
            count: count as u8,
            controllers,
        })
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
    rejected: u8,
}

/// Consecutive rejections after which the gate re-anchors.
///
/// A restarted controller begins counting from zero again, while the receiver
/// still remembers a high sequence from the previous session -- so every
/// message looks like an ancient straggler and input dies silently until the
/// counter climbs back past the old value, which can take thousands of
/// messages. Real reordering is a handful of messages at most, so a sustained
/// run of rejects means the peer restarted, not that the network is confused.
pub const SEQ_RESYNC_AFTER: u8 = 8;

impl SeqGate {
    pub fn new() -> Self {
        Self { last: None, rejected: 0 }
    }

    pub fn accept(&mut self, seq: u16) -> bool {
        match self.last {
            None => {
                self.last = Some(seq);
                self.rejected = 0;
                true
            }
            Some(prev) => {
                if (seq.wrapping_sub(prev) as i16) > 0 {
                    self.last = Some(seq);
                    self.rejected = 0;
                    true
                } else if self.rejected >= SEQ_RESYNC_AFTER {
                    // The peer restarted; re-anchor on its new numbering.
                    self.last = Some(seq);
                    self.rejected = 0;
                    true
                } else {
                    self.rejected += 1;
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
    fn pinned_screen_roundtrips() {
        let s = ScreenWire::pinned(2, "chromebook", 1512, 131, 1280, 720,
                                   [0xf0, 0x68, 0xe3, 0xe5, 0xd1, 0xb1]);
        let mut buf = [0u8; SCREEN_WIRE_LEN];
        s.encode_into(&mut buf);
        let got = ScreenWire::decode(&buf).unwrap();
        assert_eq!(got, s);
        assert!(got.is_pinned());
        assert_eq!(got.bda, [0xf0, 0x68, 0xe3, 0xe5, 0xd1, 0xb1]);
    }

    #[test]
    fn unpinned_screen_is_recognisable() {
        // An all-zero address means "whichever machine turns up", which is the
        // old behaviour and still valid for a layout the user has not pinned.
        let s = ScreenWire::with_name(1, "any", 0, 0, 100, 100);
        assert!(!s.is_pinned());
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
    fn wireless_state_roundtrips() {
        let mut w = WirelessState {
            wireless_driving: true,
            active: [0x84, 0x2f, 0x57, 0x29, 0x63, 0x64],
            window_secs: 42,
            count: 2,
            ..Default::default()
        };
        w.controllers[0] = [0x84, 0x2f, 0x57, 0x29, 0x63, 0x64];
        w.controllers[1] = [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];

        let mut buf = [0u8; 64];
        let n = w.encode_into(&mut buf).unwrap();
        assert_eq!(n, WirelessState::HEADER_LEN + 12);
        let got = WirelessState::decode(&buf[..n]).unwrap();
        assert_eq!(got, w);
        assert_eq!(got.controllers().len(), 2);
        assert!(got.is_active(&[0x84, 0x2f, 0x57, 0x29, 0x63, 0x64]));
        assert!(!got.is_active(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
    }

    #[test]
    fn the_cable_driving_is_distinct_from_nobody_driving() {
        // One flag could not tell them apart, and they are what a person is
        // actually asking about.
        let cable = WirelessState { cable_driving: true, ..Default::default() };
        let mut buf = [0u8; 16];
        let n = cable.encode_into(&mut buf).unwrap();
        let got = WirelessState::decode(&buf[..n]).unwrap();
        assert!(got.cable_driving && !got.wireless_driving);

        let idle = WirelessState::default();
        let n = idle.encode_into(&mut buf).unwrap();
        let got = WirelessState::decode(&buf[..n]).unwrap();
        assert!(!got.cable_driving && !got.wireless_driving);
    }

    #[test]
    fn a_lying_controller_count_cannot_read_past_the_payload() {
        // The dongle is trusted, but a truncated or corrupt reply is not: a
        // count of four with one address present must yield one, not a panic.
        let mut buf = [0u8; WirelessState::HEADER_LEN + 6];
        buf[8] = 4;
        let got = WirelessState::decode(&buf).unwrap();
        assert_eq!(got.controllers().len(), 1);
    }

    #[test]
    fn nothing_driving_is_not_mistaken_for_an_address() {
        // All-zero means "nobody", and must not compare equal to a real
        // controller that happens to be asked about.
        let w = WirelessState::default();
        assert!(!w.is_active(&[0u8; 6]));
    }

    #[test]
    fn control_kinds_do_not_collide_with_hot_path() {
        // The split at 0x10 is load-bearing: a reader tells the classes apart
        // by magnitude, so an overlap would route a config message into the
        // pointer path.
        for k in [kind::MOUSE, kind::PING, kind::PONG, kind::FOCUS] {
            assert!(k < 0x10, "hot-path kind {k:#x} must stay below 0x10");
        }
        for k in [
            kind::GET_CONFIG, kind::CONFIG, kind::SET_CONFIG,
            kind::GET_STATUS, kind::STATUS, kind::ACK,
            kind::GET_WIRELESS, kind::WIRELESS, kind::SET_WIRELESS,
        ] {
            assert!(k >= 0x10, "control kind {k:#x} must be 0x10 or above");
        }
    }

    #[test]
    fn identity_profile_changes_nothing() {
        let p = InputProfile::identity();
        assert_eq!(p.map_modifiers(modifier::LGUI), modifier::LGUI);
        assert_eq!(p.map_motion(37, -12), (37, -12));
        assert!(p.is_identity());
    }

    #[test]
    fn gui_ctrl_swap_is_two_way() {
        // Cmd+C on a Mac must arrive as Ctrl+C, and a target-side Ctrl must not
        // silently stay Ctrl -- otherwise both host modifiers map onto one.
        let p = InputProfile::swap_gui_ctrl();
        assert_eq!(p.map_modifiers(modifier::LGUI), modifier::LCTRL);
        assert_eq!(p.map_modifiers(modifier::LCTRL), modifier::LGUI);
        assert_eq!(p.map_modifiers(modifier::RGUI), modifier::RCTRL);
        // Untouched modifiers pass through.
        assert_eq!(p.map_modifiers(modifier::LSHIFT), modifier::LSHIFT);
        // Combinations map bit by bit.
        assert_eq!(
            p.map_modifiers(modifier::LGUI | modifier::LSHIFT),
            modifier::LCTRL | modifier::LSHIFT
        );
    }

    #[test]
    fn motion_scaling_and_inversion() {
        let mut p = InputProfile::identity();
        p.mouse_scale_pct = 200;
        assert_eq!(p.map_motion(10, -5), (20, -10));
        p.mouse_flags = mouse_flag::INVERT_Y;
        assert_eq!(p.map_motion(10, -5), (20, 10));
    }

    #[test]
    fn extreme_motion_does_not_wrap() {
        // Negating i16::MIN wraps to itself, so a fast flick would silently
        // fail to invert. saturating_neg keeps the sign correct.
        let mut p = InputProfile::identity();
        p.mouse_flags = mouse_flag::INVERT_Y;
        let (_, y) = p.map_motion(0, i16::MIN);
        assert!(y > 0, "inverting the largest negative delta must change sign");
    }

    #[test]
    fn zero_scale_is_treated_as_unchanged() {
        // An unset field would otherwise freeze the pointer with nothing to
        // explain why.
        let mut buf = [0u8; InputProfile::WIRE_LEN];
        InputProfile::identity().encode_into(&mut buf);
        buf[0] = 0;
        buf[1] = 0;
        assert_eq!(InputProfile::decode(&buf).unwrap().mouse_scale_pct, 100);
    }

    #[test]
    fn profile_survives_the_screen_round_trip() {
        let mut s = ScreenWire::pinned(2, "chromebook", 0, 0, 1280, 720, [1, 2, 3, 4, 5, 6]);
        s.profile = InputProfile::swap_gui_ctrl();
        s.profile.mouse_scale_pct = 150;
        let mut buf = [0u8; SCREEN_WIRE_LEN];
        s.encode_into(&mut buf);
        assert_eq!(ScreenWire::decode(&buf).unwrap(), s);
    }

    #[test]
    fn key_state_roundtrips() {
        let k = KeyState { modifiers: modifier::LSHIFT | modifier::LGUI, keys: [4, 5, 0, 0, 0, 0] };
        assert_eq!(KeyState::decode(&k.encode()).unwrap(), k);
        // Byte 1 is the boot protocol's reserved field and must stay zero.
        assert_eq!(k.encode()[1], 0);
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
    fn seq_gate_reanchors_when_the_peer_restarts() {
        // The bug: a restarted controller counts from 0 while the receiver
        // remembers thousands, so every message is dropped as a straggler and
        // input dies silently. Observed as 320 motion messages producing
        // nothing at all.
        let mut g = SeqGate::new();
        for s in 0..5000u16 {
            g.accept(s);
        }
        let restarted: Vec<bool> = (0..20u16).map(|s| g.accept(s)).collect();
        assert!(
            restarted[..SEQ_RESYNC_AFTER as usize].iter().all(|&a| !a),
            "a short burst of stragglers must still be rejected"
        );
        assert!(
            restarted[SEQ_RESYNC_AFTER as usize..].iter().all(|&a| a),
            "a sustained run means the peer restarted; re-anchor"
        );
    }

    #[test]
    fn brief_reordering_still_drops() {
        // Re-anchoring must not defeat the gate's actual purpose.
        let mut g = SeqGate::new();
        for s in 0..100u16 {
            g.accept(s);
        }
        assert!(!g.accept(97), "a straggler is still a straggler");
        assert!(!g.accept(98));
        assert!(g.accept(101), "and progress still gets through");
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
