//! The virtual desktop and the rules for moving a pointer across it.
//!
//! # Model
//!
//! Every screen is a rectangle placed in one shared virtual coordinate space,
//! and the pointer has a single virtual position. Whichever screen contains
//! that position holds focus.
//!
//! The alternative — naming explicit left/right neighbours per screen, as
//! Synergy does — is simpler to configure but cannot express a 1080p display
//! sitting halfway up the side of a 4K one. Absolute placement gets mismatched
//! resolutions and staggered arrangements for free, and reduces handoff to a
//! point-in-rectangle test.
//!
//! # Gaps
//!
//! Screens need not tile the plane. If motion lands the pointer in empty space
//! it is clamped to the edge of the screen it was already on, so the pointer
//! can never be lost somewhere with no screen to draw it.

use scurry_proto::Edge;

/// One screen in the virtual desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    /// Node id this screen is reached through. The controller's own screen
    /// uses [`Screen::LOCAL`].
    pub node: u8,
    pub name: String,
    /// Top-left corner in virtual coordinates.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Screen {
    /// Node id meaning "the controller's own display" — input stays local and
    /// is never sent to the radio.
    pub const LOCAL: u8 = 0;

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn clamp(&self, x: i32, y: i32) -> (i32, i32) {
        (
            x.clamp(self.x, self.x + self.width - 1),
            y.clamp(self.y, self.y + self.height - 1),
        )
    }

    /// Where along `edge` the point (x, y) sits, as a fraction of that edge's
    /// length scaled to u16.
    ///
    /// This is what makes handoff feel continuous between screens of different
    /// sizes: leaving a 2160-tall screen 30% of the way down arrives 30% of the
    /// way down a 1080-tall one, rather than at an absolute pixel row that may
    /// not even exist on the neighbour.
    fn ratio_along(&self, edge: Edge, x: i32, y: i32) -> u16 {
        let (offset, span) = match edge {
            Edge::Left | Edge::Right => (y - self.y, self.height),
            Edge::Top | Edge::Bottom => (x - self.x, self.width),
        };
        if span <= 1 {
            return 0;
        }
        let clamped = offset.clamp(0, span - 1) as i64;
        ((clamped * u16::MAX as i64) / (span - 1) as i64) as u16
    }

    /// Inverse of [`Screen::ratio_along`]: a point on `edge` at `ratio`.
    fn point_on(&self, edge: Edge, ratio: u16) -> (i32, i32) {
        let along = |span: i32| -> i32 {
            if span <= 1 {
                return 0;
            }
            ((ratio as i64 * (span - 1) as i64) / u16::MAX as i64) as i32
        };
        match edge {
            Edge::Left => (self.x, self.y + along(self.height)),
            Edge::Right => (self.x + self.width - 1, self.y + along(self.height)),
            Edge::Top => (self.x + along(self.width), self.y),
            Edge::Bottom => (self.x + along(self.width), self.y + self.height - 1),
        }
    }
}

/// What the caller must do after feeding motion in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Motion {
    /// Pointer stayed put on the same screen.
    Stayed { node: u8, x: i32, y: i32 },
    /// Pointer crossed onto another screen. The caller must tell `from` to
    /// release its buttons, then tell `to` the pointer arrived.
    Crossed {
        from: u8,
        to: u8,
        /// Edge of the *destination* the pointer arrives at.
        edge: Edge,
        ratio: u16,
        x: i32,
        y: i32,
    },
}

/// The virtual desktop plus the current pointer position.
#[derive(Debug, Clone)]
pub struct Layout {
    screens: Vec<Screen>,
    current: usize,
    x: i32,
    y: i32,
}

/// Why a set of screens could not form a usable desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    Empty,
    NoLocalScreen,
    /// Two screens claim overlapping virtual space, so containment would be
    /// ambiguous and handoff non-deterministic.
    Overlap(String, String),
    DuplicateNode(u8),
}

impl core::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LayoutError::Empty => write!(f, "layout has no screens"),
            LayoutError::NoLocalScreen => {
                write!(f, "layout has no local screen (node {})", Screen::LOCAL)
            }
            LayoutError::Overlap(a, b) => {
                write!(f, "screens {a:?} and {b:?} overlap in virtual space")
            }
            LayoutError::DuplicateNode(n) => write!(f, "node id {n} is used by two screens"),
        }
    }
}

impl std::error::Error for LayoutError {}

impl Layout {
    /// Build a desktop, rejecting arrangements whose handoff would be ambiguous.
    pub fn new(screens: Vec<Screen>) -> Result<Self, LayoutError> {
        if screens.is_empty() {
            return Err(LayoutError::Empty);
        }
        let local = screens
            .iter()
            .position(|s| s.node == Screen::LOCAL)
            .ok_or(LayoutError::NoLocalScreen)?;

        for (i, a) in screens.iter().enumerate() {
            for b in &screens[i + 1..] {
                if a.node == b.node {
                    return Err(LayoutError::DuplicateNode(a.node));
                }
                let disjoint = a.x + a.width <= b.x
                    || b.x + b.width <= a.x
                    || a.y + a.height <= b.y
                    || b.y + b.height <= a.y;
                if !disjoint {
                    return Err(LayoutError::Overlap(a.name.clone(), b.name.clone()));
                }
            }
        }

        let (x, y) = (
            screens[local].x + screens[local].width / 2,
            screens[local].y + screens[local].height / 2,
        );
        Ok(Self { screens, current: local, x, y })
    }

    /// The screen currently holding the pointer.
    pub fn active(&self) -> &Screen {
        &self.screens[self.current]
    }

    /// True while input should be handled by the controller's own machine.
    pub fn is_local(&self) -> bool {
        self.active().node == Screen::LOCAL
    }

    pub fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// Apply relative motion and report whether the pointer changed screens.
    pub fn advance(&mut self, dx: i32, dy: i32) -> Motion {
        let from = self.active().node;
        let (nx, ny) = (self.x + dx, self.y + dy);

        if self.active().contains(nx, ny) {
            self.x = nx;
            self.y = ny;
            return Motion::Stayed { node: from, x: nx, y: ny };
        }

        if let Some(idx) = self.screens.iter().position(|s| s.contains(nx, ny)) {
            // Which edge of the destination did we come in through? Derive it
            // from the direction of travel rather than from geometry, so a
            // diagonal flick into a corner picks the dominant axis instead of
            // an arbitrary one.
            let edge = if dx.abs() >= dy.abs() {
                if dx > 0 { Edge::Left } else { Edge::Right }
            } else if dy > 0 {
                Edge::Top
            } else {
                Edge::Bottom
            };
            // The ratio must be measured on the screen being *left*, not the
            // one being entered. Measuring it on the destination would just
            // reinterpret the same absolute coordinate in the destination's
            // frame, which is the bug proportional handoff exists to avoid.
            //
            // The exit point is the motion clamped back onto the source, which
            // is more faithful than the pre-move position when a fast diagonal
            // overshoots well past the boundary.
            let src = self.active();
            let (ex, ey) = src.clamp(nx, ny);
            let ratio = src.ratio_along(edge.opposite(), ex, ey);

            let dest = &self.screens[idx];
            let (px, py) = dest.point_on(edge, ratio);

            self.current = idx;
            self.x = px;
            self.y = py;
            return Motion::Crossed { from, to: dest.node, edge, ratio, x: px, y: py };
        }

        // Empty space: hold the pointer against the edge it ran into.
        let (cx, cy) = self.active().clamp(nx, ny);
        self.x = cx;
        self.y = cy;
        Motion::Stayed { node: from, x: cx, y: cy }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(node: u8, name: &str, x: i32, y: i32, w: i32, h: i32) -> Screen {
        Screen { node, name: name.into(), x, y, width: w, height: h }
    }

    /// Local 1000x1000 at origin, a neighbour of the same size to its right.
    fn pair() -> Layout {
        Layout::new(vec![
            screen(Screen::LOCAL, "mac", 0, 0, 1000, 1000),
            screen(1, "linux", 1000, 0, 1000, 1000),
        ])
        .unwrap()
    }

    #[test]
    fn starts_centred_on_the_local_screen() {
        let l = pair();
        assert!(l.is_local());
        assert_eq!(l.position(), (500, 500));
    }

    #[test]
    fn motion_within_a_screen_does_not_hand_off() {
        let mut l = pair();
        assert_eq!(l.advance(10, -20), Motion::Stayed { node: 0, x: 510, y: 480 });
        assert!(l.is_local());
    }

    #[test]
    fn crossing_right_lands_on_the_neighbours_left_edge() {
        let mut l = pair();
        match l.advance(600, 0) {
            Motion::Crossed { from, to, edge, x, .. } => {
                assert_eq!((from, to), (0, 1));
                assert_eq!(edge, Edge::Left);
                assert_eq!(x, 1000, "should arrive on the destination's left edge");
            }
            other => panic!("expected a crossing, got {other:?}"),
        }
        assert!(!l.is_local());
    }

    #[test]
    fn handoff_is_reversible() {
        let mut l = pair();
        l.advance(600, 0);
        assert!(!l.is_local());
        match l.advance(-600, 0) {
            Motion::Crossed { from, to, edge, .. } => {
                assert_eq!((from, to), (1, 0));
                assert_eq!(edge, Edge::Right);
            }
            other => panic!("expected a crossing back, got {other:?}"),
        }
        assert!(l.is_local());
    }

    #[test]
    fn vertical_position_is_preserved_proportionally() {
        // A 1000-tall screen handing off to a 500-tall one: leaving at 25% down
        // must arrive at 25% down, not at absolute row 250 of a shorter screen.
        let mut l = Layout::new(vec![
            screen(Screen::LOCAL, "tall", 0, 0, 1000, 1000),
            screen(1, "short", 1000, 0, 1000, 500),
        ])
        .unwrap();
        l.advance(0, -250); // y = 250, a quarter down the tall screen
        match l.advance(600, 0) {
            Motion::Crossed { y, .. } => {
                // A quarter down a 500-tall screen is ~125, not 250.
                assert!((120..=130).contains(&y), "arrived at y={y}, expected ~125");
            }
            other => panic!("expected a crossing, got {other:?}"),
        }
    }

    #[test]
    fn pointer_is_clamped_at_the_desktop_boundary() {
        let mut l = pair();
        // Left of the local screen there is nothing at all.
        assert_eq!(l.advance(-9999, 0), Motion::Stayed { node: 0, x: 0, y: 500 });
        assert!(l.is_local(), "must not fall off the edge of the world");
    }

    #[test]
    fn gaps_do_not_swallow_the_pointer() {
        let mut l = Layout::new(vec![
            screen(Screen::LOCAL, "mac", 0, 0, 1000, 1000),
            screen(1, "far", 5000, 0, 1000, 1000),
        ])
        .unwrap();
        // Lands in the void between the two screens.
        assert_eq!(l.advance(2000, 0), Motion::Stayed { node: 0, x: 999, y: 500 });
        assert!(l.is_local());
    }

    #[test]
    fn diagonal_entry_picks_the_dominant_axis() {
        let mut l = Layout::new(vec![
            screen(Screen::LOCAL, "mac", 0, 0, 1000, 1000),
            screen(1, "below", 0, 1000, 1000, 1000),
        ])
        .unwrap();
        // Mostly downward with a slight rightward drift: must read as entering
        // the neighbour's top, not its left.
        match l.advance(50, 600) {
            Motion::Crossed { edge, .. } => assert_eq!(edge, Edge::Top),
            other => panic!("expected a crossing, got {other:?}"),
        }
    }

    #[test]
    fn rejects_ambiguous_desktops() {
        assert_eq!(Layout::new(vec![]).unwrap_err(), LayoutError::Empty);

        assert_eq!(
            Layout::new(vec![screen(3, "remote-only", 0, 0, 100, 100)]).unwrap_err(),
            LayoutError::NoLocalScreen
        );

        let overlapping = Layout::new(vec![
            screen(Screen::LOCAL, "a", 0, 0, 1000, 1000),
            screen(1, "b", 999, 0, 1000, 1000),
        ]);
        assert!(matches!(overlapping.unwrap_err(), LayoutError::Overlap(..)));

        let dupes = Layout::new(vec![
            screen(Screen::LOCAL, "a", 0, 0, 100, 100),
            screen(Screen::LOCAL, "b", 500, 0, 100, 100),
        ]);
        assert_eq!(dupes.unwrap_err(), LayoutError::DuplicateNode(0));
    }

    #[test]
    fn edge_ratio_round_trips_through_the_wire_type() {
        // ratio_along and point_on must be inverses, else the pointer would
        // drift a little further along the edge on every crossing.
        let s = screen(1, "s", 0, 0, 1920, 1080);
        for y in [0, 1, 539, 540, 1078, 1079] {
            let r = s.ratio_along(Edge::Left, 0, y);
            let (_, back) = s.point_on(Edge::Left, r);
            assert!((back - y).abs() <= 1, "y={y} came back as {back}");
        }
    }
}
