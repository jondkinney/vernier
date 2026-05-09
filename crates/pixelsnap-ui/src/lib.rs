//! Egui-based UI surfaces for vernier. The measurement HUD is
//! rasterized directly by `vernier-platform`; this crate hosts the
//! windows that egui makes the most sense for — currently just the
//! preferences pane.

pub mod prefs;
pub mod tray_menu;

pub use prefs::run_prefs;
pub use tray_menu::{run_tray_menu, TrayMenuChoice};
