//! Wayland backend.
//!
//! Targets `wlr-layer-shell` compositors (Hyprland, Sway, KDE Plasma 6,
//! river, Cosmic). GNOME-on-Wayland falls back to a regular fullscreen
//! `xdg-toplevel` via winit, handled elsewhere.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_pointer_constraints, delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    reexports::protocols::wp::{
        cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
        pointer_constraints::zv1::client::{
            zwp_confined_pointer_v1::ZwpConfinedPointerV1,
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::Lifetime as PointerLifetime,
        },
    },
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager},
        pointer_constraints::{PointerConstraintsHandler, PointerConstraintsState},
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_callback, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

use crate::{
    Accelerator, AppIdentity, Color, EventReceiver, EventSender, Frame, HotkeyId, Hud, HudAxis,
    HudKind, MonitorId, MonitorInfo, NativeFrame, OverlayHandle, OverlayOps, PixelFormat, Platform,
    PlatformError, PlatformEvent, Rect, Result, TrayHandle, TrayMenu,
};

pub(crate) fn init() -> Result<(Box<dyn Platform>, EventReceiver)> {
    let (events_tx, events_rx) = std::sync::mpsc::channel::<PlatformEvent>();
    let (cmd_tx, cmd_rx) = calloop::channel::channel::<Cmd>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);

    let monitors = Arc::new(Mutex::new(Vec::<MonitorInfo>::new()));
    let monitors_thread = monitors.clone();
    let cmd_tx_for_thread = cmd_tx.clone();
    let events_tx_for_thread = events_tx.clone();

    thread::Builder::new()
        .name("vernier-wayland".into())
        .spawn(move || {
            let result = run_event_loop(
                cmd_rx,
                cmd_tx_for_thread,
                events_tx_for_thread,
                monitors_thread,
                ready_tx.clone(),
            );
            if let Err(e) = result {
                log::error!(
                    "wayland event loop terminated: {e:#}. Overlay is now dead — restart the daemon."
                );
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("spawn wayland thread: {e}")))?;

    ready_rx
        .recv()
        .map_err(|_| {
            PlatformError::Other(anyhow::anyhow!("wayland event loop failed before ready"))
        })??;

    let hotkey_service = match super::hotkey::create(events_tx.clone()) {
        Ok(s) => {
            log::info!("global shortcuts portal connected");
            Some(s)
        }
        Err(e) => {
            log::warn!(
                "global shortcuts portal unavailable: {e}. \
                 hotkey toggle will only work via the CLI fallback (`vernier toggle`)."
            );
            None
        }
    };

    // Kick off the screencast portal handshake + PipeWire connect on a
    // background thread so the user-consent dialog (only on first run)
    // doesn't block daemon startup.
    let screencast_session: Arc<Mutex<Option<super::screencast::CaptureService>>> =
        Arc::new(Mutex::new(None));
    let sc_clone = screencast_session.clone();
    thread::Builder::new()
        .name("vernier-screencast-init".into())
        .spawn(move || {
            let state = match super::screencast::open_session_blocking() {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("screencast: portal handshake failed: {e}");
                    return;
                }
            };
            use std::os::fd::AsRawFd;
            log::info!(
                "screencast: portal session ready — {} stream(s); pipewire fd={}",
                state.streams.len(),
                state.pipewire_fd.as_raw_fd()
            );
            for s in &state.streams {
                log::info!(
                    "  stream node_id={} pos={:?} size={:?} id={:?}",
                    s.node_id, s.position, s.size, s.stream_id
                );
            }
            match super::screencast::start_capture(state) {
                Ok(svc) => {
                    log::info!("screencast: pipewire capture service running");
                    *sc_clone.lock().unwrap() = Some(svc);
                }
                Err(e) => log::warn!("screencast: pipewire start failed: {e}"),
            }
        })
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("spawn screencast thread: {e}")))?;

    Ok((
        Box::new(WaylandPlatform {
            cmd_tx: Mutex::new(cmd_tx),
            monitors,
            events_tx,
            hotkey_service,
            screencast_session,
        }),
        events_rx,
    ))
}

// =========================================================================
// Public Platform impl
// =========================================================================

struct WaylandPlatform {
    cmd_tx: Mutex<calloop::channel::Sender<Cmd>>,
    monitors: Arc<Mutex<Vec<MonitorInfo>>>,
    events_tx: EventSender,
    hotkey_service: Option<super::hotkey::HotkeyService>,
    #[allow(dead_code)] // capture_screen wires this up in milestone 2 task 10
    screencast_session: Arc<Mutex<Option<super::screencast::CaptureService>>>,
}

impl WaylandPlatform {
    fn send(&self, cmd: Cmd) -> Result<()> {
        self.cmd_tx
            .lock()
            .unwrap()
            .send(cmd)
            .map_err(|e| PlatformError::Other(anyhow::anyhow!("event loop send: {e}")))
    }
}

impl Platform for WaylandPlatform {
    fn monitors(&self) -> Result<Vec<MonitorInfo>> {
        Ok(self.monitors.lock().unwrap().clone())
    }

    fn focused_app(&self) -> Result<Option<AppIdentity>> {
        Ok(None)
    }

    fn capture_screen_native(&self, monitor: MonitorId) -> Result<NativeFrame> {
        let guard = self.screencast_session.lock().unwrap();
        let svc = guard.as_ref().ok_or_else(|| {
            PlatformError::Other(anyhow::anyhow!("screencast not ready yet"))
        })?;
        let stream_info = svc.streams().first().ok_or_else(|| {
            PlatformError::Other(anyhow::anyhow!("screencast has no streams"))
        })?;
        let captured = svc.latest_frame(stream_info.node_id).ok_or_else(|| {
            PlatformError::Other(anyhow::anyhow!(
                "no frame captured yet — try again in a moment"
            ))
        })?;
        let monitor_info = self.monitors.lock().unwrap().iter().find(|m| m.id == monitor).cloned();
        let (bounds, scale_factor) = monitor_info
            .map(|m| (m.bounds, m.scale_factor))
            .unwrap_or((Rect::default(), 1.0));
        let format = video_format_to_pixel_format(captured.format)?;
        Ok(NativeFrame {
            width: captured.width,
            height: captured.height,
            stride: captured.stride,
            format,
            bounds,
            scale_factor,
            pixels: captured.pixels,
        })
    }

    fn capture_screen(&self, monitor: MonitorId) -> Result<Frame> {
        let guard = self.screencast_session.lock().unwrap();
        let svc = guard.as_ref().ok_or_else(|| PlatformError::Other(
            anyhow::anyhow!("screencast not ready yet — portal handshake or PipeWire connect still in flight"),
        ))?;
        // First-stream mapping: portal-side stream order matches the monitor
        // order the user picked in the consent dialog. Multi-monitor proper
        // mapping is a milestone-3 refinement.
        let stream_info = svc.streams().first().ok_or_else(|| {
            PlatformError::Other(anyhow::anyhow!("screencast has no streams"))
        })?;
        let captured = svc.latest_frame(stream_info.node_id).ok_or_else(|| {
            PlatformError::Other(anyhow::anyhow!(
                "no frame captured yet — try again in a moment"
            ))
        })?;
        let pixels = to_rgba8(
            &captured.pixels,
            captured.stride,
            captured.width,
            captured.height,
            captured.format,
        );
        let monitor_info = self.monitors.lock().unwrap().iter().find(|m| m.id == monitor).cloned();
        let (bounds, scale_factor) = monitor_info
            .map(|m| (m.bounds, m.scale_factor))
            .unwrap_or((Rect::default(), 1.0));
        Ok(Frame {
            width: captured.width,
            height: captured.height,
            scale_factor,
            bounds,
            pixels,
        })
    }

    fn register_hotkey(&self, accelerator: Accelerator, label: &str) -> Result<HotkeyId> {
        match &self.hotkey_service {
            Some(s) => s.register(accelerator, label),
            None => Err(PlatformError::Portal {
                reason: "GlobalShortcuts portal unavailable on this system".into(),
            }),
        }
    }

    fn unregister_hotkey(&self, id: HotkeyId) -> Result<()> {
        match &self.hotkey_service {
            Some(s) => s.unregister(id),
            None => Ok(()),
        }
    }

    fn create_overlay(&self, monitor: MonitorId) -> Result<OverlayHandle> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<Result<WaylandOverlay>>(1);
        self.send(Cmd::CreateOverlay {
            monitor,
            reply: reply_tx,
        })?;
        let backend = reply_rx
            .recv()
            .map_err(|_| PlatformError::Other(anyhow::anyhow!("create_overlay reply lost")))??;
        Ok(OverlayHandle::from_backend(backend))
    }

    fn create_tray(&self, menu: TrayMenu) -> Result<TrayHandle> {
        crate::tray::create(menu, self.events_tx.clone())
    }
}

// =========================================================================
// Overlay handle
// =========================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct OverlayKey(u64);

#[derive(Debug)]
enum Cmd {
    CreateOverlay {
        monitor: MonitorId,
        reply: std::sync::mpsc::SyncSender<Result<WaylandOverlay>>,
    },
    OverlayShow(OverlayKey),
    OverlayHide(OverlayKey),
    OverlaySetTint(OverlayKey, Color),
    OverlaySetInputCapturing(OverlayKey, bool),
    OverlaySetHud(OverlayKey, Option<Hud>),
    OverlayDestroy(OverlayKey),
    /// Toggle the system pointer cursor on top of the overlay. When
    /// hidden the surface keeps the empty system cursor (we draw our
    /// own crosshair / move / resize); when default we delegate to
    /// the compositor's `wp_cursor_shape_v1` so the user sees their
    /// actual theme arrow.
    OverlaySetSystemPointer(OverlayKey, SystemPointerKind),
    /// Confine the pointer to a logical-px rectangle inside this
    /// overlay's surface. The (x, y, w, h) is in surface-local
    /// (logical) px. Used while dragging a stuck-measurement pill
    /// so the cursor physically stops at the drag bound.
    OverlayConfinePointer(OverlayKey, i32, i32, i32, i32),
    /// Tear down any active pointer confinement for this overlay.
    OverlayReleasePointerConfine(OverlayKey),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemPointerKind {
    /// Daemon intent: don't show a system arrow. On Wayland this
    /// maps to the blank cursor surface so `set_cursor(None)`'s
    /// fallback to the compositor's default cursor doesn't leak
    /// (an I-beam or arrow that the screencast portal might not
    /// strip).
    Hidden,
    /// Daemon wants the compositor's default arrow (e.g. over a
    /// clickable pill or while the context menu is open).
    Default,
    /// Vernier's measurement `+` cursor painted as the OS pointer
    /// (via `wl_pointer.set_cursor`) so the screencast portal's
    /// `CursorMode::Hidden` strips it cleanly. Hyprland's portal
    /// honours this for small cursor surfaces. Selected by
    /// [`WaylandState::resolve_pointer_kind`] when the daemon's
    /// `Hidden` intent meets `hud.show_cursor == true`.
    MeasurementCross,
}

/// Pre-rendered cursor surface. Built once at init; reused for
/// every `pointer.set_cursor` call. `_buffer` holds the SCTK slot
/// alive so the compositor can re-read the same pixels.
struct CursorSurface {
    surface: wl_surface::WlSurface,
    _buffer: smithay_client_toolkit::shm::slot::Buffer,
    hotspot_x: i32,
    hotspot_y: i32,
}

struct WaylandOverlay {
    key: OverlayKey,
    monitor: MonitorId,
    cmd_tx: calloop::channel::Sender<Cmd>,
    visible: Arc<AtomicBool>,
}

impl OverlayOps for WaylandOverlay {
    fn show(&mut self) {
        self.visible.store(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(Cmd::OverlayShow(self.key));
    }
    fn hide(&mut self) {
        self.visible.store(false, Ordering::Relaxed);
        let _ = self.cmd_tx.send(Cmd::OverlayHide(self.key));
    }
    fn toggle(&mut self) {
        let was = self.visible.fetch_xor(true, Ordering::Relaxed);
        let _ = self.cmd_tx.send(if was {
            Cmd::OverlayHide(self.key)
        } else {
            Cmd::OverlayShow(self.key)
        });
    }
    fn is_visible(&self) -> bool {
        self.visible.load(Ordering::Relaxed)
    }
    fn monitor(&self) -> MonitorId {
        self.monitor
    }
    fn set_tint(&mut self, c: Color) {
        let _ = self.cmd_tx.send(Cmd::OverlaySetTint(self.key, c));
    }
    fn set_input_capturing(&mut self, capturing: bool) {
        let _ = self
            .cmd_tx
            .send(Cmd::OverlaySetInputCapturing(self.key, capturing));
    }
    fn set_hud(&mut self, hud: Option<Hud>) {
        let _ = self.cmd_tx.send(Cmd::OverlaySetHud(self.key, hud));
    }
    fn set_system_pointer_visible(&mut self, visible: bool) {
        let kind = if visible {
            SystemPointerKind::Default
        } else {
            SystemPointerKind::Hidden
        };
        let _ = self.cmd_tx.send(Cmd::OverlaySetSystemPointer(self.key, kind));
    }
    fn confine_pointer(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let _ = self
            .cmd_tx
            .send(Cmd::OverlayConfinePointer(self.key, x, y, w, h));
    }
    fn release_pointer_confine(&mut self) {
        let _ = self
            .cmd_tx
            .send(Cmd::OverlayReleasePointerConfine(self.key));
    }
}

impl Drop for WaylandOverlay {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::OverlayDestroy(self.key));
    }
}

// =========================================================================
// Event loop state
// =========================================================================

struct WaylandState {
    registry: RegistryState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    qh: QueueHandle<WaylandState>,
    /// Empty `wl_region` used as the surface input region while the overlay
    /// is passive — clicks fall through to underlying windows.
    empty_region: Region,
    seat_state: SeatState,
    /// Live pointers, one per seat with the Pointer capability. Held to
    /// keep them alive; we don't otherwise read this list.
    pointers: Vec<wl_pointer::WlPointer>,
    /// Live keyboards, similarly held alive.
    keyboards: Vec<wl_keyboard::WlKeyboard>,
    /// Calloop loop handle, populated once the event loop is built
    /// in `init_wayland_thread`. Required for
    /// `seat_state.get_keyboard_with_repeat`, which delivers
    /// auto-repeat events while a key is held — without it,
    /// holding e.g. an arrow key only fires once.
    loop_handle: Option<calloop::LoopHandle<'static, WaylandState>>,
    /// `wp_cursor_shape_manager_v1` global if the compositor advertises
    /// it (Hyprland does). Used to display the user's actual theme
    /// pointer instead of a hand-drawn arrow.
    cursor_shape_manager: Option<CursorShapeManager>,
    /// `zwp_pointer_constraints_v1` global if the compositor offers
    /// it. We use `confine_pointer` to physically bound the cursor
    /// while a stuck-measurement pill is being dragged so the user
    /// can't overshoot the 50px clamp.
    pointer_constraints: PointerConstraintsState,
    /// Currently-active pointer confinement, if any. Lifetime
    /// `Persistent` until we explicitly destroy it on drag release.
    active_confined_pointer: Option<ZwpConfinedPointerV1>,
    /// Per-pointer cursor-shape device, lazily created on first Enter.
    pointer_shape_devices: HashMap<wayland_client::backend::ObjectId, WpCursorShapeDeviceV1>,
    /// Most recent pointer + Enter serial seen across any overlay —
    /// needed so commands that arrive AFTER an Enter (e.g. main asks
    /// for "show default arrow now") can apply the cursor change.
    last_pointer_enter: Option<(wl_pointer::WlPointer, u32)>,
    /// Per-overlay desired system-pointer state. Updated by main.rs;
    /// applied to the latest pointer Enter serial.
    overlay_pointer_kind: HashMap<OverlayKey, SystemPointerKind>,
    /// Per-overlay `hud.show_cursor` snapshot used by
    /// [`Self::resolve_pointer_kind`] to upgrade the daemon's
    /// `Hidden` intent into `MeasurementCross` when the user has
    /// the "Show cursor" preference enabled.
    overlay_show_cursor: HashMap<OverlayKey, bool>,
    /// Pre-rendered `+` cursor surface. None on the unhappy path
    /// where SHM allocation failed; we fall through to the blank
    /// cursor / `set_cursor(None)` in that case.
    measurement_cursor: Option<CursorSurface>,
    /// Pre-rendered 1×1 transparent cursor used for the
    /// `SystemPointerKind::Hidden` case. `set_cursor(None)` would
    /// otherwise fall back to whatever default cursor the
    /// compositor wants to show (often an I-beam over text), which
    /// the screencast portal might not strip from the stream and
    /// would then trip our own edge detection.
    blank_cursor: Option<CursorSurface>,

    overlays: HashMap<OverlayKey, OverlayInst>,
    next_overlay_id: u64,

    monitors_pub: Arc<Mutex<Vec<MonitorInfo>>>,
    output_to_id: HashMap<u32, MonitorId>,
    next_monitor_id: u64,

    events_tx: EventSender,
    cmd_tx: calloop::channel::Sender<Cmd>,
}

struct OverlayInst {
    layer: LayerSurface,
    monitor: MonitorId,
    width: u32,
    height: u32,
    /// Buffer scale factor (HiDPI). Buffer dimensions = (width *
    /// buffer_scale, height * buffer_scale). Set on the wl_surface so
    /// the compositor doesn't upscale our pixels.
    buffer_scale: i32,
    configured: bool,
    visible_intent: bool,
    tint: Color,
    visible_atomic: Arc<AtomicBool>,
    /// Whether the surface currently accepts pointer / keyboard input.
    /// Default `false` (click-through). Toggled to `true` while a
    /// measurement session is active.
    input_capturing: bool,
    /// Optional HUD to draw on top of the background tint.
    hud: Option<Hud>,
    /// True after we've requested a `wl_surface.frame()` callback and
    /// committed; cleared when the compositor signals `Done`. While
    /// set, additional `draw_overlay` calls flip `redraw_pending`
    /// instead of committing — Hyprland disconnects clients that
    /// commit faster than the display refresh rate.
    frame_pending: bool,
    /// State changed while `frame_pending` was set; on the next
    /// callback we'll redraw with the latest state.
    redraw_pending: bool,
    /// Pre-baked "bg + static" composite. Per cursor-only frame the
    /// SHM canvas is memcpy'd from this buffer and the dynamic
    /// strokes go on top in-place — so the per-frame cost is one
    /// full-buffer copy + the (sparse) dynamic stroke set, instead
    /// of bg-fill + two full-buffer SrcOver composites. Rebuilt
    /// only when [`combined_cache_key`] changes (held rects /
    /// guides / stuck measurements / colors / measurement format /
    /// background tint). Length matches `pixmap_buf_w *
    /// pixmap_buf_h * 4`; empty before the first hud-bearing draw.
    ///
    /// On a 4K HiDPI surface (~42 MB buffer) each full-buffer pass
    /// runs ~3–6 ms even on fast desktop DDR — keeping the per
    /// cursor frame to a single such pass is what makes measure
    /// mode feel native here.
    ///
    /// [`combined_cache_key`]: Self::combined_cache_key
    combined_bg_static_pixmap: Vec<u8>,
    /// Cache key for [`combined_bg_static_pixmap`]: the static-
    /// layer digest paired with the HUD background colour. Either
    /// changing invalidates the pre-baked composite. `None` =
    /// "no valid cache, rebuild on next draw" (set after a
    /// resize).
    ///
    /// [`combined_bg_static_pixmap`]: Self::combined_bg_static_pixmap
    combined_cache_key: Option<(u64, Color)>,
    /// Buffer dimensions the cached pixmap was rendered at. A
    /// configure that changes `width * buffer_scale` invalidates
    /// the cache.
    pixmap_buf_w: i32,
    pixmap_buf_h: i32,
}

fn run_event_loop(
    cmd_rx: calloop::channel::Channel<Cmd>,
    cmd_tx: calloop::channel::Sender<Cmd>,
    events_tx: EventSender,
    monitors_pub: Arc<Mutex<Vec<MonitorInfo>>>,
    ready_tx: std::sync::mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    let conn = Connection::connect_to_env()
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("wayland connect: {e}")))?;
    let (globals, mut event_queue) = registry_queue_init::<WaylandState>(&conn)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("registry init: {e}")))?;
    let qh = event_queue.handle();

    let registry = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let compositor = CompositorState::bind(&globals, &qh)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("bind compositor: {e}")))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("bind layer-shell: {e}")))?;
    let shm = Shm::bind(&globals, &qh)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("bind shm: {e}")))?;
    // 64 MB initial pool covers a 4096×4096 RGBA buffer (mmap; uses physical
    // memory only when written). SCTK grows on demand if a larger surface
    // appears, but starting large avoids reallocs on hot paths.
    let mut pool = SlotPool::new(4096 * 4096 * 4, &shm)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("create shm pool: {e}")))?;
    let empty_region = Region::new(&compositor)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("create empty region: {e}")))?;
    let seat_state = SeatState::new(&globals, &qh);
    let cursor_shape_manager = match CursorShapeManager::bind(&globals, &qh) {
        Ok(m) => Some(m),
        Err(e) => {
            log::info!("wp_cursor_shape_v1 unavailable: {e} (falling back to drawn arrow)");
            None
        }
    };

    let pointer_constraints = PointerConstraintsState::bind(&globals, &qh);
    let measurement_cursor = match build_measurement_cursor(&compositor, &mut pool, &qh) {
        Ok(c) => Some(c),
        Err(e) => {
            log::warn!("measurement cursor: build failed: {e:#}");
            None
        }
    };
    let blank_cursor = match build_blank_cursor(&compositor, &mut pool, &qh) {
        Ok(c) => Some(c),
        Err(e) => {
            log::warn!("blank cursor: build failed: {e:#}");
            None
        }
    };
    let mut state = WaylandState {
        registry,
        output_state,
        compositor,
        layer_shell,
        shm,
        pool,
        qh: qh.clone(),
        empty_region,
        seat_state,
        pointers: Vec::new(),
        keyboards: Vec::new(),
        loop_handle: None,
        cursor_shape_manager,
        pointer_constraints,
        active_confined_pointer: None,
        pointer_shape_devices: HashMap::new(),
        last_pointer_enter: None,
        overlay_pointer_kind: HashMap::new(),
        overlay_show_cursor: HashMap::new(),
        measurement_cursor,
        blank_cursor,
        overlays: HashMap::new(),
        next_overlay_id: 1,
        monitors_pub,
        output_to_id: HashMap::new(),
        next_monitor_id: 1,
        events_tx,
        cmd_tx,
    };

    // First roundtrip — populate output info before we report ready.
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("initial roundtrip: {e}")))?;
    state.publish_monitors();

    let _ = ready_tx.send(Ok(()));

    // Build calloop event loop with the wayland source + cmd channel.
    let mut event_loop: EventLoop<WaylandState> = EventLoop::try_new()
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("event loop: {e}")))?;
    let lh = event_loop.handle();
    // Hand the loop handle to the state so the next keyboard-add
    // can register auto-repeat against it.
    state.loop_handle = Some(lh.clone());

    WaylandSource::new(conn, event_queue)
        .insert(lh.clone())
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("insert wayland source: {e:?}")))?;

    lh.insert_source(cmd_rx, |event, _, state| {
        if let calloop::channel::Event::Msg(cmd) = event {
            state.handle_cmd(cmd);
        }
    })
    .map_err(|e| PlatformError::Other(anyhow::anyhow!("insert cmd source: {e}")))?;

    loop {
        event_loop
            .dispatch(Some(Duration::from_millis(500)), &mut state)
            .map_err(|e| PlatformError::Other(anyhow::anyhow!("dispatch: {e}")))?;
    }
}

// =========================================================================
// State helpers
// =========================================================================

impl WaylandState {
    fn handle_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::CreateOverlay { monitor, reply } => {
                let res = self.create_overlay(monitor);
                let _ = reply.send(res);
            }
            Cmd::OverlayShow(key) => self.show_overlay(key),
            Cmd::OverlayHide(key) => self.hide_overlay(key),
            Cmd::OverlaySetTint(key, color) => {
                if let Some(inst) = self.overlays.get_mut(&key) {
                    inst.tint = color;
                    if inst.visible_intent {
                        self.draw_overlay(key);
                    }
                }
            }
            Cmd::OverlaySetInputCapturing(key, capturing) => {
                self.set_input_capturing(key, capturing);
            }
            Cmd::OverlaySetHud(key, hud) => {
                // The `+` cursor marker is the one overlay element
                // whose pixels would otherwise punch through the
                // foreground-coloured axis lines and confuse edge
                // detection's anchor. Route it through the OS
                // pointer cursor so the screencast portal's
                // `CursorMode::Hidden` strips it from live captures.
                // Axis lines + tick caps + pill still render on the
                // overlay layer (this is where they show up in
                // captures) and the daemon filters their foreground
                // colour out of edge detection in live mode.
                let want_cursor = hud
                    .as_ref()
                    .map(|h| h.show_cursor)
                    .unwrap_or(false);
                let masked = hud.map(|mut h| {
                    h.show_cursor = false;
                    h
                });
                let visible_intent = match self.overlays.get_mut(&key) {
                    Some(inst) => {
                        inst.hud = masked;
                        inst.visible_intent
                    }
                    None => return,
                };
                self.overlay_show_cursor.insert(key, want_cursor);
                if let Some((pointer, serial)) = self.last_pointer_enter.clone() {
                    let resolved = self.resolve_pointer_kind(key);
                    self.apply_pointer_kind(&pointer, serial, resolved);
                }
                if visible_intent {
                    self.draw_overlay(key);
                }
            }
            Cmd::OverlayDestroy(key) => {
                if let Some(inst) = self.overlays.remove(&key) {
                    let _ = self
                        .events_tx
                        .send(PlatformEvent::OverlayClosed(inst.monitor));
                    drop(inst);
                }
                self.overlay_pointer_kind.remove(&key);
                self.overlay_show_cursor.remove(&key);
            }
            Cmd::OverlaySetSystemPointer(key, kind) => {
                self.set_overlay_system_pointer(key, kind);
            }
            Cmd::OverlayConfinePointer(key, x, y, w, h) => {
                self.confine_overlay_pointer(key, x, y, w, h);
            }
            Cmd::OverlayReleasePointerConfine(_key) => {
                self.release_overlay_pointer_confine();
            }
        }
    }

    fn confine_overlay_pointer(&mut self, key: OverlayKey, x: i32, y: i32, w: i32, h: i32) {
        // Drop any prior confinement before requesting a new one
        // (the protocol bans stacking constraints on the same seat).
        self.release_overlay_pointer_confine();
        let Some(inst) = self.overlays.get(&key) else {
            return;
        };
        let pointer = match self.pointers.first() {
            Some(p) => p.clone(),
            None => {
                log::warn!("confine_pointer: no pointer bound yet");
                return;
            }
        };
        let surface = inst.layer.wl_surface().clone();
        // The region is in surface-local coords; let the compositor
        // free us when the surface loses input focus (`Persistent`
        // would keep it reapplying on every refocus, which is more
        // than we need for a single drag gesture).
        let region = Region::new(&self.compositor)
            .ok()
            .map(|r| {
                r.add(x, y, w, h);
                r
            });
        let region_ref = region.as_ref().map(|r| r.wl_region());
        match self.pointer_constraints.confine_pointer(
            &surface,
            &pointer,
            region_ref,
            PointerLifetime::Persistent,
            &self.qh,
        ) {
            Ok(cp) => {
                self.active_confined_pointer = Some(cp);
                log::debug!(
                    "pointer confined to surface rect ({x},{y}) {w}x{h}"
                );
            }
            Err(e) => log::warn!("confine_pointer failed: {e}"),
        }
        // Keep the wl_region alive for the duration of the confinement
        // by storing it alongside — actually drop it; the compositor
        // has consumed its reference at this point.
        drop(region);
    }

    fn release_overlay_pointer_confine(&mut self) {
        if let Some(cp) = self.active_confined_pointer.take() {
            cp.destroy();
            log::debug!("pointer confinement released");
        }
    }

    fn create_overlay(&mut self, monitor: MonitorId) -> Result<WaylandOverlay> {
        let qh = self.qh();
        let surface = self.compositor.create_surface(&qh);

        let target_output = self.find_output(monitor);
        let layer = self.layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("vernier.overlay"),
            target_output.as_ref(),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        layer.set_size(0, 0);
        // HiDPI: tell the compositor our buffers are at the monitor's
        // scale factor, so it shows them 1:1 without upscale blur.
        let buffer_scale = self
            .monitors_pub
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == monitor)
            .map(|m| m.scale_factor.round() as i32)
            .unwrap_or(1)
            .max(1);
        layer.wl_surface().set_buffer_scale(buffer_scale);
        // Empty input region = click-through. Measurement mode will swap this
        // for a full-coverage region when we want to capture mouse later.
        layer
            .wl_surface()
            .set_input_region(Some(self.empty_region.wl_region()));
        layer.commit();

        let key = OverlayKey(self.next_overlay_id);
        self.next_overlay_id += 1;
        let visible_atomic = Arc::new(AtomicBool::new(false));

        self.overlays.insert(
            key,
            OverlayInst {
                layer,
                monitor,
                width: 0,
                height: 0,
                buffer_scale,
                configured: false,
                visible_intent: false,
                tint: Color::rgba(0x00, 0x88, 0xFF, 0x40),
                visible_atomic: visible_atomic.clone(),
                input_capturing: false,
                hud: None,
                frame_pending: false,
                redraw_pending: false,
                combined_bg_static_pixmap: Vec::new(),
                combined_cache_key: None,
                pixmap_buf_w: 0,
                pixmap_buf_h: 0,
            },
        );

        Ok(WaylandOverlay {
            key,
            monitor,
            cmd_tx: self.cmd_tx.clone(),
            visible: visible_atomic,
        })
    }

    fn show_overlay(&mut self, key: OverlayKey) {
        log::debug!("show_overlay key={:?}", key);
        if let Some(inst) = self.overlays.get_mut(&key) {
            inst.visible_intent = true;
            inst.visible_atomic.store(true, Ordering::Relaxed);
        }
        self.draw_overlay(key);
    }

    fn hide_overlay(&mut self, key: OverlayKey) {
        log::debug!("hide_overlay key={:?}", key);
        if let Some(inst) = self.overlays.get_mut(&key) {
            inst.visible_intent = false;
            inst.visible_atomic.store(false, Ordering::Relaxed);
        }
        // Keep the surface mapped — just draw transparent. Unmapping (attach
        // None + commit) means the compositor sends a fresh Configure before
        // the next show is allowed, and any pre-configure buffer attach is a
        // protocol error.
        self.draw_overlay(key);
    }

    fn set_input_capturing(&mut self, key: OverlayKey, capturing: bool) {
        let Some(inst) = self.overlays.get_mut(&key) else {
            return;
        };
        if inst.input_capturing == capturing {
            return;
        }
        inst.input_capturing = capturing;
        if capturing {
            // None = "infinite" input region per Wayland spec — i.e. the
            // entire surface accepts pointer input. Exclusive keyboard
            // ensures keypresses (Esc) reach us instead of the focused
            // app underneath.
            inst.layer.wl_surface().set_input_region(None);
            inst.layer
                .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        } else {
            inst.layer
                .wl_surface()
                .set_input_region(Some(self.empty_region.wl_region()));
            inst.layer
                .set_keyboard_interactivity(KeyboardInteractivity::None);
        }
        inst.layer.commit();
        // Re-apply the pointer kind so toggling into measure mode
        // immediately swaps the system arrow for the measurement
        // crosshair (or out of it on exit), even when the pointer
        // is already inside the overlay and no fresh Enter event
        // is on its way.
        if let Some((pointer, serial)) = self.last_pointer_enter.clone() {
            let resolved = self.resolve_pointer_kind(key);
            self.apply_pointer_kind(&pointer, serial, resolved);
        }
    }

    /// Lookup the monitor of the first known overlay. Used to attribute
    /// keyboard events that the protocol doesn't carry a surface for in
    /// our handler signatures.
    fn first_overlay_monitor(&self) -> Option<MonitorId> {
        self.overlays.values().next().map(|inst| inst.monitor)
    }

    /// Apply a SystemPointerKind to the given pointer/serial — either
    /// hide the OS cursor, set the wp_cursor_shape "default" shape,
    /// or attach the pre-rendered measurement-`+` surface.
    fn apply_pointer_kind(
        &self,
        pointer: &wl_pointer::WlPointer,
        serial: u32,
        kind: SystemPointerKind,
    ) {
        match kind {
            SystemPointerKind::Hidden => {
                if let Some(bc) = &self.blank_cursor {
                    pointer.set_cursor(
                        serial,
                        Some(&bc.surface),
                        bc.hotspot_x,
                        bc.hotspot_y,
                    );
                } else {
                    pointer.set_cursor(serial, None, 0, 0);
                }
            }
            SystemPointerKind::Default => {
                if let Some(device) = self.pointer_shape_devices.get(&pointer.id()) {
                    device.set_shape(serial, Shape::Default);
                } else {
                    pointer.set_cursor(serial, None, 0, 0);
                }
            }
            SystemPointerKind::MeasurementCross => {
                if let Some(mc) = &self.measurement_cursor {
                    pointer.set_cursor(
                        serial,
                        Some(&mc.surface),
                        mc.hotspot_x,
                        mc.hotspot_y,
                    );
                } else {
                    pointer.set_cursor(serial, None, 0, 0);
                }
            }
        }
    }

    /// Resolve the daemon's per-overlay [`SystemPointerKind`] request
    /// against the implicit defaults. The daemon stays platform-
    /// agnostic and toggles between `Hidden` (no system arrow — we
    /// want our HUD shown via the cursor plane) and `Default` (system
    /// arrow wanted, e.g. over a clickable pill or while the context
    /// menu is open).
    ///
    /// When the daemon hasn't issued an explicit
    /// `set_system_pointer_visible` yet, fall back to the same
    /// implicit default the daemon uses internally: `Hidden` while
    /// the overlay is capturing input (measure mode wants the
    /// dynamic-HUD cursor), `Default` otherwise.
    fn resolve_pointer_kind(&self, key: OverlayKey) -> SystemPointerKind {
        let requested = match self.overlay_pointer_kind.get(&key).copied() {
            Some(k) => k,
            None => {
                let capturing = self
                    .overlays
                    .get(&key)
                    .map(|i| i.input_capturing)
                    .unwrap_or(false);
                if capturing {
                    SystemPointerKind::Hidden
                } else {
                    SystemPointerKind::Default
                }
            }
        };
        let show_cursor = self
            .overlay_show_cursor
            .get(&key)
            .copied()
            .unwrap_or(false);
        match (requested, show_cursor) {
            (SystemPointerKind::Hidden, true) => SystemPointerKind::MeasurementCross,
            (kind, _) => kind,
        }
    }

    fn set_overlay_system_pointer(&mut self, key: OverlayKey, kind: SystemPointerKind) {
        self.overlay_pointer_kind.insert(key, kind);
        if let Some((pointer, serial)) = self.last_pointer_enter.clone() {
            let resolved = self.resolve_pointer_kind(key);
            self.apply_pointer_kind(&pointer, serial, resolved);
        }
    }

    fn draw_overlay(&mut self, key: OverlayKey) {
        let Some(inst) = self.overlays.get_mut(&key) else {
            return;
        };
        log::debug!(
            "draw_overlay key={:?} configured={} {}x{} visible={} frame_pending={}",
            key, inst.configured, inst.width, inst.height, inst.visible_intent,
            inst.frame_pending,
        );
        if !inst.configured || inst.width == 0 || inst.height == 0 {
            return;
        }
        // The compositor hasn't released the previous frame yet —
        // remember that we have new state and bail; the wl_callback
        // Done handler will pick up the redraw.
        if inst.frame_pending {
            inst.redraw_pending = true;
            return;
        }
        let scale = inst.buffer_scale.max(1);
        // Buffer is at PHYSICAL resolution (surface dims × buffer_scale).
        // Compositor displays it 1:1 without upscaling, so all our
        // strokes and text render at native HiDPI clarity.
        let buf_w = inst.width as i32 * scale;
        let buf_h = inst.height as i32 * scale;
        let stride = buf_w * 4;

        let (buffer, canvas) = match self.pool.create_buffer(
            buf_w,
            buf_h,
            stride,
            wl_shm::Format::Abgr8888,
        ) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("shm create_buffer failed: {e}");
                return;
            }
        };

        if !inst.visible_intent {
            // Hidden: clear to transparent.
            canvas.fill(0);
        } else if inst.hud.is_some() {
            // Resize the cached pixmap to match the SHM buffer dims
            // before borrowing `hud`. A configure that changes
            // `width * buffer_scale` lands here too and invalidates
            // the cache.
            let pixmap_bytes = (buf_w as usize) * (buf_h as usize) * 4;
            if inst.pixmap_buf_w != buf_w
                || inst.pixmap_buf_h != buf_h
                || inst.combined_bg_static_pixmap.len() != pixmap_bytes
            {
                inst.combined_bg_static_pixmap.clear();
                inst.combined_bg_static_pixmap.resize(pixmap_bytes, 0);
                inst.pixmap_buf_w = buf_w;
                inst.pixmap_buf_h = buf_h;
                inst.combined_cache_key = None;
            }

            let hud = inst.hud.as_ref().expect("hud.is_some checked above");
            let new_key = (static_hash(hud), hud.background);

            // Rebuild the bg+static composite when either input
            // changes. Rare path — held content edits, color
            // tweaks, freeze toggle. The hot path (cursor moves)
            // hits the cache and skips this entire block.
            if Some(new_key) != inst.combined_cache_key {
                let bg = rgba8888_premul(hud.background);
                if bg == [0, 0, 0, 0] {
                    inst.combined_bg_static_pixmap.fill(0);
                } else {
                    for chunk in
                        inst.combined_bg_static_pixmap.chunks_exact_mut(4)
                    {
                        chunk.copy_from_slice(&bg);
                    }
                }
                render_static_onto(
                    &mut inst.combined_bg_static_pixmap,
                    buf_w as u32,
                    buf_h as u32,
                    scale as u32,
                    hud,
                );
                inst.combined_cache_key = Some(new_key);
            }

            // Hot path: one full-buffer memcpy (the pre-baked
            // bg+static) plus sparse dynamic strokes drawn directly
            // onto the canvas. That matches the pre-split path's
            // per-frame cost (one fill + sparse strokes) while
            // amortizing the static stroke pass across frames.
            if canvas.len() == inst.combined_bg_static_pixmap.len() {
                canvas.copy_from_slice(&inst.combined_bg_static_pixmap);
            } else {
                // Defensive: `pixmap_bytes` was validated against
                // both `inst.combined_bg_static_pixmap.len()` and
                // the buffer dims a few lines up; a mismatch here
                // means the SHM buffer disagreed about its own
                // size. Fall back to a bg-only fill so the frame
                // still commits.
                log::warn!(
                    "canvas/cache size mismatch (canvas={}, cache={}) — \
                     painting bg only this frame",
                    canvas.len(),
                    inst.combined_bg_static_pixmap.len(),
                );
                let bg = rgba8888_premul(hud.background);
                if bg == [0, 0, 0, 0] {
                    canvas.fill(0);
                } else {
                    for chunk in canvas.chunks_exact_mut(4) {
                        chunk.copy_from_slice(&bg);
                    }
                }
            }
            // Dynamic strokes (axis crosshair, tick caps, W×H pill,
            // drag rect, in-progress held outline) on top of the
            // static layer. The `+` marker is gated by
            // `hud.show_cursor`, which we mask to `false` in the
            // `OverlaySetHud` handler — it's drawn by the OS
            // pointer cursor instead, so the screencast portal
            // strips it from live captures.
            render_dynamic_onto(
                canvas,
                buf_w as u32,
                buf_h as u32,
                scale as u32,
                hud,
            );
        } else {
            // Plain tint, no HUD.
            let pixel = rgba8888_premul(inst.tint);
            for chunk in canvas.chunks_exact_mut(4) {
                chunk.copy_from_slice(&pixel);
            }
        }

        let surface = inst.layer.wl_surface();
        if let Err(e) = buffer.attach_to(surface) {
            log::warn!("buffer attach failed: {e}");
            return;
        }
        // damage_buffer is in BUFFER coords — match the buffer dims.
        surface.damage_buffer(0, 0, buf_w, buf_h);
        // Frame callback throttles us to the compositor's display
        // refresh. The Done handler clears `frame_pending` and
        // re-issues `draw_overlay` if state changed in the meantime.
        surface.frame(&self.qh, key);
        inst.frame_pending = true;
        inst.redraw_pending = false;
        surface.commit();
    }

    fn qh(&self) -> QueueHandle<WaylandState> {
        self.qh.clone()
    }

    fn find_output(&self, monitor: MonitorId) -> Option<wl_output::WlOutput> {
        for output in self.output_state.outputs() {
            if let Some(info) = self.output_state.info(&output) {
                if self
                    .output_to_id
                    .get(&info.id)
                    .copied()
                    .map(|id| id == monitor)
                    .unwrap_or(false)
                {
                    return Some(output);
                }
            }
        }
        None
    }

    fn publish_monitors(&mut self) {
        let mut vec = Vec::new();
        for output in self.output_state.outputs() {
            let Some(info) = self.output_state.info(&output) else {
                continue;
            };
            let id = *self
                .output_to_id
                .entry(info.id)
                .or_insert_with(|| {
                    let id = MonitorId(self.next_monitor_id);
                    self.next_monitor_id += 1;
                    id
                });
            let (lw, lh) = info
                .logical_size
                .map(|(w, h)| (w as u32, h as u32))
                .unwrap_or_else(|| {
                    info.modes
                        .iter()
                        .find(|m| m.current)
                        .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
                        .unwrap_or((0, 0))
                });
            let (lx, ly) = info.logical_position.unwrap_or((0, 0));
            vec.push(MonitorInfo {
                id,
                name: info
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{} {}", info.make, info.model)),
                bounds: Rect::new(lx, ly, lw, lh),
                scale_factor: info.scale_factor as f64,
                is_primary: vec.is_empty(),
            });
        }
        *self.monitors_pub.lock().unwrap() = vec;
    }
}

// =========================================================================
// SCTK handler impls
// =========================================================================

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(p) => self.pointers.push(p),
                Err(e) => log::warn!("get_pointer: {e}"),
            }
        }
        if capability == Capability::Keyboard {
            // Prefer get_keyboard_with_repeat so SCTK delivers
            // software auto-repeat events on the calloop source —
            // matches the compositor's repeat rate / delay so
            // holding an arrow key nudges continuously. Falls back
            // to the no-repeat variant only if the loop handle
            // wasn't initialized yet (shouldn't happen in practice
            // since init_wayland_thread sets it before the seat
            // capabilities round-trip).
            let result = if let Some(lh) = self.loop_handle.clone() {
                self.seat_state.get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    lh,
                    Box::new(|state, _kbd, event| {
                        let monitor = state.first_overlay_monitor();
                        if let Some(monitor) = monitor {
                            let _ = state.events_tx.send(PlatformEvent::KeyboardKey {
                                monitor,
                                keysym: event.keysym.raw(),
                                pressed: true,
                                is_repeat: true,
                            });
                        }
                    }),
                )
            } else {
                self.seat_state.get_keyboard(qh, &seat, None)
            };
            match result {
                Ok(k) => self.keyboards.push(k),
                Err(e) => log::warn!("get_keyboard: {e}"),
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        _: Capability,
    ) {
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for ev in events {
            let surf_id = ev.surface.id();
            let (monitor, overlay_key) = match self
                .overlays
                .iter()
                .find(|(_, inst)| inst.layer.wl_surface().id() == surf_id)
            {
                Some((k, inst)) => (inst.monitor, *k),
                None => continue,
            };
            // On Enter, cache the serial/pointer so later Cmd::SetSystemPointer
            // can apply visibility. Default behavior (capturing, no
            // override yet) hides the cursor — we draw our own.
            if let PointerEventKind::Enter { serial } = ev.kind {
                self.last_pointer_enter = Some((pointer.clone(), serial));
                if let Some(mgr) = &self.cursor_shape_manager {
                    let pid = pointer.id();
                    if !self.pointer_shape_devices.contains_key(&pid) {
                        let device = mgr.get_shape_device(pointer, _qh);
                        self.pointer_shape_devices.insert(pid, device);
                    }
                }
                // Don't pre-populate `overlay_pointer_kind` here:
                // `resolve_pointer_kind` already falls back to the
                // right value based on the overlay's current
                // `input_capturing` state. Inserting an explicit
                // Default during passive mode would stick around
                // and override the implicit Hidden the daemon
                // expects once measure mode begins.
                let kind = self.resolve_pointer_kind(overlay_key);
                self.apply_pointer_kind(pointer, serial, kind);
            }
            let (x, y) = ev.position;
            let plat_event = match &ev.kind {
                PointerEventKind::Enter { .. } => PlatformEvent::PointerEnter { monitor, x, y },
                PointerEventKind::Leave { .. } => PlatformEvent::PointerLeave { monitor },
                PointerEventKind::Motion { .. } => PlatformEvent::PointerMove { monitor, x, y },
                PointerEventKind::Press { button, .. } => PlatformEvent::PointerButton {
                    monitor,
                    button: *button,
                    pressed: true,
                    x,
                    y,
                },
                PointerEventKind::Release { button, .. } => PlatformEvent::PointerButton {
                    monitor,
                    button: *button,
                    pressed: false,
                    x,
                    y,
                },
                _ => continue,
            };
            let _ = self.events_tx.send(plat_event);
        }
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.publish_monitors();
        let _ = self.events_tx.send(PlatformEvent::MonitorsChanged);
    }
    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        self.publish_monitors();
    }
    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        if let Some(info) = self.output_state.info(&output) {
            self.output_to_id.remove(&info.id);
        }
        self.publish_monitors();
        let _ = self.events_tx.send(PlatformEvent::MonitorsChanged);
    }
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }
    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for WaylandState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        let surf_id = layer.wl_surface().id();
        let key = self
            .overlays
            .iter()
            .find(|(_, inst)| inst.layer.wl_surface().id() == surf_id)
            .map(|(k, _)| *k);
        if let Some(k) = key {
            if let Some(inst) = self.overlays.remove(&k) {
                let _ = self
                    .events_tx
                    .send(PlatformEvent::OverlayClosed(inst.monitor));
            }
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        log::debug!(
            "layer configure size={:?} surface_id={:?}",
            configure.new_size,
            layer.wl_surface().id()
        );
        let surf_id = layer.wl_surface().id();
        let key = self
            .overlays
            .iter()
            .find(|(_, inst)| inst.layer.wl_surface().id() == surf_id)
            .map(|(k, _)| *k);
        let Some(key) = key else {
            return;
        };
        if let Some(inst) = self.overlays.get_mut(&key) {
            inst.width = configure.new_size.0.max(1);
            inst.height = configure.new_size.1.max(1);
            inst.configured = true;
            self.draw_overlay(key);
        }
    }
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl KeyboardHandler for WaylandState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let monitor = self.first_overlay_monitor();
        if let Some(monitor) = monitor {
            let _ = self.events_tx.send(PlatformEvent::KeyboardKey {
                monitor,
                keysym: event.keysym.raw(),
                pressed: true,
                is_repeat: false,
            });
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let monitor = self.first_overlay_monitor();
        if let Some(monitor) = monitor {
            let _ = self.events_tx.send(PlatformEvent::KeyboardKey {
                monitor,
                keysym: event.keysym.raw(),
                pressed: false,
                is_repeat: false,
            });
        }
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // Auto-repeat fires at the compositor's repeat rate while a
        // key is held. We forward it as `pressed: true, is_repeat:
        // true` so handlers that should self-fire (nudge, tolerance
        // up/down) opt in, while one-shots (clear-and-hide,
        // capture, color toggle, etc.) ignore it on the daemon side.
        let monitor = self.first_overlay_monitor();
        if let Some(monitor) = monitor {
            let _ = self.events_tx.send(PlatformEvent::KeyboardKey {
                monitor,
                keysym: event.keysym.raw(),
                pressed: true,
                is_repeat: true,
            });
        }
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
    }
    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: smithay_client_toolkit::seat::keyboard::RepeatInfo,
    ) {
    }
}

/// Frame-callback Done means "the previous commit landed; you can
/// commit the next one." If state changed while we were waiting, draw
/// the latest immediately — otherwise we'll redraw the next time the
/// app pushes a HUD update.
impl Dispatch<wl_callback::WlCallback, OverlayKey> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        event: wl_callback::Event,
        data: &OverlayKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, wl_callback::Event::Done { .. }) {
            return;
        }
        let mut redraw_now = false;
        if let Some(inst) = state.overlays.get_mut(data) {
            inst.frame_pending = false;
            if inst.redraw_pending {
                inst.redraw_pending = false;
                redraw_now = true;
            }
        }
        if redraw_now {
            state.draw_overlay(*data);
        }
    }
}

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_layer!(WaylandState);
delegate_shm!(WaylandState);
delegate_seat!(WaylandState);
delegate_pointer!(WaylandState);
delegate_pointer_constraints!(WaylandState);
delegate_keyboard!(WaylandState);
delegate_registry!(WaylandState);

impl PointerConstraintsHandler for WaylandState {
    fn confined(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
        _confined_pointer: &ZwpConfinedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {
        log::debug!("pointer confined by compositor");
    }
    fn unconfined(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
        _confined_pointer: &ZwpConfinedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {
        log::debug!("pointer unconfined by compositor");
    }
    fn locked(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
        _locked_pointer: &ZwpLockedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {}
    fn unlocked(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
        _locked_pointer: &ZwpLockedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {}
}
// (delegate_pointer! already wires Dispatch for the wp_cursor_shape
// manager + device through SCTK's CursorShapeManager.)

// =========================================================================
// Cursor helpers
// =========================================================================

/// Pre-render the `+` measurement cursor into an SHM buffer attached
/// to its own `wl_surface`. Used as the OS pointer cursor via
/// `wl_pointer.set_cursor` whenever the daemon wants its cursor
/// crosshair visible — Hyprland honours `CursorMode::Hidden` for
/// small cursor surfaces, so painting the `+` here keeps it off the
/// live screencast and out of edge detection.
fn build_measurement_cursor(
    compositor: &CompositorState,
    pool: &mut SlotPool,
    qh: &QueueHandle<WaylandState>,
) -> anyhow::Result<CursorSurface> {
    // 32×32 surface px leaves margin around the 6-logical-px arm
    // length so round line caps don't get clipped. Buffer-scale 2
    // matches typical HiDPI without needing per-monitor handling
    // (cursor surfaces are downscaled cleanly on 1× displays).
    const SURFACE_PX: i32 = 32;
    const BUFFER_SCALE: i32 = 2;
    let buf_w = SURFACE_PX * BUFFER_SCALE;
    let buf_h = SURFACE_PX * BUFFER_SCALE;
    let stride = buf_w * 4;

    let (buffer, canvas) = pool
        .create_buffer(buf_w, buf_h, stride, wl_shm::Format::Abgr8888)
        .map_err(|e| anyhow::anyhow!("create cursor shm buffer: {e}"))?;
    canvas.fill(0);

    let scale_f = BUFFER_SCALE as f32;
    let mut pixmap = tiny_skia::PixmapMut::from_bytes(canvas, buf_w as u32, buf_h as u32)
        .ok_or_else(|| anyhow::anyhow!("wrap cursor buffer as tiny-skia pixmap"))?;
    let cx = buf_w as f32 / 2.0;
    let cy = buf_h as f32 / 2.0;
    let arm = 6.0 * scale_f;
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(cx - arm, cy);
    pb.line_to(cx + arm, cy);
    pb.move_to(cx, cy - arm);
    pb.line_to(cx, cy + arm);
    if let Some(path) = pb.finish() {
        let mut outline = tiny_skia::Paint::default();
        outline.set_color_rgba8(255, 255, 255, 255);
        outline.anti_alias = true;
        pixmap.stroke_path(
            &path,
            &outline,
            &tiny_skia::Stroke {
                width: 8.0,
                line_cap: tiny_skia::LineCap::Round,
                ..Default::default()
            },
            tiny_skia::Transform::identity(),
            None,
        );
        let mut core = tiny_skia::Paint::default();
        core.set_color_rgba8(0, 0, 0, 255);
        core.anti_alias = true;
        pixmap.stroke_path(
            &path,
            &core,
            &tiny_skia::Stroke {
                width: 2.0,
                line_cap: tiny_skia::LineCap::Round,
                ..Default::default()
            },
            tiny_skia::Transform::identity(),
            None,
        );
    }

    let surface = compositor.create_surface(qh);
    buffer
        .attach_to(&surface)
        .map_err(|e| anyhow::anyhow!("attach cursor buffer: {e}"))?;
    surface.set_buffer_scale(BUFFER_SCALE);
    surface.damage_buffer(0, 0, buf_w, buf_h);
    surface.commit();

    Ok(CursorSurface {
        surface,
        _buffer: buffer,
        hotspot_x: SURFACE_PX / 2,
        hotspot_y: SURFACE_PX / 2,
    })
}

/// 1×1 fully-transparent cursor surface for
/// [`SystemPointerKind::Hidden`]. Using a client-attached
/// transparent surface gives a genuine "no visible pointer" that
/// the screencast portal still treats as the cursor (and therefore
/// strips), unlike `set_cursor(None)` which lets the compositor's
/// default cursor leak through.
fn build_blank_cursor(
    compositor: &CompositorState,
    pool: &mut SlotPool,
    qh: &QueueHandle<WaylandState>,
) -> anyhow::Result<CursorSurface> {
    let (buffer, canvas) = pool
        .create_buffer(1, 1, 4, wl_shm::Format::Abgr8888)
        .map_err(|e| anyhow::anyhow!("create blank cursor buffer: {e}"))?;
    canvas.fill(0);

    let surface = compositor.create_surface(qh);
    buffer
        .attach_to(&surface)
        .map_err(|e| anyhow::anyhow!("attach blank cursor buffer: {e}"))?;
    surface.damage_buffer(0, 0, 1, 1);
    surface.commit();

    Ok(CursorSurface {
        surface,
        _buffer: buffer,
        hotspot_x: 0,
        hotspot_y: 0,
    })
}

// =========================================================================
// Pixel helpers
// =========================================================================

/// Convert a PipeWire-format buffer into tightly-packed RGBA8 (stride =
/// width*4). Hyprland gives us BGRA; other compositors may pick BGRx /
/// RGBA / RGBx / xRGB / xBGR. We honor the PipeWire stride to skip any
/// per-row padding. Unknown formats fall through as-is.
fn to_rgba8(
    src: &[u8],
    stride: u32,
    width: u32,
    height: u32,
    format: pipewire::spa::param::video::VideoFormat,
) -> Vec<u8> {
    use pipewire::spa::param::video::VideoFormat as VF;
    let stride = stride as usize;
    let row_bytes = (width as usize) * 4;
    let mut dst = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let off = y * stride;
        if off + row_bytes > src.len() {
            break;
        }
        let row = &src[off..off + row_bytes];
        for chunk in row.chunks_exact(4) {
            match format {
                VF::BGRA => dst.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]),
                VF::BGRx => dst.extend_from_slice(&[chunk[2], chunk[1], chunk[0], 0xFF]),
                VF::RGBA => dst.extend_from_slice(chunk),
                VF::RGBx => dst.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xFF]),
                VF::xRGB => dst.extend_from_slice(&[chunk[1], chunk[2], chunk[3], 0xFF]),
                VF::xBGR => dst.extend_from_slice(&[chunk[3], chunk[2], chunk[1], 0xFF]),
                _ => dst.extend_from_slice(chunk),
            }
        }
    }
    dst
}


// HUD rasterization lives in `crate::hud_render`. The Wayland backend
// caches the pre-baked bg+static composite and per cursor frame just
// memcpys it into the SHM canvas then strokes the dynamic layer on
// top in-place — one full-buffer pass plus sparse strokes, which is
// the cost ceiling we need at 4K HiDPI where each extra full-buffer
// composite costs several ms.
use crate::hud_render::{render_dynamic_onto, render_static_onto, rgba8888_premul, static_hash};
fn video_format_to_pixel_format(
    vf: pipewire::spa::param::video::VideoFormat,
) -> Result<PixelFormat> {
    use pipewire::spa::param::video::VideoFormat as VF;
    match vf {
        VF::BGRA => Ok(PixelFormat::Bgra8),
        VF::BGRx => Ok(PixelFormat::Bgrx8),
        VF::RGBA => Ok(PixelFormat::Rgba8),
        VF::RGBx => Ok(PixelFormat::Rgbx8),
        VF::xRGB => Ok(PixelFormat::Xrgb8),
        VF::xBGR => Ok(PixelFormat::Xbgr8),
        other => Err(PlatformError::Unsupported {
            what: match other {
                _ => "unrecognized PipeWire video format",
            },
        }),
    }
}
