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

pub use aspect::{classify as classify_aspect, CommonRatio, Mode as AspectMode, Ratio};
pub use color::Rgba;
pub use edge::{
    detect_edges, shrink_to_content, shrink_to_content_with_bg, Direction, EdgeCandidate,
    EdgeQuad, Tolerance,
};
pub use frame::FrameView;
pub use geometry::{Px, PxRect};
pub use handoff::{
    find_installed_apps as find_installed_handoff_apps, lookup_for_binary, render_args,
    resolve_icon, HandoffApp, KNOWN_HANDOFF_APPS,
};
pub use measurement::{
    axis_biased_snap, best_snap, Axis, Measurement, Mode as InteractionMode, SnapPoint,
};
pub use settings::{
    AppearanceSettings, ColorRgba, CopyFormat, GeneralSettings, IntegrationSettings,
    RoundingMode, ScreenshotSettings, Settings, ShortcutSettings, ToleranceLevel,
    ToleranceSettings, Units, settings_path,
};
