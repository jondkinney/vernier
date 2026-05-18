//! Platform-neutral app icon rasterization. Consumers ask for an
//! RGBA8 buffer at the size they need (tray, launcher PNG, prefs
//! About card).
//!
//! The SVGs are kept crate-local under `assets/icons/svg/` so the
//! crate is self-contained when published to crates.io (a published
//! tarball can't reach the repo-root `assets/`). The repo-root copy
//! remains the canonical brand source used by the packaging
//! scripts; these two files mirror it.

const APP_ICON_SVG: &[u8] = include_bytes!("../assets/icons/svg/vernier.svg");
const TRAY_ICON_SVG: &[u8] = include_bytes!("../assets/icons/svg/vernier-symbolic.svg");

/// Render the colored Vernier app icon at `size × size`. Used by
/// the daemon to drop a PNG on disk for desktop / launcher entries
/// and by the prefs window's About panel.
pub fn render_app_icon_rgba(size: u32) -> Vec<u8> {
    rasterize_or_transparent(APP_ICON_SVG, size)
}

/// The raw colored app-icon SVG. Consumers that install icons into
/// an XDG hicolor theme write this verbatim to `scalable/apps/` so
/// theme-aware launchers get a resolution-independent icon.
pub fn app_icon_svg() -> &'static [u8] {
    APP_ICON_SVG
}

/// Render the monochrome tray/menubar icon at `size × size`. The
/// source SVG uses `currentColor`; substitute white so it reads
/// against the dark waybar background most distros ship.
pub fn render_tray_icon_rgba(size: u32) -> Vec<u8> {
    let recolored = std::str::from_utf8(TRAY_ICON_SVG)
        .unwrap_or("")
        .replace("currentColor", "#ffffff");
    rasterize_or_transparent(recolored.as_bytes(), size)
}

fn rasterize_or_transparent(svg_bytes: &[u8], size: u32) -> Vec<u8> {
    crate::rasterize_svg(svg_bytes, size).unwrap_or_else(|| vec![0u8; (size * size * 4) as usize])
}
