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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, Copy, PartialEq, Default)]
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
    /// A measurement was committed; show the segment from `start` to
    /// `end` until the user starts another one.
    Held { start: (f64, f64), end: (f64, f64) },
}

#[derive(Debug, Clone, Copy)]
pub struct HudEdge {
    pub axis: HudAxis,
    pub position: (f64, f64),
    pub distance_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            key: Key::Char('p'),
        }
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
                TrayMenuItem::Separator,
                TrayMenuItem::Action {
                    id: "quit".into(),
                    label: "Quit vernier".into(),
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
    TrayIconLeftClicked,
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
    /// release.
    KeyboardKey {
        monitor: MonitorId,
        keysym: u32,
        pressed: bool,
    },
    Quit,
}

pub type EventReceiver = mpsc::Receiver<PlatformEvent>;
#[allow(dead_code)]
pub(crate) type EventSender = mpsc::Sender<PlatformEvent>;
