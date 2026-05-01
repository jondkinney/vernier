//! Measurement state machine and distance math.
//!
//! A *measurement* is the segment between two snap points the user
//! committed during a drag. Snap points come from edge detection at
//! the cursor pixel; if no edge is present, the cursor pixel itself
//! serves as the snap point.

use crate::edge::{Direction, EdgeCandidate, EdgeQuad};
use crate::geometry::Px;

/// One end of a measurement: where the user clicked, plus optionally
/// the edge their cursor was snapped to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SnapPoint {
    /// Pixel actually used (after snapping). Equal to `cursor` when
    /// no edge was applied.
    pub pixel: Px,
    /// Where the user's cursor was when this snap was captured.
    pub cursor: Px,
    /// The edge that snapped, if any.
    pub edge: Option<EdgeCandidate>,
}

impl SnapPoint {
    pub fn loose(cursor: Px) -> Self {
        Self {
            pixel: cursor,
            cursor,
            edge: None,
        }
    }
}

/// A finished measurement spanning two snap points. Geometry helpers
/// compute distances in cardinal/diagonal terms.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub start: SnapPoint,
    pub end: SnapPoint,
}

impl Measurement {
    pub fn new(start: SnapPoint, end: SnapPoint) -> Self {
        Self { start, end }
    }

    /// Horizontal pixel distance between the two snap points.
    pub fn dx(&self) -> i32 {
        self.end.pixel.x - self.start.pixel.x
    }

    /// Vertical pixel distance between the two snap points.
    pub fn dy(&self) -> i32 {
        self.end.pixel.y - self.start.pixel.y
    }

    /// Width of the bounding box (always non-negative).
    pub fn width(&self) -> u32 {
        self.dx().unsigned_abs()
    }

    /// Height of the bounding box (always non-negative).
    pub fn height(&self) -> u32 {
        self.dy().unsigned_abs()
    }

    /// Euclidean pixel distance.
    pub fn euclid(&self) -> f64 {
        let x = self.dx() as f64;
        let y = self.dy() as f64;
        (x * x + y * y).sqrt()
    }

    /// True if the segment is essentially horizontal (height 0).
    pub fn is_horizontal(&self) -> bool {
        self.dy() == 0
    }

    /// True if the segment is essentially vertical (width 0).
    pub fn is_vertical(&self) -> bool {
        self.dx() == 0
    }
}

/// Pick the best snap candidate from a 4-direction edge scan.
///
/// "Best" today is "closest distance"; we'll likely refine this with
/// an axis-bias for measurement gestures (horizontal drag prefers
/// horizontal edges) once the HUD is wired up.
pub fn best_snap(cursor: Px, edges: &EdgeQuad) -> SnapPoint {
    let nearest = edges
        .iter()
        .filter_map(|c| c.as_ref())
        .min_by_key(|c| c.distance);
    match nearest {
        Some(e) => SnapPoint {
            pixel: e.position,
            cursor,
            edge: Some(*e),
        },
        None => SnapPoint::loose(cursor),
    }
}

/// Pick the best snap candidate, biased toward a particular axis.
/// Useful while the user is mid-drag and we know they're measuring
/// along an axis.
pub fn axis_biased_snap(cursor: Px, edges: &EdgeQuad, axis: Axis) -> SnapPoint {
    let want_horizontal = matches!(axis, Axis::Horizontal);
    let mut candidates: Vec<&EdgeCandidate> = edges.iter().filter_map(|c| c.as_ref()).collect();
    candidates.sort_by_key(|c| {
        let off_axis_penalty = match (c.direction, want_horizontal) {
            (Direction::Left | Direction::Right, true) => 0,
            (Direction::Up | Direction::Down, false) => 0,
            _ => u32::MAX / 2, // strongly de-prefer off-axis edges
        };
        c.distance.saturating_add(off_axis_penalty)
    });
    match candidates.first().copied().copied() {
        Some(e) => SnapPoint {
            pixel: e.position,
            cursor,
            edge: Some(e),
        },
        None => SnapPoint::loose(cursor),
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Top-level interaction state for the overlay.
#[derive(Clone, Debug)]
pub enum Mode {
    /// Overlay is hidden / not measuring.
    Idle,
    /// Overlay is shown; cursor moves trigger live edge detection but
    /// no segment is being drawn yet.
    Hover { cursor: Px },
    /// User pressed and is dragging out a measurement.
    Drawing { start: SnapPoint, cursor: Px },
    /// A measurement was committed (mouse released) and is being
    /// displayed; the next press starts a new one.
    Held { measurement: Measurement, cursor: Px },
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Rgba;
    use crate::edge::Direction;

    fn fake_edge(dir: Direction, distance: u32, pos: Px) -> EdgeCandidate {
        EdgeCandidate {
            direction: dir,
            distance,
            position: pos,
            anchor_color: Rgba::WHITE,
            edge_color: Rgba::BLACK,
            strength: 100,
        }
    }

    #[test]
    fn measurement_dimensions() {
        let m = Measurement::new(
            SnapPoint::loose(Px::new(10, 20)),
            SnapPoint::loose(Px::new(40, 80)),
        );
        assert_eq!(m.dx(), 30);
        assert_eq!(m.dy(), 60);
        assert_eq!(m.width(), 30);
        assert_eq!(m.height(), 60);
        assert!((m.euclid() - 67.0820).abs() < 0.01);
    }

    #[test]
    fn measurement_is_horizontal_when_dy_zero() {
        let m = Measurement::new(
            SnapPoint::loose(Px::new(10, 20)),
            SnapPoint::loose(Px::new(40, 20)),
        );
        assert!(m.is_horizontal());
        assert!(!m.is_vertical());
    }

    #[test]
    fn best_snap_picks_nearest_edge() {
        let cursor = Px::new(10, 10);
        let edges: EdgeQuad = [
            Some(fake_edge(Direction::Left, 5, Px::new(5, 10))),
            Some(fake_edge(Direction::Right, 3, Px::new(13, 10))),
            None,
            Some(fake_edge(Direction::Down, 7, Px::new(10, 17))),
        ];
        let snap = best_snap(cursor, &edges);
        assert_eq!(snap.pixel, Px::new(13, 10));
        assert_eq!(snap.cursor, cursor);
        assert!(matches!(
            snap.edge.map(|e| e.direction),
            Some(Direction::Right)
        ));
    }

    #[test]
    fn best_snap_returns_loose_when_no_edges() {
        let cursor = Px::new(10, 10);
        let edges: EdgeQuad = [None; 4];
        let snap = best_snap(cursor, &edges);
        assert_eq!(snap.pixel, cursor);
        assert!(snap.edge.is_none());
    }

    #[test]
    fn axis_biased_snap_prefers_axis_direction() {
        // Closest edge is "up" (distance 2) but we're measuring
        // horizontally — so the right edge (distance 5) should win.
        let cursor = Px::new(10, 10);
        let edges: EdgeQuad = [
            Some(fake_edge(Direction::Left, 9, Px::new(1, 10))),
            Some(fake_edge(Direction::Right, 5, Px::new(15, 10))),
            Some(fake_edge(Direction::Up, 2, Px::new(10, 8))),
            Some(fake_edge(Direction::Down, 99, Px::new(10, 109))),
        ];
        let snap = axis_biased_snap(cursor, &edges, Axis::Horizontal);
        assert!(matches!(
            snap.edge.map(|e| e.direction),
            Some(Direction::Right)
        ));
    }
}
