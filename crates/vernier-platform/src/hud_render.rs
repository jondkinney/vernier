//! Platform-neutral HUD rasterizer. Takes a `Hud` description and
//! paints it into a pre-allocated RGBA8 buffer using tiny-skia +
//! fontdue. Originally lived inside `linux/wayland.rs`; lifted here
//! so the macOS backend can blit the same pixels into a CGImage
//! and assign it to its overlay view's CALayer.
//!
//! All functions are crate-private; the only entry point the
//! platform backends need is [`render_hud_into`].

#![allow(dead_code)]

use crate::{
    Color, CursorKind, Guide, GuideAxis, HeldRect, Hud, HudAxis, HudContextMenu,
    HudContextMenuIcon, HudContextMenuItem, HudEdge, HudKind, HudMeasurementFormat,
    HudRounding, HudToast, StuckMeasurement,
};

// Re-export the shared hud_font so existing call sites keep working.
// The actual font + caching lives in `crate::font` so the
// placement module can measure widths against the exact same font.
use crate::font::hud_font;

struct PillLayout {
    text: String,
    /// Pen X position of the first glyph in BUFFER coords.
    text_x: f32,
    /// Baseline Y position in BUFFER coords (descenders go below).
    baseline_y: f32,
    /// fontdue rasterization size in BUFFER pixels.
    px_size: f32,
}

/// Pixel size of the dimension-readout text in LOGICAL pixels. Sized
/// to fill the pill comfortably against a 2 physical-px stroke.
pub(crate) const TEXT_LOGICAL_PX: f32 = 12.5;
/// Smaller text size for "stuck" measurement pills — keeps the
/// frozen readouts visually subordinate to the live W×H pill.
pub(crate) const TEXT_STUCK_LOGICAL_PX: f32 = 10.0;
/// Toast pills get their own (larger) text size so status messages
/// stay readable at a distance. Kept independent so tweaking the
/// measurement-pill text doesn't shrink the toast.
pub(crate) const TOAST_TEXT_LOGICAL_PX: f32 = 18.0;

// Re-export the shared hud_font so existing call sites keep working.
// The actual font + caching lives in `crate::font` so the
// placement module can measure widths against the exact same font.

/// Fallback font for glyphs the primary HUD font doesn't carry —
/// notably the macOS modifier symbols (⇧⌃⌘⌥). Adwaita Sans includes
/// them; we also try DejaVu / Noto as additional fallbacks for less
/// common Linux distros.
fn hud_symbol_font() -> Option<&'static fontdue::Font> {
    use std::sync::OnceLock;
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        const CANDIDATES: &[&str] = &[
            "/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        ];
        for path in CANDIDATES {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(f) = fontdue::Font::from_bytes(
                    bytes.as_slice(),
                    fontdue::FontSettings::default(),
                ) {
                    log::info!("hud symbol font: {path}");
                    return Some(f);
                }
            }
        }
        None
    })
    .as_ref()
}

/// Pick the best font for `c`: primary if it carries the glyph,
/// otherwise the symbol fallback, otherwise the Omarchy font (carries
/// the U+E900 SUPER logo the right-click menu uses on Omarchy hosts).
/// Used so per-glyph rendering can substitute for missing characters
/// without leaving tofu boxes.
fn font_for_char<'a>(primary: &'a fontdue::Font, c: char) -> &'a fontdue::Font {
    if primary.lookup_glyph_index(c) != 0 {
        return primary;
    }
    if let Some(symbol) = hud_symbol_font() {
        if symbol.lookup_glyph_index(c) != 0 {
            return symbol;
        }
    }
    if let Some(omarchy) = omarchy_font() {
        if omarchy.lookup_glyph_index(c) != 0 {
            return omarchy;
        }
    }
    primary
}

/// Lazily load `~/.local/share/fonts/omarchy.ttf` so the right-click
/// menu can render the U+E900 SUPER glyph on Omarchy hosts. Returns
/// `None` if the font isn't installed or fails to parse — in which
/// case the SUPER hint falls back to the literal text "Super".
fn omarchy_font() -> Option<&'static fontdue::Font> {
    use std::sync::OnceLock;
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        let home = std::env::var_os("HOME")?;
        let path = std::path::PathBuf::from(home).join(".local/share/fonts/omarchy.ttf");
        let bytes = std::fs::read(&path).ok()?;
        let font = fontdue::Font::from_bytes(
            bytes.as_slice(),
            fontdue::FontSettings::default(),
        )
        .ok()?;
        log::info!("hud omarchy font: {}", path.display());
        Some(font)
    })
    .as_ref()
}

fn measure_text_width(font: &fontdue::Font, text: &str, px_size: f32) -> f32 {
    text.chars()
        .map(|c| font_for_char(font, c).metrics(c, px_size).advance_width)
        .sum()
}

/// Pill bg dimensions for `text` at `text_logical_px`. Padding is
/// proportional to the text size (matches push_pill).
fn pill_dims_at(text: &str, text_logical_px: f32, scale_f: f32) -> (f32, f32, f32, f32) {
    let px_size = text_logical_px * scale_f;
    let (text_w, ascent, descent) = if let Some(font) = hud_font() {
        let w = measure_text_width(font, text, px_size);
        let (a, d) = font
            .horizontal_line_metrics(px_size)
            .map(|m| (m.ascent, -m.descent))
            .unwrap_or((px_size * 0.8, px_size * 0.2));
        (w, a, d)
    } else {
        (
            text.len() as f32 * px_size * 0.55,
            px_size * 0.8,
            px_size * 0.2,
        )
    };
    let pad_x = 0.8 * text_logical_px * scale_f;
    let pad_y = 0.4 * text_logical_px * scale_f;
    let pill_w = text_w.ceil() + pad_x * 2.0;
    let pill_h = (ascent + descent).ceil() + pad_y * 2.0;
    (pill_w, pill_h, ascent, descent)
}

/// Draw the dark pill background only — caller is responsible for
/// pushing the centered text glyph layout afterwards. Useful when
/// the displayed glyph is a different size from the bg's nominal
/// content (e.g. hover-X overlay on a stuck-measurement pill).
fn draw_pill_bg(pixmap: &mut tiny_skia::PixmapMut, x: f32, y: f32, w: f32, h: f32) {
    use tiny_skia::*;
    let mut bg_paint = Paint::default();
    bg_paint.set_color_rgba8(40, 40, 40, 230);
    bg_paint.anti_alias = true;
    if let Some(path) = pill_path(x, y, w, h) {
        pixmap.fill_path(&path, &bg_paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Push a glyph layout centered in the rectangle `(x, y, w, h)` at
/// `text_logical_px`. The text may be larger than the box (e.g.
/// stuck-pill hover X overflows).
fn push_text_in_box(
    pills: &mut Vec<PillLayout>,
    text: String,
    box_x: f32,
    box_y: f32,
    box_w: f32,
    box_h: f32,
    text_logical_px: f32,
    scale_f: f32,
) {
    let Some(font) = hud_font() else { return };
    let px_size = text_logical_px * scale_f;
    let text_w = measure_text_width(font, &text, px_size);
    let (ascent, descent) = font
        .horizontal_line_metrics(px_size)
        .map(|m| (m.ascent, -m.descent))
        .unwrap_or((px_size * 0.8, px_size * 0.2));
    let cx = box_x + box_w * 0.5;
    let cy = box_y + box_h * 0.5;
    pills.push(PillLayout {
        text,
        text_x: (cx - text_w * 0.5).round(),
        baseline_y: (cy + (ascent - descent) * 0.5).round(),
        px_size,
    });
}

/// Render a [`Hud`] into a wl_shm Abgr8888 buffer at the given buffer
/// dimensions and HiDPI scale factor. Cursor / edge coords are in
/// surface (logical) pixels and get multiplied by `scale` internally.
///
/// One-shot convenience that fills `hud.background` then composites
/// the static + dynamic layers into a single buffer. Backends that
/// want to skip the static rasterize on hot paths call
/// [`render_static_into`] / [`render_dynamic_into`] directly and key
/// the static layer off [`static_hash`].
pub(crate) fn render_hud_into(canvas: &mut [u8], buf_w: u32, buf_h: u32, scale: u32, hud: &Hud) {
    fill_background(canvas, hud);
    render_static_layer(canvas, buf_w, buf_h, scale, hud);
    render_dynamic_layer(canvas, buf_w, buf_h, scale, hud);
}

/// Rasterize only the layer that's invariant under cursor movement —
/// held rects, stuck measurements, guides — into a fresh transparent
/// canvas. Pair with [`static_hash`] so the backend can skip this
/// call entirely when the static-affecting inputs haven't changed.
pub(crate) fn render_static_into(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) {
    canvas.fill(0);
    render_static_layer(canvas, buf_w, buf_h, scale, hud);
}

/// Rasterize only the cursor-driven layer — live drag rect / Held
/// overlay, crosshair, move/resize cursor, toast, context menu,
/// corner indicator — into a fresh transparent canvas. Re-run every
/// time `set_hud` fires; the backend composites this on top of the
/// cached static buffer.
pub(crate) fn render_dynamic_into(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) {
    canvas.fill(0);
    render_dynamic_layer(canvas, buf_w, buf_h, scale, hud);
}

/// Like [`render_static_into`] but does NOT clear the canvas first —
/// strokes blend (premul SrcOver) onto whatever pixels are already
/// there. Pair with [`render_dynamic_onto`] for backends that want to
/// pre-composite background + static into a single cached buffer and
/// avoid an extra full-buffer SrcOver per frame.
pub(crate) fn render_static_onto(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) {
    render_static_layer(canvas, buf_w, buf_h, scale, hud);
}

/// Like [`render_dynamic_into`] but does NOT clear the canvas first.
/// On Wayland the SHM canvas is memcpy'd from a pre-baked bg+static
/// buffer and the dynamic strokes go on top in-place — saves a
/// full-buffer zero-fill + a full-buffer SrcOver per frame vs the
/// `_into` + `draw_pixmap` form.
pub(crate) fn render_dynamic_onto(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) {
    render_dynamic_layer(canvas, buf_w, buf_h, scale, hud);
}

/// Digest of the [`Hud`] fields that affect the static layer. A change
/// in the digest is the signal to re-rasterize the static cache; a
/// match means the cached buffer is still valid. Hash collisions only
/// cause a spurious rebuild, never a stale image, so we use `DefaultHasher`
/// without worrying about cryptographic strength.
///
/// Keep this in sync with [`render_static_layer`]: any new
/// static-affecting input has to be fed into both functions.
pub(crate) fn static_hash(hud: &Hud) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();

    // Background tint is painted by the overlay backend (CALayer fill
    // on macOS, parent surface on Wayland), not by the static stroke
    // pass — so it doesn't belong in this digest. Same for hud.foreground,
    // which is only consumed by the dynamic (live drag / crosshair)
    // path.

    hud.held_rects.len().hash(&mut h);
    for r in &hud.held_rects {
        hash_f64(&mut h, r.rect_start.0);
        hash_f64(&mut h, r.rect_start.1);
        hash_f64(&mut h, r.rect_end.0);
        hash_f64(&mut h, r.rect_end.1);
        r.camera_armed.hash(&mut h);
        r.color_alternate.hash(&mut h);
    }

    hud.guides.len().hash(&mut h);
    for g in &hud.guides {
        // Guide has no floats so it could `derive(Hash)`, but keeping
        // the hashing explicit here matches the rest of the function
        // and avoids the per-type `Hash` derive boilerplate.
        g.axis.hash(&mut h);
        g.position.hash(&mut h);
        g.color_alternate.hash(&mut h);
        g.hovered.hash(&mut h);
    }

    hud.stuck_measurements.len().hash(&mut h);
    for m in &hud.stuck_measurements {
        m.axis.hash(&mut h);
        hash_f64(&mut h, m.at);
        hash_f64(&mut h, m.start);
        hash_f64(&mut h, m.end);
        hash_f64(&mut h, m.pill_offset.0);
        hash_f64(&mut h, m.pill_offset.1);
        m.color_alternate.hash(&mut h);
        m.hovered.hash(&mut h);
    }

    hud.align_mode.hash(&mut h);
    hud.guide_color.hash(&mut h);
    hud.alternative_guide_color.hash(&mut h);
    hud.primary_fg.hash(&mut h);
    hud.alternate_fg.hash(&mut h);

    let fmt = &hud.measurement_format;
    fmt.unit_suffix.hash(&mut h);
    fmt.rounding.hash(&mut h);
    hash_f64(&mut h, fmt.scale_factor);
    fmt.wh_indicators.hash(&mut h);
    fmt.aspect_in_area.hash(&mut h);
    // AspectMode lives in vernier-core without a Hash derive; hash by
    // discriminant so we don't need to touch that crate for an enum of
    // unit variants.
    std::mem::discriminant(&fmt.aspect_mode).hash(&mut h);
    hash_f64(&mut h, fmt.dimension_divisor);

    h.finish()
}

/// Quantize an `f64` to its bit pattern and feed it to the hasher.
/// Daemon-side layout produces the same bit pattern frame-to-frame for
/// an unchanging logical value, so `to_bits` is enough — no need for
/// rounding-based quantization.
fn hash_f64<H: std::hash::Hasher>(h: &mut H, v: f64) {
    h.write_u64(v.to_bits());
}

/// Fill `canvas` with `hud.background` as premultiplied RGBA. Used by
/// [`render_hud_into`] before the static + dynamic layers paint on
/// top. The per-layer entry points clear to transparent instead so
/// backend composition starts with two transparent layers.
fn fill_background(canvas: &mut [u8], hud: &Hud) {
    let bg = rgba8888_premul(hud.background);
    if bg == [0, 0, 0, 0] {
        canvas.fill(0);
    } else {
        for chunk in canvas.chunks_exact_mut(4) {
            chunk.copy_from_slice(&bg);
        }
    }
}

/// Static-layer strokes + pill text. Mutates `canvas` in place,
/// expecting an existing background (transparent or tint-filled). Does
/// NOT clear the canvas — the caller does that.
fn render_static_layer(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) {
    // tiny-skia phase scoped so its &mut borrow on canvas is released
    // before the glyph rasterizer writes into it.
    let pills = render_static_strokes(canvas, buf_w, buf_h, scale, hud);
    if !pills.is_empty() {
        if let Some(font) = hud_font() {
            for layout in &pills {
                render_pill_text(canvas, buf_w, buf_h, font, layout);
            }
        }
    }
}

/// Dynamic-layer strokes + pill text. Same canvas contract as
/// [`render_static_layer`].
fn render_dynamic_layer(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) {
    let pills = render_dynamic_strokes(canvas, buf_w, buf_h, scale, hud);
    if !pills.is_empty() {
        if let Some(font) = hud_font() {
            for layout in &pills {
                render_pill_text(canvas, buf_w, buf_h, font, layout);
            }
        }
    }
}

/// Rasterize the static stroke pass — held rects, stuck measurements,
/// and guides — and return the corresponding pill layouts so the
/// caller can run the glyph pass.
fn render_static_strokes(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) -> Vec<PillLayout> {
    use tiny_skia::*;
    let Some(mut pixmap) = PixmapMut::from_bytes(canvas, buf_w, buf_h) else {
        return Vec::new();
    };

    let stroke = Stroke {
        // Hard 2 physical pixels regardless of buffer scale — narrow
        // enough not to obscure the pixel boundary being measured,
        // wide enough to stay legible against busy backgrounds.
        width: 2.0,
        ..Default::default()
    };

    let mut pills: Vec<PillLayout> = Vec::new();
    // Pre-compute every committed pill's final position once. Both
    // the renderer and the main loop's hit-test use the same function
    // so a click always lands on whatever pill the user sees.
    let pill_layout = crate::placement::compute_pill_layout(
        &hud.held_rects,
        &hud.stuck_measurements,
        &hud.measurement_format,
        (buf_w as f32 / scale as f32) as f64,
        (buf_h as f32 / scale as f32) as f64,
    );

    // Held rects are additive — drawn first. Each accumulated drag
    // stays visible.
    for (i, rect) in hud.held_rects.iter().enumerate() {
        let dim_bbox = pill_layout.rect_dim_bboxes.get(i).copied();
        let rect_fg = if rect.color_alternate {
            hud.alternate_fg
        } else {
            hud.primary_fg
        };
        let mut rect_paint = Paint::default();
        rect_paint.set_color_rgba8(rect_fg.r, rect_fg.g, rect_fg.b, rect_fg.a);
        rect_paint.anti_alias = false;
        draw_area_rect(
            &mut pixmap,
            &mut pills,
            &rect.rect_start,
            &rect.rect_end,
            buf_w as f32,
            buf_h as f32,
            scale,
            rect_fg,
            &hud.measurement_format,
            &stroke,
            &rect_paint,
            rect.camera_armed,
            dim_bbox,
        );
    }

    if !hud.stuck_measurements.is_empty() {
        draw_stuck_measurements(
            &mut pixmap,
            &mut pills,
            &hud.stuck_measurements,
            &pill_layout.stuck_bboxes,
            hud.primary_fg,
            hud.alternate_fg,
            &hud.measurement_format,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }

    if !hud.guides.is_empty() {
        // Cursor is needed by `draw_guides` only as a comment-anchor
        // hint; the function ignores it today. Pass it through so the
        // signature stays unchanged if hovered-X behavior moves back
        // to the cursor later.
        let cursor = match &hud.kind {
            HudKind::Hover { cursor, .. } => Some(*cursor),
            HudKind::Drawing { cursor, .. } => Some(*cursor),
            HudKind::Held { cursor, .. } => Some(*cursor),
            HudKind::None => None,
        };
        draw_guides(
            &mut pixmap,
            &mut pills,
            &hud.guides,
            cursor,
            hud.align_mode,
            hud.guide_color,
            hud.alternative_guide_color,
            &hud.measurement_format,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }

    pills
}

/// Rasterize the dynamic stroke pass — live drag rect, Held overlay,
/// crosshair, custom cursors, toast, context menu, corner indicator —
/// and return the corresponding pill layouts.
fn render_dynamic_strokes(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) -> Vec<PillLayout> {
    use tiny_skia::*;
    let Some(mut pixmap) = PixmapMut::from_bytes(canvas, buf_w, buf_h) else {
        return Vec::new();
    };

    let fg = hud.foreground;
    let mut paint = Paint::default();
    paint.set_color_rgba8(fg.r, fg.g, fg.b, fg.a);
    paint.anti_alias = false;
    let stroke = Stroke {
        width: 2.0,
        ..Default::default()
    };
    let tick_stroke = Stroke {
        width: 2.0,
        ..Default::default()
    };

    let mut pills: Vec<PillLayout> = Vec::new();

    // Live drag rect — drawn before the cursor so the crosshair sits
    // on top.
    if let HudKind::Drawing { start, cursor } = &hud.kind {
        draw_area_rect(
            &mut pixmap,
            &mut pills,
            start,
            cursor,
            buf_w as f32,
            buf_h as f32,
            scale,
            fg,
            &hud.measurement_format,
            &stroke,
            &paint,
            false,
            None,
        );
    }
    if let HudKind::Held {
        rect_start,
        rect_end,
        camera_armed,
        ..
    } = &hud.kind
    {
        draw_area_rect(
            &mut pixmap,
            &mut pills,
            rect_start,
            rect_end,
            buf_w as f32,
            buf_h as f32,
            scale,
            fg,
            &hud.measurement_format,
            &stroke,
            &paint,
            *camera_armed,
            None,
        );
    }

    // Cursor crosshair / arrow goes on top of every other dynamic
    // primitive so the user's pointer indicator never disappears
    // behind a pill. The live measurement crosshair (axis lines + tick
    // caps + W×H pill) always renders — that's the actual measurement,
    // not the cursor. Inside `draw_hover_indicators`, the white-outlined
    // `+` marker (the cursor itself) is gated by `hud.show_cursor`.
    if let HudKind::Hover { cursor, edges } = &hud.kind {
        if hud.align_mode {
            draw_hover_indicators(
                &mut pixmap,
                &mut pills,
                cursor,
                edges,
                buf_w as f32,
                buf_h as f32,
                scale,
                &paint,
                &stroke,
                &tick_stroke,
                hud.measurement_format.wh_indicators,
                &hud.measurement_format.unit_suffix,
                hud.measurement_format.dimension_divisor,
                hud.show_cursor,
            );
        } else if hud.move_cursor_at.is_some() {
            // The dedicated draw_move_cursor block below paints it.
        } else if hud.cursor_in_rect {
            // System cursor is shown via wp_cursor_shape from main.rs
            // when cursor_in_rect is true — no custom drawing here.
            let _ = cursor;
        } else {
            draw_hover_indicators(
                &mut pixmap,
                &mut pills,
                cursor,
                edges,
                buf_w as f32,
                buf_h as f32,
                scale,
                &paint,
                &stroke,
                &tick_stroke,
                hud.measurement_format.wh_indicators,
                &hud.measurement_format.unit_suffix,
                hud.measurement_format.dimension_divisor,
                hud.show_cursor,
            );
        }
    }
    if let HudKind::Held {
        cursor,
        edges,
        cursor_in_rect,
        ..
    } = &hud.kind
    {
        if *cursor_in_rect {
            draw_arrow_cursor(
                &mut pixmap,
                cursor.0 as f32 * scale as f32,
                cursor.1 as f32 * scale as f32,
                scale as f32,
            );
        } else {
            draw_hover_indicators(
                &mut pixmap,
                &mut pills,
                cursor,
                edges,
                buf_w as f32,
                buf_h as f32,
                scale,
                &paint,
                &stroke,
                &tick_stroke,
                hud.measurement_format.wh_indicators,
                &hud.measurement_format.unit_suffix,
                hud.measurement_format.dimension_divisor,
                hud.show_cursor,
            );
        }
    }
    if let Some((cx, cy)) = hud.move_cursor_at {
        let bx = cx as f32 * scale as f32;
        let by = cy as f32 * scale as f32;
        match hud.cursor_kind {
            crate::CursorKind::Move => {
                draw_move_cursor(&mut pixmap, bx, by, scale as f32);
            }
            crate::CursorKind::ResizeNS => {
                draw_resize_cursor(&mut pixmap, bx, by, scale as f32, 0.0);
            }
            crate::CursorKind::ResizeEW => {
                draw_resize_cursor(&mut pixmap, bx, by, scale as f32, 90.0);
            }
            crate::CursorKind::ResizeNWSE => {
                draw_resize_cursor(&mut pixmap, bx, by, scale as f32, -45.0);
            }
            crate::CursorKind::ResizeNESW => {
                draw_resize_cursor(&mut pixmap, bx, by, scale as f32, 45.0);
            }
        }
    }
    if let Some(toast) = &hud.toast {
        draw_toast(
            &mut pixmap,
            &mut pills,
            &toast.text,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }
    if let Some(menu) = &hud.context_menu {
        draw_context_menu(
            &mut pixmap,
            &mut pills,
            menu,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }
    if let Some(text) = hud.corner_indicator.as_deref() {
        draw_corner_indicator(
            &mut pixmap,
            &mut pills,
            text,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }

    pills
}

/// Top-right pill that signals an active integration is rewriting
/// the on-screen values (e.g. `F · 200%` while the Figma plugin is
/// connected and a Figma tab is focused). Drawn last so it sits
/// above measurement HUD elements but below the context menu.
fn draw_corner_indicator(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    text: &str,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    let _ = buf_h;
    let margin = 12.0 * scale_f;
    push_pill(
        pixmap,
        pills,
        text.to_string(),
        buf_w - margin,
        margin,
        PillAnchor::AnchorTopRight,
        buf_w,
        buf_h,
        scale_f,
        TEXT_LOGICAL_PX,
    );
}

/// 2-direction resize cursor — black bar with arrowheads at both
/// ends and a white halo. `rotate_deg` orients the arrows: 0 = NS
/// (vertical), 90 = EW (horizontal), -45 = NWSE, 45 = NESW.
fn draw_resize_cursor(
    pixmap: &mut tiny_skia::PixmapMut,
    cx: f32,
    cy: f32,
    scale_f: f32,
    rotate_deg: f32,
) {
    use tiny_skia::*;
    let l = 7.5 * scale_f;   // half-length of arms
    let a = 3.5 * scale_f;   // arrowhead extent — smaller arrowheads
    let t = 1.0 * scale_f;   // arm half-thickness
    let s = a + 4.0 * scale_f; // serif half-width — 4 px wider than the arrowhead on each side
    let sh = 1.0 * scale_f;  // serif half-height (along arm axis)
    let mut pb = PathBuilder::new();
    // I-beam style: NS double-arrow with a horizontal serif at the
    // center. Trace outer boundary clockwise from top tip.
    pb.move_to(0.0, -l);
    pb.line_to(-a, -l + a);
    pb.line_to(-t, -l + a);
    pb.line_to(-t, -sh);
    pb.line_to(-s, -sh);
    pb.line_to(-s, sh);
    pb.line_to(-t, sh);
    pb.line_to(-t, l - a);
    pb.line_to(-a, l - a);
    pb.line_to(0.0, l);
    pb.line_to(a, l - a);
    pb.line_to(t, l - a);
    pb.line_to(t, sh);
    pb.line_to(s, sh);
    pb.line_to(s, -sh);
    pb.line_to(t, -sh);
    pb.line_to(t, -l + a);
    pb.line_to(a, -l + a);
    pb.close();
    let path = match pb.finish() {
        Some(p) => p,
        None => return,
    };
    let transform = Transform::from_rotate(rotate_deg).post_translate(cx, cy);
    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 255);
    white.anti_alias = true;
    pixmap.stroke_path(
        &path,
        &white,
        &Stroke {
            // Lighter white outline so the cursor stays slim against
            // smaller geometry.
            width: 4.0,
            line_join: LineJoin::Miter,
            ..Default::default()
        },
        transform,
        None,
    );
    let mut black = Paint::default();
    black.set_color_rgba8(0, 0, 0, 255);
    black.anti_alias = true;
    pixmap.fill_path(&path, &black, FillRule::Winding, transform, None);
}

/// 4-direction "move" cursor — black diamond/plus with arrowheads on
/// each tip and a white halo, drawn while the user is placing a
/// guide. Centered on `(cx, cy)` in BUFFER pixels.
fn draw_move_cursor(pixmap: &mut tiny_skia::PixmapMut, cx: f32, cy: f32, scale_f: f32) {
    use tiny_skia::*;
    let l = 11.0 * scale_f; // half-length of each arm (tip distance from center)
    let a = 5.0 * scale_f;  // arrowhead extent
    let t = 1.5 * scale_f;  // arm half-thickness
    // Center square matches the arm thickness so the arms flow into
    // each other without a notch — gives the cleaner +-with-arrows
    // shape of a standard move cursor.
    let c = t;
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy - l);
    pb.line_to(cx - a, cy - l + a);
    pb.line_to(cx - t, cy - l + a);
    pb.line_to(cx - t, cy - c);
    pb.line_to(cx - c, cy - c);
    pb.line_to(cx - c, cy - t);
    pb.line_to(cx - l + a, cy - t);
    pb.line_to(cx - l + a, cy - a);
    pb.line_to(cx - l, cy);
    pb.line_to(cx - l + a, cy + a);
    pb.line_to(cx - l + a, cy + t);
    pb.line_to(cx - c, cy + t);
    pb.line_to(cx - c, cy + c);
    pb.line_to(cx - t, cy + c);
    pb.line_to(cx - t, cy + l - a);
    pb.line_to(cx - a, cy + l - a);
    pb.line_to(cx, cy + l);
    pb.line_to(cx + a, cy + l - a);
    pb.line_to(cx + t, cy + l - a);
    pb.line_to(cx + t, cy + c);
    pb.line_to(cx + c, cy + c);
    pb.line_to(cx + c, cy + t);
    pb.line_to(cx + l - a, cy + t);
    pb.line_to(cx + l - a, cy + a);
    pb.line_to(cx + l, cy);
    pb.line_to(cx + l - a, cy - a);
    pb.line_to(cx + l - a, cy - t);
    pb.line_to(cx + c, cy - t);
    pb.line_to(cx + c, cy - c);
    pb.line_to(cx + t, cy - c);
    pb.line_to(cx + t, cy - l + a);
    pb.line_to(cx + a, cy - l + a);
    pb.close();
    if let Some(path) = pb.finish() {
        // White halo first, then black fill on top — same contrast
        // affordance as the regular cross marker.
        let mut white = Paint::default();
        white.set_color_rgba8(255, 255, 255, 255);
        white.anti_alias = true;
        pixmap.stroke_path(
            &path,
            &white,
            &Stroke {
                width: 4.0,
                line_join: LineJoin::Miter,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
        let mut black = Paint::default();
        black.set_color_rgba8(0, 0, 0, 255);
        black.anti_alias = true;
        pixmap.fill_path(&path, &black, FillRule::Winding, Transform::identity(), None);
    }
}

/// Draw frozen single-axis measurements — coral line + tick caps +
/// pill with the pixel count. Same visual language as the live
/// crosshair so the user reads them as "stuck" measurements.
fn draw_stuck_measurements(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    measurements: &[crate::StuckMeasurement],
    pill_bboxes: &[crate::placement::PillRect],
    primary_fg: Color,
    alternate_fg: Color,
    fmt: &crate::HudMeasurementFormat,
    _buf_w: f32,
    _buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    use crate::GuideAxis;
    let line_stroke = Stroke { width: 2.0, ..Default::default() };
    let tick_stroke = Stroke { width: 2.0, ..Default::default() };
    let tick_half = 5.0 * scale_f; // tick reach in buffer px

    for (i, m) in measurements.iter().enumerate() {
        // Per-stuck color (snapshot at placement). Existing pieces
        // keep whatever color they were placed in even if `X`
        // re-flips the live HUD afterward.
        let fg = if m.color_alternate { alternate_fg } else { primary_fg };
        let mut paint = Paint::default();
        paint.set_color_rgba8(fg.r, fg.g, fg.b, fg.a);
        paint.anti_alias = false;
        // Snap endpoints to the physical pixel grid (same `floor`
        // step the live crosshair uses), subtract in buffer px, and
        // divide back by scale. Rounded to an integer so the pill
        // matches the live W×H readout exactly — without it, HiDPI
        // half-pixel offsets and fractional rounding modes can drift
        // the displayed length relative to the live pill.
        let start_buf = (m.start as f32 * scale_f).floor();
        let end_buf = (m.end as f32 * scale_f).floor();
        let length = ((end_buf - start_buf).abs() / scale_f).round() as f64;
        let value_text = fmt.format_value(length);
        let display_text = if m.hovered {
            "\u{00D7}".to_string()
        } else {
            value_text.clone()
        };
        let display_size = if m.hovered {
            TEXT_STUCK_LOGICAL_PX * 1.5
        } else {
            TEXT_STUCK_LOGICAL_PX
        };
        let half = 1.0; // half of the 2px stroke for pixel-grid snap
        let bbox = match pill_bboxes.get(i) {
            Some(b) => *b,
            None => continue,
        };
        // Logical → buffer.
        let pill_x = (bbox.x as f32 * scale_f).floor();
        let pill_y = (bbox.y as f32 * scale_f).floor();
        let pill_w = (bbox.w as f32 * scale_f).floor();
        let pill_h = (bbox.h as f32 * scale_f).floor();
        match m.axis {
            GuideAxis::Vertical => {
                let x = (m.at as f32 * scale_f).floor() + half;
                let y0 = (m.start as f32 * scale_f).floor() + half;
                let y1 = (m.end as f32 * scale_f).floor() + half;
                // Main vertical line.
                let mut pb = PathBuilder::new();
                pb.move_to(x, y0);
                pb.line_to(x, y1);
                if let Some(p) = pb.finish() {
                    pixmap.stroke_path(&p, &paint, &line_stroke, Transform::identity(), None);
                }
                // Horizontal tick caps at start and end.
                for ty in [y0, y1] {
                    let mut pb = PathBuilder::new();
                    pb.move_to(x - tick_half, ty);
                    pb.line_to(x + tick_half, ty);
                    if let Some(p) = pb.finish() {
                        pixmap.stroke_path(&p, &paint, &tick_stroke, Transform::identity(), None);
                    }
                }
                // Tether (drawn before the pill bg so the pill sits
                // on top): a dashed half-alpha line from the pill's
                // nearer edge to the projection on the measurement
                // line. Skipped when the pill is centered on the
                // line itself.
                draw_pill_tether(
                    pixmap,
                    crate::GuideAxis::Vertical,
                    x,
                    y0.min(y1),
                    y0.max(y1),
                    pill_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    fg,
                    scale_f,
                );
            }
            GuideAxis::Horizontal => {
                let y = (m.at as f32 * scale_f).floor() + half;
                let x0 = (m.start as f32 * scale_f).floor() + half;
                let x1 = (m.end as f32 * scale_f).floor() + half;
                // Main horizontal line.
                let mut pb = PathBuilder::new();
                pb.move_to(x0, y);
                pb.line_to(x1, y);
                if let Some(p) = pb.finish() {
                    pixmap.stroke_path(&p, &paint, &line_stroke, Transform::identity(), None);
                }
                // Vertical tick caps at left and right.
                for tx in [x0, x1] {
                    let mut pb = PathBuilder::new();
                    pb.move_to(tx, y - tick_half);
                    pb.line_to(tx, y + tick_half);
                    if let Some(p) = pb.finish() {
                        pixmap.stroke_path(&p, &paint, &tick_stroke, Transform::identity(), None);
                    }
                }
                draw_pill_tether(
                    pixmap,
                    crate::GuideAxis::Horizontal,
                    y,
                    x0.min(x1),
                    x0.max(x1),
                    pill_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    fg,
                    scale_f,
                );
            }
        }
        draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
        push_text_in_box(
            pills,
            display_text.clone(),
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            display_size,
            scale_f,
        );
    }
}

/// Draw a half-alpha dashed line from the nearest edge of a stuck
/// measurement's value pill to the projection of the pill's center
/// onto the measurement line. Skipped when the pill overlaps the
/// line (centered on it) — there's nothing to tether back to.
///
/// `axis` is the orientation of the MEASUREMENT line, not the
/// tether. For a Vertical measurement, `line_pos` is its `x` and
/// `line_lo/hi` are the `y` extent; for Horizontal it's flipped.
#[allow(clippy::too_many_arguments)]
fn draw_pill_tether(
    pixmap: &mut tiny_skia::PixmapMut,
    axis: crate::GuideAxis,
    line_pos: f32,
    line_lo: f32,
    line_hi: f32,
    pill_x: f32,
    pill_y: f32,
    pill_w: f32,
    pill_h: f32,
    fg: Color,
    scale_f: f32,
) {
    use tiny_skia::*;
    let (anchor_pt, pill_pt) = match axis {
        crate::GuideAxis::Vertical => {
            // Line is vertical at x = line_pos. The pill sits to the
            // left or right; the tether is horizontal.
            let pill_cy = (pill_y + pill_h * 0.5).clamp(line_lo, line_hi);
            // Pill side closest to the line.
            let pill_near_x = if pill_x + pill_w * 0.5 < line_pos {
                pill_x + pill_w // right edge of left-side pill
            } else {
                pill_x // left edge of right-side pill
            };
            // No tether if the pill straddles or sits on the line.
            if (pill_near_x - line_pos).abs() < 1.0 {
                return;
            }
            ((line_pos, pill_cy), (pill_near_x, pill_cy))
        }
        crate::GuideAxis::Horizontal => {
            // Line is horizontal at y = line_pos.
            let pill_cx = (pill_x + pill_w * 0.5).clamp(line_lo, line_hi);
            let pill_near_y = if pill_y + pill_h * 0.5 < line_pos {
                pill_y + pill_h
            } else {
                pill_y
            };
            if (pill_near_y - line_pos).abs() < 1.0 {
                return;
            }
            ((pill_cx, line_pos), (pill_cx, pill_near_y))
        }
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(fg.r, fg.g, fg.b, (fg.a as u16 * 128 / 255) as u8);
    paint.anti_alias = true;
    let dash = StrokeDash::new(vec![4.0 * scale_f, 3.0 * scale_f], 0.0);
    let stroke = Stroke {
        width: 1.0,
        dash,
        ..Default::default()
    };
    let mut pb = PathBuilder::new();
    pb.move_to(anchor_pt.0, anchor_pt.1);
    pb.line_to(pill_pt.0, pill_pt.1);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
}

/// Draw persistent reference guides — 1 physical-pixel blue lines
/// spanning the full buffer along each guide's axis. Drawn after the
/// rest of the HUD so the guides sit on top of measurement strokes.
/// When a guide is `hovered` and we have a `cursor`, draw a small dark
/// "X" badge on the line at the cursor's free axis to signal removal.
fn draw_guides(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    guides: &[crate::Guide],
    cursor: Option<(f64, f64)>,
    align_mode: bool,
    guide_color: crate::Color,
    alternative_guide_color: crate::Color,
    fmt: &crate::HudMeasurementFormat,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    use crate::GuideAxis;
    let stroke = Stroke {
        width: 1.0,
        ..Default::default()
    };
    for guide in guides {
        // Per-guide color (snapshot at placement, except for the
        // pending preview which mirrors the live `color_alternate`).
        let c = if guide.color_alternate {
            alternative_guide_color
        } else {
            guide_color
        };
        let mut paint = Paint::default();
        paint.set_color_rgba8(c.r, c.g, c.b, c.a);
        paint.anti_alias = false;
        let pos = (guide.position as f32 * scale_f).floor() + 0.5;
        let mut pb = PathBuilder::new();
        match guide.axis {
            GuideAxis::Horizontal => {
                pb.move_to(0.0, pos);
                pb.line_to(buf_w, pos);
            }
            GuideAxis::Vertical => {
                pb.move_to(pos, 0.0);
                pb.line_to(pos, buf_h);
            }
        }
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
        if guide.hovered {
            // Anchor the X badge at the line's midpoint on the
            // perpendicular axis (screen center) — the cursor itself
            // becomes a drag handle instead of being the X target.
            let _ = cursor;
            let (badge_x, badge_y) = match guide.axis {
                GuideAxis::Horizontal => (buf_w * 0.5, pos),
                GuideAxis::Vertical => (pos, buf_h * 0.5),
            };
            draw_remove_x_badge(pixmap, pills, badge_x, badge_y, buf_w, buf_h, scale_f);
        }
    }

    let _ = align_mode;
    // Inter-guide distance pills. For each adjacent pair of guides
    // sharing an axis, render a small pill (same style as a stuck
    // measurement) showing the px gap between them, centered between
    // the two guides on the spanning axis.
    let mut horiz: Vec<i32> = guides
        .iter()
        .filter(|g| g.axis == GuideAxis::Horizontal)
        .map(|g| g.position)
        .collect();
    horiz.sort_unstable();
    horiz.dedup();
    for win in horiz.windows(2) {
        let dist = (win[1] - win[0]).abs();
        if dist == 0 {
            continue;
        }
        let value = fmt.format_value(dist as f64);
        let (pill_w, pill_h, _, _) =
            pill_dims_at(&value, TEXT_STUCK_LOGICAL_PX, scale_f);
        // Horizontal pair (gap is vertical) → label anchored at the
        // LEFT of the screen, vertically centered between the two
        // guide ys.
        let mid_y = (win[0] + win[1]) as f32 * 0.5 * scale_f;
        let pill_x = (50.0 * scale_f).floor().max(0.0);
        let pill_y = (mid_y - pill_h * 0.5)
            .floor()
            .min(buf_h - pill_h - 1.0)
            .max(0.0);
        draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
        push_text_in_box(
            pills,
            value,
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            TEXT_STUCK_LOGICAL_PX,
            scale_f,
        );
    }
    let mut vert: Vec<i32> = guides
        .iter()
        .filter(|g| g.axis == GuideAxis::Vertical)
        .map(|g| g.position)
        .collect();
    vert.sort_unstable();
    vert.dedup();
    for win in vert.windows(2) {
        let dist = (win[1] - win[0]).abs();
        if dist == 0 {
            continue;
        }
        let value = fmt.format_value(dist as f64);
        let (pill_w, pill_h, _, _) =
            pill_dims_at(&value, TEXT_STUCK_LOGICAL_PX, scale_f);
        // Vertical pair (gap is horizontal) → label anchored at the
        // TOP of the screen, horizontally centered between the two
        // guide xs.
        let mid_x = (win[0] + win[1]) as f32 * 0.5 * scale_f;
        let pill_x = (mid_x - pill_w * 0.5)
            .floor()
            .min(buf_w - pill_w - 1.0)
            .max(0.0);
        let pill_y = (50.0 * scale_f).floor().max(0.0);
        draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
        push_text_in_box(
            pills,
            value,
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            TEXT_STUCK_LOGICAL_PX,
            scale_f,
        );
    }
}

/// Small oval "remove" pill with a `×` glyph, drawn on a hovered
/// guide. Same visual treatment as a hovered stuck-measurement pill
/// — bg sized for a single digit at TEXT_STUCK_LOGICAL_PX, × glyph
/// rendered at 1.5× that size and overflowing slightly.
fn draw_remove_x_badge(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    cx: f32,
    cy: f32,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    let (pill_w, pill_h, _, _) = pill_dims_at("0", TEXT_STUCK_LOGICAL_PX, scale_f);
    let pill_x = (cx - pill_w * 0.5)
        .floor()
        .min(buf_w - pill_w - 1.0)
        .max(0.0);
    let pill_y = (cy - pill_h * 0.5)
        .floor()
        .min(buf_h - pill_h - 1.0)
        .max(0.0);
    draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
    push_text_in_box(
        pills,
        "\u{00D7}".to_string(),
        pill_x,
        pill_y,
        pill_w,
        pill_h,
        TEXT_STUCK_LOGICAL_PX * 1.5,
        scale_f,
    );
}

/// Draw the live measure crosshair: axis lines through the cursor with
/// tick caps where edges were detected, plus the white `+` cursor
/// marker on top, and a W×H pill in the lower-right of the cursor.
#[allow(clippy::too_many_arguments)]
fn draw_hover_indicators(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    cursor: &(f64, f64),
    edges: &[Option<crate::HudEdge>; 4],
    buf_w: f32,
    buf_h: f32,
    scale: u32,
    paint: &tiny_skia::Paint,
    stroke: &tiny_skia::Stroke,
    tick_stroke: &tiny_skia::Stroke,
    wh_indicators: bool,
    unit_suffix: &str,
    dimension_divisor: f64,
    show_cursor: bool,
) {
    use tiny_skia::*;
    let scale_f = scale as f32;
    {
            // Convert surface-logical coords to buffer-physical, snap
            // to the pixel grid, offset by stroke half-width so non-AA
            // strokes land cleanly on integer columns / rows. Without
            // this, integer positions sit on the boundary between two
            // pixels and the rasterizer's tie-break rule picks one or
            // the other, giving uneven tick lengths and shimmer.
            let half = stroke.width * 0.5;
            let snap = |v: f64| (v * scale as f64).floor() as f32 + half;
            let cx = snap(cursor.0);
            let cy = snap(cursor.1);
            let surface_w = buf_w;
            let surface_h = buf_h;

            // Horizontal axis line: spans from left snap edge (or screen
            // left) to right snap edge (or screen right), through cursor.
            let left = edges
                .iter()
                .filter_map(|e| e.as_ref())
                .find(|e| e.axis == HudAxis::Left);
            let right = edges
                .iter()
                .filter_map(|e| e.as_ref())
                .find(|e| e.axis == HudAxis::Right);
            let left_x = left.map(|e| snap(e.position.0)).unwrap_or(half);
            let right_x = right
                .map(|e| snap(e.position.0))
                .unwrap_or(surface_w - half);
            let mut pb = PathBuilder::new();
            pb.move_to(left_x, cy);
            pb.line_to(right_x, cy);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }

            // Vertical axis line.
            let up = edges
                .iter()
                .filter_map(|e| e.as_ref())
                .find(|e| e.axis == HudAxis::Up);
            let down = edges
                .iter()
                .filter_map(|e| e.as_ref())
                .find(|e| e.axis == HudAxis::Down);
            let up_y = up.map(|e| snap(e.position.1)).unwrap_or(half);
            let down_y = down
                .map(|e| snap(e.position.1))
                .unwrap_or(surface_h - half);
            let mut pb = PathBuilder::new();
            pb.move_to(cx, up_y);
            pb.line_to(cx, down_y);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }

            // Tick marks. Anchor the tick CENTER on the matching axis
            // line (cy for left/right ticks, cx for up/down ticks) so
            // they sit exactly on the main lines.
            // Tick half-length = 5 LOGICAL pixels. Drawn with the
            // thicker `tick_stroke` so caps look like filled bars.
            let tick = 5.0 * scale_f;
            for edge in edges.iter().flatten() {
                let ex = snap(edge.position.0);
                let ey = snap(edge.position.1);
                let (px, py, tdx, tdy) = match edge.axis {
                    HudAxis::Left | HudAxis::Right => (ex, cy, 0.0, tick),
                    HudAxis::Up | HudAxis::Down => (cx, ey, tick, 0.0),
                };
                let mut pb = PathBuilder::new();
                pb.move_to(px - tdx, py - tdy);
                pb.line_to(px + tdx, py + tdy);
                if let Some(path) = pb.finish() {
                    pixmap.stroke_path(&path, &paint, &tick_stroke, Transform::identity(), None);
                }
            }

            // Cursor `+` marker: black interior with a white outline,
            // The white outline keeps the
            // mark visible against dark UI; the black core makes it
            // pop on light UI. Drawn after the axis lines so it sits
            // on top of their crossing point.
            //
            // Gated by `show_cursor` (the prefs "Show cursor" toggle)
            // — the rest of this function (axis lines, tick caps,
            // W×H pill) is the measurement HUD itself and stays
            // visible either way.
            if show_cursor {
                let arm = 6.0 * scale_f;
                let mut pb = PathBuilder::new();
                pb.move_to(cx - arm, cy);
                pb.line_to(cx + arm, cy);
                pb.move_to(cx, cy - arm);
                pb.line_to(cx, cy + arm);
                if let Some(path) = pb.finish() {
                    // Hard physical-pixel widths: 4 px white outline,
                    // 2 px black core, regardless of buffer scale.
                    let mut outline = Paint::default();
                    outline.set_color_rgba8(255, 255, 255, 255);
                    outline.anti_alias = true;
                    pixmap.stroke_path(
                        &path,
                        &outline,
                        &Stroke {
                            // Total stroke = black core 2 + 3 px white
                            // halo on each side.
                            width: 8.0,
                            line_cap: tiny_skia::LineCap::Round,
                            ..Default::default()
                        },
                        Transform::identity(),
                        None,
                    );
                    let mut fill = Paint::default();
                    fill.set_color_rgba8(0, 0, 0, 255);
                    fill.anti_alias = true;
                    pixmap.stroke_path(
                        &path,
                        &fill,
                        &Stroke {
                            width: 2.0,
                            line_cap: tiny_skia::LineCap::Round,
                            ..Default::default()
                        },
                        Transform::identity(),
                        None,
                    );
                }
            }

            // Width / height in LOGICAL pixels. Buffer span / scale,
            // then divided by the configured dimension divisor (1.0
            // by default, > 1.0 when Figma zoom-correction is active).
            let div = if dimension_divisor > 0.0 { dimension_divisor as f32 } else { 1.0 };
            let w_px = (((right_x - left_x) / scale_f) / div).round() as u32;
            let h_px = (((down_y - up_y) / scale_f) / div).round() as u32;

            // "W × H" with the Unicode multiplication sign. The
            // optional unit suffix (e.g. "px") trails the second
            // number, or each number when `wh_indicators` is on —
            // matches the held-rect pill so the live and committed
            // readouts agree.
            let text = if wh_indicators {
                format!(
                    "W: {}{} \u{00D7} H: {}{}",
                    w_px, unit_suffix, h_px, unit_suffix
                )
            } else {
                format!("{} \u{00D7} {}{}", w_px, h_px, unit_suffix)
            };
            let px_size = TEXT_LOGICAL_PX * scale_f;
            // Measure text via fontdue. If the font is missing we still
            // render the pill (just empty) at a sensible width using the
            // average glyph metric.
            let (text_w, ascent, descent) = if let Some(font) = hud_font() {
                let w = measure_text_width(font, &text, px_size);
                let lm = font.horizontal_line_metrics(px_size);
                let (a, d) = lm
                    .map(|m| (m.ascent, -m.descent))
                    .unwrap_or((px_size * 0.8, px_size * 0.2));
                (w, a, d)
            } else {
                (text.len() as f32 * px_size * 0.55, px_size * 0.8, px_size * 0.2)
            };
            let pad_x = 10.0 * scale_f;
            let pad_y = 5.0 * scale_f;
            let pill_w = text_w.ceil() + pad_x * 2.0;
            let pill_h = (ascent + descent).ceil() + pad_y * 2.0;
            // Lower-right of cursor by 14 LOGICAL px each axis.
            let cursor_buf_x = (cursor.0 * scale as f64) as f32;
            let cursor_buf_y = (cursor.1 * scale as f64) as f32;
            let offset = 14.0 * scale_f;
            let mut pill_x = (cursor_buf_x + offset).floor();
            let mut pill_y = (cursor_buf_y + offset).floor();
            pill_x = pill_x.min(surface_w - pill_w - 1.0).max(0.0);
            pill_y = pill_y.min(surface_h - pill_h - 1.0).max(0.0);

            // Slightly translucent dark gray (not pure black). The background still shows through a
            // little, which keeps the pill from looking overweight.
            let mut bg_paint = Paint::default();
            bg_paint.set_color_rgba8(40, 40, 40, 230);
            bg_paint.anti_alias = true;
            if let Some(path) = pill_path(pill_x, pill_y, pill_w, pill_h) {
                pixmap.fill_path(&path, &bg_paint, FillRule::Winding, Transform::identity(), None);
            }

            pills.push(PillLayout {
                text,
                text_x: pill_x + pad_x,
                baseline_y: pill_y + pad_y + ascent,
                px_size,
            });
    }
}

/// Draw the rectangle for an in-progress drag (Drawing) or a committed
/// measurement (Held), plus the W×H and aspect-ratio pills. When
/// `camera_armed` is true, the W×H pill renders a camera icon instead
/// of the dimension text — that signals to the user that clicking will
/// capture the held region as a screenshot.
#[allow(clippy::too_many_arguments)]
fn draw_area_rect(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    a: &(f64, f64),
    b: &(f64, f64),
    buf_w: f32,
    buf_h: f32,
    scale: u32,
    fg: Color,
    fmt: &crate::HudMeasurementFormat,
    stroke: &tiny_skia::Stroke,
    line_paint: &tiny_skia::Paint,
    camera_armed: bool,
    // Pre-computed dim-pill bbox (logical px) for committed rects;
    // None for live drag / live held rects → default position, no
    // collision search.
    precomputed_dim_bbox: Option<crate::placement::PillRect>,
) {
    use tiny_skia::*;
    let scale_f = scale as f32;
    let half = scale_f * 0.5;
    let snap = |v: f64| (v * scale as f64).floor() as f32 + half;
    let ax = snap(a.0);
    let ay = snap(a.1);
    let bx = snap(b.0);
    let by = snap(b.1);
    let rx = ax.min(bx);
    let ry = ay.min(by);
    let rw = (ax - bx).abs();
    let rh = (ay - by).abs();
    if rw < scale_f || rh < scale_f {
        return;
    }
    if let Some(rect) = Rect::from_xywh(rx, ry, rw, rh) {
        // Translucent fill — keeps the underlying content readable.
        let mut fill_paint = Paint::default();
        fill_paint.set_color_rgba8(fg.r, fg.g, fg.b, 40);
        pixmap.fill_rect(rect, &fill_paint, Transform::identity(), None);
        // Solid border at the same stroke as axis lines.
        let mut pb = PathBuilder::new();
        pb.push_rect(rect);
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, line_paint, stroke, Transform::identity(), None);
        }
    }
    let w_logical_f = (rw / scale_f) as f64;
    let h_logical_f = (rh / scale_f) as f64;
    let w_logical = w_logical_f.round() as u32;
    let h_logical = h_logical_f.round() as u32;

    let dim_text = if fmt.wh_indicators {
        format!(
            "W: {}{} \u{00D7} H: {}{}",
            fmt.format_number(w_logical_f),
            fmt.unit_suffix,
            fmt.format_number(h_logical_f),
            fmt.unit_suffix,
        )
    } else {
        format!(
            "{} \u{00D7} {}{}",
            fmt.format_number(w_logical_f),
            fmt.format_number(h_logical_f),
            fmt.unit_suffix
        )
    };
    let pill_below = w_logical < 70 || h_logical < 35;
    // Resolve the dim pill bbox: pre-computed (logical px) for
    // committed rects, default for transient live rects.
    let dim_bbox_logical = match precomputed_dim_bbox {
        Some(b) => b,
        None => {
            let (dim_pill_w_buf, dim_pill_h_buf) =
                pill_dimensions_for_text(&dim_text, scale_f);
            let dim_pill_w = dim_pill_w_buf / scale_f;
            let dim_pill_h = dim_pill_h_buf / scale_f;
            let cx_log = (rx + rw * 0.5) as f64 / scale as f64;
            if pill_below {
                crate::placement::PillRect {
                    x: cx_log - dim_pill_w as f64 * 0.5,
                    y: (ry as f64 + rh as f64 + 8.0 * scale as f64) / scale as f64,
                    w: dim_pill_w as f64,
                    h: dim_pill_h as f64,
                }
            } else {
                crate::placement::PillRect {
                    x: cx_log - dim_pill_w as f64 * 0.5,
                    y: (ry as f64 + rh as f64 * 0.5) / scale as f64
                        - dim_pill_h as f64 * 0.5,
                    w: dim_pill_w as f64,
                    h: dim_pill_h as f64,
                }
            }
        }
    };
    let dim_pill_x = (dim_bbox_logical.x as f32 * scale_f)
        .floor()
        .min(buf_w - (dim_bbox_logical.w as f32 * scale_f) - 1.0)
        .max(0.0);
    let dim_pill_y = (dim_bbox_logical.y as f32 * scale_f)
        .floor()
        .min(buf_h - (dim_bbox_logical.h as f32 * scale_f) - 1.0)
        .max(0.0);
    let dim_pill_w = (dim_bbox_logical.w as f32 * scale_f).floor();
    let dim_pill_h = (dim_bbox_logical.h as f32 * scale_f).floor();
    // Did the W×H pill end up above the rect? Aspect pill follows.
    let dim_flipped_up = pill_below && dim_pill_y < ry;
    if camera_armed {
        draw_pill_bg(pixmap, dim_pill_x, dim_pill_y, dim_pill_w, dim_pill_h);
        draw_camera_icon(
            pixmap,
            dim_pill_x + dim_pill_w * 0.5,
            dim_pill_y + dim_pill_h * 0.5,
            scale_f,
        );
    } else {
        draw_pill_bg(pixmap, dim_pill_x, dim_pill_y, dim_pill_w, dim_pill_h);
        push_text_in_box(
            pills,
            dim_text,
            dim_pill_x,
            dim_pill_y,
            dim_pill_w,
            dim_pill_h,
            TEXT_LOGICAL_PX,
            scale_f,
        );
    }

    // Aspect ratio pill — stays attached to whichever side the
    // dimension pill landed on. Not collision-resolved; it tracks
    // the dim pill (the dim pill already won the collision search).
    let aspect_text = if fmt.aspect_in_area {
        estimate_aspect_text(w_logical, h_logical, fmt.aspect_mode)
    } else {
        None
    };
    if let Some(aspect_text) = aspect_text {
        let center_x = rx + rw * 0.5;
        let aspect_y_anchor = if dim_flipped_up {
            dim_pill_y - 6.0 * scale_f
        } else if pill_below {
            dim_pill_y + dim_pill_h + 6.0 * scale_f
        } else {
            ry + rh + 24.0 * scale_f
        };
        let (apill_w, apill_h) = pill_dimensions_for_text(&aspect_text, scale_f);
        let apill_x = (center_x - apill_w * 0.5)
            .floor()
            .min(buf_w - apill_w - 1.0)
            .max(0.0);
        let apill_y = if dim_flipped_up {
            (aspect_y_anchor - apill_h)
                .floor()
                .min(buf_h - apill_h - 1.0)
                .max(0.0)
        } else {
            aspect_y_anchor.floor().min(buf_h - apill_h - 1.0).max(0.0)
        };
        draw_pill_bg(pixmap, apill_x, apill_y, apill_w, apill_h);
        push_text_in_box(
            pills,
            aspect_text,
            apill_x,
            apill_y,
            apill_w,
            apill_h,
            TEXT_LOGICAL_PX,
            scale_f,
        );
    }
}

/// Format the aspect-ratio pill for the area tool. Delegates to the
/// shared `vernier_core::aspect` classifier so the pill respects the
/// user's configured `AspectMode` (Automatic / Standard / Reduced /
/// CommonOnly). Returns `None` when the configured mode declines to
/// report a ratio (CommonOnly with no curated match).
fn estimate_aspect_text(
    width: u32,
    height: u32,
    mode: vernier_core::AspectMode,
) -> Option<String> {
    use vernier_core::{CommonRatio, Ratio};
    if width == 0 || height == 0 {
        return None;
    }
    let ratio = vernier_core::classify_aspect(width, height, mode, 0.02)?;
    let (n, d) = match ratio {
        Ratio::Common(c) => match c {
            CommonRatio::R16x9 => (16, 9),
            CommonRatio::R4x3 => (4, 3),
            CommonRatio::R1x1 => (1, 1),
            CommonRatio::R21x9 => (21, 9),
            CommonRatio::R16x10 => (16, 10),
            CommonRatio::R5x4 => (5, 4),
            CommonRatio::R3x2 => (3, 2),
            CommonRatio::R2x1 => (2, 1),
            CommonRatio::R9x16 => (9, 16),
            CommonRatio::R3x4 => (3, 4),
        },
        Ratio::Reduced { num, den } => (num, den),
    };
    Some(format!("{} : {}", n, d))
}

// Pill placement (collision-avoiding bbox selection) lives in
// `crate::placement` so the renderer and the main loop's hit-test
// stay in lock-step. See `placement::compute_pill_layout`.

#[derive(Copy, Clone)]
#[allow(dead_code)]
enum PillAnchor {
    /// Position pill so its center lands at (anchor_x, anchor_y).
    Centered,
    /// Position pill so its top-center lands at (anchor_x, anchor_y).
    AnchorTop,
    /// Position pill so its top-right lands at (anchor_x, anchor_y).
    AnchorTopRight,
    /// Position pill so its left edge sits at `anchor_x` and its
    /// vertical center sits at `anchor_y`.
    LeftCenter,
    /// Lower-right of the anchor by the given buffer-pixel offset.
    BelowRight(f32),
}

#[allow(clippy::too_many_arguments)]
fn push_pill(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    text: String,
    anchor_x: f32,
    anchor_y: f32,
    anchor: PillAnchor,
    surface_w: f32,
    surface_h: f32,
    scale_f: f32,
    text_logical_px: f32,
) {
    use tiny_skia::*;
    let Some(font) = hud_font() else { return; };
    let px_size = text_logical_px * scale_f;
    let text_w = measure_text_width(font, &text, px_size);
    let (ascent, descent) = font
        .horizontal_line_metrics(px_size)
        .map(|m| (m.ascent, -m.descent))
        .unwrap_or((px_size * 0.8, px_size * 0.2));
    // Padding scales with the chosen text size so smaller pills stay
    // visually balanced (8/4 ratio matches the active pill's 10/5 at
    // 12.5 px).
    let pad_x = 0.8 * text_logical_px * scale_f;
    let pad_y = 0.4 * text_logical_px * scale_f;
    let pill_w = text_w.ceil() + pad_x * 2.0;
    let pill_h = (ascent + descent).ceil() + pad_y * 2.0;

    let (mut pill_x, mut pill_y) = match anchor {
        PillAnchor::Centered => (anchor_x - pill_w * 0.5, anchor_y - pill_h * 0.5),
        PillAnchor::AnchorTop => (anchor_x - pill_w * 0.5, anchor_y),
        PillAnchor::AnchorTopRight => (anchor_x - pill_w, anchor_y),
        PillAnchor::LeftCenter => (anchor_x, anchor_y - pill_h * 0.5),
        PillAnchor::BelowRight(off) => (anchor_x + off, anchor_y + off),
    };
    pill_x = pill_x.floor().min(surface_w - pill_w - 1.0).max(0.0);
    pill_y = pill_y.floor().min(surface_h - pill_h - 1.0).max(0.0);

    let mut bg_paint = Paint::default();
    bg_paint.set_color_rgba8(40, 40, 40, 230);
    bg_paint.anti_alias = true;
    if let Some(path) = pill_path(pill_x, pill_y, pill_w, pill_h) {
        pixmap.fill_path(&path, &bg_paint, FillRule::Winding, Transform::identity(), None);
    }
    pills.push(PillLayout {
        text,
        text_x: pill_x + pad_x,
        baseline_y: pill_y + pad_y + ascent,
        px_size,
    });
}

/// Rasterize the dimension-readout text into the buffer using fontdue.
/// Each glyph's grayscale alpha bitmap is alpha-blended onto the pill
/// background that `render_hud_strokes` already drew. The buffer is
/// premultiplied RGBA, and the source is fully-opaque white at the
/// glyph's per-pixel alpha — so premul source = (a, a, a, a) and
/// `out = src + dst * (1 - src.a)` reduces to the inner block here.
fn render_pill_text(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    font: &fontdue::Font,
    layout: &PillLayout,
) {
    let mut pen_x = layout.text_x;
    let baseline = layout.baseline_y;
    for ch in layout.text.chars() {
        let active = font_for_char(font, ch);
        let (metrics, bitmap) = active.rasterize(ch, layout.px_size);
        let glyph_origin_x = pen_x + metrics.xmin as f32;
        // The Omarchy SUPER logo (U+E900) is drawn to the top of the em
        // box rather than the cap height, so at the same baseline it
        // floats noticeably above neighbouring letters. Nudge it down
        // ~1 logical px (≈ 10% of the px size, scale-aware via the
        // font size already being in physical px) so it sits on the
        // shared visual baseline of the shortcut row.
        let y_bias = if ch == '\u{e900}' {
            layout.px_size * 0.10
        } else {
            0.0
        };
        let glyph_origin_y =
            baseline - metrics.ymin as f32 - metrics.height as f32 + y_bias;
        composite_glyph(
            canvas,
            buf_w,
            buf_h,
            &bitmap,
            metrics.width as u32,
            metrics.height as u32,
            glyph_origin_x,
            glyph_origin_y,
        );
        pen_x += metrics.advance_width;
    }
}

fn composite_glyph(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    bitmap: &[u8],
    glyph_w: u32,
    glyph_h: u32,
    pos_x: f32,
    pos_y: f32,
) {
    if glyph_w == 0 || glyph_h == 0 {
        return;
    }
    let base_x = pos_x.round() as i32;
    let base_y = pos_y.round() as i32;
    for j in 0..glyph_h as i32 {
        let y = base_y + j;
        if y < 0 || y as u32 >= buf_h {
            continue;
        }
        for i in 0..glyph_w as i32 {
            let x = base_x + i;
            if x < 0 || x as u32 >= buf_w {
                continue;
            }
            let alpha = bitmap[(j as u32 * glyph_w + i as u32) as usize];
            if alpha == 0 {
                continue;
            }
            let idx = (y as u32 * buf_w + x as u32) as usize * 4;
            let inv = 255u16 - alpha as u16;
            // Source is opaque white at `alpha`; premultiplied = (a,a,a,a).
            // out = src + dst * (1 - src.a)
            //     = alpha + dst * inv / 255 (per channel, including alpha)
            canvas[idx] = (alpha as u16 + (canvas[idx] as u16 * inv) / 255) as u8;
            canvas[idx + 1] = (alpha as u16 + (canvas[idx + 1] as u16 * inv) / 255) as u8;
            canvas[idx + 2] = (alpha as u16 + (canvas[idx + 2] as u16 * inv) / 255) as u8;
            canvas[idx + 3] = (alpha as u16 + (canvas[idx + 3] as u16 * inv) / 255) as u8;
        }
    }
}

/// Compute the pill dimensions (in buffer pixels) that would house
/// `text` at the HUD's standard text size. Used by both the text-pill
/// path and the camera-icon path so the pill bounds stay stable when
/// the cursor hovers in / out of the held rect.
fn pill_dimensions_for_text(text: &str, scale_f: f32) -> (f32, f32) {
    let px_size = TEXT_LOGICAL_PX * scale_f;
    let (text_w, ascent, descent) = if let Some(font) = hud_font() {
        let w = measure_text_width(font, text, px_size);
        let (a, d) = font
            .horizontal_line_metrics(px_size)
            .map(|m| (m.ascent, -m.descent))
            .unwrap_or((px_size * 0.8, px_size * 0.2));
        (w, a, d)
    } else {
        (text.len() as f32 * px_size * 0.55, px_size * 0.8, px_size * 0.2)
    };
    let pad_x = 10.0 * scale_f;
    let pad_y = 5.0 * scale_f;
    (text_w.ceil() + pad_x * 2.0, (ascent + descent).ceil() + pad_y * 2.0)
}

/// Tiny line-art camera icon centered at `(cx, cy)`. Sized in LOGICAL
/// pixels and multiplied by `scale_f` so it's crisp at HiDPI.
fn draw_camera_icon(pixmap: &mut tiny_skia::PixmapMut, cx: f32, cy: f32, scale_f: f32) {
    use tiny_skia::*;
    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 245);
    white.anti_alias = true;
    let mut dark = Paint::default();
    dark.set_color_rgba8(35, 35, 35, 255);
    dark.anti_alias = true;

    // Body geometry — sized smaller than the pill so it sits with
    // visible margin around it.
    let body_w = 17.0 * scale_f;
    let body_h = 10.0 * scale_f;
    let body_x = cx - body_w * 0.5;
    let body_y = cy - body_h * 0.5 + 0.75 * scale_f;
    let radius = 1.25 * scale_f;

    // Bump (small viewfinder/hot-shoe bar) just above the body, slightly
    // offset to one side for a less-symmetrical, more iconic camera shape.
    let bump_w = 5.0 * scale_f;
    let bump_h = 1.6 * scale_f;
    let bump_x = cx - body_w * 0.5 + 2.0 * scale_f;
    let bump_y = body_y - bump_h + 0.4 * scale_f;
    if let Some(rect) = Rect::from_xywh(bump_x, bump_y, bump_w, bump_h) {
        pixmap.fill_rect(rect, &white, Transform::identity(), None);
    }

    // Body — rounded rect built from cubic-free quad corners.
    let bx2 = body_x + body_w;
    let by2 = body_y + body_h;
    let mut pb = PathBuilder::new();
    pb.move_to(body_x + radius, body_y);
    pb.line_to(bx2 - radius, body_y);
    pb.quad_to(bx2, body_y, bx2, body_y + radius);
    pb.line_to(bx2, by2 - radius);
    pb.quad_to(bx2, by2, bx2 - radius, by2);
    pb.line_to(body_x + radius, by2);
    pb.quad_to(body_x, by2, body_x, by2 - radius);
    pb.line_to(body_x, body_y + radius);
    pb.quad_to(body_x, body_y, body_x + radius, body_y);
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
    }

    // Lens (dark filled circle) and a small highlight for liveliness.
    let lens_cx = cx;
    let lens_cy = body_y + body_h * 0.5;
    let lens_r = 2.7 * scale_f;
    let mut pb = PathBuilder::new();
    pb.push_circle(lens_cx, lens_cy, lens_r);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &dark, FillRule::Winding, Transform::identity(), None);
    }
    let hi_r = 0.8 * scale_f;
    let mut pb = PathBuilder::new();
    pb.push_circle(lens_cx + 0.8 * scale_f, lens_cy - 0.8 * scale_f, hi_r);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
    }
}

/// Standard left-pointer arrow drawn at `(cx, cy)` (top-left tip).
/// Rendered ourselves because we hide the system pointer for the whole
/// measurement session — when the user is inside the held region we
/// want them to see a click-affordance pointer in software.
fn draw_arrow_cursor(pixmap: &mut tiny_skia::PixmapMut, cx: f32, cy: f32, scale_f: f32) {
    use tiny_skia::*;
    let s = scale_f;
    // Slimmer and slightly taller — closer to the Hyprland default
    // silhouette in image #30 (sharp tip, refined tail).
    let pts: [(f32, f32); 7] = [
        (0.0, 0.0),    // sharp tip
        (0.0, 17.0),   // bottom of left edge
        (4.5, 13.5),   // notch where tail starts
        (7.5, 18.0),   // tail bottom-left
        (9.0, 17.5),   // tail bottom-right
        (5.5, 11.5),   // right notch
        (10.5, 11.0),  // top of right edge
    ];
    let mut pb = PathBuilder::new();
    pb.move_to(cx + pts[0].0 * s, cy + pts[0].1 * s);
    for p in &pts[1..] {
        pb.line_to(cx + p.0 * s, cy + p.1 * s);
    }
    pb.close();
    let path = match pb.finish() {
        Some(p) => p,
        None => return,
    };
    // Hyprland-style pointer: black body with a thin white halo.
    // Stroke white first (forms the outline), then fill black on top
    // so the halo is visible only along the arrow's edge.
    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 255);
    white.anti_alias = true;
    let mut black = Paint::default();
    black.set_color_rgba8(0, 0, 0, 255);
    black.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = 2.0;
    stroke.line_join = LineJoin::Miter;
    pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
    pixmap.fill_path(&path, &black, FillRule::Winding, Transform::identity(), None);
}

/// Toast pill ("Tolerance: High" / "Screenshot taken"). Anchored in
/// the lower third of the buffer — far enough below the cursor that
/// it doesn't visually fight the measurement crosshair, close enough
/// to bottom that the user's gaze doesn't have to leave the work.
fn draw_toast(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    text: &str,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    let px_size = TOAST_TEXT_LOGICAL_PX * scale_f;
    let (text_w, ascent, descent) = if let Some(font) = hud_font() {
        let w = measure_text_width(font, text, px_size);
        let (a, d) = font
            .horizontal_line_metrics(px_size)
            .map(|m| (m.ascent, -m.descent))
            .unwrap_or((px_size * 0.8, px_size * 0.2));
        (w, a, d)
    } else {
        (text.len() as f32 * px_size * 0.55, px_size * 0.8, px_size * 0.2)
    };
    let pad_x = 22.0 * scale_f;
    let pad_y = 12.0 * scale_f;
    let pill_w = text_w.ceil() + pad_x * 2.0;
    let pill_h = (ascent + descent).ceil() + pad_y * 2.0;
    let pill_x = ((buf_w - pill_w) * 0.5).floor().max(0.0);
    // Lower-third anchor: pill top at ~2/3 of the buffer height so the
    // pill body sits inside the bottom third regardless of resolution.
    let pill_y = (buf_h * 2.0 / 3.0).floor().max(0.0);

    let mut bg = Paint::default();
    bg.set_color_rgba8(20, 20, 20, 235);
    bg.anti_alias = true;
    if let Some(path) = pill_path(pill_x, pill_y, pill_w, pill_h) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, Transform::identity(), None);
    }
    pills.push(PillLayout {
        text: text.to_string(),
        text_x: (pill_x + pad_x).round(),
        baseline_y: (pill_y + pad_y + ascent).round(),
        px_size,
    });
}

// format_number / format_value live on crate::HudMeasurementFormat now
// (see crate::types). Renderer + placement use the same impls.

/// Right-click context menu — floating list of actions anchored at
/// the cursor where the right-click happened. Drawn last (on top of
/// every other HUD layer including the toast). Hovered row gets a
/// lighter bg; each row is icon + label + optional shortcut hint.
fn draw_context_menu(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    menu: &crate::HudContextMenu,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    let Some(font) = hud_font() else { return };

    const ROW_H: f32 = 32.0;
    const RADIUS: f32 = 12.0;
    const PAD_X: f32 = 14.0;
    const PAD_Y: f32 = 10.0;
    const ICON_COL_W: f32 = 32.0;
    const SHORTCUT_GAP: f32 = 16.0;
    const DIV_PAD_V: f32 = 8.0;
    const DIV_HEIGHT: f32 = 1.0;

    let label_px = TEXT_LOGICAL_PX * scale_f;
    let shortcut_px = TEXT_STUCK_LOGICAL_PX * scale_f;

    let icon_col = ICON_COL_W * scale_f;
    let pad_x = PAD_X * scale_f;
    let pad_y = PAD_Y * scale_f;
    let row_h = ROW_H * scale_f;
    let radius = RADIUS * scale_f;
    let div_pad_v = DIV_PAD_V * scale_f;
    let div_h = DIV_HEIGHT * scale_f;
    let _ = SHORTCUT_GAP; // kept for parity with hit-tester

    let inner_label_x = pad_x + icon_col;
    let menu_w = (menu.width as f32) * scale_f;

    let mut content_h = pad_y * 2.0;
    for (i, it) in menu.items.iter().enumerate() {
        content_h += row_h;
        if it.divider_after && i + 1 < menu.items.len() {
            content_h += 2.0 * div_pad_v + div_h;
        }
    }

    let mx = (menu.origin.0 as f32) * scale_f;
    let my = (menu.origin.1 as f32) * scale_f;
    let mx = mx.min(buf_w - menu_w - 1.0).max(0.0);
    let my = my.min(buf_h - content_h - 1.0).max(0.0);

    // Drop any pre-existing (measurement) pills whose text would
    // bleed through the menu — the menu sits on top, so its area
    // should be clean. Menu pills themselves are pushed below this
    // filter, so they're not affected.
    pills.retain(|p| !pill_text_overlaps_rect(p, mx, my, menu_w, content_h, font));

    let mut bg = Paint::default();
    bg.set_color_rgba8(22, 22, 22, 248);
    bg.anti_alias = true;
    if let Some(path) = rounded_rect_path(mx, my, menu_w, content_h, radius) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, Transform::identity(), None);
    }

    let mut row_y = my + pad_y;
    for (i, it) in menu.items.iter().enumerate() {
        if menu.hovered == Some(i) {
            let mut hbg = Paint::default();
            hbg.set_color_rgba8(48, 48, 48, 235);
            hbg.anti_alias = true;
            let inset = pad_x * 0.5;
            if let Some(path) =
                rounded_rect_path(mx + inset, row_y, menu_w - inset * 2.0, row_h, radius * 0.5)
            {
                pixmap.fill_path(&path, &hbg, FillRule::Winding, Transform::identity(), None);
            }
        }

        let icon_cx = mx + pad_x + icon_col * 0.5;
        let icon_cy = row_y + row_h * 0.5;
        draw_menu_icon(pixmap, it.icon, icon_cx, icon_cy, scale_f);

        let (l_asc, l_desc) = font
            .horizontal_line_metrics(label_px)
            .map(|m| (m.ascent, -m.descent))
            .unwrap_or((label_px * 0.8, label_px * 0.2));
        pills.push(PillLayout {
            text: it.label.clone(),
            text_x: (mx + inner_label_x).round(),
            baseline_y: (icon_cy + (l_asc - l_desc) * 0.5).round(),
            px_size: label_px,
        });

        if let Some(s) = &it.shortcut {
            let sw = measure_text_width(font, s, shortcut_px);
            let (s_asc, s_desc) = font
                .horizontal_line_metrics(shortcut_px)
                .map(|m| (m.ascent, -m.descent))
                .unwrap_or((shortcut_px * 0.8, shortcut_px * 0.2));
            let shortcut_x_end = mx + menu_w - pad_x;
            pills.push(PillLayout {
                text: s.clone(),
                text_x: (shortcut_x_end - sw).round(),
                baseline_y: (icon_cy + (s_asc - s_desc) * 0.5).round(),
                px_size: shortcut_px,
            });
        }

        row_y += row_h;
        if it.divider_after && i + 1 < menu.items.len() {
            row_y += div_pad_v;
            let mut dpaint = Paint::default();
            dpaint.set_color_rgba8(60, 60, 60, 235);
            dpaint.anti_alias = false;
            let dx0 = mx + pad_x;
            let dx1 = mx + menu_w - pad_x;
            let mut dpb = PathBuilder::new();
            dpb.move_to(dx0, row_y);
            dpb.line_to(dx1, row_y);
            dpb.line_to(dx1, row_y + div_h);
            dpb.line_to(dx0, row_y + div_h);
            dpb.close();
            if let Some(path) = dpb.finish() {
                pixmap.fill_path(&path, &dpaint, FillRule::Winding, Transform::identity(), None);
            }
            row_y += div_h + div_pad_v;
        }
    }
}

/// True when `pill`'s rasterized text region intersects the rect
/// `(mx, my, mw, mh)`. Used by the context menu to suppress
/// underlying measurement pill text from bleeding through.
fn pill_text_overlaps_rect(
    pill: &PillLayout,
    mx: f32,
    my: f32,
    mw: f32,
    mh: f32,
    font: &fontdue::Font,
) -> bool {
    let text_w = measure_text_width(font, &pill.text, pill.px_size);
    let p_left = pill.text_x;
    let p_right = pill.text_x + text_w;
    let p_top = pill.baseline_y - pill.px_size;
    let p_bot = pill.baseline_y + pill.px_size * 0.3;
    p_right > mx && p_left < mx + mw && p_bot > my && p_top < my + mh
}

/// Build a path for a rectangle with all four corners rounded by
/// radius `r`. `r` is clamped to `min(w/2, h/2)`.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    use tiny_skia::PathBuilder;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
    let k = r * 0.5523;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Render the small (~16 logical px) icon for a context-menu row.
/// `cx`/`cy` are the icon's center in BUFFER pixels.
fn draw_menu_icon(
    pixmap: &mut tiny_skia::PixmapMut,
    icon: crate::HudContextMenuIcon,
    cx: f32,
    cy: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    let mut accent = Paint::default();
    accent.set_color_rgba8(120, 180, 255, 240);
    accent.anti_alias = true;
    let mut coral = Paint::default();
    coral.set_color_rgba8(0xFF, 0x5C, 0x5C, 245);
    coral.anti_alias = true;
    let mut white = Paint::default();
    white.set_color_rgba8(220, 220, 220, 240);
    white.anti_alias = true;
    let stroke = Stroke {
        width: 1.5 * scale_f,
        line_cap: LineCap::Round,
        ..Default::default()
    };

    use crate::HudContextMenuIcon as I;
    match icon {
        I::GuideH => {
            let half = 8.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - half, cy);
            pb.line_to(cx + half, cy);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &accent, &stroke, Transform::identity(), None);
            }
        }
        I::GuideV => {
            let half = 8.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - half);
            pb.line_to(cx, cy + half);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &accent, &stroke, Transform::identity(), None);
            }
        }
        I::StuckH => {
            let len = 6.0 * scale_f;
            let cap = 4.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - len, cy);
            pb.line_to(cx + len, cy);
            pb.move_to(cx - len, cy - cap);
            pb.line_to(cx - len, cy + cap);
            pb.move_to(cx + len, cy - cap);
            pb.line_to(cx + len, cy + cap);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &coral, &stroke, Transform::identity(), None);
            }
        }
        I::StuckV => {
            let len = 6.0 * scale_f;
            let cap = 4.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - len);
            pb.line_to(cx, cy + len);
            pb.move_to(cx - cap, cy - len);
            pb.line_to(cx + cap, cy - len);
            pb.move_to(cx - cap, cy + len);
            pb.line_to(cx + cap, cy + len);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &coral, &stroke, Transform::identity(), None);
            }
        }
        I::Camera => {
            let bw = 12.0 * scale_f;
            let bh = 8.0 * scale_f;
            let bx = cx - bw * 0.5;
            let by = cy - bh * 0.5 + 1.0 * scale_f;
            if let Some(path) = rounded_rect_path(bx, by, bw, bh, 1.5 * scale_f) {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let mut pb = PathBuilder::new();
            pb.push_circle(cx, cy + 1.0 * scale_f, 2.0 * scale_f);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let bump_w = 4.0 * scale_f;
            let bump_h = 2.0 * scale_f;
            let bump_x = cx - bump_w * 0.5;
            let bump_y = by - bump_h;
            let mut pb = PathBuilder::new();
            pb.move_to(bump_x, bump_y);
            pb.line_to(bump_x + bump_w, bump_y);
            pb.line_to(bump_x + bump_w, bump_y + bump_h);
            pb.line_to(bump_x, bump_y + bump_h);
            pb.close();
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
        }
        I::Background => {
            let s = 12.0 * scale_f;
            let x = cx - s * 0.5;
            let y = cy - s * 0.5;
            if let Some(path) = rounded_rect_path(x, y, s, s, 2.0 * scale_f) {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let dot_r = 1.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.push_circle(cx - 2.5 * scale_f, cy + 1.0 * scale_f, dot_r);
            pb.push_circle(cx + 2.5 * scale_f, cy + 1.0 * scale_f, dot_r);
            if let Some(path) = pb.finish() {
                pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
            }
        }
        I::Restore => {
            let r = 5.0 * scale_f;
            let k = r * 0.5523;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - r, cy);
            pb.cubic_to(cx - r, cy + k, cx - k, cy + r, cx, cy + r);
            pb.cubic_to(cx + k, cy + r, cx + r, cy + k, cx + r, cy);
            pb.cubic_to(cx + r, cy - k, cx + k, cy - r, cx, cy - r);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let a = 2.5 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - r);
            pb.line_to(cx - a, cy - r - a);
            pb.move_to(cx, cy - r);
            pb.line_to(cx + a, cy - r - a);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
        }
        I::Clear => {
            let bw = 8.0 * scale_f;
            let bh = 9.0 * scale_f;
            let bx = cx - bw * 0.5;
            let by = cy - bh * 0.5 + 1.5 * scale_f;
            if let Some(path) = rounded_rect_path(bx, by, bw, bh, 1.5 * scale_f) {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let lid_w = 11.0 * scale_f;
            let lid_x = cx - lid_w * 0.5;
            let lid_y = by - 1.5 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(lid_x, lid_y);
            pb.line_to(lid_x + lid_w, lid_y);
            let h_w = 4.0 * scale_f;
            let h_x = cx - h_w * 0.5;
            pb.move_to(h_x, lid_y);
            pb.line_to(h_x, lid_y - 1.5 * scale_f);
            pb.line_to(h_x + h_w, lid_y - 1.5 * scale_f);
            pb.line_to(h_x + h_w, lid_y);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
        }
        I::Close => {
            let s = 5.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - s, cy - s);
            pb.line_to(cx + s, cy + s);
            pb.move_to(cx + s, cy - s);
            pb.line_to(cx - s, cy + s);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
        }
        I::Settings => {
            // Three horizontal sliders with knobs at staggered
            // positions — the standard "adjustments / preferences"
            // glyph. Three lines + three filled circles; the knob
            // positions vary so it visually reads as sliders rather
            // than just a hamburger menu.
            let half_w = 7.0 * scale_f;
            let row_spacing = 4.5 * scale_f;
            let knob_r = 1.8 * scale_f;
            let knob_offsets = [-3.0_f32, 2.0, -1.0];
            let mut lines = PathBuilder::new();
            for (i, _) in knob_offsets.iter().enumerate() {
                let y = cy + (i as f32 - 1.0) * row_spacing;
                lines.move_to(cx - half_w, y);
                lines.line_to(cx + half_w, y);
            }
            if let Some(path) = lines.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let mut knobs = PathBuilder::new();
            for (i, &x_off) in knob_offsets.iter().enumerate() {
                let y = cy + (i as f32 - 1.0) * row_spacing;
                knobs.push_circle(cx + x_off * scale_f, y, knob_r);
            }
            if let Some(path) = knobs.finish() {
                pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
            }
        }
    }
}

/// Build a horizontal pill path (rectangle with fully-rounded ends).
/// `w` must be ≥ `h`; otherwise returns `None`.
fn pill_path(x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Path> {
    use tiny_skia::PathBuilder;
    if w < h {
        return None;
    }
    let r = h * 0.5;
    let cy = y + r;
    // Cubic Bezier circle approximation: control offset = r * 0.5523.
    let k = r * 0.5523;
    let mut pb = PathBuilder::new();
    // Top edge (left-corner end → right-corner start).
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    // Right cap as two cubic quarters.
    pb.cubic_to(x + w - r + k, y, x + w, cy - k, x + w, cy);
    pb.cubic_to(x + w, cy + k, x + w - r + k, y + h, x + w - r, y + h);
    // Bottom edge.
    pb.line_to(x + r, y + h);
    // Left cap.
    pb.cubic_to(x + r - k, y + h, x, cy + k, x, cy);
    pb.cubic_to(x, cy - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}



/// Pre-multiplied RGBA, stored in memory as R G B A. Matches both
/// tiny-skia's `PremultipliedColorU8` byte layout and wl_shm's
/// `Abgr8888` format.
pub(crate) fn rgba8888_premul(c: Color) -> [u8; 4] {
    let a = c.a as u16;
    let r = (c.r as u16 * a / 255) as u8;
    let g = (c.g as u16 * a / 255) as u8;
    let b = (c.b as u16 * a / 255) as u8;
    [r, g, b, c.a]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Guide, GuideAxis, HeldRect, Hud, HudEdge, HudKind, HudMeasurementFormat, HudToast,
        StuckMeasurement,
    };

    /// Static-heavy fixture WITHOUT a crosshair. Held rect, guide, and
    /// stuck measurement live in the upper portion; toast + corner
    /// indicator live far away in the dynamic layer. Designed so no
    /// pixel is painted by BOTH a static and a dynamic stroke — the
    /// composite path then matches the single-pass render byte-for-byte
    /// even though `composite_glyph` and tiny-skia's `draw_pixmap` use
    /// slightly different SrcOver rounding.
    fn fixture_no_overlap() -> Hud {
        let mut hud = Hud::hover((0.0, 0.0));
        // No crosshair → no full-surface axis lines that would touch
        // both layers' painted regions.
        hud.kind = HudKind::None;
        hud.held_rects.push(HeldRect {
            rect_start: (10.0, 10.0),
            rect_end: (60.0, 60.0),
            camera_armed: false,
            color_alternate: false,
        });
        hud.guides.push(Guide {
            axis: GuideAxis::Horizontal,
            position: 80,
            color_alternate: false,
            hovered: false,
        });
        hud.stuck_measurements.push(StuckMeasurement {
            axis: GuideAxis::Horizontal,
            at: 110.0,
            start: 10.0,
            end: 90.0,
            pill_offset: (0.0, 0.0),
            color_alternate: false,
            hovered: false,
        });
        hud.toast = Some(HudToast { text: "ok".into() });
        hud.corner_indicator = Some("F".into());
        hud
    }

    /// Wider fixture with a live crosshair whose axis lines DO sweep
    /// across the static guide. Used to catch catastrophic drift
    /// (e.g. a stroke disappearing from a layer would put hundreds of
    /// pixels off), while tolerating a handful of 1-ULP rounding
    /// differences at stroke intersections — `composite_glyph`'s
    /// integer SrcOver and tiny-skia's float SrcOver round differently
    /// in the last byte.
    fn fixture_with_crosshair() -> Hud {
        let mut hud = Hud::hover((180.0, 180.0));
        hud.kind = HudKind::Hover {
            cursor: (180.0, 180.0),
            edges: [None; 4],
        };
        hud.held_rects.push(HeldRect {
            rect_start: (10.0, 10.0),
            rect_end: (60.0, 60.0),
            camera_armed: false,
            color_alternate: false,
        });
        hud.guides.push(Guide {
            axis: GuideAxis::Vertical,
            position: 100,
            color_alternate: false,
            hovered: false,
        });
        hud.stuck_measurements.push(StuckMeasurement {
            axis: GuideAxis::Horizontal,
            at: 80.0,
            start: 10.0,
            end: 90.0,
            pill_offset: (0.0, 0.0),
            color_alternate: false,
            hovered: false,
        });
        hud
    }

    /// Composite `static_buf` then `dynamic_buf` onto a fresh
    /// background-tinted pixmap and return the resulting bytes. Uses
    /// tiny-skia's `draw_pixmap` so the SrcOver math matches the
    /// stroke pipeline.
    fn compose_layers(hud: &Hud, w: u32, h: u32, scale: u32) -> Vec<u8> {
        use tiny_skia::{IntSize, Pixmap, PixmapPaint, Transform};
        let mut sta = vec![0u8; (w * h * 4) as usize];
        render_static_into(&mut sta, w, h, scale, hud);
        let mut dyn_ = vec![0u8; (w * h * 4) as usize];
        render_dynamic_into(&mut dyn_, w, h, scale, hud);

        let size = IntSize::from_wh(w, h).unwrap();
        let sta_pix = Pixmap::from_vec(sta, size).unwrap();
        let dyn_pix = Pixmap::from_vec(dyn_, size).unwrap();
        let mut composed = Pixmap::new(w, h).unwrap();
        composed
            .data_mut()
            .chunks_exact_mut(4)
            .for_each(|c| c.copy_from_slice(&rgba8888_premul(hud.background)));
        composed.draw_pixmap(
            0,
            0,
            sta_pix.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        composed.draw_pixmap(
            0,
            0,
            dyn_pix.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        composed.data().to_vec()
    }

    fn diff_pixels(a: &[u8], b: &[u8]) -> usize {
        a.chunks_exact(4)
            .zip(b.chunks_exact(4))
            .filter(|(x, y)| x != y)
            .count()
    }

    /// Byte-exact: two-buffer composite produces the same pixels as
    /// the single-buffer render when no pixel is painted by both
    /// layers. This is the strictest signal the split is correct —
    /// any draw call leaking between layers would change the output.
    #[test]
    fn split_matches_single_pass_byte_for_byte() {
        let (w, h, scale) = (240u32, 240u32, 1u32);
        let hud = fixture_no_overlap();
        let mut all = vec![0u8; (w * h * 4) as usize];
        render_hud_into(&mut all, w, h, scale, &hud);
        let composed = compose_layers(&hud, w, h, scale);
        assert_eq!(
            all, composed,
            "no-overlap split composite must match single-pass byte-for-byte"
        );
    }

    /// Looser bound: when static and dynamic strokes share pixels at
    /// the cursor crosshair, integer-rounding noise in
    /// `composite_glyph` vs tiny-skia leaves a handful of 1-ULP-off
    /// pixels (~2 in the standard fixture). Catches any change that
    /// would corrupt the composite at scale — a removed stroke or
    /// glyph would be thousands of bad pixels, not single digits.
    #[test]
    fn split_matches_single_pass_with_crosshair_overlap() {
        let (w, h, scale) = (240u32, 240u32, 1u32);
        let hud = fixture_with_crosshair();
        let mut all = vec![0u8; (w * h * 4) as usize];
        render_hud_into(&mut all, w, h, scale, &hud);
        let composed = compose_layers(&hud, w, h, scale);
        let diffs = diff_pixels(&all, &composed);
        // Empirically 2 pixels at the guide × crosshair intersection.
        // Bump the bound if a future refactor adds another overlap;
        // catch a serious regression if the count balloons.
        assert!(
            diffs <= 16,
            "overlapping composite drifted from single-pass by {diffs} pixels; expected ≤ 16"
        );
    }

    /// Hash must be stable across mutations to dynamic-only fields.
    /// If it isn't, the static cache invalidates on every cursor
    /// move, defeating the whole optimization.
    #[test]
    fn static_hash_ignores_dynamic_fields() {
        let hud = fixture_with_crosshair();
        let base = static_hash(&hud);

        let mut moved = hud.clone();
        moved.kind = HudKind::Hover {
            cursor: (50.0, 50.0),
            edges: [None; 4],
        };
        assert_eq!(static_hash(&moved), base, "cursor move changed static hash");

        let mut toasted = hud.clone();
        toasted.toast = Some(HudToast { text: "hi".into() });
        assert_eq!(
            static_hash(&toasted),
            base,
            "toast set changed static hash"
        );

        let mut menu_open = hud.clone();
        menu_open.context_menu = Some(crate::HudContextMenu {
            origin: (10.0, 10.0),
            width: 200.0,
            items: vec![],
            hovered: None,
        });
        assert_eq!(
            static_hash(&menu_open),
            base,
            "context menu open changed static hash"
        );

        let mut corner = hud.clone();
        corner.corner_indicator = Some("F · 200%".into());
        assert_eq!(
            static_hash(&corner),
            base,
            "corner indicator changed static hash"
        );

        let mut shown = hud.clone();
        shown.show_cursor = !shown.show_cursor;
        assert_eq!(
            static_hash(&shown),
            base,
            "show_cursor toggle changed static hash"
        );

        let mut foreground = hud.clone();
        foreground.foreground = Color::rgba(1, 2, 3, 4);
        assert_eq!(
            static_hash(&foreground),
            base,
            "foreground change leaked into static hash"
        );

        let mut bg = hud.clone();
        bg.background = Color::rgba(9, 9, 9, 255);
        assert_eq!(
            static_hash(&bg),
            base,
            "background change leaked into static hash"
        );

        let mut held_kind = hud.clone();
        held_kind.kind = HudKind::Held {
            rect_start: (1.0, 2.0),
            rect_end: (3.0, 4.0),
            cursor: (5.0, 6.0),
            edges: [Some(HudEdge {
                axis: crate::HudAxis::Left,
                position: (5.0, 5.0),
                distance_px: 10,
            }), None, None, None],
            camera_armed: true,
            cursor_in_rect: false,
        };
        assert_eq!(
            static_hash(&held_kind),
            base,
            "HudKind::Held change leaked into static hash"
        );
    }

    /// Hash must change when any static-affecting field changes; if
    /// it doesn't, the backend reuses a stale cache and the held /
    /// stuck / guide draws disappear.
    #[test]
    fn static_hash_tracks_static_fields() {
        let hud = fixture_with_crosshair();
        let base = static_hash(&hud);

        let mut more_rects = hud.clone();
        more_rects.held_rects.push(HeldRect {
            rect_start: (70.0, 70.0),
            rect_end: (80.0, 80.0),
            camera_armed: false,
            color_alternate: false,
        });
        assert_ne!(
            static_hash(&more_rects),
            base,
            "adding a held rect must invalidate"
        );

        let mut moved_rect = hud.clone();
        moved_rect.held_rects[0].rect_start.0 += 1.0;
        assert_ne!(
            static_hash(&moved_rect),
            base,
            "moving a held rect must invalidate"
        );

        let mut more_guides = hud.clone();
        more_guides.guides.push(Guide {
            axis: GuideAxis::Horizontal,
            position: 120,
            color_alternate: false,
            hovered: false,
        });
        assert_ne!(
            static_hash(&more_guides),
            base,
            "adding a guide must invalidate"
        );

        let mut more_stuck = hud.clone();
        more_stuck.stuck_measurements.push(StuckMeasurement {
            axis: GuideAxis::Vertical,
            at: 150.0,
            start: 10.0,
            end: 200.0,
            pill_offset: (0.0, 0.0),
            color_alternate: false,
            hovered: false,
        });
        assert_ne!(
            static_hash(&more_stuck),
            base,
            "adding a stuck must invalidate"
        );

        let mut align = hud.clone();
        align.align_mode = !align.align_mode;
        assert_ne!(
            static_hash(&align),
            base,
            "align_mode toggle must invalidate"
        );

        let mut units = hud.clone();
        units.measurement_format = HudMeasurementFormat {
            unit_suffix: "pt".into(),
            ..hud.measurement_format.clone()
        };
        assert_ne!(
            static_hash(&units),
            base,
            "unit suffix change must invalidate"
        );

        let mut scale = hud.clone();
        scale.measurement_format.scale_factor += 1.0;
        assert_ne!(
            static_hash(&scale),
            base,
            "scale_factor change must invalidate"
        );

        let mut primary = hud.clone();
        primary.primary_fg = Color::rgba(1, 1, 1, 255);
        assert_ne!(
            static_hash(&primary),
            base,
            "primary_fg change must invalidate"
        );

        let mut guide_color = hud.clone();
        guide_color.guide_color = Color::rgba(2, 2, 2, 255);
        assert_ne!(
            static_hash(&guide_color),
            base,
            "guide_color change must invalidate"
        );
    }

    /// A static_hash that varies frame-to-frame for the SAME inputs
    /// would never let the cache hit. The daemon clones the Hud
    /// before sending; clones must produce the same digest.
    #[test]
    fn static_hash_stable_under_clone() {
        let hud = fixture_with_crosshair();
        let a = static_hash(&hud);
        let b = static_hash(&hud.clone());
        assert_eq!(a, b);
    }
}
