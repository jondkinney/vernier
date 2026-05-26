//! Per-OS backends behind a single [`Platform`] trait.
//!
//! The rest of the codebase imports this trait and the [`init`] constructor;
//! concrete OS modules are private and selected via cfg.

pub mod figma_bridge;
pub mod font;
mod hud_render;
mod icon;
pub mod placement;
#[cfg(target_os = "linux")]
mod tray;
mod types;
pub use types::*;

/// Render the procedural app icon (purple/teal gradient with the
/// cross + T-caps + tick pill) as a non-premultiplied RGBA8 buffer
/// at `size × size`. Used by the daemon to drop a PNG on disk so
/// app launchers can show the same icon as the tray.
pub fn render_app_icon_rgba(size: u32) -> Vec<u8> {
    icon::render_app_icon_rgba(size)
}

/// The raw colored app-icon SVG, for writing into the `scalable/`
/// branch of an XDG hicolor icon theme. The bytes are embedded in
/// this crate (`include_bytes!`), so this works from a crates.io
/// install with no repo checkout present.
pub fn app_icon_svg() -> &'static [u8] {
    icon::app_icon_svg()
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

/// Decode a PNG (`png_bytes`) and rescale it into a `size × size`
/// RGBA8 non-premultiplied buffer suitable for `egui::ColorImage`.
/// Returns `None` if the PNG can't be parsed or the pixmap can't be
/// allocated. Used by the prefs window to surface macOS `.app` icons
/// after `sips` has rasterized them from the bundle's `.icns`.
///
/// Mirror of `rasterize_svg`'s output contract — both return
/// demultiplied RGBA so the caller can hand the bytes straight to
/// `ColorImage::from_rgba_unmultiplied`.
pub fn rasterize_png(png_bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let source = resvg::tiny_skia::Pixmap::decode_png(png_bytes).ok()?;
    let mut out_pixmap = resvg::tiny_skia::Pixmap::new(size, size)?;
    let src_w = source.width() as f32;
    let src_h = source.height() as f32;
    let src_max = src_w.max(src_h).max(1.0);
    let scale = size as f32 / src_max;
    let dx = (size as f32 - src_w * scale) * 0.5;
    let dy = (size as f32 - src_h * scale) * 0.5;
    let paint = resvg::tiny_skia::PixmapPaint {
        opacity: 1.0,
        blend_mode: resvg::tiny_skia::BlendMode::Source,
        // FilterNearest would alias the icon's edges; Bilinear is
        // the right speed/quality for icon downscales at this size.
        quality: resvg::tiny_skia::FilterQuality::Bilinear,
    };
    out_pixmap.draw_pixmap(
        0,
        0,
        source.as_ref(),
        &paint,
        resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy),
        None,
    );
    let mut out = out_pixmap.data().to_vec();
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
pub mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows_impl;

/// macOS-only: bootstrap the AppKit main thread, then spawn the
/// daemon body on a worker thread and run NSApp's event loop
/// forever. Never returns. See `macos::app::bootstrap_main` for
/// the threading model. Callers on non-macOS targets should
/// invoke their daemon body directly.
#[cfg(target_os = "macos")]
pub fn bootstrap_main<F>(daemon_body: F) -> !
where
    F: FnOnce() + Send + 'static,
{
    macos::bootstrap_main(daemon_body)
}

/// macOS-only: extract a `.app` bundle's icon as `size × size` RGBA8
/// non-premultiplied bytes. Routes through `NSWorkspace.iconForFile`
/// so it handles `.icns`, asset-catalog (`Assets.car`), and custom
/// icons uniformly. See `macos::handoff_icon` for details.
///
/// MUST be called on the main thread.
#[cfg(target_os = "macos")]
pub fn extract_macos_app_icon_rgba(bundle_path: &std::path::Path, size: u32) -> Option<Vec<u8>> {
    macos::extract_macos_app_icon_rgba(bundle_path, size)
}

/// macOS-only: primary display's visible (working) height in logical
/// points. Excludes the menu bar and the Dock, so callers sizing a
/// new window can ask for at most this height and trust the window
/// won't poke under either. Returns `None` if NSScreen has no main
/// display registered yet (rare — typical only at very early app
/// startup before AppKit has enumerated screens).
#[cfg(target_os = "macos")]
pub fn primary_screen_visible_height() -> Option<f32> {
    macos::primary_screen_visible_height()
}

/// macOS-only: declare *this* process a foreground (Dock-visible,
/// Cmd-Tab-listable) application AND make it active. Must be called
/// before AppKit initializes — i.e. before the first
/// `NSApplication::sharedApplication` invocation by the GUI framework
/// (eframe, winit, …). Used by the prefs subprocess at startup.
///
/// Background: when the daemon spawns the prefs subprocess via
/// `Command::spawn`, the new process inherits enough of the daemon's
/// state that Sequoia treats it as non-promotable — every
/// `setActivationPolicy(.Regular)` / `activate()` call from inside
/// the subprocess silently no-ops, and `NSRunningApplication
/// .activateWithOptions` from the daemon side also no-ops because
/// the target isn't eligible. Calling `TransformProcessType` with
/// `kProcessTransformToForegroundApplication` BEFORE AppKit comes up
/// breaks the inheritance: the process is reclassified at the
/// kernel level, so when AppKit / eframe initializes it sees a fresh
/// foreground app, the window appears in the Dock, and subsequent
/// activation calls work.
#[cfg(target_os = "macos")]
pub fn promote_to_foreground_application() {
    use std::os::raw::c_int;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcessSerialNumber {
        high: u32,
        low: u32,
    }
    const K_CURRENT_PROCESS: u32 = 2;
    const K_PROCESS_TRANSFORM_TO_FOREGROUND_APPLICATION: u32 = 1;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn TransformProcessType(psn: *const ProcessSerialNumber, transform_type: u32) -> c_int;
    }
    let psn = ProcessSerialNumber {
        high: 0,
        low: K_CURRENT_PROCESS,
    };
    let status =
        unsafe { TransformProcessType(&psn, K_PROCESS_TRANSFORM_TO_FOREGROUND_APPLICATION) };
    if status != 0 {
        log::warn!("promote_to_foreground_application: TransformProcessType returned {status}");
        return;
    }
    log::info!("macos: promoted process to ForegroundApplication");

    // The transform alone classifies the process as foreground, but
    // doesn't bring its window to the front — there's no window yet
    // when this function runs (the GUI framework hasn't initialized
    // AppKit). Schedule the actual activate for ~400 ms after
    // startup, by which time eframe / winit will have created the
    // NSWindow and NSRunningApplication.activateWithOptions has
    // something to surface. Without this delay every activate call
    // during AppKit startup is a no-op against a not-yet-existent
    // window, and the prefs window opens behind every other app
    // until the user manually clicks it.
    std::thread::Builder::new()
        .name("vernier-foreground-activate".into())
        .spawn(|| {
            use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
            std::thread::sleep(std::time::Duration::from_millis(400));
            let running = NSRunningApplication::currentApplication();
            running.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
            log::info!("macos: deferred activate fired (NSRunningApplication.activateWithOptions)");
        })
        .expect("spawn vernier-foreground-activate thread");
}

/// macOS-only: bring the app owning `pid` to the foreground. Routes
/// through `NSRunningApplication.activateWithOptions(.AllWindows)`
/// so every window the prefs process owns gets raised, regardless
/// of which one was last key. Silently no-ops when there's no
/// running application for the pid (process exited between the
/// daemon's `try_wait()` check and this call — race window is tiny
/// but possible).
#[cfg(target_os = "macos")]
pub fn focus_macos_app_by_pid(pid: i32) {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        log::debug!("focus_macos_app_by_pid: no NSRunningApplication for pid {pid}");
        return;
    };
    app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
}

/// Start an asynchronous probe of whether *this* process is authorized
/// to capture the screen — the macOS Screen Recording ("Screen &
/// System Audio Recording") grant.
///
/// Returns immediately with a `Receiver` that yields exactly one
/// `bool`. On macOS the probe goes through ScreenCaptureKit's
/// `SCShareableContent` (see `macos::screen_recording`) — the only API
/// that reports the grant truthfully. `CGPreflightScreenCaptureAccess`
/// and the CoreGraphics capture calls all return false positives when
/// the grant is missing (verified on macOS 15 against a real `tccutil
/// reset ScreenCapture`). Because `SCShareableContent` is asynchronous,
/// the result lands on the channel a short time after this call.
///
/// On every other platform there is no equivalent permission — capture
/// works unconditionally — so the channel immediately yields `true`,
/// and callers treat that as "nothing to warn about".
///
/// Without the grant `CGDisplayCreateImage` / `CGWindowListCreateImage`
/// hand back degraded frames and edge detection, the freeze-screen
/// background, and screenshots all stop working — hence the prefs
/// banner this drives. The grant is keyed on the app's code signature,
/// so an ad-hoc rebuild changes the cdhash and can silently invalidate
/// a previously-granted permission — exactly when the banner earns its
/// keep.
pub fn probe_screen_recording() -> std::sync::mpsc::Receiver<bool> {
    #[cfg(target_os = "macos")]
    {
        macos::probe_screen_recording()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = tx.send(true);
        rx
    }
}

/// Open System Settings at Privacy & Security → Screen & System Audio
/// Recording, so the user can grant Vernier the capture permission.
/// macOS-only; a no-op elsewhere (the banner that calls this never
/// shows on other platforms because [`screen_recording_authorized`]
/// is always `true` there).
pub fn open_screen_recording_settings() {
    #[cfg(target_os = "macos")]
    {
        // `open` resolves the `x-apple.systempreferences:` scheme to
        // System Settings and deep-links to the named privacy anchor.
        // `Privacy_ScreenCapture` is the Screen & System Audio
        // Recording pane.
        if let Err(e) = std::process::Command::new("open")
            .arg(
                "x-apple.systempreferences:com.apple.preference.security\
                 ?Privacy_ScreenCapture",
            )
            .spawn()
        {
            log::warn!("open_screen_recording_settings: spawn `open` failed: {e}");
        }
    }
}

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
    /// Paint `frame` as the overlay's opaque background, beneath any
    /// HUD strokes. Used by measure mode's "freeze screen" feature
    /// so anything moving underneath the overlay (browser scroll,
    /// playing video) doesn't visually change while the user
    /// measures — the user sees the exact pixels Vernier captured
    /// for edge detection. Pass `None` to clear, restoring the
    /// transparent-with-tint background.
    ///
    /// Default impl is a no-op so backends can opt in incrementally.
    /// Without an implementation, the overlay stays transparent and
    /// the user keeps seeing live content (the pre-existing
    /// behavior), which is functionally fine — edge detection works
    /// against the frozen frame either way.
    fn set_background_frame(&mut self, frame: Option<Frame>) {
        let _ = frame;
    }
    /// Show or hide the system pointer over this overlay. `true` →
    /// the compositor draws its theme cursor (via wp_cursor_shape v1).
    /// `false` → the cursor is hidden so we can draw our own custom
    /// crosshair / move / resize cursor instead.
    fn set_system_pointer_visible(&mut self, visible: bool);
    /// Swap the visible system pointer between the default arrow and
    /// the pointing-hand cursor used to indicate a clickable element
    /// (e.g. the camera-icon pill on a held rect). Has no visible
    /// effect while the pointer is hidden — backends should latch the
    /// requested kind and apply it the next time the pointer is
    /// shown. Default impl is a no-op so non-macOS backends opt in
    /// at their own pace.
    fn set_pointing_hand_cursor(&mut self, pointing: bool) {
        let _ = pointing;
    }
    /// Confine the system pointer to a (x, y, w, h) rectangle in
    /// surface-local (logical) px, via `wp_pointer_constraints_v1`.
    /// Used to physically clamp the cursor while a stuck-pill drag
    /// is in progress so it can't run past the offset bound.
    fn confine_pointer(&mut self, x: i32, y: i32, w: i32, h: i32);
    /// Tear down whatever confinement is active for this overlay,
    /// freeing the cursor.
    fn release_pointer_confine(&mut self);
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
    pub fn set_background_frame(&mut self, frame: Option<Frame>) {
        self.inner.set_background_frame(frame)
    }
    pub fn set_system_pointer_visible(&mut self, visible: bool) {
        self.inner.set_system_pointer_visible(visible)
    }
    pub fn set_pointing_hand_cursor(&mut self, pointing: bool) {
        self.inner.set_pointing_hand_cursor(pointing)
    }
    pub fn confine_pointer(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.inner.confine_pointer(x, y, w, h)
    }
    pub fn release_pointer_confine(&mut self) {
        self.inner.release_pointer_confine()
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

#[cfg(all(test, target_os = "macos"))]
mod png_test {
    use super::*;
    #[test]
    fn rasterize_macos_cached_icon() {
        let path = "/Users/jon/Library/Caches/vernier/handoff-icons/CleanShot_X.png";
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("(skip — cache PNG not present)");
            return;
        };
        let rgba = rasterize_png(&bytes, 128).expect("rasterize_png Some");
        assert_eq!(rgba.len(), 128 * 128 * 4);
        // count non-zero alpha pixels — icon should not be fully blank
        let nonzero = rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
        eprintln!("nonzero alpha pixels: {nonzero}");
        assert!(nonzero > 1000);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod icon_extract_test {
    use super::*;
    #[test]
    fn extract_each_installed_app() {
        for app in &[
            "/Applications/Setapp/CleanShot X.app",
            "/Applications/Shottr.app",
            "/System/Applications/Preview.app",
        ] {
            let result = extract_macos_app_icon_rgba(std::path::Path::new(app), 128);
            eprintln!(
                "{app}: {}",
                match &result {
                    Some(b) => format!(
                        "Some({} bytes, {} non-transparent)",
                        b.len(),
                        b.chunks_exact(4).filter(|p| p[3] > 0).count()
                    ),
                    None => "None".into(),
                }
            );
        }
    }
}
