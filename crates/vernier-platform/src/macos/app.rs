//! NSApplication bootstrap on the main thread, plus the dispatch
//! helpers used by every other macOS module to marshal AppKit
//! work onto main from the daemon worker thread.

use std::os::raw::c_int;
use std::sync::{Mutex, OnceLock};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate};

use super::install_main_state;
use crate::PlatformEvent;

/// `ProcessApplicationTransformState` — convert a non-UI process
/// into a foreground (`kProcessTransformToForegroundApplication`)
/// or accessory (`kProcessTransformToUIElementApplication`)
/// process. Required for unbundled binaries on Sequoia to register
/// status items reliably; calling `setActivationPolicy(.accessory)`
/// alone leaves the process classed as `kProcessNotAnApplication`
/// and the menu-bar process silently drops your item.
const K_PROCESS_TRANSFORM_TO_UI_ELEMENT: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessSerialNumber {
    high_long_of_psn: u32,
    low_long_of_psn: u32,
}

const K_CURRENT_PROCESS: u32 = 2;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn TransformProcessType(psn: *const ProcessSerialNumber, transform_type: u32) -> c_int;
    fn GetCurrentProcess(psn: *mut ProcessSerialNumber) -> c_int;
}

fn transform_to_ui_element() {
    let mut psn = ProcessSerialNumber {
        high_long_of_psn: 0,
        low_long_of_psn: K_CURRENT_PROCESS,
    };
    let status = unsafe { TransformProcessType(&psn, K_PROCESS_TRANSFORM_TO_UI_ELEMENT) };
    if status != 0 {
        log::warn!("macos: TransformProcessType returned {status}");
    } else {
        log::info!("macos: process transformed to UIElement");
    }
    let _ = unsafe { GetCurrentProcess(&mut psn) };
}

/// Bootstrap the AppKit main thread, spawn `daemon_body` as a
/// background worker, and run `NSApp.run()` forever.
///
/// MUST be called from the OS main thread — AppKit asserts on the
/// thread identity inside `NSApplication::sharedApplication`. The
/// only call site is `fn main()` in vernier-app.
pub fn bootstrap_main<F>(daemon_body: F) -> !
where
    F: FnOnce() + Send + 'static,
{
    let mtm = MainThreadMarker::new()
        .expect("vernier_platform::bootstrap_main must be called on the main thread");

    // Stash the main-thread state holder in TLS before anyone can
    // dispatch back to us.
    install_main_state();

    // Promote the process type BEFORE constructing NSApplication.
    // On Sequoia, status items created by a process classed as
    // `kProcessNotAnApplication` (the default for any binary
    // launched outside an .app bundle) are silently dropped by
    // the menu-bar agent — even with `setActivationPolicy`. The
    // documented fix is to call `TransformProcessType` with
    // `kProcessTransformToUIElementApplication`.
    transform_to_ui_element();

    // Mark NSApp as "accessory": no Dock icon, no menu bar, but
    // status items and windows still work. Matches every other
    // menu-bar utility.
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    // `finishLaunching` posts NSApplicationWillFinishLaunching /
    // DidFinishLaunching, which Sequoia uses as the trigger to
    // register status items with the menu bar process. Calling
    // it eagerly (instead of waiting for `run()` to do it) avoids
    // the race where a status item created from the worker thread
    // arrives at AppKit before NSApp has finished bringing up
    // its menu-bar plumbing.
    unsafe { app.finishLaunching() };
    log::info!("macos: NSApp finished launching, activation=Accessory");

    // Install our NSApplicationDelegate so the standard macOS
    // "double-click the .app while the daemon is running" gesture
    // surfaces the prefs window. LaunchServices sends the running
    // process a `kAEReopenApplication` Apple Event; NSApp routes
    // that through `applicationShouldHandleReopen:hasVisibleWindows:`
    // on its delegate. Without a delegate, the event is silently
    // dropped and `open /Applications/Vernier.app` is a no-op when
    // we're already running. The delegate forwards the event into
    // the same TrayMenuActivated channel the "Open Preferences"
    // tray menu item uses, so the existing main-loop handler does
    // the work (spawn-or-focus the prefs subprocess).
    let delegate = VernierAppDelegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    // NSApp keeps only a weak reference to its delegate, so we
    // have to hold a strong ref for the lifetime of the process.
    // Stash it next to NSAPP.
    APP_DELEGATE.set(SendableDelegate(delegate)).ok();

    // Park the NSApp handle for shutdown. Other modules look this
    // up to call `terminate:` from a menu click.
    NSAPP.set(SendableApp(app.clone())).ok();

    // Kick off the daemon body. It calls `vernier_platform::init`
    // and then drives the existing event loop. When the loop
    // returns, ask NSApp to stop so the process exits cleanly.
    std::thread::Builder::new()
        .name("vernier-daemon".into())
        .spawn(move || {
            daemon_body();
            // Daemon exited (Quit handler ran). Ask NSApp to stop;
            // sendEvent posts a no-op event so the loop wakes and
            // notices the stop flag.
            terminate_nsapp();
        })
        .expect("spawn vernier daemon worker thread");

    // Run forever. AppKit pumps events here. When `terminate_nsapp`
    // fires, `run` returns and we exit the process.
    unsafe { app.run() };
    std::process::exit(0)
}

/// Hold the shared NSApplication pointer in a Mutex so `terminate`
/// can reach it from off-main. Wrapped in a newtype that asserts
/// Send/Sync — `NSApplication` is documented as main-thread-only
/// for *most* methods, but `terminate:` and `stop:` are safe to
/// call cross-thread (they ultimately post a quit event).
struct SendableApp(objc2::rc::Retained<NSApplication>);
unsafe impl Send for SendableApp {}
unsafe impl Sync for SendableApp {}

static NSAPP: OnceLock<SendableApp> = OnceLock::new();

fn terminate_nsapp() {
    // Dispatch onto main and look up the NSApp there — Retained
    // pointers are main-thread-only and can't cross the closure
    // boundary as captures.
    run_on_main_async(|| {
        let Some(SendableApp(app)) = NSAPP.get() else {
            return;
        };
        unsafe { app.stop(None) };
        wake_main_event_loop();
    });
}

/// Post a no-op NSEvent so a `NSApp.run()` blocked on `nextEvent`
/// returns immediately and notices the `stop:` flag. Must run on
/// main; callers dispatch first.
fn wake_main_event_loop() {
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
    use objc2_foundation::NSPoint;

    let Some(SendableApp(app)) = NSAPP.get() else {
        return;
    };
    let _ = MainThreadMarker::new().expect("main");
    unsafe {
        let event = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint { x: 0.0, y: 0.0 },
            NSEventModifierFlags(0),
            0.0,
            0,
            None,
            0,
            0,
            0,
        );
        if let Some(event) = event {
            app.postEvent_atStart(&event, true);
        }
    }
}

/// Dispatch `f` to the main thread and block until it returns.
/// Safe to call from any thread, including main (in which case
/// libdispatch short-circuits to a direct call).
pub(crate) fn run_on_main_sync<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send,
    R: Send,
{
    // dispatch2 requires the closure to be Send and the queue to
    // outlive it; main_queue() returns a `&'static DispatchQueue`
    // so the lifetime works. We can't pass FnOnce that returns R
    // through the C API directly, so we shuttle the return value
    // through a Mutex<Option<R>>.
    let result: Mutex<Option<R>> = Mutex::new(None);
    DispatchQueue::main().exec_sync(|| {
        let r = f();
        *result.lock().expect("dispatch sync result lock") = Some(r);
    });
    result
        .into_inner()
        .expect("dispatch sync result mutex")
        .expect("dispatch sync closure did not produce a result")
}

/// Fire-and-forget dispatch to the main thread.
pub(crate) fn run_on_main_async<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    DispatchQueue::main().exec_async(f);
}

// --- NSApplicationDelegate ---------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "VernierAppDelegate"]
    pub(crate) struct VernierAppDelegate;

    unsafe impl NSObjectProtocol for VernierAppDelegate {}

    unsafe impl NSApplicationDelegate for VernierAppDelegate {
        /// Called when the user re-launches the running .app (Finder
        /// double-click, `open /Applications/Vernier.app`, Dock-icon
        /// click) — i.e. when LaunchServices sends the running
        /// process a `kAEReopenApplication` event. We always treat
        /// it as "open Preferences": the daemon has no main window,
        /// so any re-launch is the user asking for the prefs UI.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn application_should_handle_reopen(
            &self,
            _sender: &NSApplication,
            _has_visible_windows: bool,
        ) -> bool {
            if let Some(tx) = super::event_tx() {
                let _ = tx.send(PlatformEvent::TrayMenuActivated {
                    id: "open_prefs".to_string(),
                });
                log::info!("macos: reopen Apple Event → open_prefs");
            } else {
                log::warn!("macos: reopen Apple Event arrived before event channel ready");
            }
            // Returning true tells AppKit to perform its default
            // post-reopen handling (un-minimize a main window). We
            // have no main window, so the return value is cosmetic,
            // but `true` keeps us aligned with the AppKit contract.
            true
        }
    }
);

impl VernierAppDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![mtm.alloc::<Self>(), init] }
    }
}

/// Strong-ref holder for the delegate. NSApp's `setDelegate:` only
/// installs a weak reference, so the delegate would dealloc the
/// instant `bootstrap_main`'s local goes out of scope without this.
struct SendableDelegate(Retained<VernierAppDelegate>);
// Safety: VernierAppDelegate is a MainThreadOnly NSObject subclass.
// We never read/write it off-main; this storage exists purely to
// keep the retain count alive for the process lifetime.
unsafe impl Send for SendableDelegate {}
unsafe impl Sync for SendableDelegate {}
static APP_DELEGATE: OnceLock<SendableDelegate> = OnceLock::new();
