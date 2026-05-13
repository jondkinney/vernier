//! Display + frontmost-app discovery.

use crate::{AppIdentity, MonitorId, MonitorInfo, PlatformError, Rect, Result};

pub(crate) fn monitors() -> Result<Vec<MonitorInfo>> {
    super::app::run_on_main_sync(|| monitors_main_thread())
}

fn monitors_main_thread() -> Result<Vec<MonitorInfo>> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;
    let mtm = MainThreadMarker::new().expect("monitors_main_thread off-main");
    let screens = NSScreen::screens(mtm);
    let main_screen = NSScreen::mainScreen(mtm);
    let main_screen_ptr =
        main_screen.as_deref().map(|s| s as *const _ as *const () as usize);

    let mut out = Vec::with_capacity(screens.len());
    for (idx, screen) in screens.iter().enumerate() {
        let frame = screen.frame();
        let scale = screen.backingScaleFactor() as f64;
        // NSScreen::localizedName landed in macOS 10.15.
        let name = screen.localizedName().to_string();
        let screen_ptr = (&**screen) as *const _ as *const () as usize;
        let is_primary = main_screen_ptr.map(|mp| screen_ptr == mp).unwrap_or(idx == 0);
        out.push(MonitorInfo {
            // Stable per-session: NSScreen ordering is stable
            // until display configuration changes. `idx` works.
            id: MonitorId(idx as u64),
            name,
            bounds: Rect::new(
                frame.origin.x as i32,
                frame.origin.y as i32,
                frame.size.width as u32,
                frame.size.height as u32,
            ),
            scale_factor: scale,
            is_primary,
        });
    }
    Ok(out)
}

pub(crate) fn focused_app() -> Result<Option<AppIdentity>> {
    super::app::run_on_main_sync(|| focused_app_main_thread())
}

fn focused_app_main_thread() -> Result<Option<AppIdentity>> {
    use objc2_app_kit::NSWorkspace;
    let workspace = NSWorkspace::sharedWorkspace();
    let Some(app) = workspace.frontmostApplication() else {
        return Ok(None);
    };
    let bundle = app.bundleIdentifier().map(|s| s.to_string());
    let name = app.localizedName().map(|s| s.to_string()).unwrap_or_default();
    let exe = app
        .executableURL()
        .and_then(|url| url.path())
        .map(|s| s.to_string());
    Ok(Some(AppIdentity {
        id: bundle.unwrap_or_else(|| name.clone()),
        display_name: name,
        executable: exe,
    }))
}

/// Look up an `NSScreen` for the given [`MonitorId`]. Returns
/// `None` if the display has been disconnected since `monitors()`
/// was last called. Caller is on the main thread.
pub(crate) fn ns_screen_for(
    monitor: MonitorId,
) -> Option<objc2::rc::Retained<objc2_app_kit::NSScreen>> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;
    let mtm = MainThreadMarker::new()?;
    let screens = NSScreen::screens(mtm);
    let idx = monitor.0 as usize;
    if idx >= screens.len() {
        return None;
    }
    // NSArray indexed access returns a Retained for us.
    Some(unsafe { screens.objectAtIndex(idx) })
}

/// Find the [`MonitorId`] for the NSScreen at `idx`. Returns an
/// error so the trait method can propagate.
#[allow(dead_code)]
pub(crate) fn require_screen(
    monitor: MonitorId,
) -> Result<objc2::rc::Retained<objc2_app_kit::NSScreen>> {
    ns_screen_for(monitor).ok_or(PlatformError::MonitorNotFound(monitor))
}

/// Primary display's `visibleFrame.size.height` (logical points) —
/// already excludes the menu bar and Dock per AppKit. Returns `None`
/// when there's no main screen yet (very early startup, headless CI).
///
/// Used by the prefs window to choose an initial inner-size height
/// it knows will fit on the current display. Cheap: just queries
/// AppKit; no extra Cocoa machinery.
pub fn primary_screen_visible_height() -> Option<f32> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;
    let mtm = MainThreadMarker::new()?;
    let screen = NSScreen::mainScreen(mtm)?;
    Some(screen.visibleFrame().size.height as f32)
}
