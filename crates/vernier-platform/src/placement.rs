//! Pill-placement logic shared between the renderer and the main
//! loop's hit-test.
//!
//! Both have to agree on where each pill landed: the renderer paints
//! it there, and the main loop's click / hover detection has to
//! resolve against the same rectangle. Centralizing the algorithm
//! here keeps them from drifting out of sync.
//!
//! Coordinates are surface (logical) pixels, matching the units of
//! [`StuckMeasurement`] / [`HeldRect`] fields. The renderer scales
//! these to buffer pixels by multiplying by its HiDPI scale factor.

use crate::{GuideAxis, HeldRect, HudMeasurementFormat, StuckMeasurement, font};

/// Pill bounding box in surface logical pixels.
#[derive(Debug, Clone, Copy)]
pub struct PillRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl PillRect {
    pub fn contains_point(&self, px: f64, py: f64) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
    fn overlaps_with_pad(&self, other: &Self, pad: f64) -> bool {
        !(self.x + self.w + pad <= other.x
            || other.x + other.w + pad <= self.x
            || self.y + self.h + pad <= other.y
            || other.y + other.h + pad <= self.y)
    }
}

/// Result of laying out every pill in a single render pass. Order
/// matches the input slices.
#[derive(Debug, Clone)]
pub struct PillLayout {
    pub rect_dim_bboxes: Vec<PillRect>,
    pub stuck_bboxes: Vec<PillRect>,
}

// Pill-text geometry constants. Kept in sync with the renderer's
// `pill_dims_at` / `pill_dimensions_for_text`.
const TEXT_STUCK_LOGICAL_PX: f32 = 10.0;
const TEXT_RECT_LOGICAL_PX: f32 = 12.5;
const STUCK_PAD_X: f64 = 0.8 * TEXT_STUCK_LOGICAL_PX as f64;
const STUCK_PAD_Y: f64 = 0.4 * TEXT_STUCK_LOGICAL_PX as f64;
const RECT_PAD_X: f64 = 0.8 * TEXT_RECT_LOGICAL_PX as f64;
const RECT_PAD_Y: f64 = 0.4 * TEXT_RECT_LOGICAL_PX as f64;

/// Logical-pixel breathing room reserved around every placed pill
/// during collision search.
const PILL_GAP_LOGICAL: f64 = 10.0;
/// Tick-cap reach on stuck measurement lines (logical px). Used to
/// decide where the "beside the line" anchor sits.
const STUCK_TICK_HALF: f64 = 5.0;

#[derive(Debug, Clone, Copy)]
enum SlideAxis {
    X,
    Y,
}

/// Walk outward from `default` looking for a spot that doesn't
/// overlap any rect in `placed`. Searches in two phases:
///
/// * Phase A — slide along `slide_axis` only. Keeps the pill on
///   the line it's attached to. Earlier offsets win, so the result
///   is as close to ideal as possible.
/// * Phase B — 2D fallback. When sliding along the line direction
///   can't clear (e.g. a perpendicular-axis pill is in the way),
///   walk a distance-ordered grid of (dx, dy) offsets. Movement
///   off the line is weighted higher than along it so the pill
///   still ends up near its line when both options exist.
///
/// Each phase runs twice — once with the 10 px padding and once
/// without, so we'd rather move the pill further than crowd
/// neighbours. Falls back to `default` when nothing fits.
fn place_pill(
    default: PillRect,
    flipped: Option<PillRect>,
    slide_axis: SlideAxis,
    placed: &[PillRect],
) -> PillRect {
    let pad = PILL_GAP_LOGICAL;
    let step = 4.0;
    let max_steps: i32 = 60;

    let try_at = |dx: f64, dy: f64, pad: f64| -> Option<PillRect> {
        for base in [Some(default), flipped].into_iter().flatten() {
            let cand = PillRect {
                x: base.x + dx,
                y: base.y + dy,
                w: base.w,
                h: base.h,
            };
            if !placed.iter().any(|p| cand.overlaps_with_pad(p, pad)) {
                return Some(cand);
            }
        }
        None
    };

    // Phase A — 1D slide along the line direction. Almost every
    // realistic collision resolves here.
    for pass in 0..2 {
        let pad = if pass == 0 { pad } else { 0.0 };
        for n in 0..=max_steps {
            let slide = n as f64 * step;
            let signs: &[f64] = if n == 0 { &[0.0] } else { &[1.0, -1.0] };
            for &s in signs {
                let (dx, dy) = match slide_axis {
                    SlideAxis::X => (slide * s, 0.0),
                    SlideAxis::Y => (0.0, slide * s),
                };
                if let Some(c) = try_at(dx, dy, pad) {
                    return c;
                }
            }
        }
    }

    // Phase B — 2D fallback. Build a candidate set ordered by a
    // weighted distance: cheaper to move along the line, more
    // expensive to move off it. First non-overlapping candidate
    // wins.
    let perp_weight: f64 = 4.0;
    let along = |dx: f64, dy: f64| -> f64 {
        match slide_axis {
            SlideAxis::X => dx.abs(),
            SlideAxis::Y => dy.abs(),
        }
    };
    let perp = |dx: f64, dy: f64| -> f64 {
        match slide_axis {
            SlideAxis::X => dy.abs(),
            SlideAxis::Y => dx.abs(),
        }
    };
    let cost = |dx: f64, dy: f64| -> f64 {
        let a = along(dx, dy);
        let p = perp(dx, dy);
        a * a + perp_weight * p * p
    };
    let mut candidates: Vec<(f64, f64, f64)> =
        Vec::with_capacity((2 * max_steps as usize + 1).pow(2));
    for nx in -max_steps..=max_steps {
        for ny in -max_steps..=max_steps {
            let dx = nx as f64 * step;
            let dy = ny as f64 * step;
            candidates.push((dx, dy, cost(dx, dy)));
        }
    }
    candidates.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal));
    for pass in 0..2 {
        let pad = if pass == 0 { pad } else { 0.0 };
        for &(dx, dy, _) in &candidates {
            if let Some(c) = try_at(dx, dy, pad) {
                return c;
            }
        }
    }
    default
}

/// Compute the exact logical-pixel pill dimensions for `text` at
/// `text_logical_px`, using fontdue when available and falling back
/// to a 0.55-advance-per-char approximation otherwise.
fn pill_dims(text: &str, text_logical_px: f32, pad_x: f64, pad_y: f64) -> (f64, f64) {
    let text_w = if let Some(f) = font::hud_font() {
        font::measure_text_width(f, text, text_logical_px) as f64
    } else {
        text.chars().count() as f64 * text_logical_px as f64 * 0.55
    };
    let glyph_h = if let Some(f) = font::hud_font() {
        f.horizontal_line_metrics(text_logical_px)
            .map(|m| (m.ascent - m.descent) as f64)
            .unwrap_or(text_logical_px as f64)
    } else {
        text_logical_px as f64
    };
    let pill_w = text_w.ceil().max(20.0 - 2.0 * pad_x) + 2.0 * pad_x;
    let pill_h = glyph_h.ceil() + 2.0 * pad_y;
    (pill_w, pill_h)
}

fn stuck_pill_text(m: &StuckMeasurement, fmt: &HudMeasurementFormat) -> String {
    fmt.format_value((m.end - m.start).abs())
}

fn stuck_default_bbox(
    m: &StuckMeasurement,
    fmt: &HudMeasurementFormat,
) -> (PillRect, Option<PillRect>) {
    let text = stuck_pill_text(m, fmt);
    let (pill_w, pill_h) = pill_dims(&text, TEXT_STUCK_LOGICAL_PX, STUCK_PAD_X, STUCK_PAD_Y);
    let inside_long = (m.end - m.start).abs() >= 3.0 * pill_h;
    match m.axis {
        GuideAxis::Vertical => {
            let mid = (m.start + m.end) * 0.5;
            if inside_long {
                (
                    PillRect {
                        x: m.at - pill_w * 0.5,
                        y: mid - pill_h * 0.5,
                        w: pill_w,
                        h: pill_h,
                    },
                    None,
                )
            } else {
                let default = PillRect {
                    x: m.at + STUCK_TICK_HALF + 4.0,
                    y: mid - pill_h * 0.5,
                    w: pill_w,
                    h: pill_h,
                };
                let flipped = PillRect {
                    x: m.at - STUCK_TICK_HALF - 4.0 - pill_w,
                    y: mid - pill_h * 0.5,
                    w: pill_w,
                    h: pill_h,
                };
                (default, Some(flipped))
            }
        }
        GuideAxis::Horizontal => {
            let mid = (m.start + m.end) * 0.5;
            if inside_long {
                (
                    PillRect {
                        x: mid - pill_w * 0.5,
                        y: m.at - pill_h * 0.5,
                        w: pill_w,
                        h: pill_h,
                    },
                    None,
                )
            } else {
                let default = PillRect {
                    x: mid - pill_w * 0.5,
                    y: m.at + STUCK_TICK_HALF + 4.0,
                    w: pill_w,
                    h: pill_h,
                };
                let flipped = PillRect {
                    x: mid - pill_w * 0.5,
                    y: m.at - STUCK_TICK_HALF - 4.0 - pill_h,
                    w: pill_w,
                    h: pill_h,
                };
                (default, Some(flipped))
            }
        }
    }
}

fn rect_pill_text(r: &HeldRect, fmt: &HudMeasurementFormat) -> String {
    let rw = (r.rect_end.0 - r.rect_start.0).abs();
    let rh = (r.rect_end.1 - r.rect_start.1).abs();
    fmt.format_wh(rw, rh)
}

fn rect_dim_default_bbox(r: &HeldRect, fmt: &HudMeasurementFormat) -> (PillRect, Option<PillRect>) {
    let text = rect_pill_text(r, fmt);
    let (pill_w, pill_h) = pill_dims(&text, TEXT_RECT_LOGICAL_PX, RECT_PAD_X, RECT_PAD_Y);
    let rx = r.rect_start.0.min(r.rect_end.0);
    let ry = r.rect_start.1.min(r.rect_end.1);
    let rw = (r.rect_end.0 - r.rect_start.0).abs();
    let rh = (r.rect_end.1 - r.rect_start.1).abs();
    let center_x = rx + rw * 0.5;
    let pill_below = rw < 70.0 || rh < 35.0;
    if pill_below {
        let default = PillRect {
            x: center_x - pill_w * 0.5,
            y: ry + rh + 8.0,
            w: pill_w,
            h: pill_h,
        };
        let flipped = PillRect {
            x: center_x - pill_w * 0.5,
            y: ry - 8.0 - pill_h,
            w: pill_w,
            h: pill_h,
        };
        (default, Some(flipped))
    } else {
        (
            PillRect {
                x: center_x - pill_w * 0.5,
                y: ry + rh * 0.5 - pill_h * 0.5,
                w: pill_w,
                h: pill_h,
            },
            None,
        )
    }
}

fn clamp_to_surface(rect: PillRect, surface_w: f64, surface_h: f64) -> PillRect {
    let max_x = (surface_w - rect.w - 1.0).max(0.0);
    let max_y = (surface_h - rect.h - 1.0).max(0.0);
    PillRect {
        x: rect.x.clamp(0.0, max_x),
        y: rect.y.clamp(0.0, max_y),
        w: rect.w,
        h: rect.h,
    }
}

/// Lay out every pill that participates in the per-frame collision
/// avoidance. Held-rect dimension pills go first (rects accumulate
/// in user-add order; later rects' pills avoid earlier ones), then
/// stuck measurements (which avoid every rect pill plus all earlier
/// stuck pills).
///
/// `surface_w` / `surface_h` are the overlay surface dimensions in
/// logical pixels — pill positions are clamped so they don't fall
/// outside.
pub fn compute_pill_layout(
    rects: &[HeldRect],
    stucks: &[StuckMeasurement],
    fmt: &HudMeasurementFormat,
    surface_w: f64,
    surface_h: f64,
) -> PillLayout {
    // Obstacles each rect's dim pill avoids: only OTHER rects' dim
    // pills (it can sit inside its own outline). Built up rect by
    // rect.
    let mut rect_pill_obstacles: Vec<PillRect> = Vec::with_capacity(rects.len());
    let mut rect_dim_bboxes = Vec::with_capacity(rects.len());
    for r in rects {
        let (default, flipped) = rect_dim_default_bbox(r, fmt);
        let chosen = place_pill(default, flipped, SlideAxis::X, &rect_pill_obstacles);
        let final_rect = clamp_to_surface(chosen, surface_w, surface_h);
        rect_pill_obstacles.push(final_rect);
        rect_dim_bboxes.push(final_rect);
    }

    // Obstacles every stuck pill avoids: every rect's drawn box,
    // every rect's dim pill, and every earlier stuck pill.
    let mut stuck_obstacles: Vec<PillRect> = Vec::with_capacity(rects.len() * 2 + stucks.len());
    for r in rects {
        let rx = r.rect_start.0.min(r.rect_end.0);
        let ry = r.rect_start.1.min(r.rect_end.1);
        let rw = (r.rect_end.0 - r.rect_start.0).abs();
        let rh = (r.rect_end.1 - r.rect_start.1).abs();
        if rw > 0.0 && rh > 0.0 {
            stuck_obstacles.push(PillRect {
                x: rx,
                y: ry,
                w: rw,
                h: rh,
            });
        }
    }
    for &b in &rect_dim_bboxes {
        stuck_obstacles.push(b);
    }
    let mut stuck_bboxes = Vec::with_capacity(stucks.len());
    for m in stucks {
        let (default, flipped) = stuck_default_bbox(m, fmt);
        let chosen = if m.pill_offset != (0.0, 0.0) {
            default
        } else {
            let slide_axis = match m.axis {
                GuideAxis::Vertical => SlideAxis::Y,
                GuideAxis::Horizontal => SlideAxis::X,
            };
            place_pill(default, flipped, slide_axis, &stuck_obstacles)
        };
        let with_offset = PillRect {
            x: chosen.x + m.pill_offset.0,
            y: chosen.y + m.pill_offset.1,
            w: chosen.w,
            h: chosen.h,
        };
        let final_rect = clamp_to_surface(with_offset, surface_w, surface_h);
        stuck_obstacles.push(final_rect);
        stuck_bboxes.push(final_rect);
    }
    PillLayout {
        rect_dim_bboxes,
        stuck_bboxes,
    }
}

/// Compatibility wrapper for callers that only need the stuck-pill
/// positions (the main loop's hit-test).
pub fn stuck_pill_bboxes(
    stucks: &[StuckMeasurement],
    rects: &[HeldRect],
    fmt: &HudMeasurementFormat,
    surface_w: f64,
    surface_h: f64,
) -> Vec<PillRect> {
    compute_pill_layout(rects, stucks, fmt, surface_w, surface_h).stuck_bboxes
}
