//! Extract a macOS `.app` bundle's icon as RGBA bytes, suitable for
//! handing to egui's `ColorImage::from_rgba_unmultiplied`.
//!
//! Uses [`NSWorkspace.iconForFile`] — Apple's canonical icon
//! resolver — which handles every bundle storage format uniformly:
//!
//! * Standalone `.icns` (`Contents/Resources/AppIcon.icns`) — common
//!   for older / non-Xcode bundles. CleanShot X, Preview.
//! * Compiled asset catalogs (`Assets.car`) — common for modern
//!   Xcode-built apps that ship an `AppIcon.appiconset`. Shottr.
//! * Custom Finder icons via `com.apple.ResourceFork` metadata.
//! * Generic / placeholder icons when the bundle has no asset.
//!
//! Going through NSWorkspace means we don't have to parse Info.plist,
//! decompile asset catalogs, or guess at filenames — the same code
//! path works for every bundle the user might pick.

use std::path::Path;

use objc2::AnyThread;
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageRep, NSCalibratedRGBColorSpace, NSCompositingOperation,
    NSGraphicsContext, NSImage, NSWorkspace,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

/// Return `size × size` RGBA8 non-premultiplied bytes for the icon of
/// the `.app` bundle at `bundle_path`. The byte layout matches what
/// `egui::ColorImage::from_rgba_unmultiplied` expects (4 bytes per
/// pixel: R, G, B, A).
///
/// Returns `None` when:
/// * The bundle doesn't exist or NSWorkspace can't resolve an icon.
/// * The graphics context required for rendering can't be created
///   (typically because we're not on the main thread, which AppKit
///   requires for NSImage drawing).
/// * The pixel buffer's stride doesn't match what we asked for (very
///   defensive — NSBitmapImageRep can over-align rows on some
///   ancient Macs, though I've never seen it happen on Apple Silicon
///   or recent Intel hardware).
///
/// Recommended to call on the main thread (AppKit's drawing APIs
/// formally require it), though `NSWorkspace.iconForFile` itself is
/// thread-safe and the NSImage `drawInRect:` path used here works
/// from background threads in practice for off-screen bitmap targets.
pub fn extract_macos_app_icon_rgba(bundle_path: &Path, size: u32) -> Option<Vec<u8>> {
    let workspace = NSWorkspace::sharedWorkspace();
    let path_str = bundle_path.to_string_lossy();
    let ns_path = NSString::from_str(&path_str);
    let icon: objc2::rc::Retained<NSImage> = workspace.iconForFile(&ns_path);
    // NSImage.size defaults to the icon's natural rep size; force
    // our target so drawInRect rasterizes at the size we want
    // instead of scaling whatever the bitmap representation happens
    // to be.
    let target = NSSize {
        width: size as f64,
        height: size as f64,
    };
    icon.setSize(target);

    // RGBA8 non-premultiplied bitmap. Bytes-per-row = width * 4 (no
    // alignment padding), bits-per-pixel = 32, hasAlpha = true. The
    // `NSCalibratedRGBColorSpace` constant is the device-independent
    // RGB space; `NSDeviceRGBColorSpace` works too but pulls the
    // display's gamut, which for a static icon thumbnail is a
    // distinction without practical difference.
    // NSGraphicsContext only accepts premultiplied-alpha bitmaps as
    // drawing destinations — passing `AlphaNonpremultiplied` makes
    // `graphicsContextWithBitmapImageRep` return nil. Use the default
    // (premultiplied) format here and demultiply after the read so the
    // caller still gets unpremultiplied bytes (egui's convention).
    let bitmap = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            size as isize,
            size as isize,
            8,
            4,
            true,
            false,
            NSCalibratedRGBColorSpace,
            NSBitmapFormat(0),
            (size * 4) as isize,
            32,
        )?
    };
    let ctx = NSGraphicsContext::graphicsContextWithBitmapImageRep(&bitmap)?;
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&ctx));
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: target,
    };
    // `drawInRect:fromRect:operation:fraction:` with
    // `NSCompositingOperationCopy` and fraction=1.0 paints the icon
    // opaquely over whatever's underneath. fromRect=NSZeroRect tells
    // NSImage to use its full source rect. The bitmap's
    // initialization didn't zero its backing store, but Copy
    // overwrites every pixel so we don't need an explicit clear.
    let zero = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: 0.0,
            height: 0.0,
        },
    };
    icon.drawInRect_fromRect_operation_fraction(rect, zero, NSCompositingOperation::Copy, 1.0);
    // Force pending Core Graphics commands to flush before reading
    // pixels. Without this, on some macOS versions the bitmapData
    // pointer can momentarily return the pre-draw pixels.
    ctx.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();

    let row_bytes = bitmap.bytesPerRow() as usize;
    let expected_row = (size as usize) * 4;
    if row_bytes != expected_row {
        return None;
    }
    let total = expected_row * (size as usize);
    let data_ptr = bitmap.bitmapData();
    if data_ptr.is_null() {
        return None;
    }
    let mut bytes = unsafe { std::slice::from_raw_parts(data_ptr, total) }.to_vec();
    // Bitmap is premultiplied (drawing-context requirement);
    // demultiply for egui's `from_rgba_unmultiplied`.
    for px in bytes.chunks_exact_mut(4) {
        let a = px[3];
        if a == 0 {
            continue;
        }
        let inv = 255.0 / a as f32;
        px[0] = ((px[0] as f32 * inv).round() as u32).min(255) as u8;
        px[1] = ((px[1] as f32 * inv).round() as u32).min(255) as u8;
        px[2] = ((px[2] as f32 * inv).round() as u32).min(255) as u8;
    }
    Some(bytes)
}
