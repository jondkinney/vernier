//! Cursor-out edge detection.
//!
//! Given an RGBA8 frame and a cursor pixel, [`detect_edges`] scans
//! outward in each of the four cardinal directions and returns the
//! nearest pixel where the color delta from the cursor anchor exceeds
//! the configured tolerance.
//!
//! This is intentionally simple. Diagonal scanning, sub-pixel snapping,
//! and ranking of multiple candidates are follow-ups.

use crate::color::Rgba;
use crate::frame::FrameView;
use crate::geometry::Px;

/// One of the four scan axes used by [`detect_edges`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub const ALL: [Direction; 4] = [
        Direction::Left,
        Direction::Right,
        Direction::Up,
        Direction::Down,
    ];

    fn step(self) -> (i32, i32) {
        match self {
            Direction::Left => (-1, 0),
            Direction::Right => (1, 0),
            Direction::Up => (0, -1),
            Direction::Down => (0, 1),
        }
    }
}

/// Color tolerance for edge detection, expressed as the minimum
/// sum-of-channel difference from the anchor color (range 0..=765).
///
/// Smaller = more sensitive. The default of 30 is roughly "any visually
/// noticeable color change", chosen to catch anti-aliased edges without
/// firing on JPEG-style noise.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Tolerance(pub u32);

impl Tolerance {
    pub const DEFAULT: Tolerance = Tolerance(30);
    pub const STRICT: Tolerance = Tolerance(8);
    pub const LOOSE: Tolerance = Tolerance(90);
}

impl Default for Tolerance {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// One detected edge: where the scan stopped, how far that is from the
/// cursor, and the color delta there.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EdgeCandidate {
    pub direction: Direction,
    pub distance: u32,
    pub position: Px,
    pub anchor_color: Rgba,
    pub edge_color: Rgba,
    pub strength: u32,
}

/// Result of a 4-direction scan. `[Left, Right, Up, Down]` slots, each
/// `None` if no edge was found before hitting the frame boundary.
pub type EdgeQuad = [Option<EdgeCandidate>; 4];

/// Scan four directions from `cursor`. Returns one candidate per
/// direction (or `None` if the scan ran off the frame without finding an
/// edge). The order matches [`Direction::ALL`].
pub fn detect_edges(frame: &FrameView, cursor: Px, tolerance: Tolerance) -> EdgeQuad {
    let Some(anchor) = pixel_for_cursor(frame, cursor) else {
        return [None, None, None, None];
    };
    [
        scan(frame, cursor, Direction::Left, anchor, tolerance),
        scan(frame, cursor, Direction::Right, anchor, tolerance),
        scan(frame, cursor, Direction::Up, anchor, tolerance),
        scan(frame, cursor, Direction::Down, anchor, tolerance),
    ]
}

fn pixel_for_cursor(frame: &FrameView, cursor: Px) -> Option<Rgba> {
    if cursor.x < 0 || cursor.y < 0 {
        return None;
    }
    frame.pixel(cursor.x as u32, cursor.y as u32)
}

fn scan(
    frame: &FrameView,
    cursor: Px,
    dir: Direction,
    anchor: Rgba,
    tol: Tolerance,
) -> Option<EdgeCandidate> {
    let (dx, dy) = dir.step();
    let mut x = cursor.x;
    let mut y = cursor.y;
    let mut dist = 0u32;
    loop {
        x += dx;
        y += dy;
        dist += 1;
        if x < 0 || y < 0 {
            return None;
        }
        let Some(here) = frame.pixel(x as u32, y as u32) else {
            return None;
        };
        let delta = anchor.rgb_delta(here);
        if delta >= tol.0 {
            return Some(EdgeCandidate {
                direction: dir,
                distance: dist,
                position: Px { x, y },
                anchor_color: anchor,
                edge_color: here,
                strength: delta,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `width × height` packed RGBA8 buffer pre-filled with `bg`.
    fn solid(width: u32, height: u32, bg: Rgba) -> Vec<u8> {
        let mut v = Vec::with_capacity((width * height * 4) as usize);
        for _ in 0..(width * height) {
            v.extend_from_slice(&[bg.r, bg.g, bg.b, bg.a]);
        }
        v
    }

    fn put(buf: &mut [u8], width: u32, x: u32, y: u32, c: Rgba) {
        let i = ((y * width + x) * 4) as usize;
        buf[i..i + 4].copy_from_slice(&[c.r, c.g, c.b, c.a]);
    }

    #[test]
    fn solid_frame_has_no_edges() {
        let buf = solid(16, 16, Rgba::WHITE);
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        let edges = detect_edges(&frame, Px::new(8, 8), Tolerance::DEFAULT);
        assert!(edges.iter().all(|e| e.is_none()));
    }

    #[test]
    fn cursor_off_frame_returns_none() {
        let buf = solid(16, 16, Rgba::WHITE);
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        let edges = detect_edges(&frame, Px::new(99, 99), Tolerance::DEFAULT);
        assert!(edges.iter().all(|e| e.is_none()));
    }

    #[test]
    fn detects_edge_in_each_direction() {
        // White frame with one black column at x=11 and one black row at y=3.
        let mut buf = solid(16, 16, Rgba::WHITE);
        for y in 0..16 {
            put(&mut buf, 16, 11, y, Rgba::BLACK);
        }
        for x in 0..16 {
            put(&mut buf, 16, x, 3, Rgba::BLACK);
        }
        let frame = FrameView::packed(&buf, 16, 16).unwrap();

        let edges = detect_edges(&frame, Px::new(8, 8), Tolerance::DEFAULT);

        // Right: from x=8, the black column at x=11 → distance 3.
        let right = edges[1].expect("right edge");
        assert_eq!(right.direction, Direction::Right);
        assert_eq!(right.distance, 3);
        assert_eq!(right.position, Px::new(11, 8));
        assert_eq!(right.edge_color, Rgba::BLACK);

        // Up: from y=8, the black row at y=3 → distance 5.
        let up = edges[2].expect("up edge");
        assert_eq!(up.direction, Direction::Up);
        assert_eq!(up.distance, 5);
        assert_eq!(up.position, Px::new(8, 3));

        // Left and Down should hit the frame edge with no edge in between.
        assert!(edges[0].is_none(), "left should run off frame");
        assert!(edges[3].is_none(), "down should run off frame");
    }

    #[test]
    fn returns_nearest_when_multiple_edges_present() {
        // Two black columns at x=10 and x=14. From x=8 the nearest is x=10.
        let mut buf = solid(16, 16, Rgba::WHITE);
        for y in 0..16 {
            put(&mut buf, 16, 10, y, Rgba::BLACK);
            put(&mut buf, 16, 14, y, Rgba::BLACK);
        }
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        let edges = detect_edges(&frame, Px::new(8, 8), Tolerance::DEFAULT);
        let right = edges[1].expect("right");
        assert_eq!(right.distance, 2);
        assert_eq!(right.position, Px::new(10, 8));
    }

    #[test]
    fn anti_aliased_edge_catches_first_transition() {
        // White → mid-gray (AA) → black across x=8..=10. With default
        // tolerance (30) the gray pixel already exceeds the threshold.
        let mut buf = solid(16, 16, Rgba::WHITE);
        let gray = Rgba::new(180, 180, 180, 255);
        for y in 0..16 {
            put(&mut buf, 16, 9, y, gray);
            put(&mut buf, 16, 10, y, Rgba::BLACK);
        }
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        let edges = detect_edges(&frame, Px::new(7, 8), Tolerance::DEFAULT);
        let right = edges[1].expect("right");
        assert_eq!(right.position, Px::new(9, 8));
        assert_eq!(right.edge_color, gray);
    }

    #[test]
    fn strict_tolerance_skips_subtle_changes() {
        // 1-step gradient should NOT trip the strict tolerance
        // (delta = 3, threshold = 8) but should at default (30) — wait,
        // delta=3 also fails default. Use a delta-of-12 step.
        let mut buf = solid(16, 16, Rgba::new(200, 200, 200, 255));
        let near = Rgba::new(196, 196, 196, 255); // delta = 12
        for y in 0..16 {
            put(&mut buf, 16, 12, y, near);
        }
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        // Default (30): no edge found — delta 12 < 30.
        assert!(detect_edges(&frame, Px::new(8, 8), Tolerance::DEFAULT)[1].is_none());
        // Strict (8): edge found at x=12.
        let edges = detect_edges(&frame, Px::new(8, 8), Tolerance::STRICT);
        assert_eq!(edges[1].expect("strict right").position, Px::new(12, 8));
    }

    #[test]
    fn ignores_alpha_channel() {
        // Two pixels with the same RGB but different alpha should NOT
        // count as an edge.
        let mut buf = solid(16, 16, Rgba::new(120, 120, 120, 255));
        let translucent_same = Rgba::new(120, 120, 120, 50);
        for y in 0..16 {
            put(&mut buf, 16, 11, y, translucent_same);
        }
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        let edges = detect_edges(&frame, Px::new(8, 8), Tolerance::DEFAULT);
        assert!(edges[1].is_none());
    }
}
