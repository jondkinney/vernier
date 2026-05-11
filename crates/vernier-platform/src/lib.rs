//! Per-OS backends behind a single [`Platform`] trait.
//!
//! The rest of the codebase imports this trait and the [`init`] constructor;
//! concrete OS modules are private and selected via cfg.

pub mod figma_bridge;
mod tray;
mod types;
pub use types::*;

/// Render the procedural app icon (purple/teal gradient with the
/// cross + T-caps + tick pill) as a non-premultiplied RGBA8 buffer
/// at `size × size`. Used by the daemon to drop a PNG on disk so
/// app launchers can show the same icon as the tray.
pub fn render_app_icon_rgba(size: u32) -> Vec<u8> {
    tray::render_app_icon_rgba(size)
}

/// Rasterize an SVG (`svg_bytes`) into a `size × size` RGBA8
/// non-premultiplied buffer, fitting the SVG into the square with
/// uniform scaling. Returns `None` if the SVG can't be parsed or
/// the pixmap can't be allocated. Used by the prefs window to
/// surface third-party app icons (e.g. Satty) loaded from
/// `/usr/share/icons/hicolor/.../apps/*.svg`.
pub fn rasterize_svg(svg_bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    // Use resvg's re-exported tiny-skia / usvg types directly —
    // resvg pins its own version and mixing 0.11 vs 0.12 fails to
    // compile.
    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(svg_bytes, &opts).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    let svg_size = tree.size();
    let svg_max = svg_size.width().max(svg_size.height()).max(1.0);
    let scale = size as f32 / svg_max;
    // Center the (uniformly scaled) SVG inside the square.
    let dx = (size as f32 - svg_size.width() * scale) * 0.5;
    let dy = (size as f32 - svg_size.height() * scale) * 0.5;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy),
        &mut pixmap.as_mut(),
    );
    // tiny-skia pixmaps are RGBA premultiplied; egui wants
    // unpremultiplied so it can apply alpha correctly. Demultiply
    // by dividing each channel by alpha (when alpha > 0).
    let mut out = pixmap.data().to_vec();
    for px in out.chunks_exact_mut(4) {
        let a = px[3];
        if a == 0 {
            continue;
        }
        let inv = 255.0 / a as f32;
        px[0] = ((px[0] as f32 * inv).round() as u32).min(255) as u8;
        px[1] = ((px[1] as f32 * inv).round() as u32).min(255) as u8;
        px[2] = ((px[2] as f32 * inv).round() as u32).min(255) as u8;
    }
    Some(out)
}

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
