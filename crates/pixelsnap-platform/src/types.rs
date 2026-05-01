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
    Quit,
}

pub type EventReceiver = mpsc::Receiver<PlatformEvent>;
#[allow(dead_code)]
pub(crate) type EventSender = mpsc::Sender<PlatformEvent>;
