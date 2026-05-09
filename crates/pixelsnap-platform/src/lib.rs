//! Per-OS backends behind a single [`Platform`] trait.
//!
//! The rest of the codebase imports this trait and the [`init`] constructor;
//! concrete OS modules are private and selected via cfg.

mod tray;
mod types;
pub use types::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows_impl;

/// OS abstraction. Returned by [`init`] and shared between threads.
pub trait Platform: Send + Sync {
    /// All connected monitors.
    fn monitors(&self) -> Result<Vec<MonitorInfo>>;

    /// The currently focused application, if known.
    fn focused_app(&self) -> Result<Option<AppIdentity>>;

    /// Capture a still RGBA8 frame of `monitor`.
    fn capture_screen(&self, monitor: MonitorId) -> Result<Frame>;

    /// Like [`capture_screen`] but returns the buffer in its source
    /// pixel format with the original per-row stride. Designed for the
    /// live measurement loop, where R/B order doesn't matter (the edge
    /// detector's color delta is symmetric) and the conversion to
    /// tightly-packed RGBA8 would dominate the per-frame cost.
    fn capture_screen_native(&self, _monitor: MonitorId) -> Result<NativeFrame> {
        Err(PlatformError::Unsupported {
            what: "capture_screen_native is not implemented for this platform",
        })
    }

    /// Register a global hotkey. Activations arrive as
    /// [`PlatformEvent::HotkeyPressed`]. `label` is shown to the user by the
    /// host (e.g. portal dialog) and stored alongside the binding.
    fn register_hotkey(&self, accelerator: Accelerator, label: &str) -> Result<HotkeyId>;

    fn unregister_hotkey(&self, id: HotkeyId) -> Result<()>;

    /// Create a transparent fullscreen overlay covering `monitor`. Initially
    /// hidden — call [`OverlayHandle::show`] to reveal.
    fn create_overlay(&self, monitor: MonitorId) -> Result<OverlayHandle>;

    /// Create the system tray icon. At most one tray per process.
    fn create_tray(&self, menu: TrayMenu) -> Result<TrayHandle>;
}

/// Backend-side overlay operations used by [`OverlayHandle`].
pub trait OverlayOps: Send {
    fn show(&mut self);
    fn hide(&mut self);
    fn toggle(&mut self);
    fn is_visible(&self) -> bool;
    fn monitor(&self) -> MonitorId;
    fn set_tint(&mut self, tint: Color);
    /// Toggle whether the overlay surface captures pointer/keyboard
    /// input. When `false` (the default), the surface is click-through
    /// — pointer events go to underlying windows. When `true`, the
    /// surface receives pointer enter/move/button + keyboard events.
    fn set_input_capturing(&mut self, capturing: bool);
    /// Replace the overlay's heads-up display with `hud`. Pass `None`
    /// to clear the HUD and revert to the bare tint.
    fn set_hud(&mut self, hud: Option<Hud>);
    /// Show or hide the system pointer over this overlay. `true` →
    /// the compositor draws its theme cursor (via wp_cursor_shape v1).
    /// `false` → the cursor is hidden so we can draw our own custom
    /// crosshair / move / resize cursor instead.
    fn set_system_pointer_visible(&mut self, visible: bool);
}

/// Owned handle to an overlay surface. Drop to destroy.
pub struct OverlayHandle {
    inner: Box<dyn OverlayOps>,
}

impl OverlayHandle {
    pub fn from_backend<B: OverlayOps + 'static>(b: B) -> Self {
        Self { inner: Box::new(b) }
    }
    pub fn show(&mut self) {
        self.inner.show()
    }
    pub fn hide(&mut self) {
        self.inner.hide()
    }
    pub fn toggle(&mut self) {
        self.inner.toggle()
    }
    pub fn is_visible(&self) -> bool {
        self.inner.is_visible()
    }
    pub fn monitor(&self) -> MonitorId {
        self.inner.monitor()
    }
    pub fn set_tint(&mut self, c: Color) {
        self.inner.set_tint(c)
    }
    pub fn set_input_capturing(&mut self, capturing: bool) {
        self.inner.set_input_capturing(capturing)
    }
    pub fn set_hud(&mut self, hud: Option<Hud>) {
        self.inner.set_hud(hud)
    }
    pub fn set_system_pointer_visible(&mut self, visible: bool) {
        self.inner.set_system_pointer_visible(visible)
    }
}

/// Backend-side tray operations used by [`TrayHandle`].
pub trait TrayOps: Send {
    fn update_menu(&mut self, menu: TrayMenu) -> Result<()>;
    fn set_active(&mut self, active: bool);
}

/// Owned handle to the tray icon. Drop to remove.
pub struct TrayHandle {
    inner: Box<dyn TrayOps>,
}

impl TrayHandle {
    pub fn from_backend<B: TrayOps + 'static>(b: B) -> Self {
        Self { inner: Box::new(b) }
    }
    pub fn update_menu(&mut self, menu: TrayMenu) -> Result<()> {
        self.inner.update_menu(menu)
    }
    pub fn set_active(&mut self, active: bool) {
        self.inner.set_active(active)
    }
}

/// Initialise the platform backend appropriate for the current OS / session.
///
/// On Linux: Wayland if `$WAYLAND_DISPLAY` is set, otherwise X11.
pub fn init() -> Result<(Box<dyn Platform>, EventReceiver)> {
    #[cfg(target_os = "linux")]
    {
        return linux::init();
    }
    #[cfg(target_os = "macos")]
    {
        return macos::init();
    }
    #[cfg(target_os = "windows")]
    {
        return windows_impl::init();
    }
    #[allow(unreachable_code)]
    Err(PlatformError::Unsupported {
        what: "this platform has no vernier backend",
    })
}
