//! HUD-text font lookup, shared between the renderer and any code
//! that needs to compute exact text widths (e.g. the pill-placement
//! logic in [`crate::placement`]).
//!
//! The font is loaded once on first request and cached. Falls back
//! through a small list of well-known system paths.

use std::sync::OnceLock;

/// Lazily-loaded TTF font for the dimension readout. Returns `None`
/// when no candidate is available — callers degrade to empty pills
/// rather than crashing.
pub fn hud_font() -> Option<&'static fontdue::Font> {
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        const CANDIDATES: &[&str] = &[
            // Linux distros (Arch / Fedora layouts).
            "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            // macOS system fonts. fontdue can't parse .ttc
            // (TrueType collection) files so we avoid Helvetica.ttc
            // and prefer the standalone .otf System Font fallbacks
            // that ship on every modern macOS, plus Arial Bold which
            // is reliably present.
            "/System/Library/Fonts/SFNS.ttf",
            "/System/Library/Fonts/SFNSRounded.ttf",
            "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/Library/Fonts/Arial Bold.ttf",
            "/Library/Fonts/Arial.ttf",
        ];
        for path in CANDIDATES {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = fontdue::Font::from_bytes(
                    bytes.as_slice(),
                    fontdue::FontSettings::default(),
                ) {
                    log::info!("hud font: {path}");
                    return Some(font);
                }
            }
        }
        log::warn!("hud font: no usable system font found; pills will render empty");
        None
    })
    .as_ref()
}

/// Sum of horizontal glyph advances in `text` at `px_size` pixels.
/// Falls back to an approximation when a glyph is missing metrics.
pub fn measure_text_width(font: &fontdue::Font, text: &str, px_size: f32) -> f32 {
    let mut w = 0.0;
    for ch in text.chars() {
        let m = font.metrics(ch, px_size);
        w += m.advance_width.max(0.0);
    }
    w
}
