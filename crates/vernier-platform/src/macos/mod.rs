//! macOS backend.
//!
//! Threading model: AppKit (NSApplication, NSWindow, NSStatusItem)
//! must run on the OS main thread. The daemon body, by contrast,
//! is the long-lived synchronous event loop in `vernier-app` and
//! we don't want to restructure it. So:
//!
//! * Callers invoke [`bootstrap_main`] from `fn main`. That sets
//!   up NSApp with `.accessory` activation policy (no Dock icon),
//!   stores per-process state in a main-thread TLS, spawns the
//!   daemon body on a worker thread, and calls `NSApp.run()` to
//!   drive the AppKit event loop.
//! * The daemon worker calls [`init`], which returns a [`Platform`]
//!   impl whose methods marshal their AppKit work onto the main
//!   thread via libdispatch.
//! * AppKit callbacks (tray clicks, hotkey presses, mouse / key
//!   events on the overlay) push [`PlatformEvent`]s into the same
//!   `mpsc::Sender` the worker is `recv`-ing from.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use crate::{
    Accelerator, AppIdentity, EventReceiver, Frame, HotkeyId, MonitorId, MonitorInfo, NativeFrame,
    OverlayHandle, Platform, PlatformError, PlatformEvent, Result, TrayHandle, TrayMenu,
};

mod app;
mod capture;
mod handoff_icon;
mod hotkey;
mod keymap;
mod monitor;
mod overlay;
mod screen_recording;
mod tray;

pub use app::bootstrap_main;
pub use handoff_icon::extract_macos_app_icon_rgba;
pub use monitor::primary_screen_visible_height;
pub(crate) use screen_recording::probe_screen_recording;

/// Main-thread-only registry. Owns every retained AppKit object
/// the backend creates. Accessed via [`with_main_state`] from
/// closures dispatched onto the main queue.
pub(crate) struct MainState {
    /// Sender into the daemon's [`PlatformEvent`] channel. Cloned
    /// into AppKit callbacks (menu actions, hotkey handler, etc.)
    /// so they can wake the worker thread when something happens.
    pub event_tx: Option<Sender<PlatformEvent>>,
    pub overlays: HashMap<MonitorId, overlay::OverlayResources>,
    pub tray: Option<tray::TrayResources>,
    pub hotkeys: HashMap<HotkeyId, hotkey::HotkeyResources>,
    /// Single shared Carbon event handler reference. Installed
    /// lazily on the first [`Platform::register_hotkey`] call;
    /// every per-hotkey entry shares it.
    pub carbon_handler_installed: bool,
}

impl MainState {
    fn new() -> Self {
        Self {
            event_tx: None,
            overlays: HashMap::new(),
            tray: None,
            hotkeys: HashMap::new(),
            carbon_handler_installed: false,
        }
    }
}

thread_local! {
    static MAIN_STATE_TLS: RefCell<Option<MainState>> = const { RefCell::new(None) };
}

/// Run `f` with mutable access to the main-thread state. Panics
/// if called off the main thread (the TLS is initialised by
/// [`bootstrap_main`] which only runs there).
pub(crate) fn with_main_state<R>(f: impl FnOnce(&mut MainState) -> R) -> R {
    MAIN_STATE_TLS.with(|cell| {
        let mut borrow = cell.borrow_mut();
        let state = borrow
            .as_mut()
            .expect("main-thread state not initialised; was bootstrap_main called?");
        f(state)
    })
}

/// Install the empty main-thread state. Called once by
/// [`bootstrap_main`] before NSApp starts.
pub(crate) fn install_main_state() {
    MAIN_STATE_TLS.with(|cell| {
        let mut borrow = cell.borrow_mut();
        if borrow.is_none() {
            *borrow = Some(MainState::new());
        }
    });
}

/// Global pointer to the daemon's [`PlatformEvent`] sender. Set
/// once by [`init`] (called from the worker) and read from main-
/// thread AppKit callbacks. We mirror it here (in addition to
/// `MainState::event_tx`) so callbacks that fire before the worker
/// has finished initialising — most notably stale Carbon hotkey
/// events on a fresh NSApp — can be dropped cleanly with a `None`
/// guard rather than panicking on a missing TLS.
static EVENT_TX: Mutex<Option<Sender<PlatformEvent>>> = Mutex::new(None);

pub(crate) fn event_tx() -> Option<Sender<PlatformEvent>> {
    EVENT_TX.lock().ok().and_then(|g| g.clone())
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn init() -> Result<(Box<dyn Platform>, EventReceiver)> {
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let mut guard = EVENT_TX
            .lock()
            .map_err(|e| PlatformError::Other(anyhow::anyhow!("EVENT_TX poisoned: {e}")))?;
        *guard = Some(tx.clone());
    }
    let tx_for_state = tx.clone();
    app::run_on_main_sync(move || {
        with_main_state(|s| {
            s.event_tx = Some(tx_for_state);
        });
    });
    Ok((Box::new(MacPlatform), rx))
}

struct MacPlatform;

impl Platform for MacPlatform {
    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        monitor::monitors()
    }

    fn focused_app(&self) -> Result<Option<AppIdentity>> {
        monitor::focused_app()
    }

    fn capture_screen(&self, monitor: MonitorId) -> Result<Frame> {
        capture::capture_screen(monitor)
    }

    fn capture_screen_native(&self, monitor: MonitorId) -> Result<NativeFrame> {
        capture::capture_screen_native(monitor)
    }

    fn register_hotkey(&self, accelerator: Accelerator, label: &str) -> Result<HotkeyId> {
        hotkey::register(accelerator, label)
    }

    fn unregister_hotkey(&self, id: HotkeyId) -> Result<()> {
        hotkey::unregister(id)
    }

    fn create_overlay(&self, monitor: MonitorId) -> Result<OverlayHandle> {
        overlay::create(monitor)
    }

    fn create_tray(&self, menu: TrayMenu) -> Result<TrayHandle> {
        tray::create(menu)
    }
}
