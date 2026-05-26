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

/// Color tolerance for edge **detection**, expressed as the minimum
/// sum-of-channel difference from the anchor color (range 0..=765).
///
/// Smaller = more sensitive. The default of 30 is roughly "any visually
/// noticeable color change", chosen to catch anti-aliased edges without
/// firing on JPEG-style noise.
///
/// Tolerance governs only *whether* a transition registers as an edge.
/// Once an edge is detected, *where* it is localized (the soft-edge
/// midpoint) is computed independently of this value — see
/// [`localize_edge`]. Adjusting tolerance never moves a measurement.
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

/// Where along a soft edge a measurement should land. A soft edge has
/// a multi-pixel ramp between two stable plateaus; this selects which
/// of three principled points the localizer reports.
///
/// On a *crisp* edge the ramp is zero pixels wide, so all three values
/// collapse to the same half-pixel boundary — `EdgeBias` has no effect.
/// The choice only matters for genuinely soft edges (anti-aliasing,
/// shadows, glows, fractional-scaling resampling).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum EdgeBias {
    /// The inside-plateau side of the ramp — boundary at
    /// `last_inside_pixel + 0.5`. Reports the SMALLEST extent
    /// (excludes the entire soft transition).
    Inner,
    /// The 50%-brightness crossing between the two plateaus — the
    /// principled perceptual midpoint. Default; right for typical
    /// anti-aliased UI content.
    #[default]
    Midpoint,
    /// The outside-plateau side of the ramp — boundary at
    /// `first_outside_pixel - 0.5`. Reports the LARGEST extent
    /// (includes the entire soft transition).
    Outer,
}

impl EdgeBias {
    /// Human-readable label for HUD toasts and log lines.
    pub fn label(self) -> &'static str {
        match self {
            EdgeBias::Inner => "Inner",
            EdgeBias::Midpoint => "Midpoint",
            EdgeBias::Outer => "Outer",
        }
    }
    /// Cycle to the next bias: Inner → Midpoint → Outer → Inner. Used
    /// by the cycle-bias hotkey so a single key sweeps the three modes.
    pub fn cycle(self) -> Self {
        match self {
            EdgeBias::Inner => EdgeBias::Midpoint,
            EdgeBias::Midpoint => EdgeBias::Outer,
            EdgeBias::Outer => EdgeBias::Inner,
        }
    }
}

/// One detected edge: where the scan stopped, how far that is from the
/// cursor, and the color delta there.
///
/// `position` / `distance` mark the first over-tolerance pixel — the
/// near side of the transition. `edge_phys` is the *localized* edge:
/// the gradient's perceptual midpoint as a possibly-fractional
/// coordinate (see [`localize_edge`]). On a crisp edge `edge_phys`
/// collapses to the exact boundary line between the last inside pixel
/// and the first outside pixel. Measurements must use `edge_phys`, not
/// `position`, so soft and hard edges localise consistently.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct EdgeCandidate {
    pub direction: Direction,
    pub distance: u32,
    pub position: Px,
    pub anchor_color: Rgba,
    pub edge_color: Rgba,
    pub strength: u32,
    /// Fractional physical-pixel coordinate of the localized edge
    /// boundary along the scan axis — an x for [`Direction::Left`] /
    /// [`Direction::Right`], a y for [`Direction::Up`] /
    /// [`Direction::Down`].
    pub edge_phys: f64,
}

/// Result of a 4-direction scan. `[Left, Right, Up, Down]` slots, each
/// `None` if no edge was found before hitting the frame boundary.
pub type EdgeQuad = [Option<EdgeCandidate>; 4];

/// Scan four directions from `cursor`. Returns one candidate per
/// direction (or `None` if the scan ran off the frame without finding an
/// edge). The order matches [`Direction::ALL`]. `bias` selects which
/// point along a soft edge the localizer reports — see [`EdgeBias`].
pub fn detect_edges(
    frame: &FrameView,
    cursor: Px,
    tolerance: Tolerance,
    bias: EdgeBias,
) -> EdgeQuad {
    let Some(anchor) = pixel_for_cursor(frame, cursor) else {
        return [None, None, None, None];
    };
    [
        scan(frame, cursor, Direction::Left, anchor, tolerance, bias),
        scan(frame, cursor, Direction::Right, anchor, tolerance, bias),
        scan(frame, cursor, Direction::Up, anchor, tolerance, bias),
        scan(frame, cursor, Direction::Down, anchor, tolerance, bias),
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
    bias: EdgeBias,
) -> Option<EdgeCandidate> {
    let (dx, dy) = dir.step();
    // `sample(k)` probes the pixel `k` steps outward along this axis.
    let sample = |k: i32| -> Option<Rgba> {
        let x = cursor.x + dx * k;
        let y = cursor.y + dy * k;
        if x < 0 || y < 0 {
            return None;
        }
        frame.pixel(x as u32, y as u32)
    };
    let (first_over, here, boundary) = localize_edge(sample, anchor, tol.0, bias)?;
    let pos = Px {
        x: cursor.x + dx * first_over,
        y: cursor.y + dy * first_over,
    };
    // `boundary` is a fractional outward offset; project it back onto
    // the scan axis to an absolute fractional frame coordinate.
    let edge_phys = match dir {
        Direction::Left => cursor.x as f64 - boundary,
        Direction::Right => cursor.x as f64 + boundary,
        Direction::Up => cursor.y as f64 - boundary,
        Direction::Down => cursor.y as f64 + boundary,
    };
    Some(EdgeCandidate {
        direction: dir,
        distance: first_over as u32,
        position: pos,
        anchor_color: anchor,
        edge_color: here,
        strength: anchor.rgb_delta(here),
        edge_phys,
    })
}

/// Widest gradient, in pixels, the localizer will walk before giving
/// up and treating the transition as crisp. A genuine anti-aliasing
/// ramp is only a handful of pixels wide; this cap bounds the scan on
/// noisy or photographic content whose colour never truly plateaus.
const MAX_GRADIENT: i32 = 64;

/// Flatness threshold (sum-of-channel delta) for deciding the colour
/// has stopped changing — i.e. the scan has reached a stable plateau,
/// or two plateaus are "the same colour".
///
/// This is a property of the *image* (pixels identical modulo
/// dithering noise), NOT a user preference. It is deliberately
/// independent of the detection [`Tolerance`]: adjusting tolerance
/// changes only WHICH transitions register as edges, never WHERE a
/// registered edge is localized.
const PLATEAU_EPS: u32 = 8;

/// Localize a (possibly soft / anti-aliased) edge to its perceptual
/// midpoint — the shared routine behind both [`scan`] (crosshair mode)
/// and [`shrink_to_content_with_bg`] (area-rectangle mode), so the two
/// measurement modes can't drift apart on a gradient.
///
/// `sample(k)` probes the colour `k` pixels outward from the scan
/// anchor (`k = 0` is the anchor); it returns `None` once the scan
/// walks off the frame. `anchor` is the stable inside colour.
///
/// Detection and localization are kept strictly separate:
///
/// - **Detection** (phase 1) uses the user's `tol`: find `first_over`,
///   the nearest pixel whose delta from `anchor` exceeds tolerance.
///   This is the *only* place `tol` is consulted. Strict `>` (not
///   `>=`) so `Tolerance(0)` means "stop on any colour change at all".
/// - **Localization** (phases 2–3) is tolerance-independent. Phase 2
///   walks to the outside plateau colour using the fixed
///   [`PLATEAU_EPS`]; phase 3 ([`localize_within`]) returns the
///   sub-pixel 50%-brightness crossing between the two plateaus.
///
/// Because the localized position comes from interpolating the two
/// plateau colours — not from where `first_over` happened to land —
/// a detected edge resolves to the *same* point at any tolerance.
///
/// On a crisp edge this collapses to exactly `first_over - 0.5` — the
/// boundary line between the last inside pixel and the first outside
/// pixel — so hard-edge measurements stay byte-identical.
///
/// Returns `(first_over, first_over_color, boundary)` where `boundary`
/// is the fractional outward offset of the localized edge, or `None`
/// if no over-tolerance pixel is found before the frame edge.
fn localize_edge(
    sample: impl Fn(i32) -> Option<Rgba>,
    anchor: Rgba,
    tol: u32,
    bias: EdgeBias,
) -> Option<(i32, Rgba, f64)> {
    // Phase 1 — DETECTION. Find the first pixel that breaks the user's
    // tolerance with the anchor.
    let mut first_over = 1;
    let first_over_color = loop {
        let here = sample(first_over)?;
        if anchor.rgb_delta(here) > tol {
            break here;
        }
        first_over += 1;
    };
    // Phase 2 — find the OUTSIDE PLATEAU. Walk through the transition
    // until the colour stabilises: two consecutive pixels within
    // `PLATEAU_EPS` of each other mark the plateau's first pixel.
    let last_inside = first_over - 1;
    let limit = first_over + MAX_GRADIENT;
    let mut prev = first_over_color;
    let mut i = first_over + 1;
    let first_outside = loop {
        if i > limit {
            // No plateau within the cap — treat the transition as
            // crisp and localise to its near side.
            break first_over;
        }
        match sample(i) {
            Some(p) => {
                if prev.rgb_delta(p) <= PLATEAU_EPS {
                    // `prev` (at i-1) and `p` (at i) agree: the
                    // outside plateau started at i-1.
                    break i - 1;
                }
                prev = p;
                i += 1;
            }
            // Gradient ran to the frame edge; the last in-bounds pixel
            // is as far as the outside plateau gets.
            None => break i - 1,
        }
    };
    let outside = sample(first_outside)?;
    // Phase 3 — LOCALIZATION (tolerance-independent).
    let boundary = localize_within(sample, anchor, outside, last_inside, first_outside, bias);
    Some((first_over, first_over_color, boundary))
}

/// Localize the edge within the transition that ends at `first_outside`,
/// given the inside plateau colour `inside`, the outside plateau colour
/// `outside`, and the user's [`EdgeBias`]. Returns a fractional offset
/// in `sample`'s coordinate.
///
/// - [`EdgeBias::Inner`] / [`EdgeBias::Outer`] return the half-pixel
///   boundary just inside the inside plateau / outside plateau —
///   constant-time, independent of the ramp shape.
/// - [`EdgeBias::Midpoint`] returns the sub-pixel **50%-brightness
///   crossing**: walking outward, the point where the colour —
///   projected onto the `inside → outside` axis — passes the halfway
///   mark. Depends only on the two plateau colours and the ramp shape,
///   never on the detection tolerance, and is barely sensitive to a
///   mis-judged plateau. When the plateaus are ~equal
///   (`outside ≈ inside` within [`PLATEAU_EPS`] — a thin line or other
///   feature the scan passed through and back out of) there is no
///   meaningful brightness midpoint, so it falls back to the geometric
///   centre of the transition run.
///
/// On a crisp edge the gradient is zero pixels wide
/// (`first_outside == last_inside + 1`), so all three biases collapse
/// to exactly `last_inside + 0.5` — bias has no effect on hard edges.
fn localize_within(
    sample: impl Fn(i32) -> Option<Rgba>,
    inside: Rgba,
    outside: Rgba,
    last_inside: i32,
    first_outside: i32,
    bias: EdgeBias,
) -> f64 {
    match bias {
        EdgeBias::Inner => last_inside as f64 + 0.5,
        EdgeBias::Outer => first_outside as f64 - 0.5,
        EdgeBias::Midpoint => {
            let geometric_centre = (last_inside + first_outside) as f64 / 2.0;
            // Barely-distinguishable plateaus → thin feature, no
            // meaningful 50% crossing.
            if inside.rgb_delta(outside) <= PLATEAU_EPS {
                return geometric_centre;
            }
            // Colour axis from the inside plateau to the outside plateau.
            let axis = (
                outside.r as i32 - inside.r as i32,
                outside.g as i32 - inside.g as i32,
                outside.b as i32 - inside.b as i32,
            );
            let den = axis.0 * axis.0 + axis.1 * axis.1 + axis.2 * axis.2;
            // `proj(c)` ∈ [0, 1]: how far colour `c` has travelled
            // from the inside plateau (0.0) toward the outside (1.0).
            let proj = |c: Rgba| -> f64 {
                let d = (
                    c.r as i32 - inside.r as i32,
                    c.g as i32 - inside.g as i32,
                    c.b as i32 - inside.b as i32,
                );
                (d.0 * axis.0 + d.1 * axis.1 + d.2 * axis.2) as f64 / den as f64
            };

            // Walk the transition; the first consecutive pair
            // straddling the 0.5 mark brackets the crossing —
            // interpolate the sub-pixel position linearly. `k = 0` is
            // the anchor itself (`proj == 0`).
            let mut prev_k = 0;
            let mut prev_p = 0.0;
            for k in 1..=first_outside {
                let Some(c) = sample(k) else { break };
                let p = proj(c);
                if prev_p <= 0.5 && p > 0.5 {
                    return prev_k as f64 + (0.5 - prev_p) / (p - prev_p);
                }
                prev_k = k;
                prev_p = p;
            }
            // No clean upward crossing (non-monotone / degenerate ramp).
            geometric_centre
        }
    }
}

/// Shrink the rectangle `(x0, y0, x1, y1)` to the content bounding box
/// within `frame`, returning INCLUSIVE integer content-pixel bounds.
///
/// This is the integer-rounded wrapper around
/// [`shrink_to_content_frac`]; see that function for the algorithm.
/// The returned `(left, top, right, bottom)` are inclusive content
/// pixels — an N-physical-pixel-wide region has `right - left + 1 ==
/// N`. Callers that need the soft-edge-aware fractional bounds (and
/// the `+1`-free span) should call [`shrink_to_content_frac`] instead.
pub fn shrink_to_content(
    frame: &FrameView,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    tolerance: Tolerance,
    bias: EdgeBias,
) -> (i32, i32, i32, i32) {
    round_bounds(shrink_to_content_frac(
        frame, x0, y0, x1, y1, tolerance, bias,
    ))
}

/// Integer-rounded wrapper around [`shrink_to_content_with_bg_frac`].
/// Returns INCLUSIVE integer content-pixel bounds — width is
/// `right - left + 1`, height is `bottom - top + 1`.
#[allow(clippy::too_many_arguments)]
pub fn shrink_to_content_with_bg(
    frame: &FrameView,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    bg: Px,
    tolerance: Tolerance,
    bias: EdgeBias,
) -> (i32, i32, i32, i32) {
    round_bounds(shrink_to_content_with_bg_frac(
        frame, x0, y0, x1, y1, bg, tolerance, bias,
    ))
}

/// Convert fractional edge-boundary bounds back to inclusive integer
/// content pixels: a left/top boundary sits at `pixel - 0.5`, a
/// right/bottom boundary at `pixel + 0.5`, so the inverse is `+0.5` /
/// `-0.5` then round. On a crisp edge this is exact.
fn round_bounds((l, t, r, b): (f64, f64, f64, f64)) -> (i32, i32, i32, i32) {
    (
        (l + 0.5).round() as i32,
        (t + 0.5).round() as i32,
        (r - 0.5).round() as i32,
        (b - 0.5).round() as i32,
    )
}

/// Shrink the rectangle `(x0, y0, x1, y1)` to the content bounding box
/// within `frame`. Walks inward from each side until hitting the first
/// row/column with pixels that differ from the rect's top-left corner
/// pixel by more than `tolerance`, then localises each side to its
/// soft-edge midpoint via [`localize_edge`]. Useful for "fit-to-content"
/// snapping on a user-dragged region.
///
/// The returned `(left, top, right, bottom)` are fractional edge
/// *boundaries* — the half-pixel lines bracketing the content, so the
/// span is `right - left` directly with NO `+1`. On a crisp edge the
/// left/top boundary lands on `content_pixel - 0.5` and the
/// right/bottom on `content_pixel + 0.5`, so `right - left` still
/// equals the inclusive `last - first + 1` count.
///
/// Coordinates are in frame pixel space and may extend outside the
/// frame; they're clamped before scanning. If shrinking would
/// degenerate the rect to zero/negative area, the original
/// (unclamped) rect is returned as boundaries unchanged.
pub fn shrink_to_content_frac(
    frame: &FrameView,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    tolerance: Tolerance,
    bias: EdgeBias,
) -> (f64, f64, f64, f64) {
    // Default bg sample = top-left of the input rect, matching the
    // original draw-from-cursor-out behavior.
    let bg_x = x0.min(x1).max(0).min(frame.width as i32 - 1);
    let bg_y = y0.min(y1).max(0).min(frame.height as i32 - 1);
    shrink_to_content_with_bg_frac(frame, x0, y0, x1, y1, Px::new(bg_x, bg_y), tolerance, bias)
}

/// Same as [`shrink_to_content_frac`] but lets the caller pick the bg
/// reference pixel explicitly. Useful for resize, where the rect's
/// own top-left can land inside content and the default sample would
/// collapse the algorithm.
///
/// Returns fractional edge boundaries — see [`shrink_to_content_frac`].
#[allow(clippy::too_many_arguments)]
pub fn shrink_to_content_with_bg_frac(
    frame: &FrameView,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    bg: Px,
    tolerance: Tolerance,
    bias: EdgeBias,
) -> (f64, f64, f64, f64) {
    // The degenerate fallback returns the original (unclamped) rect as
    // boundaries: an inclusive pixel `p` becomes the boundary `p ∓ 0.5`,
    // so `round_bounds` recovers `(x0, y0, x1, y1)` exactly.
    let fallback = (
        x0 as f64 - 0.5,
        y0 as f64 - 0.5,
        x1 as f64 + 0.5,
        y1 as f64 + 0.5,
    );
    let (rx0, rx1) = (x0.min(x1), x0.max(x1));
    let (ry0, ry1) = (y0.min(y1), y0.max(y1));
    let fw = frame.width as i32;
    let fh = frame.height as i32;
    let cx0 = rx0.max(0).min(fw - 1);
    let cy0 = ry0.max(0).min(fh - 1);
    let cx1 = rx1.max(0).min(fw - 1);
    let cy1 = ry1.max(0).min(fh - 1);
    if cx1 <= cx0 || cy1 <= cy0 {
        return fallback;
    }
    let bx = bg.x.max(0).min(fw - 1);
    let by = bg.y.max(0).min(fh - 1);
    let bg = match frame.pixel(bx as u32, by as u32) {
        Some(p) => p,
        None => return fallback,
    };
    let tol = tolerance.0;

    let row_has_content = |y: i32, x_start: i32, x_end: i32| -> bool {
        for x in x_start..=x_end {
            if let Some(p) = frame.pixel(x as u32, y as u32) {
                if bg.rgb_delta(p) > tol {
                    return true;
                }
            }
        }
        false
    };
    let col_has_content = |x: i32, y_start: i32, y_end: i32| -> bool {
        for y in y_start..=y_end {
            if let Some(p) = frame.pixel(x as u32, y as u32) {
                if bg.rgb_delta(p) > tol {
                    return true;
                }
            }
        }
        false
    };

    let mut new_top = cy0;
    for y in cy0..=cy1 {
        if row_has_content(y, cx0, cx1) {
            new_top = y;
            break;
        }
    }
    let mut new_bot = cy1;
    for y in (new_top..=cy1).rev() {
        if row_has_content(y, cx0, cx1) {
            new_bot = y;
            break;
        }
    }
    let mut new_left = cx0;
    for x in cx0..=cx1 {
        if col_has_content(x, new_top, new_bot) {
            new_left = x;
            break;
        }
    }
    let mut new_right = cx1;
    for x in (new_left..=cx1).rev() {
        if col_has_content(x, new_top, new_bot) {
            new_right = x;
            break;
        }
    }

    if new_right <= new_left || new_bot <= new_top {
        return fallback;
    }

    // Refine each integer side to its soft-edge midpoint by running the
    // shared localizer along a single probe line through the content
    // box's centre. The localizer walks inward from `bg`; if the probe
    // line misses the content (it never trips tolerance) — or the
    // rect's clamp already sits inside content, so the probe can't even
    // start on the background plateau — fall back to the crisp integer
    // boundary `pixel ∓ 0.5`.
    let mid_x = (new_left + new_right) / 2;
    let mid_y = (new_top + new_bot) / 2;
    // True only if `p` is a real background pixel the probe can anchor
    // on. A probe whose start pixel is already content has no clean
    // edge to localise.
    let on_bg = |p: Option<Rgba>| matches!(p, Some(p) if bg.rgb_delta(p) <= tol);
    // Horizontal probe: walk along row `at_y` from `start_x` stepping
    // `step` (+1 inward from the left side, -1 inward from the right).
    let probe_h = |start_x: i32, at_y: i32, step: i32, fallback: f64| -> f64 {
        let sample = |k: i32| -> Option<Rgba> {
            let x = start_x + step * k;
            if x < 0 || at_y < 0 {
                return None;
            }
            frame.pixel(x as u32, at_y as u32)
        };
        if !on_bg(sample(0)) {
            return fallback;
        }
        match localize_edge(sample, bg, tol, bias) {
            Some((_, _, boundary)) => start_x as f64 + step as f64 * boundary,
            None => fallback,
        }
    };
    let probe_v = |at_x: i32, start_y: i32, step: i32, fallback: f64| -> f64 {
        let sample = |k: i32| -> Option<Rgba> {
            let y = start_y + step * k;
            if y < 0 || at_x < 0 {
                return None;
            }
            frame.pixel(at_x as u32, y as u32)
        };
        if !on_bg(sample(0)) {
            return fallback;
        }
        match localize_edge(sample, bg, tol, bias) {
            Some((_, _, boundary)) => start_y as f64 + step as f64 * boundary,
            None => fallback,
        }
    };

    let left = probe_h(cx0, mid_y, 1, new_left as f64 - 0.5);
    let right = probe_h(cx1, mid_y, -1, new_right as f64 + 0.5);
    let top = probe_v(mid_x, cy0, 1, new_top as f64 - 0.5);
    let bottom = probe_v(mid_x, cy1, -1, new_bot as f64 + 0.5);
    (left, top, right, bottom)
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
        let edges = detect_edges(
            &frame,
            Px::new(8, 8),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );
        assert!(edges.iter().all(|e| e.is_none()));
    }

    #[test]
    fn cursor_off_frame_returns_none() {
        let buf = solid(16, 16, Rgba::WHITE);
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        let edges = detect_edges(
            &frame,
            Px::new(99, 99),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );
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

        let edges = detect_edges(
            &frame,
            Px::new(8, 8),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );

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
        let edges = detect_edges(
            &frame,
            Px::new(8, 8),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );
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
        let edges = detect_edges(
            &frame,
            Px::new(7, 8),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );
        let right = edges[1].expect("right");
        // `position` still marks the first over-tolerance pixel (the
        // near side of the transition)…
        assert_eq!(right.position, Px::new(9, 8));
        assert_eq!(right.edge_color, gray);
        // …but `edge_phys` localises to the perceptual midpoint: the
        // last white pixel is x=8, the first white pixel past the dark
        // gray+black feature is x=11, so the boundary is (8 + 11) / 2.
        assert_eq!(right.edge_phys, 9.5);
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
        assert!(
            detect_edges(
                &frame,
                Px::new(8, 8),
                Tolerance::DEFAULT,
                EdgeBias::Midpoint
            )[1]
            .is_none()
        );
        // Strict (8): edge found at x=12.
        let edges = detect_edges(&frame, Px::new(8, 8), Tolerance::STRICT, EdgeBias::Midpoint);
        assert_eq!(edges[1].expect("strict right").position, Px::new(12, 8));
    }

    #[test]
    fn shrink_fits_inner_content() {
        // 32x32 white frame with a black 8x8 block at (12..20, 14..22).
        let mut buf = solid(32, 32, Rgba::WHITE);
        for y in 14..22 {
            for x in 12..20 {
                put(&mut buf, 32, x, y, Rgba::BLACK);
            }
        }
        let frame = FrameView::packed(&buf, 32, 32).unwrap();
        // Drag rect from (5, 5) to (28, 28) — should shrink to fit
        // the black block.
        let (x0, y0, x1, y1) =
            shrink_to_content(&frame, 5, 5, 28, 28, Tolerance::DEFAULT, EdgeBias::Midpoint);
        assert_eq!((x0, y0, x1, y1), (12, 14, 19, 21));
    }

    #[test]
    fn shrink_returns_original_on_uniform_content() {
        // Uniform frame — no content to shrink to.
        let buf = solid(16, 16, Rgba::WHITE);
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        let r = shrink_to_content(&frame, 2, 2, 14, 14, Tolerance::DEFAULT, EdgeBias::Midpoint);
        assert_eq!(r, (2, 2, 14, 14));
    }

    #[test]
    fn shrink_handles_out_of_bounds_rect() {
        let buf = solid(16, 16, Rgba::WHITE);
        let frame = FrameView::packed(&buf, 16, 16).unwrap();
        let r = shrink_to_content(
            &frame,
            -10,
            -10,
            100,
            100,
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );
        // Clamps to the frame; uniform white inside means "no content
        // boundary found", so we return the clamped rect rather than
        // the original off-screen one.
        assert_eq!(r, (0, 0, 15, 15));
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
        let edges = detect_edges(
            &frame,
            Px::new(8, 8),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );
        assert!(edges[1].is_none());
    }

    /// Build a 48×48 frame with a soft-edged dark block on white. Each
    /// edge is a 3-pixel ramp whose centre pixel is the *exact*
    /// 50%-brightness colour (153 — the midpoint of white 255 and the
    /// dark core 51), so the localizer's brightness crossing lands on
    /// a clean integer: x = 14 on the left, 33 on the right (likewise
    /// vertically). `level(c)` is the grayscale value along one axis;
    /// a pixel is `max(level(x), level(y))` so it is dark only where
    /// both axes are inside the block.
    fn soft_block() -> Vec<u8> {
        fn level(c: i32) -> u8 {
            match c {
                13 | 34 => 204, // ramp — 25% toward the dark core
                14 | 33 => 153, // ramp — exact 50% crossing
                15 | 32 => 102, // ramp — 75% toward the dark core
                16..=31 => 51,  // solid dark core
                _ => 255,       // white background
            }
        }
        let mut buf = solid(48, 48, Rgba::WHITE);
        for y in 0..48i32 {
            for x in 0..48i32 {
                let v = level(x).max(level(y));
                put(&mut buf, 48, x as u32, y as u32, Rgba::new(v, v, v, 255));
            }
        }
        buf
    }

    #[test]
    fn scan_localizes_soft_edge_to_midpoint() {
        // Crosshair mode: from inside the solid core, each direction's
        // `edge_phys` lands on the gradient's perceptual midpoint.
        let buf = soft_block();
        let frame = FrameView::packed(&buf, 48, 48).unwrap();
        let edges = detect_edges(
            &frame,
            Px::new(23, 23),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );
        assert_eq!(edges[0].expect("left").edge_phys, 14.0);
        assert_eq!(edges[1].expect("right").edge_phys, 33.0);
        assert_eq!(edges[2].expect("up").edge_phys, 14.0);
        assert_eq!(edges[3].expect("down").edge_phys, 33.0);
    }

    #[test]
    fn shrink_localizes_soft_edge_to_midpoint() {
        // Area-rect mode: a rect dragged loosely around the soft block
        // shrinks to the same fractional midpoints.
        let buf = soft_block();
        let frame = FrameView::packed(&buf, 48, 48).unwrap();
        let (l, t, r, b) =
            shrink_to_content_frac(&frame, 2, 2, 45, 45, Tolerance::DEFAULT, EdgeBias::Midpoint);
        assert_eq!((l, t, r, b), (14.0, 14.0, 33.0, 33.0));
        // Soft-edge span is midpoint-to-midpoint — taken directly, with
        // NO `+1` fence-post term.
        assert_eq!(r - l, 19.0);
        assert_eq!(b - t, 19.0);
    }

    #[test]
    fn soft_edge_modes_agree() {
        // The crux: crosshair (`scan`) and area-rect (`shrink`)
        // localise the SAME soft edge to the SAME fractional value.
        let buf = soft_block();
        let frame = FrameView::packed(&buf, 48, 48).unwrap();
        let edges = detect_edges(
            &frame,
            Px::new(23, 23),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        );
        let (l, t, r, b) =
            shrink_to_content_frac(&frame, 2, 2, 45, 45, Tolerance::DEFAULT, EdgeBias::Midpoint);
        assert_eq!(edges[0].unwrap().edge_phys, l);
        assert_eq!(edges[1].unwrap().edge_phys, r);
        assert_eq!(edges[2].unwrap().edge_phys, t);
        assert_eq!(edges[3].unwrap().edge_phys, b);
    }

    #[test]
    fn crisp_edge_collapses_to_half_pixel_boundary() {
        // A hard white→black edge: black fills x ≥ 20. The localized
        // edge must land exactly on the boundary line between the last
        // white pixel (x=19) and the first black pixel (x=20): 19.5.
        let mut buf = solid(32, 32, Rgba::WHITE);
        for y in 0..32 {
            for x in 20..32 {
                put(&mut buf, 32, x, y, Rgba::BLACK);
            }
        }
        let frame = FrameView::packed(&buf, 32, 32).unwrap();
        let right = detect_edges(
            &frame,
            Px::new(5, 16),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        )[1]
        .expect("right edge");
        assert_eq!(right.distance, 15);
        assert_eq!(right.position, Px::new(20, 16));
        assert_eq!(right.edge_phys, 19.5);

        // Area-rect mode on the same crisp edge: the left side is a
        // clean white→black transition (boundary 19.5); the right side
        // runs into black that fills the rect to its clamp, so it has
        // no clean edge and falls back to the integer boundary.
        let (l, _, r, _) =
            shrink_to_content_frac(&frame, 2, 2, 29, 29, Tolerance::DEFAULT, EdgeBias::Midpoint);
        assert_eq!(l, 19.5);
        assert_eq!(r, 29.5);
    }

    /// Fill rows of a 32×32 white frame with `ramp` (grayscale values)
    /// starting at x=11, then solid black from x=16 on. The white→black
    /// 50%-brightness crossing is what the localizer should report.
    fn ramp_frame(ramp: &[u8]) -> Vec<u8> {
        let mut buf = solid(32, 32, Rgba::WHITE);
        for y in 0..32 {
            for (i, &v) in ramp.iter().enumerate() {
                put(&mut buf, 32, 11 + i as u32, y, Rgba::new(v, v, v, 255));
            }
            for x in 16..32 {
                put(&mut buf, 32, x, y, Rgba::BLACK);
            }
        }
        buf
    }

    #[test]
    fn localizes_asymmetric_ramp_to_brightness_midpoint() {
        // White → an ASYMMETRIC ramp (steep first step, shallow tail)
        // → black. The 50%-brightness crossing sits near the steep
        // end, NOT at the geometric centre of the ramp's pixel run —
        // this is what the crossing buys over a pixel-index midpoint.
        let buf = ramp_frame(&[51, 30, 16, 8, 3]); // x = 11..=15
        let frame = FrameView::packed(&buf, 32, 32).unwrap();
        let right = detect_edges(
            &frame,
            Px::new(2, 16),
            Tolerance::DEFAULT,
            EdgeBias::Midpoint,
        )[1]
        .expect("right edge");
        // 50% brightness (127.5) is crossed between x=10 (255) and
        // x=11 (51): 10 + (255 - 127.5) / (255 - 51) = 10.625. The
        // geometric centre of the white→black run would instead be 13.
        assert!((right.edge_phys - 10.625).abs() < 1e-9);
    }

    #[test]
    fn localization_is_independent_of_tolerance() {
        // The crux of the decouple: a gentle-start ramp where phase-1
        // detection lands `first_over` on a DIFFERENT pixel per
        // tolerance (x=11 at STRICT, x=12 at DEFAULT, x=13 at LOOSE) —
        // yet every tolerance localizes the edge to the exact same
        // point. Tolerance changes whether an edge registers, never
        // where it sits.
        let buf = ramp_frame(&[250, 242, 220, 120, 20]); // x = 11..=15
        let frame = FrameView::packed(&buf, 32, 32).unwrap();
        let at = |tol| {
            detect_edges(&frame, Px::new(2, 16), tol, EdgeBias::Midpoint)[1]
                .expect("right edge")
                .edge_phys
        };
        let strict = at(Tolerance::STRICT);
        assert_eq!(strict, at(Tolerance::DEFAULT));
        assert_eq!(strict, at(Tolerance::LOOSE));
        // 50% brightness is crossed between x=13 (220) and x=14 (120):
        // 13 + (220 - 127.5) / (220 - 120) = 13.925.
        assert!((strict - 13.925).abs() < 1e-9);
    }

    #[test]
    fn bias_picks_inner_midpoint_outer_on_soft_edge() {
        // Soft block: 3-pixel ramp on each side, midpoints at x = 14
        // (left) and x = 33 (right). The ramp pixels are x = 13/34
        // (val 204) and x = 15/32 (val 102); the dark core starts at
        // x = 16 and ends at x = 31; the white plateau ends at x = 12
        // and resumes at x = 35. So:
        //   Inner    -> last_inside + 0.5 = the dark core's outer edge
        //   Midpoint -> the 50%-brightness pixel = clean integer
        //   Outer    -> first_outside − 0.5 = the white plateau's edge
        let buf = soft_block();
        let frame = FrameView::packed(&buf, 48, 48).unwrap();
        let cursor = Px::new(23, 23);
        let inner = detect_edges(&frame, cursor, Tolerance::DEFAULT, EdgeBias::Inner);
        let mid = detect_edges(&frame, cursor, Tolerance::DEFAULT, EdgeBias::Midpoint);
        let outer = detect_edges(&frame, cursor, Tolerance::DEFAULT, EdgeBias::Outer);
        // Inner: dark core extends to x = 16 / 31 → boundaries 15.5 / 31.5.
        assert_eq!(inner[0].unwrap().edge_phys, 15.5);
        assert_eq!(inner[1].unwrap().edge_phys, 31.5);
        // Midpoint: the brightness 50%-crossing at x = 14 / 33.
        assert_eq!(mid[0].unwrap().edge_phys, 14.0);
        assert_eq!(mid[1].unwrap().edge_phys, 33.0);
        // Outer: white plateau resumes at x = 12 / 35 → boundaries
        // 12.5 / 34.5.
        assert_eq!(outer[0].unwrap().edge_phys, 12.5);
        assert_eq!(outer[1].unwrap().edge_phys, 34.5);
        // Spans: Inner 16, Midpoint 19, Outer 22 — Outer − Inner = 6
        // = 3-pixel ramp on each side.
        assert_eq!(
            inner[1].unwrap().edge_phys - inner[0].unwrap().edge_phys,
            16.0
        );
        assert_eq!(mid[1].unwrap().edge_phys - mid[0].unwrap().edge_phys, 19.0);
        assert_eq!(
            outer[1].unwrap().edge_phys - outer[0].unwrap().edge_phys,
            22.0
        );
    }

    #[test]
    fn bias_has_no_effect_on_crisp_edge() {
        // A hard white→black edge: all three biases must collapse to
        // exactly the same half-pixel boundary, since the gradient is
        // zero pixels wide. This is the crisp-invariant guarantee.
        let mut buf = solid(32, 32, Rgba::WHITE);
        for y in 0..32 {
            for x in 20..32 {
                put(&mut buf, 32, x, y, Rgba::BLACK);
            }
        }
        let frame = FrameView::packed(&buf, 32, 32).unwrap();
        let at = |b| {
            detect_edges(&frame, Px::new(5, 16), Tolerance::DEFAULT, b)[1]
                .unwrap()
                .edge_phys
        };
        assert_eq!(at(EdgeBias::Inner), 19.5);
        assert_eq!(at(EdgeBias::Midpoint), 19.5);
        assert_eq!(at(EdgeBias::Outer), 19.5);
    }

    #[test]
    fn shrink_crisp_bounds_round_trip_to_inclusive_pixels() {
        // The integer `shrink_to_content` wrapper must reproduce the
        // exact inclusive bounds from the fractional boundaries — the
        // crisp pixel-perfect guarantee.
        let mut buf = solid(32, 32, Rgba::WHITE);
        for y in 14..22 {
            for x in 12..20 {
                put(&mut buf, 32, x, y, Rgba::BLACK);
            }
        }
        let frame = FrameView::packed(&buf, 32, 32).unwrap();
        let frac =
            shrink_to_content_frac(&frame, 5, 5, 28, 28, Tolerance::DEFAULT, EdgeBias::Midpoint);
        // Black block spans x ∈ 12..=19, y ∈ 14..=21 → half-pixel
        // boundaries bracketing those inclusive pixels.
        assert_eq!(frac, (11.5, 13.5, 19.5, 21.5));
        assert_eq!(round_bounds(frac), (12, 14, 19, 21));
    }
}
