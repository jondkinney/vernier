//! User-configurable settings for the daemon and overlay.
//!
//! Stored as TOML at `$XDG_CONFIG_HOME/vernier/settings.toml`
//! (falling back to `$HOME/.config/vernier/settings.toml`). Both
//! the daemon and the prefs UI read and write this file; the daemon
//! reloads on receiving the `reload-settings` IPC command.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Top-level settings document. Fields default to sensible values so
/// a missing file or partial document still yields a usable config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub general: GeneralSettings,
    pub screenshots: ScreenshotSettings,
    pub tolerance: ToleranceSettings,
    pub appearance: AppearanceSettings,
    pub integrations: IntegrationSettings,
    pub shortcuts: ShortcutSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    /// Add an autostart entry on login (Linux: writes
    /// `~/.config/autostart/vernier.desktop`).
    pub launch_at_login: bool,
    /// Hide the system-tray icon. The daemon still runs; the user
    /// drives it through the global hotkey + `vernier toggle`.
    pub hide_tray_icon: bool,
    /// Clipboard format used when the copy-dimensions shortcut
    /// copies a held rectangle's width and height.
    pub copy_dimensions_format: CopyFormat,
    /// Unit the CSS / SASS clipboard formats use (`px` or `rem`).
    pub copy_dimensions_unit: ClipboardUnit,
    /// Base font size, in px, the `rem` clipboard unit divides by.
    pub copy_dimensions_rem_base: u32,
    /// Put the width and height on separate lines in clipboard output.
    pub copy_dimensions_linebreak: bool,
    /// Append the unit suffix (`px` / `pt`) to dimension pills.
    /// When false, pills show bare numbers — useful when the user
    /// wants the screen to stay clean and already knows the unit.
    pub display_units: bool,
    /// How dimension values are computed for display: fractional
    /// logical pixels, rounded logical pixels, or physical (device)
    /// pixels. See [`RoundingMode`].
    pub rounding_mode: RoundingMode,
    /// Prefix area-pill values with `W:` and `H:` labels:
    /// `W: 1024 × H: 768` instead of `1024 × 768`.
    pub display_wh_indicators: bool,
    /// Aspect-ratio reporting style for the area tool.
    pub aspect_mode: crate::AspectMode,
    /// Show the aspect-ratio pill underneath area-tool rectangles.
    pub aspect_in_area_tool: bool,
    /// Show the aspect-ratio pill on the live distance-tool readout.
    pub aspect_in_distance_tool: bool,
    /// Snap distance / area drags to placed reference guides.
    /// Disable for free-cursor measurement near guides.
    pub snap_to_guides: bool,
    /// Snap the live distance measurement to the edges of held
    /// rectangles, the same way it snaps to reference guides.
    pub snap_to_objects: bool,
    /// Freeze the captured frame at measurement-mode entry. When
    /// false, the daemon refreshes the frame on every pointer move
    /// so edge detection follows live content.
    pub freeze_screen: bool,
    /// Show the live measurement crosshair (axis lines + tick
    /// caps + `+` cursor marker + W×H pill) while measuring. When
    /// false, the renderer skips that whole block — the user just
    /// sees the held rects, guides, and stuck measurements they've
    /// already placed. The move-cursor (placing/dragging guides)
    /// and resize-cursors (held-rect handles) still appear because
    /// they're separate code paths tied to specific interactions.
    pub show_cursor: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            launch_at_login: false,
            hide_tray_icon: false,
            copy_dimensions_format: CopyFormat::WidthCommaHeight,
            copy_dimensions_unit: ClipboardUnit::Px,
            copy_dimensions_rem_base: 16,
            copy_dimensions_linebreak: false,
            display_units: true,
            // Physical (device) pixels by default: an exact integer
            // count on every display scale — no fractional values to
            // round, and identical to logical pixels on 1x displays.
            rounding_mode: RoundingMode::ScreenPixels,
            display_wh_indicators: false,
            aspect_mode: crate::AspectMode::Automatic,
            aspect_in_area_tool: true,
            aspect_in_distance_tool: false,
            snap_to_guides: true,
            snap_to_objects: true,
            freeze_screen: true,
            show_cursor: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenshotSettings {
    /// Directory PNGs land in. Empty / missing means use
    /// `$XDG_PICTURES_DIR` then `$HOME/Pictures`.
    pub output_dir: Option<PathBuf>,
    /// Filename template — `{ts}` is replaced by the timestamp,
    /// `{w}`/`{h}` by the captured size.
    pub filename_template: String,
    /// Add transparent padding around the captured rect for room
    /// to annotate. 0 = no padding.
    pub padding_px: u32,
    /// Downscale captured PNGs from physical pixels back to logical
    /// pixels (pre-divides by `scale_factor`). Off = ship the raw
    /// HiDPI buffer.
    pub retina_downscale: bool,
    /// Play a shutter sound when a capture completes.
    pub capture_sound: bool,
    /// Copy the image to the clipboard in addition to saving the
    /// file. Off = file-only.
    pub copy_to_clipboard: bool,
    /// Run the post-capture notify-send notification with an "Edit"
    /// action that opens the file in the configured handoff app.
    /// Renamed from `satty_edit_action` (alias kept for old configs).
    #[serde(alias = "satty_edit_action")]
    pub handoff_edit_action: bool,
    /// When true, the daemon hands every screenshot directly to the
    /// configured `handoff_command` (writing to a temp PNG and
    /// spawning the app with [`crate::render_args`] applied to
    /// `handoff_args`). The handoff app then owns the file save /
    /// clipboard / share flow, so `output_dir`, `filename_template`,
    /// `copy_to_clipboard`, and `handoff_edit_action` are skipped.
    /// `padding_px`, `retina_downscale`, and `capture_sound` still
    /// apply because they shape the image bytes (or the local audio
    /// feedback) regardless of who saves the file. Renamed from
    /// `satty_integration` (alias kept for old configs).
    #[serde(alias = "satty_integration")]
    pub handoff_enabled: bool,
    /// Binary to spawn for handoff — absolute path or PATH-resolved
    /// name. Empty defers to [`crate::detect_default_handoff`]
    /// (currently Satty when installed) so a fresh install works
    /// out-of-the-box.
    pub handoff_command: String,
    /// Display name shown on the prefs card and in the notification's
    /// `Open in <name>` action. Empty falls back to the detected
    /// default's name (or the binary basename).
    pub handoff_app_name: String,
    /// Whitespace-tokenized arg template for the handoff spawn.
    /// `{file}` is substituted with the captured PNG path. Defaults
    /// come from the chosen app's `.desktop` Exec line.
    pub handoff_args: String,
    /// Absolute path to the handoff app's icon (SVG/PNG). Resolved
    /// from the chosen app's `.desktop` `Icon=` field at pick time;
    /// the prefs UI rasterizes it for the integration card.
    pub handoff_icon_path: String,
    /// Escape-hatch shell command for the right-click menu's "Take
    /// normal screenshot" action. Runs *outside* the measurement
    /// pipeline — Vernier exits measure mode and hides the overlay,
    /// then spawns this command, which is expected to do its own
    /// full-screen grab via grim/spectacle/etc. Independent of
    /// `handoff_*` (which controls where Vernier's own measurement
    /// captures get routed).
    pub external_screenshot_command: String,
}

impl Default for ScreenshotSettings {
    fn default() -> Self {
        Self {
            output_dir: None,
            filename_template: "screenshot-{ts}.png".to_string(),
            padding_px: 0,
            // macOS's `screencapture` always writes at native pixel
            // resolution (2x on Retina), so the W×H pill in the
            // captured image won't match the W×H pill the user saw
            // during measurement unless we downscale. Linux's `grim`
            // honors `-s 1` separately, and most Linux users want the
            // raw HiDPI buffer, so the default is platform-split.
            #[cfg(target_os = "macos")]
            retina_downscale: true,
            #[cfg(not(target_os = "macos"))]
            retina_downscale: false,
            capture_sound: true,
            copy_to_clipboard: true,
            handoff_edit_action: true,
            // Off by default — the user opts in by picking an app
            // from the prefs dropdown (or browsing to a custom
            // binary). No auto-selection of Satty / etc.
            handoff_enabled: false,
            handoff_command: String::new(),
            handoff_app_name: String::new(),
            handoff_args: String::new(),
            handoff_icon_path: String::new(),
            external_screenshot_command: "omarchy-capture-screenshot".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToleranceSettings {
    /// Default tolerance level applied each time the daemon enters
    /// measure mode. Live `+`/`-` keys still cycle within a session.
    pub default_level: ToleranceLevel,
    /// Per-level numeric values (sum-of-channel difference,
    /// 0..=255 in the prefs UI). The active level's value is what
    /// the edge detector compares against.
    pub zero_value: u32,
    pub low_value: u32,
    pub medium_value: u32,
    pub high_value: u32,
}

impl Default for ToleranceSettings {
    fn default() -> Self {
        Self {
            default_level: ToleranceLevel::Medium,
            zero_value: 0,
            low_value: 14,
            medium_value: 26,
            high_value: 52,
        }
    }
}

impl ToleranceSettings {
    /// Look up the configured value for `level`. Used by the edge
    /// detector and the HUD readouts so the user's slider changes
    /// take effect on the next reload-settings.
    pub fn value_for(&self, level: ToleranceLevel) -> u32 {
        match level {
            ToleranceLevel::Zero => self.zero_value,
            ToleranceLevel::Low => self.low_value,
            ToleranceLevel::Medium => self.medium_value,
            ToleranceLevel::High => self.high_value,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToleranceLevel {
    Zero,
    Low,
    Medium,
    High,
}

impl ToleranceLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Zero => "Zero",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }
    pub fn higher(self) -> Self {
        match self {
            Self::Zero => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::High,
        }
    }
    pub fn lower(self) -> Self {
        match self {
            Self::Zero => Self::Zero,
            Self::Low => Self::Zero,
            Self::Medium => Self::Low,
            Self::High => Self::Medium,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// Foreground color used by the live measurement HUD (axis
    /// lines, drag rect, pill borders). Coral
    /// red default.
    pub primary_color: ColorRgba,
    /// Alternative foreground swapped in by the `x` key — useful
    /// when the underlying UI clashes with `primary_color`.
    pub alternative_color: ColorRgba,
    /// Color of reference guide lines (Shift+H / Shift+V).
    pub guide_color: ColorRgba,
    /// Alternate color a guide can be placed in. The `x` color
    /// toggle, while a guide is pending, swaps the preview's color
    /// to this; once committed, that guide keeps the color it was
    /// placed with regardless of further toggles.
    pub alternative_guide_color: ColorRgba,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            primary_color: ColorRgba::new(0xFF, 0x5C, 0x5C, 0xF5),
            alternative_color: ColorRgba::new(0x10, 0x10, 0x10, 0xF5),
            guide_color: ColorRgba::new(0x78, 0xB4, 0xFF, 0xF0),
            // Warm coral that contrasts with the default blue guide
            // and the red HUD primary — easy to distinguish at a
            // glance which color a given guide was placed in.
            alternative_guide_color: ColorRgba::new(0xFF, 0xA9, 0x4A, 0xF0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorRgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl ColorRgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoundingMode {
    /// Display values as logical (point) pixels, fractional values
    /// allowed to one decimal place. `100.5px`.
    Points,
    /// Round logical values to the nearest integer. `101px`.
    PointsRounded,
    /// Display physical (device) pixels — multiplies by the display
    /// scale factor before rounding. `201px` on a 2× display.
    ScreenPixels,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationSettings {
    /// Divide on-screen measurements by the current Figma viewport
    /// zoom so dimensions reflect canvas pixels at any zoom level.
    /// Requires the companion Figma plugin (`figma-plugin/`) to be
    /// running in the active Figma file.
    pub figma_zoom_correction: bool,
    /// TCP port the Figma plugin connects to. Must match
    /// `figma-plugin/ui.html`.
    pub figma_bridge_port: u16,
    /// Window classes treated as "browser tab" candidates for
    /// Figma detection. The daemon checks `class` against this list
    /// and matches `title` against the suffix `figma_title_suffix`.
    pub figma_browser_classes: Vec<String>,
    /// Title suffix that marks a Figma tab. Default ` – Figma`
    /// (en-dash) matches Figma's current tab-title convention.
    pub figma_title_suffix: String,
}

impl Default for IntegrationSettings {
    fn default() -> Self {
        Self {
            figma_zoom_correction: true,
            figma_bridge_port: 8765,
            figma_browser_classes: vec![
                "chromium".into(),
                "Chromium".into(),
                "Google-chrome".into(),
                "google-chrome".into(),
                "firefox".into(),
                "Firefox".into(),
                "Brave-browser".into(),
                "brave-browser".into(),
                "zen".into(),
                "zen-alpha".into(),
                "zen-browser".into(),
                "Vivaldi-stable".into(),
            ],
            figma_title_suffix: " \u{2013} Figma".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyFormat {
    /// `1024,768`
    WidthCommaHeight,
    /// `768,1024`
    HeightCommaWidth,
    /// `width: 1024px; height: 768px;`
    CssWidthFirst,
    /// `height: 768px; width: 1024px;`
    CssHeightFirst,
    /// `$width: 1024px; $height: 768px;`
    SassWidthFirst,
    /// `$height: 768px; $width: 1024px;`
    SassHeightFirst,
}

/// Unit the CSS / SASS clipboard formats render dimensions in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardUnit {
    /// CSS pixels: `width: 1024px;`.
    Px,
    /// Root-em: the pixel value divided by the configured base font
    /// size — `width: 64rem;` at a 16 px base.
    Rem,
}

impl CopyFormat {
    pub fn label(self) -> &'static str {
        match self {
            Self::WidthCommaHeight => "width, height",
            Self::HeightCommaWidth => "height, width",
            Self::CssWidthFirst => "CSS (width first)",
            Self::CssHeightFirst => "CSS (height first)",
            Self::SassWidthFirst => "SASS (width first)",
            Self::SassHeightFirst => "SASS (height first)",
        }
    }
    /// Render width × height for the clipboard. `unit` and `rem_base`
    /// only affect the CSS / SASS variants — the comma variants are
    /// always raw pixel integers. `linebreak` puts the width and the
    /// height on separate lines.
    pub fn render(
        self,
        width: u32,
        height: u32,
        unit: ClipboardUnit,
        rem_base: u32,
        linebreak: bool,
    ) -> String {
        let w = css_dimension(width, unit, rem_base);
        let h = css_dimension(height, unit, rem_base);
        let sep = if linebreak { "\n" } else { " " };
        match self {
            Self::WidthCommaHeight => {
                if linebreak {
                    format!("{width}\n{height}")
                } else {
                    format!("{width},{height}")
                }
            }
            Self::HeightCommaWidth => {
                if linebreak {
                    format!("{height}\n{width}")
                } else {
                    format!("{height},{width}")
                }
            }
            Self::CssWidthFirst => format!("width: {w};{sep}height: {h};"),
            Self::CssHeightFirst => format!("height: {h};{sep}width: {w};"),
            Self::SassWidthFirst => format!("$width: {w};{sep}$height: {h};"),
            Self::SassHeightFirst => format!("$height: {h};{sep}$width: {w};"),
        }
    }
}

/// Format a pixel value as a CSS dimension token in the chosen unit.
/// `Rem` divides by the base font size and trims trailing zeros, so
/// `768 / 16` reads `48rem` rather than `48.0000rem`.
fn css_dimension(value_px: u32, unit: ClipboardUnit, rem_base: u32) -> String {
    match unit {
        ClipboardUnit::Px => format!("{value_px}px"),
        ClipboardUnit::Rem => {
            let base = if rem_base == 0 { 16.0 } else { rem_base as f32 };
            let mut s = format!("{:.4}", value_px as f32 / base);
            if s.contains('.') {
                s = s.trim_end_matches('0').trim_end_matches('.').to_string();
            }
            format!("{s}rem")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShortcutSettings {
    /// Toggle measure mode. Stored as a textual accelerator
    /// (`CTRL+SHIFT+SUPER+F`); the platform layer parses on init.
    pub toggle: String,
    /// Exit measure mode in a single press. Held content is
    /// preserved — held rects / guides / stuck measurements stay
    /// visible in the passthrough overlay — so this never wipes a
    /// session. (Clearing content is an explicit action via the
    /// right-click "Clear" menu item.)
    #[serde(alias = "background_mode")]
    pub clear_and_hide: String,
    /// Clear every held rect / guide / stuck measurement and exit
    /// measure mode in a single press. Only matched while measure
    /// mode is active, so it can't collide with the same combo bound
    /// in another app.
    pub clear_and_exit: String,
    /// Restore last saved session.
    pub restore_session: String,
    /// Capture the held rect (the menu Camera item).
    pub capture: String,
    /// Modifier whose held state activates Crosshair (alignment)
    /// mode — full-screen axis lines with measurements suppressed
    /// for visual alignment work. Stored as one of "SHIFT" /
    /// "CTRL" / "ALT" / "SUPER" (or empty to disable).
    pub crosshair_mode: String,
    /// Place a horizontal reference guide (click the next mouse
    /// button to commit it at the cursor's y).
    pub guide_horizontal: String,
    /// Place a vertical reference guide.
    pub guide_vertical: String,
    /// Toggle the HUD foreground between primary and alternate
    /// color (coral red ↔ black by default).
    pub color_toggle: String,
    /// Freeze the current crosshair's horizontal extent as a
    /// stuck measurement.
    pub stuck_horizontal: String,
    /// Freeze the current crosshair's vertical extent as a stuck
    /// measurement.
    pub stuck_vertical: String,
    /// Recapture the screen so subsequent edge-detection sees the
    /// latest content.
    pub refresh_capture: String,
    /// Bump tolerance up one level (more aggressive edge merging).
    pub tolerance_up: String,
    /// Bump tolerance down one level.
    pub tolerance_down: String,
    /// Nudge the hovered held rect 1 px left (10 px with Shift).
    pub nudge_left: String,
    /// Nudge the hovered held rect 1 px right (10 px with Shift).
    pub nudge_right: String,
    /// Nudge the hovered held rect 1 px up (10 px with Shift).
    pub nudge_up: String,
    /// Nudge the hovered held rect 1 px down (10 px with Shift).
    pub nudge_down: String,
    /// Run the External Screenshot Tool action (the right-click
    /// menu's "Take Normal Screenshot"). Triggers the same ESC
    /// exit + detached spawn of
    /// `screenshots.external_screenshot_command` while in measure
    /// mode.
    pub take_normal_screenshot: String,
}

impl Default for ShortcutSettings {
    fn default() -> Self {
        Self {
            toggle: "CTRL+SHIFT+SUPER+F".to_string(),
            clear_and_hide: "ESC".to_string(),
            clear_and_exit: "CTRL+F".to_string(),
            restore_session: "SHIFT+R".to_string(),
            capture: "ENTER".to_string(),
            crosshair_mode: "SHIFT".to_string(),
            guide_horizontal: "SHIFT+H".to_string(),
            guide_vertical: "SHIFT+V".to_string(),
            color_toggle: "X".to_string(),
            stuck_horizontal: "H".to_string(),
            stuck_vertical: "V".to_string(),
            refresh_capture: "R".to_string(),
            tolerance_up: "EQUAL".to_string(),
            tolerance_down: "MINUS".to_string(),
            nudge_left: "LEFT".to_string(),
            nudge_right: "RIGHT".to_string(),
            nudge_up: "UP".to_string(),
            nudge_down: "DOWN".to_string(),
            take_normal_screenshot: "CTRL+S".to_string(),
        }
    }
}

/// Resolved on-disk path for the settings file.
pub fn settings_path() -> PathBuf {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    dir.join("vernier").join("settings.toml")
}

impl Settings {
    /// Load from [`settings_path`]. A missing file yields default
    /// values; a parse error returns `Err`.
    pub fn load() -> Result<Self> {
        let path = settings_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read settings: {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse settings: {}", path.display()))
    }

    /// Persist to [`settings_path`], creating parent dirs if needed.
    pub fn save(&self) -> Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create config dir: {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialize settings")?;
        std::fs::write(&path, text).with_context(|| format!("write settings: {}", path.display()))
    }
}

/// Default-supplying types for Tolerance so `#[serde(default)]` works.
impl Default for ToleranceLevel {
    fn default() -> Self {
        Self::Medium
    }
}
impl Default for RoundingMode {
    fn default() -> Self {
        Self::ScreenPixels
    }
}
impl Default for CopyFormat {
    fn default() -> Self {
        Self::WidthCommaHeight
    }
}
impl Default for ColorRgba {
    fn default() -> Self {
        Self::new(0, 0, 0, 0)
    }
}
