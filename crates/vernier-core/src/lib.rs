//! Algorithms, geometry, and color math for vernier.
//!
//! No GUI or platform dependencies live in this crate.

pub mod aspect;
pub mod color;
pub mod edge;
pub mod frame;
pub mod geometry;
pub mod handoff;
pub mod measurement;
pub mod settings;

pub use aspect::{CommonRatio, Mode as AspectMode, Ratio, classify as classify_aspect};
pub use color::Rgba;
pub use edge::{
    Direction, EdgeCandidate, EdgeQuad, Tolerance, detect_edges, shrink_to_content,
    shrink_to_content_with_bg,
};
pub use frame::FrameView;
pub use geometry::{Px, PxRect};
pub use handoff::{
    HandoffApp, KNOWN_HANDOFF_APPS, find_installed_apps as find_installed_handoff_apps,
    lookup_for_binary, render_args, resolve_icon,
};
pub use measurement::{
    Axis, Measurement, Mode as InteractionMode, SnapPoint, axis_biased_snap, best_snap,
};
pub use settings::{
    AppearanceSettings, ClipboardUnit, ColorRgba, CopyFormat, GeneralSettings, IntegrationSettings,
    RoundingMode, ScreenshotSettings, Settings, ShortcutSettings, ToleranceLevel,
    ToleranceSettings, settings_path,
};

/// Stable per-process build identifier — the mtime (in seconds since
/// the epoch, hex-encoded) of the executable that launched this
/// process. Captured the first time it's called, so a later rebuild
/// of the on-disk binary doesn't invalidate the value for an
/// already-running process. Used by the prefs window's daemon-probe
/// to detect when the daemon is from an older build than itself.
pub fn build_id() -> String {
    use std::sync::OnceLock;
    use std::time::UNIX_EPOCH;
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let Ok(exe) = std::env::current_exe() else {
            return "unknown".to_string();
        };
        let Ok(meta) = std::fs::metadata(&exe) else {
            return "unknown".to_string();
        };
        let secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("{secs:x}")
    })
    .clone()
}
