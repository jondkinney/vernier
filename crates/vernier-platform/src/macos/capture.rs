//! Screen capture for macOS via `CGDisplayCreateImage`.
//!
//! Used by the measurement loop's "freeze frame" path — `toggle_measurement`
//! calls `capture_screen_native` on entry so the live edge detector has a
//! pixel buffer to scan. The buffer also drives the W×H pill's
//! snap-to-detected-edge behavior; without it, the daemon logs "press
//! 'r' once a frame is available" and the user gets cursor-tracking but
//! no edge snapping.
//!
//! ## Permission
//!
//! `CGDisplayCreateImage` requires the calling process to hold the
//! Screen Recording TCC grant (System Settings → Privacy & Security →
//! Screen & System Audio Recording). The first call from a process
//! without the grant returns `None` and macOS pushes a permission
//! prompt; subsequent calls return `None` until the user grants.
//!
//! ## Deprecation
//!
//! Apple marked `CGDisplayCreateImage` deprecated in macOS 14 in favor
//! of `ScreenCaptureKit`. The function still works through (at least)
//! macOS 15 / Sequoia, and ScreenCaptureKit's API surface is async +
//! callback-heavy which doesn't fit the synchronous "give me one frame
//! right now" semantics this caller wants. Migrate when the symbol
//! actually goes away.
//!
//! ## Pixel layout
//!
//! On Apple Silicon and modern Intel, the returned `CGImage` is 32-bit
//! per pixel with `kCGBitmapByteOrder32Little | kCGImageAlphaPremultipliedFirst`.
//! In memory that's `B, G, R, A` — `PixelFormat::Bgra8`. The image's
//! `bytesPerRow` is typically slightly larger than `width * 4` because
//! Core Graphics aligns rows to 16/64 bytes for vectorization. Pass that
//! stride along to [`NativeFrame`] consumers verbatim; only the packed
//! [`Frame`] path strips the padding (and swizzles to RGBA).

// TODO(macos-modernize): migrate CGDisplayCreateImage +
// CGWindowListCreateImage off CoreGraphics to ScreenCaptureKit. Their
// "use ScreenCaptureKit instead" deprecation is allowed here because
// ScreenCaptureKit's async/callback API doesn't fit the synchronous
// "give me one frame right now" semantics this caller wants. See the
// Deprecation section in the module-level docstring above.
// CGBitmapInfo::ByteOrder32Little is in the same boat — replaced by
// constants that ship with the new API.
#![allow(deprecated)]

use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGDataProvider, CGDirectDisplayID, CGDisplayCreateImage, CGImage,
    CGImageAlphaInfo, CGMainDisplayID, CGWindowImageOption, CGWindowListCreateImage,
    CGWindowListOption,
};

use crate::{Frame, MonitorId, NativeFrame, PixelFormat, PlatformError, Rect, Result};

pub(crate) fn capture_screen(monitor: MonitorId) -> Result<Frame> {
    let native = capture_screen_native(monitor)?;
    Ok(native_to_packed_rgba(native))
}

pub(crate) fn capture_screen_native(monitor: MonitorId) -> Result<NativeFrame> {
    // Capture *below* the overlay window when we know its ID, so the
    // captured frame doesn't include Vernier's own HUD (crosshair, W×H
    // pill, axis lines). Without the exclusion, the live edge detector
    // "sees" our crosshair strokes as edges and snaps to them — the
    // user ends up with stuck-on-cursor measurements like "7px / 12px"
    // that are actually measuring our HUD strokes, not the underlying
    // app.
    //
    // The exclusion uses `CGWindowListCreateImage` with
    // `kCGWindowListOptionOnScreenBelowWindow`. Falls back to
    // `CGDisplayCreateImage` if we can't look up the overlay (e.g. the
    // first frozen capture happens before the overlay's window is
    // created in some setups, or measure mode hasn't entered yet).
    let monitor_bounds = bounds_and_scale_for(monitor).0;
    let image: CFRetained<CGImage> = match overlay_window_id_for(monitor) {
        Some(wid) => {
            let bounds = CGRect {
                origin: CGPoint {
                    x: monitor_bounds.x as f64,
                    y: monitor_bounds.y as f64,
                },
                size: CGSize {
                    width: monitor_bounds.w as f64,
                    height: monitor_bounds.h as f64,
                },
            };
            CGWindowListCreateImage(
                bounds,
                CGWindowListOption::OptionOnScreenBelowWindow,
                wid,
                CGWindowImageOption::Default,
            )
            .ok_or(PlatformError::Unsupported {
                what: "macos: CGWindowListCreateImage returned null \
                       (Screen Recording permission may not be granted)",
            })?
        }
        None => {
            let display_id = cg_display_id_for(monitor);
            // The safe wrapper already converts the +1-retained
            // CGImageRef into `Option<CFRetained<CGImage>>` —
            // CFRetained::Drop calls CFRelease for us.
            CGDisplayCreateImage(display_id).ok_or(PlatformError::Unsupported {
                what: "macos: CGDisplayCreateImage returned null \
                       (Screen Recording permission may not be granted)",
            })?
        }
    };

    // CGImage::width / height / bytes_per_row / etc. are associated
    // functions (not &self methods) that take `Option<&CGImage>` —
    // pass `Some(&image)` and let CFRetained deref-coerce.
    let width = CGImage::width(Some(&image)) as u32;
    let height = CGImage::height(Some(&image)) as u32;
    let stride = CGImage::bytes_per_row(Some(&image)) as u32;
    let bitmap = CGImage::bitmap_info(Some(&image));
    let alpha = CGImage::alpha_info(Some(&image));
    let format = pixel_format_from_cg(bitmap, alpha);

    let provider = CGImage::data_provider(Some(&image)).ok_or(PlatformError::Unsupported {
        what: "macos: CGImage::data_provider returned null",
    })?;
    let data: CFRetained<objc2_core_foundation::CFData> = CGDataProvider::data(Some(&provider))
        .ok_or(PlatformError::Unsupported {
            what: "macos: CGDataProvider::data returned null",
        })?;
    let len = data.length() as usize;
    let ptr = data.byte_ptr();
    if ptr.is_null() {
        return Err(PlatformError::Unsupported {
            what: "macos: CFDataGetBytePtr returned null",
        });
    }
    let pixels = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();

    let (bounds, scale_factor) = bounds_and_scale_for(monitor);
    Ok(NativeFrame {
        width,
        height,
        stride,
        format,
        bounds,
        scale_factor,
        pixels,
    })
}

/// Map an `NSScreen`-indexed [`MonitorId`] to the matching
/// `CGDirectDisplayID` via `deviceDescription[@"NSScreenNumber"]`,
/// which is Apple's documented way to bridge the two namespaces.
/// Falls back to the main display when the lookup fails (e.g. the
/// display was disconnected between `monitors()` and now).
fn cg_display_id_for(monitor: MonitorId) -> CGDirectDisplayID {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSScreen;
    use objc2_foundation::NSString;

    let mtm = match MainThreadMarker::new() {
        Some(m) => m,
        None => return CGMainDisplayID(),
    };
    let screens = NSScreen::screens(mtm);
    let idx = monitor.0 as usize;
    let Some(screen) = screens.iter().nth(idx) else {
        return CGMainDisplayID();
    };
    let desc = screen.deviceDescription();
    let key = NSString::from_str("NSScreenNumber");
    let value = desc.objectForKey(&key);
    let Some(value) = value else {
        return CGMainDisplayID();
    };
    // The value is an NSNumber boxing a CGDirectDisplayID (u32).
    // Cast via the `unsignedIntValue` selector. Using msg_send! to
    // avoid pulling in NSNumber bindings just for one accessor.
    let id: u32 = unsafe { objc2::msg_send![&*value, unsignedIntValue] };
    if id == 0 { CGMainDisplayID() } else { id }
}

/// Resolve the overlay window's `CGWindowID` for `monitor`, if one
/// has been created. Returns `None` when no overlay exists yet
/// (early daemon startup, or before measure mode has ever been
/// entered). The lookup hops onto the AppKit main thread because
/// `MAIN_STATE_TLS` is main-thread-only — `run_on_main_sync` waits
/// for the queue, which is a tiny price compared to the
/// CGWindowListCreateImage call we're about to make.
///
/// `NSWindow.windowNumber()` returns `NSInteger`; CGWindow IDs are
/// `u32`. Apple guarantees the window number is positive for live
/// windows, so the cast is safe.
fn overlay_window_id_for(monitor: MonitorId) -> Option<u32> {
    super::app::run_on_main_sync(move || {
        super::with_main_state(|s| {
            s.overlays.get(&monitor).and_then(|o| {
                // `OnScreenBelowWindow` against an orderOut (hidden)
                // overlay captures only the desktop wallpaper. The
                // window id is a usable capture-exclusion reference
                // ONLY while the overlay is on screen; when it's
                // hidden, return None so the caller falls back to
                // CGDisplayCreateImage (full screen, all windows).
                o.window.isVisible().then(|| o.window.windowNumber() as u32)
            })
        })
    })
}

fn bounds_and_scale_for(monitor: MonitorId) -> (Rect, f64) {
    // Re-query monitors to avoid a circular reference (the Platform
    // impl owns its monitor cache; capture is a free function).
    let monitors = super::monitor::monitors().unwrap_or_default();
    monitors
        .into_iter()
        .find(|m| m.id == monitor)
        .map(|m| (m.bounds, m.scale_factor))
        .unwrap_or((Rect::default(), 1.0))
}

/// CGImage bitmap-info + alpha-info → our `PixelFormat`. The vast
/// majority of CGDisplayCreateImage outputs land in the `Bgra8` bucket;
/// the other branches are defensive against display configurations
/// that produce big-endian or alpha-last layouts.
fn pixel_format_from_cg(bitmap: CGBitmapInfo, alpha: CGImageAlphaInfo) -> PixelFormat {
    // ByteOrder32Little = bytes in memory are reversed from the
    // pixel-order constants. So "alpha-first" + little-endian =
    // BGRA in memory; "alpha-first" + big-endian = ARGB in memory.
    let little_endian =
        (bitmap.0 & CGBitmapInfo::ByteOrder32Little.0) == CGBitmapInfo::ByteOrder32Little.0;
    let alpha_first = matches!(
        alpha,
        CGImageAlphaInfo::PremultipliedFirst
            | CGImageAlphaInfo::First
            | CGImageAlphaInfo::NoneSkipFirst
    );
    let has_alpha = !matches!(
        alpha,
        CGImageAlphaInfo::NoneSkipFirst | CGImageAlphaInfo::NoneSkipLast
    );
    match (little_endian, alpha_first, has_alpha) {
        (true, true, true) => PixelFormat::Bgra8,
        (true, true, false) => PixelFormat::Bgrx8,
        (true, false, true) => PixelFormat::Rgba8,
        (true, false, false) => PixelFormat::Rgbx8,
        (false, true, true) => PixelFormat::Xrgb8,
        (false, true, false) => PixelFormat::Xrgb8,
        (false, false, true) => PixelFormat::Xbgr8,
        (false, false, false) => PixelFormat::Xbgr8,
    }
}

/// Repack a (possibly strided, native byte order) `NativeFrame` into
/// the zero-padding RGBA8 layout [`Frame`] requires. Handles
/// Bgra8/Bgrx8 (the common macOS case — swizzle B↔R) and Rgba8/Rgbx8
/// (already in the right order — just strip the row padding).
fn native_to_packed_rgba(native: NativeFrame) -> Frame {
    let w = native.width as usize;
    let h = native.height as usize;
    let stride = native.stride as usize;
    let row_used = w * 4;
    let mut out = Vec::with_capacity(w * h * 4);
    let swizzle = matches!(
        native.format,
        PixelFormat::Bgra8 | PixelFormat::Bgrx8 | PixelFormat::Xbgr8
    );
    let drop_alpha_skip_first = matches!(native.format, PixelFormat::Xrgb8);
    for row in 0..h {
        let row_start = row * stride;
        let row_end = row_start + row_used;
        if row_end > native.pixels.len() {
            break;
        }
        let src = &native.pixels[row_start..row_end];
        if swizzle {
            // B, G, R, A → R, G, B, A
            for chunk in src.chunks_exact(4) {
                out.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
            }
        } else if drop_alpha_skip_first {
            // A, R, G, B → R, G, B, A
            for chunk in src.chunks_exact(4) {
                out.extend_from_slice(&[chunk[1], chunk[2], chunk[3], chunk[0]]);
            }
        } else {
            out.extend_from_slice(src);
        }
    }
    Frame {
        width: native.width,
        height: native.height,
        scale_factor: native.scale_factor,
        bounds: native.bounds,
        pixels: out,
    }
}
