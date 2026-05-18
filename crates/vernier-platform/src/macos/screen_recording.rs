//! Screen Recording (TCC) authorization probe via ScreenCaptureKit.
//!
//! `SCShareableContent.getShareableContentWithCompletionHandler:` is
//! the only macOS API that reports Screen Recording authorization
//! truthfully. Every CoreGraphics alternative gives a false positive
//! when the grant is missing — verified on macOS 15 against a real
//! `tccutil reset ScreenCapture com.jondkinney.vernier`:
//!
//!   - `CGPreflightScreenCaptureAccess` returns `true`.
//!   - `CGWindowListCreateImage` / `CGDisplayCreateImageForRect`
//!     return a non-null *degraded* image (the desktop wallpaper plus
//!     the caller's own windows) instead of failing.
//!
//! ScreenCaptureKit instead invokes its completion handler with a nil
//! `SCShareableContent` and a non-nil `NSError` when the process is
//! not authorized — an unambiguous signal. The first unauthorized
//! call also makes macOS surface the Screen Recording prompt.
//!
//! The API is asynchronous: the completion handler runs on a private
//! queue. [`probe_screen_recording`] therefore returns a
//! `Receiver<bool>` the caller drains without blocking.

use std::sync::mpsc::{self, Receiver};

use block2::RcBlock;
use objc2_foundation::NSError;
use objc2_screen_capture_kit::SCShareableContent;

/// Start an asynchronous Screen Recording authorization probe.
///
/// Returns immediately. The `Receiver` yields exactly one `bool`:
/// `true` if this process is authorized to capture the screen,
/// `false` otherwise.
pub(crate) fn probe_screen_recording() -> Receiver<bool> {
    let (tx, rx) = mpsc::channel::<bool>();
    // ScreenCaptureKit invokes the handler exactly once, on a private
    // background queue. `RcBlock` needs an `Fn` closure; `Sender::send`
    // takes `&self`, so moving the `Sender` in and calling it is fine.
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // Authorized ⇔ SCK handed back content and no error.
            let authorized = !content.is_null() && error.is_null();
            // `send` errors only if the Receiver was already dropped
            // (prefs window closed before the probe landed) — harmless.
            let _ = tx.send(authorized);
        },
    );
    // SAFETY: `getShareableContentWithCompletionHandler:` copies
    // (retains) the completion block, so it stays valid after our
    // `RcBlock` drops at the end of this scope.
    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&handler);
    }
    rx
}
