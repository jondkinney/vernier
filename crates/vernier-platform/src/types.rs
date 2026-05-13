//! Shared value types, errors, and events for the platform layer.

use std::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    #[error("not supported on this platform/session: {what}")]
    Unsupported { what: &'static str },
    #[error("portal request denied or unavailable: {reason}")]
    Portal { reason: String },
    #[error("monitor not found: {0:?}")]
    MonitorNotFound(MonitorId),
    #[error("backend i/o error")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, PlatformError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MonitorId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
    pub const TRANSPARENT: Self = Self::rgba(0, 0, 0, 0);
}

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub id: MonitorId,
    pub name: String,
    pub bounds: Rect,
    pub scale_factor: f64,
    pub is_primary: bool,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
    pub bounds: Rect,
    pub pixels: Vec<u8>,
}

/// Pixel layout used by [`NativeFrame`]. Edge detection treats all
/// 4-byte formats as equivalent because its color delta is symmetric
/// across R/G/B; consumers that care about the exact byte order (e.g.
/// PNG export) need to inspect this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8,
    Bgrx8,
    Rgba8,
    Rgbx8,
    Xrgb8,
    Xbgr8,
}

/// Capture in the source's native pixel format, with the original
/// per-row `stride`. Used by the live measurement loop to skip the
/// BGRA→RGBA conversion that [`Frame`] requires.
#[derive(Debug, Clone)]
pub struct NativeFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row. May exceed `width * 4` when the source pads rows.
    pub stride: u32,
    pub format: PixelFormat,
    pub bounds: Rect,
    pub scale_factor: f64,
    pub pixels: Vec<u8>,
}

/// A heads-up display the overlay should render on top of its background
/// tint. Coordinates are surface-local pixels (logical px on Wayland).
#[derive(Debug, Clone)]
pub struct Hud {
    pub kind: HudKind,
    /// Background tint. Pass `Color::TRANSPARENT` for an undecorated HUD
    /// over the bare desktop.
    pub background: Color,
    /// Foreground stroke color for HUD primitives.
    pub foreground: Color,
    /// Optional dark status pill drawn on top of `kind` (e.g.
    /// "Tolerance: High" while the user is cycling tolerance levels,
    /// or "Screenshot taken" right after a capture).
    pub toast: Option<HudToast>,
    /// Persistent reference guides. Drawn last so they sit on top of
    /// `kind` and `toast` — used as anchors to measure against.
    pub guides: Vec<Guide>,
    /// "Stuck" axis measurements — frozen snapshots of the live
    /// crosshair's vertical or horizontal extent, with the pixel
    /// distance pinned in place.
    pub stuck_measurements: Vec<StuckMeasurement>,
    /// Committed rectangle measurements ("held" rects). Each finished
    /// drag pushes one into this vec — they all stay visible while
    /// new ones are drawn on top.
    pub held_rects: Vec<HeldRect>,
    /// True when the cursor is currently inside any of the held
    /// rects. Suppresses the live crosshair and draws a plain arrow
    /// cursor instead.
    pub cursor_in_rect: bool,
    /// When set, the renderer draws a non-default cursor at this
    /// position (logical px). Used during guide placement /
    /// dragging, and over the resize handles of held rects.
    pub move_cursor_at: Option<(f64, f64)>,
    /// Style of cursor to draw when [`Hud::move_cursor_at`] is set.
    /// `Move` is the 4-direction arrow used by guides; the four
    /// `Resize*` variants are 2-headed arrows aligned with the edge
    /// or corner the cursor is hovering on a held rect.
    pub cursor_kind: CursorKind,
    /// "Crosshairs alignment" mode (held while Shift is down). When
    /// true, the renderer suppresses every measurement value and
    /// extends the live axis lines to the screen edges, leaving just
    /// guides + a clean crosshair for visual alignment.
    pub align_mode: bool,
    /// Right-click context menu, drawn on top of every other HUD
    /// layer. `None` while the menu is closed.
    pub context_menu: Option<HudContextMenu>,
    /// Color of the persistent reference guide lines. Sourced from
    /// the user's appearance prefs. Guides with `color_alternate=true`
    /// use [`Self::alternative_guide_color`] instead.
    pub guide_color: Color,
    /// Second guide-line color, chosen via the `X` toggle while a
    /// guide is pending placement. Once placed, the guide carries
    /// its own `color_alternate` flag so the renderer can pick which
    /// of these two colors to draw it with.
    pub alternative_guide_color: Color,
    /// Foreground primary color (red default). Used by HUD/stuck/rect
    /// renders when their `color_alternate` is false. Set from
    /// appearance prefs; the renderer reads this so it can pick
    /// per-element colors instead of relying on a single foreground.
    pub primary_fg: Color,
    /// Foreground alternate color (black default). Paired with
    /// `primary_fg` for the per-element color toggle.
    pub alternate_fg: Color,
    /// How distance / dimension values render in pills (units +
    /// rounding mode). Defaults to integer logical pixels with a
    /// "px" suffix.
    pub measurement_format: HudMeasurementFormat,
    /// Show the live measurement crosshair (axis lines + tick caps
    /// + `+` marker + W×H pill) on hover/held screens. When false
    /// the renderer skips that whole block. The move cursor
    /// (guides) and resize cursors (held-rect handles) are NOT
    /// gated by this — they remain visible since they're the
    /// only feedback for those interactions.
    pub show_cursor: bool,
    /// Optional small "F · 200%" pill rendered in the top-right
    /// corner. Set when the Figma plugin is connected and the
    /// active window looks like a Figma tab — signals that the
    /// dimensions are being scaled to canvas pixels.
    pub corner_indicator: Option<String>,
}

/// Knobs the renderer reads to format measurement labels.
#[derive(Debug, Clone, PartialEq)]
pub struct HudMeasurementFormat {
    pub unit_suffix: String,
    pub rounding: HudRounding,
    /// Display scale factor of the active monitor; multiplied in
    /// when [`HudRounding::ScreenPixels`] is selected.
    pub scale_factor: f64,
    /// Prefix area-pill numbers with `W:` and `H:` labels.
    pub wh_indicators: bool,
    /// Show the aspect-ratio pill on area rectangles. When false,
    /// the rect renders without an aspect pill regardless of mode.
    pub aspect_in_area: bool,
    /// Reporting style for the aspect-ratio pill: Automatic picks
    /// a curated common ratio when within tolerance, otherwise the
    /// reduced fraction; CommonOnly hides the pill if no curated
    /// match exists; Standard always picks a common ratio; Reduced
    /// always picks the reduced fraction.
    pub aspect_mode: vernier_core::AspectMode,
    /// Divide raw on-screen pixel values by this before rounding so
    /// dimensions reflect canvas-coordinate pixels (Figma plugin
    /// integration). 1.0 = no scaling; 2.0 = halve every value
    /// because the user is viewing at 200% zoom.
    pub dimension_divisor: f64,
}

impl Default for HudMeasurementFormat {
    fn default() -> Self {
        Self {
            unit_suffix: "px".to_string(),
            rounding: HudRounding::PointsRounded,
            scale_factor: 1.0,
            wh_indicators: false,
            aspect_in_area: true,
            aspect_mode: vernier_core::AspectMode::Automatic,
            dimension_divisor: 1.0,
        }
    }
}

impl HudMeasurementFormat {
    /// Render a logical-pixel measurement value with the configured
    /// rounding mode. No unit suffix is appended.
    pub fn format_number(&self, value_logical: f64) -> String {
        let divisor = if self.dimension_divisor > 0.0 {
            self.dimension_divisor
        } else {
            1.0
        };
        let value = value_logical / divisor;
        match self.rounding {
            HudRounding::Points => {
                let r = (value * 10.0).round() / 10.0;
                if (r - r.round()).abs() < f64::EPSILON {
                    format!("{}", r as i64)
                } else {
                    format!("{r:.1}")
                }
            }
            HudRounding::PointsRounded => format!("{}", value.round() as i64),
            HudRounding::ScreenPixels => {
                format!("{}", (value * self.scale_factor).round() as i64)
            }
        }
    }

    /// `format_number` with the configured unit suffix appended.
    pub fn format_value(&self, value_logical: f64) -> String {
        format!("{}{}", self.format_number(value_logical), self.unit_suffix)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HudRounding {
    /// Logical (point) pixels, fractional values allowed to one
    /// decimal place.
    Points,
    /// Logical pixels rounded to the nearest integer.
    PointsRounded,
    /// Physical pixels (logical × `scale_factor`), rounded.
    ScreenPixels,
}

/// A floating right-click menu rendered by the overlay. The list of
/// items, including their labels and shortcut hints, is supplied by
/// the app loop; the renderer just draws and tracks hover.
#[derive(Debug, Clone)]
pub struct HudContextMenu {
    /// Top-left of the menu in logical pixels. The app loop is
    /// responsible for clamping this so the menu fits on-screen,
    /// since hit-testing has to use the same clamped origin.
    pub origin: (f64, f64),
    /// Menu width in logical px, set by the app loop so that both
    /// renderer and hit-tester agree on layout (no font-measurement
    /// drift between the two).
    pub width: f64,
    pub items: Vec<HudContextMenuItem>,
    /// Index into `items` of the row currently under the cursor, or
    /// `None` if the cursor is outside any row (e.g. on a divider).
    pub hovered: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct HudContextMenuItem {
    pub label: String,
    /// Optional right-aligned shortcut hint (e.g. "⇧H"). Rendered in a
    /// muted color at a smaller size than `label`.
    pub shortcut: Option<String>,
    pub icon: HudContextMenuIcon,
    /// When `true`, the renderer draws a thin separator line below this
    /// row (groups items into sections).
    pub divider_after: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HudContextMenuIcon {
    GuideH,
    GuideV,
    StuckH,
    StuckV,
    Camera,
    Background,
    Restore,
    Clear,
    Close,
    /// Sliders / "preferences" glyph — three horizontal lines with
    /// staggered knobs. Used for the "Preferences…" menu item.
    Settings,
}

/// A committed rectangle measurement — the data drawn for each
/// finished drag. `camera_armed` is per-frame transient state set by
/// the app loop when the cursor hovers the rect's pill.
#[derive(Debug, Clone, Copy)]
pub struct HeldRect {
    pub rect_start: (f64, f64),
    pub rect_end: (f64, f64),
    pub camera_armed: bool,
    /// Foreground variant snapshotted at the moment this rect was
    /// placed. `false` uses `appearance.primary_color`, `true` uses
    /// `appearance.alternative_color`. The global `color_alternate`
    /// (toggled by `X`) only affects new placements + the live HUD —
    /// existing rects keep whichever color they had when committed.
    pub color_alternate: bool,
}

/// A frozen single-axis measurement. Drawn as a coral line spanning
/// `start..end` with tick caps on both ends and a pill showing the
/// pixel length. Positions are kept as `f64` so the renderer can do
/// the same `subtract-then-round` step the live W×H pill uses; if
/// the endpoints were rounded individually before subtraction, HiDPI
/// edge positions could drift the displayed length by 1 px.
#[derive(Debug, Clone, Copy)]
pub struct StuckMeasurement {
    /// `Vertical` = a vertical line measuring up-to-down extent.
    /// `Horizontal` = a horizontal line measuring left-to-right.
    pub axis: GuideAxis,
    /// Perpendicular position in logical px (x for Vertical,
    /// y for Horizontal).
    pub at: f64,
    /// Start of the measured span in logical px.
    pub start: f64,
    /// End of the measured span in logical px.
    pub end: f64,
    /// User-applied translation of the value pill, in logical px,
    /// from its computed default anchor. Clamped to ±50 in each
    /// axis at the input layer (click-and-drag on the pill). The
    /// measurement line itself stays fixed; only the pill moves.
    pub pill_offset: (f64, f64),
    /// Foreground variant snapshotted when this measurement was
    /// dropped. Same semantics as `HeldRect::color_alternate`.
    pub color_alternate: bool,
    /// Transient: true when the cursor is over this measurement's
    /// pill — renderer swaps the value text for "×" to signal
    /// "click to remove".
    pub hovered: bool,
}

/// A persistent measurement guide line — a 1 physical-pixel blue line
/// spanning the full buffer along the configured axis.
#[derive(Debug, Clone, Copy)]
pub struct Guide {
    pub axis: GuideAxis,
    /// Logical pixels on the surface. For [`GuideAxis::Horizontal`]
    /// this is the y-coordinate; for [`GuideAxis::Vertical`] it's x.
    pub position: i32,
    /// Color variant snapshotted at placement time. `false` =
    /// `appearance.guide_color` (the default blue), `true` =
    /// `appearance.alternative_color`. The global `color_alternate`
    /// (toggled by `X`) only retags the pending preview + new
    /// placements; already-placed guides keep their color.
    pub color_alternate: bool,
    /// Transient: true when the cursor is hovering this line and the
    /// renderer should draw an "×" hint at the cursor. Click-while-
    /// hovered removes the guide.
    pub hovered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuideAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorKind {
    /// 4-direction arrows (used while placing or dragging a guide).
    Move,
    /// Vertical double-arrow — top/bottom edge of a held rect.
    ResizeNS,
    /// Horizontal double-arrow — left/right edge.
    ResizeEW,
    /// `↖↘` diagonal — top-left or bottom-right corner.
    ResizeNWSE,
    /// `↗↙` diagonal — top-right or bottom-left corner.
    ResizeNESW,
}

#[derive(Debug, Clone)]
pub struct HudToast {
    pub text: String,
}

impl Hud {
    pub fn hover(cursor: (f64, f64)) -> Self {
        Self {
            kind: HudKind::Hover {
                cursor,
                edges: [None; 4],
            },
            // Fully transparent so the user can read what's behind the
            // overlay while measuring — only the HUD strokes draw.
            background: Color::TRANSPARENT,
            // Coral/red.
            foreground: Color::rgba(0xFF, 0x5C, 0x5C, 0xF5),
            toast: None,
            guides: Vec::new(),
            stuck_measurements: Vec::new(),
            held_rects: Vec::new(),
            cursor_in_rect: false,
            move_cursor_at: None,
            cursor_kind: CursorKind::Move,
            align_mode: false,
            context_menu: None,
            guide_color: Color::rgba(0x42, 0x9C, 0xFF, 0xF5),
            alternative_guide_color: Color::rgba(0xFF, 0xA9, 0x4A, 0xF0),
            primary_fg: Color::rgba(0xFF, 0x5C, 0x5C, 0xF5),
            alternate_fg: Color::rgba(0x10, 0x10, 0x10, 0xF5),
            measurement_format: HudMeasurementFormat::default(),
            show_cursor: true,
            corner_indicator: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum HudKind {
    /// User is hovering — show a crosshair at the cursor and tick marks
    /// at any detected edges.
    Hover {
        cursor: (f64, f64),
        edges: [Option<HudEdge>; 4],
    },
    /// User is mid-drag from `start` to `cursor`.
    Drawing { start: (f64, f64), cursor: (f64, f64) },
    /// A measurement was committed and is being held on screen, while the
    /// live crosshair still tracks the cursor on top of it. Click again to
    /// start a new measurement; click on the W×H pill to capture the held
    /// region as a screenshot.
    Held {
        rect_start: (f64, f64),
        rect_end: (f64, f64),
        cursor: (f64, f64),
        edges: [Option<HudEdge>; 4],
        /// True when the cursor is over the W×H pill — the renderer
        /// replaces the dimension text with a camera icon to signal
        /// that clicking will capture the held region.
        camera_armed: bool,
        /// True when the cursor is inside the held rectangle. Hides the
        /// measurement guides (axis lines, ticks, cross marker) and
        /// draws an arrow cursor instead — signals "you're inside the held region, you can click
        /// the pill or click elsewhere to start over".
        cursor_in_rect: bool,
    },
    /// Render no measurement primitives at all — useful when the
    /// overlay only needs to show a toast (e.g. immediately after a
    /// screenshot, before the overlay closes).
    None,
}

#[derive(Debug, Clone, Copy)]
pub struct HudEdge {
    pub axis: HudAxis,
    pub position: (f64, f64),
    pub distance_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HudAxis {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone)]
pub struct AppIdentity {
    pub id: String,
    pub display_name: String,
    pub executable: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct HotkeyId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Accelerator {
    pub modifiers: Modifiers,
    pub key: Key,
}

impl Default for Accelerator {
    fn default() -> Self {
        Self {
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            key: Key::Char('f'),
        }
    }
}

impl Accelerator {
    /// Parse a `+`-separated accelerator string like `"CTRL+SHIFT+F"`,
    /// `"super+space"`, or `"alt+f12"`. Tokens are case-insensitive;
    /// modifiers must precede the single key. Returns `None` on
    /// unrecognised input so the caller can fall back to the default
    /// accelerator without crashing the daemon.
    pub fn parse(s: &str) -> Option<Self> {
        let mut modifiers = Modifiers::NONE;
        let mut key: Option<Key> = None;
        for tok_raw in s.split('+') {
            let tok = tok_raw.trim();
            if tok.is_empty() {
                continue;
            }
            let lower = tok.to_ascii_lowercase();
            match lower.as_str() {
                "shift" => modifiers |= Modifiers::SHIFT,
                "ctrl" | "control" => modifiers |= Modifiers::CTRL,
                "alt" | "opt" | "option" => modifiers |= Modifiers::ALT,
                "super" | "meta" | "cmd" | "command" | "win" => modifiers |= Modifiers::META,
                "esc" | "escape" => key = Some(Key::Escape),
                "enter" | "return" => key = Some(Key::Enter),
                "space" => key = Some(Key::Space),
                "tab" => key = Some(Key::Tab),
                "backspace" => key = Some(Key::Backspace),
                "delete" | "del" => key = Some(Key::Delete),
                "up" => key = Some(Key::Up),
                "down" => key = Some(Key::Down),
                "left" => key = Some(Key::Left),
                "right" => key = Some(Key::Right),
                // Punctuation: spelled out so the `+` separator
                // doesn't have to be escaped. Keypad variants are
                // normalized to the same `Char` so binding "PLUS"
                // catches both main-row and numpad keys.
                "plus" | "kp_add" => key = Some(Key::Char('+')),
                "minus" | "kp_subtract" => key = Some(Key::Char('-')),
                "equal" | "equals" => key = Some(Key::Char('=')),
                "underscore" => key = Some(Key::Char('_')),
                other => {
                    if let Some(rest) = other.strip_prefix('f') {
                        if let Ok(n) = rest.parse::<u8>() {
                            if (1..=24).contains(&n) {
                                key = Some(Key::F(n));
                                continue;
                            }
                        }
                    }
                    if other.chars().count() == 1 {
                        key = other.chars().next().map(|c| Key::Char(c.to_ascii_lowercase()));
                        continue;
                    }
                    return None;
                }
            }
        }
        Some(Self { modifiers, key: key? })
    }

    /// Render back to a stable text form (`SHIFT+CTRL+ALT+SUPER+KEY`)
    /// — handy for prefs UI display and round-trip tests.
    pub fn to_string_key(&self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(Modifiers::SHIFT) {
            parts.push("SHIFT".to_string());
        }
        if self.modifiers.contains(Modifiers::CTRL) {
            parts.push("CTRL".to_string());
        }
        if self.modifiers.contains(Modifiers::ALT) {
            parts.push("ALT".to_string());
        }
        if self.modifiers.contains(Modifiers::META) {
            parts.push("SUPER".to_string());
        }
        let key_str = match self.key {
            // Punctuation is spelled out so the saved string
            // doesn't collide with the `+` modifier separator.
            Key::Char('+') => "PLUS".to_string(),
            Key::Char('-') => "MINUS".to_string(),
            Key::Char('=') => "EQUAL".to_string(),
            Key::Char('_') => "UNDERSCORE".to_string(),
            Key::Char(c) => c.to_ascii_uppercase().to_string(),
            Key::F(n) => format!("F{n}"),
            Key::Escape => "ESC".to_string(),
            Key::Enter => "ENTER".to_string(),
            Key::Space => "SPACE".to_string(),
            Key::Tab => "TAB".to_string(),
            Key::Backspace => "BACKSPACE".to_string(),
            Key::Delete => "DELETE".to_string(),
            Key::Up => "UP".to_string(),
            Key::Down => "DOWN".to_string(),
            Key::Left => "LEFT".to_string(),
            Key::Right => "RIGHT".to_string(),
        };
        parts.push(key_str);
        parts.join("+")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1 << 0);
    pub const CTRL: Self = Self(1 << 1);
    pub const ALT: Self = Self(1 << 2);
    pub const META: Self = Self(1 << 3);

    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    F(u8),
    Escape,
    Enter,
    Space,
    Tab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
}

impl Default for Key {
    fn default() -> Self {
        Key::Char('\0')
    }
}

#[derive(Debug, Clone)]
pub struct TrayMenu {
    pub tooltip: String,
    pub items: Vec<TrayMenuItem>,
}

impl TrayMenu {
    pub fn minimal(tooltip: impl Into<String>) -> Self {
        Self {
            tooltip: tooltip.into(),
            items: vec![
                TrayMenuItem::Action {
                    id: "toggle_overlay".into(),
                    label: "Toggle overlay".into(),
                    enabled: true,
                    accelerator: None,
                },
                TrayMenuItem::Action {
                    id: "open_prefs".into(),
                    label: "Preferences…".into(),
                    enabled: true,
                    accelerator: None,
                },
                TrayMenuItem::Separator,
                TrayMenuItem::Action {
                    id: "quit".into(),
                    label: "Quit Vernier".into(),
                    enabled: true,
                    accelerator: None,
                },
            ],
        }
    }
}

#[derive(Debug, Clone)]
pub enum TrayMenuItem {
    Action {
        id: String,
        label: String,
        enabled: bool,
        accelerator: Option<Accelerator>,
    },
    Toggle {
        id: String,
        label: String,
        enabled: bool,
        checked: bool,
    },
    Separator,
    Submenu {
        id: String,
        label: String,
        items: Vec<TrayMenuItem>,
    },
}

#[derive(Debug, Clone)]
pub enum PlatformEvent {
    HotkeyPressed(HotkeyId),
    TrayMenuActivated { id: String },
    /// Tray icon was activated by the SNI host (left click on
    /// waybar). `x`/`y` are the host-supplied screen coordinates if
    /// available — many hosts pass `(0, 0)` on Wayland because
    /// `x_root` isn't meaningful, in which case the daemon falls
    /// back to querying the cursor position.
    TrayIconLeftClicked { x: i32, y: i32 },
    OverlayClosed(MonitorId),
    MonitorsChanged,
    /// Pointer entered the overlay surface for `monitor`.
    PointerEnter { monitor: MonitorId, x: f64, y: f64 },
    /// Pointer left the overlay surface for `monitor`.
    PointerLeave { monitor: MonitorId },
    /// Pointer moved over the overlay. Coordinates are surface-local
    /// pixels (not logical points; multiply your scale_factor when
    /// rendering at HiDPI).
    PointerMove { monitor: MonitorId, x: f64, y: f64 },
    /// A mouse button was pressed (`pressed=true`) or released
    /// (`pressed=false`). `button` is a Linux input event code
    /// (BTN_LEFT=0x110, BTN_RIGHT=0x111, BTN_MIDDLE=0x112).
    PointerButton {
        monitor: MonitorId,
        button: u32,
        pressed: bool,
        x: f64,
        y: f64,
    },
    /// A keyboard key was pressed/released while the overlay had focus.
    /// `keysym` is an XKB keysym; `pressed` distinguishes press from
    /// release. `is_repeat` is true for auto-repeat events fired by
    /// the compositor while the key is held — daemon handlers opt
    /// into repeats per-action (nudge / tolerance ±) so things like
    /// double-tap-to-clear don't accidentally self-trigger.
    KeyboardKey {
        monitor: MonitorId,
        keysym: u32,
        pressed: bool,
        is_repeat: bool,
    },
    Quit,
}

pub type EventReceiver = mpsc::Receiver<PlatformEvent>;
#[allow(dead_code)]
pub(crate) type EventSender = mpsc::Sender<PlatformEvent>;
