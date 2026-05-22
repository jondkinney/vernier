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
    reexports::protocols::wp::{
        cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
        pointer_constraints::zv1::client::{
            zwp_confined_pointer_v1::ZwpConfinedPointerV1,
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::Lifetime as PointerLifetime,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{
            PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager,
        },
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
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

use crate::{
    Accelerator, AppIdentity, Color, EventReceiver, EventSender, Frame, HotkeyId, Hud, MonitorId,
    MonitorInfo, NativeFrame, OverlayHandle, OverlayOps, PixelFormat, Platform, PlatformError,
    PlatformEvent, Rect, Result, TrayHandle, TrayMenu,
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

    ready_rx.recv().map_err(|_| {
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
                    s.node_id,
                    s.position,
                    s.size,
                    s.stream_id
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
        let svc = guard
            .as_ref()
            .ok_or_else(|| PlatformError::Other(anyhow::anyhow!("screencast not ready yet")))?;
        let stream_info = svc
            .streams()
            .first()
            .ok_or_else(|| PlatformError::Other(anyhow::anyhow!("screencast has no streams")))?;
        let captured = svc.latest_frame(stream_info.node_id).ok_or_else(|| {
            PlatformError::Other(anyhow::anyhow!(
                "no frame captured yet — try again in a moment"
            ))
        })?;
        let monitor_info = self
            .monitors
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == monitor)
            .cloned();
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
        let svc = guard.as_ref().ok_or_else(|| {
            PlatformError::Other(anyhow::anyhow!(
                "screencast not ready yet — portal handshake or PipeWire connect still in flight"
            ))
        })?;
        // First-stream mapping: portal-side stream order matches the monitor
        // order the user picked in the consent dialog. Multi-monitor proper
        // mapping is a milestone-3 refinement.
        let stream_info = svc
            .streams()
            .first()
            .ok_or_else(|| PlatformError::Other(anyhow::anyhow!("screencast has no streams")))?;
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
        let monitor_info = self
            .monitors
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == monitor)
            .cloned();
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
    // `Hud` is large (~440 bytes); box it so it doesn't inflate every
    // other `Cmd` variant sitting in the calloop channel. `Option<Box>`
    // keeps the `None` case a null pointer with no allocation.
    OverlaySetHud(OverlayKey, Option<Box<Hud>>),
    /// Paint a captured frame as the overlay's opaque background —
    /// measure mode's freeze-screen visual. `None` clears it. Boxed
    /// for the same reason as `OverlaySetHud`: a `Frame` holds a full
    /// screen of pixels (tens of MB), so inlining it would bloat every
    /// variant sitting in the channel.
    OverlaySetBackgroundFrame(OverlayKey, Option<Box<Frame>>),
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
    Hidden,
    Default,
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
        let _ = self
            .cmd_tx
            .send(Cmd::OverlaySetHud(self.key, hud.map(Box::new)));
    }
    fn set_background_frame(&mut self, frame: Option<Frame>) {
        let _ = self.cmd_tx.send(Cmd::OverlaySetBackgroundFrame(
            self.key,
            frame.map(Box::new),
        ));
    }
    fn set_system_pointer_visible(&mut self, visible: bool) {
        let kind = if visible {
            SystemPointerKind::Default
        } else {
            SystemPointerKind::Hidden
        };
        let _ = self
            .cmd_tx
            .send(Cmd::OverlaySetSystemPointer(self.key, kind));
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
    /// `wp_fractional_scale_manager_v1` global if the compositor offers
    /// it. Lets us learn each surface's true (possibly fractional)
    /// preferred scale so the overlay buffer renders at native
    /// resolution instead of a rounded integer scale.
    fractional_scale_manager: Option<WpFractionalScaleManagerV1>,
    /// `wp_viewporter` global if the compositor offers it. Paired with
    /// fractional-scale: the buffer is sized to the fractional scale
    /// and the viewport's `set_destination` maps it back to the
    /// surface's logical dimensions.
    viewporter: Option<WpViewporter>,
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
    /// True (possibly fractional) display scale, e.g. 1.6. Physical
    /// buffer dimensions = ceil(width * frac_scale, height *
    /// frac_scale). Seeded from the monitor's scale factor and
    /// refined by `wp_fractional_scale_v1`'s `preferred_scale` event.
    frac_scale: f32,
    /// Per-surface viewport, present when `wp_viewporter` is bound. Its
    /// `set_destination` maps the fractionally-scaled buffer back to
    /// the surface's logical dimensions.
    viewport: Option<WpViewport>,
    /// Per-surface fractional-scale object, present when
    /// `wp_fractional_scale_manager_v1` is bound. Delivers
    /// `preferred_scale` events.
    frac_scale_obj: Option<WpFractionalScaleV1>,
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
    /// Captured frame painted as an opaque base layer beneath the HUD
    /// — measure mode's freeze-screen visual. `None` ⇒ transparent, so
    /// live content shows through (the pre-existing behavior). Baked
    /// into `combined_bg_static_pixmap` when the cache is rebuilt.
    background_frame: Option<Frame>,
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

impl Drop for OverlayInst {
    fn drop(&mut self) {
        // Release the per-surface viewport + fractional-scale objects.
        // Covers both teardown paths — `Cmd::OverlayDestroy` and
        // `LayerShellHandler::closed` — since both `remove` the inst
        // from the map, dropping it here.
        if let Some(vp) = self.viewport.take() {
            vp.destroy();
        }
        if let Some(fs) = self.frac_scale_obj.take() {
            fs.destroy();
        }
    }
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
    let pool = SlotPool::new(4096 * 4096 * 4, &shm)
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

    // Fractional-scale + viewporter: bind both or neither. With both we
    // render the overlay buffer at the monitor's true fractional scale
    // (e.g. 1.6×) and let the viewport map it back to logical size.
    // Missing either ⇒ fall back to a rounded integer buffer scale.
    let fractional_scale_manager: Option<WpFractionalScaleManagerV1> =
        globals.bind(&qh, 1..=1, ()).ok();
    if fractional_scale_manager.is_none() {
        log::info!(
            "wp_fractional_scale_v1 unavailable — overlay falls back to integer buffer scale"
        );
    }
    let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
    if viewporter.is_none() {
        log::info!("wp_viewporter unavailable — overlay falls back to integer buffer scale");
    }

    let pointer_constraints = PointerConstraintsState::bind(&globals, &qh);
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
        fractional_scale_manager,
        viewporter,
        pointer_constraints,
        active_confined_pointer: None,
        pointer_shape_devices: HashMap::new(),
        last_pointer_enter: None,
        overlay_pointer_kind: HashMap::new(),
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
                if let Some(inst) = self.overlays.get_mut(&key) {
                    inst.hud = hud.map(|h| *h);
                    if inst.visible_intent {
                        self.draw_overlay(key);
                    }
                }
            }
            Cmd::OverlaySetBackgroundFrame(key, frame) => {
                if let Some(inst) = self.overlays.get_mut(&key) {
                    inst.background_frame = frame.map(|f| *f);
                    // The frozen frame is baked into the cached
                    // bg+static pixmap; force a rebuild next draw.
                    inst.combined_cache_key = None;
                    let visible = inst.visible_intent;
                    if visible {
                        self.draw_overlay(key);
                    }
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
        let region = Region::new(&self.compositor).ok().inspect(|r| {
            r.add(x, y, w, h);
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
                log::debug!("pointer confined to surface rect ({x},{y}) {w}x{h}");
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
        // True (possibly fractional) display scale. Seed from the
        // monitor's scale factor WITHOUT rounding — the fractional-
        // scale event will refine it once the compositor knows which
        // output the surface lives on.
        let mut frac_scale = self
            .monitors_pub
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == monitor)
            .map(|m| m.scale_factor as f32)
            .unwrap_or(1.0)
            .max(1.0);

        let key = OverlayKey(self.next_overlay_id);
        self.next_overlay_id += 1;

        // Per-surface viewport + fractional-scale objects. With both
        // present the compositor shows our fractionally-scaled buffer
        // at native resolution; the wl_surface buffer scale stays 1.
        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(layer.wl_surface(), &qh, ()));
        let frac_scale_obj = self
            .fractional_scale_manager
            .as_ref()
            .map(|mgr| mgr.get_fractional_scale(layer.wl_surface(), &qh, key));
        if viewport.is_some() && frac_scale_obj.is_some() {
            // wp_fractional_scale spec: keep buffer scale at 1; the
            // viewport carries the logical→buffer mapping instead.
            layer.wl_surface().set_buffer_scale(1);
        } else {
            // Fallback: no fractional-scale support. Snap `frac_scale`
            // to an integer so the ceil-sized buffer in `draw_overlay`
            // matches the integer buffer scale declared here — the
            // legacy HiDPI path.
            frac_scale = frac_scale.round();
            layer.wl_surface().set_buffer_scale(frac_scale as i32);
        }
        // Empty input region = click-through. Measurement mode will swap this
        // for a full-coverage region when we want to capture mouse later.
        layer
            .wl_surface()
            .set_input_region(Some(self.empty_region.wl_region()));
        layer.commit();

        let visible_atomic = Arc::new(AtomicBool::new(false));

        self.overlays.insert(
            key,
            OverlayInst {
                layer,
                monitor,
                width: 0,
                height: 0,
                frac_scale,
                viewport,
                frac_scale_obj,
                configured: false,
                visible_intent: false,
                tint: Color::rgba(0x00, 0x88, 0xFF, 0x40),
                visible_atomic: visible_atomic.clone(),
                input_capturing: false,
                hud: None,
                background_frame: None,
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
    }

    /// Lookup the monitor of the first known overlay. Used to attribute
    /// keyboard events that the protocol doesn't carry a surface for in
    /// our handler signatures.
    fn first_overlay_monitor(&self) -> Option<MonitorId> {
        self.overlays.values().next().map(|inst| inst.monitor)
    }

    /// Apply a SystemPointerKind to the given pointer/serial — either
    /// hide the OS cursor or set the wp_cursor_shape "default" shape.
    fn apply_pointer_kind(
        &self,
        pointer: &wl_pointer::WlPointer,
        serial: u32,
        kind: SystemPointerKind,
    ) {
        match kind {
            SystemPointerKind::Hidden => {
                pointer.set_cursor(serial, None, 0, 0);
            }
            SystemPointerKind::Default => {
                if let Some(device) = self.pointer_shape_devices.get(&pointer.id()) {
                    device.set_shape(serial, Shape::Default);
                } else {
                    // No cursor-shape support — leave the cursor as-is
                    // rather than hiding it (compositor's default).
                    pointer.set_cursor(serial, None, 0, 0);
                }
            }
        }
    }

    fn set_overlay_system_pointer(&mut self, key: OverlayKey, kind: SystemPointerKind) {
        self.overlay_pointer_kind.insert(key, kind);
        if let Some((pointer, serial)) = self.last_pointer_enter.clone() {
            self.apply_pointer_kind(&pointer, serial, kind);
        }
    }

    fn draw_overlay(&mut self, key: OverlayKey) {
        let Some(inst) = self.overlays.get_mut(&key) else {
            return;
        };
        log::debug!(
            "draw_overlay key={:?} configured={} {}x{} visible={} frame_pending={}",
            key,
            inst.configured,
            inst.width,
            inst.height,
            inst.visible_intent,
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
        let scale_f = inst.frac_scale.max(1.0);
        // Buffer is at PHYSICAL resolution (surface dims × frac_scale).
        // `ceil` so a fractional scale never under-sizes the buffer
        // and leaves an uncovered logical strip. Compositor displays
        // it 1:1 (via the viewport, or via buffer-scale in the integer
        // fallback), so strokes and text render at native clarity.
        let buf_w = (inst.width as f32 * scale_f).ceil() as i32;
        let buf_h = (inst.height as f32 * scale_f).ceil() as i32;
        let stride = buf_w * 4;

        let (buffer, canvas) =
            match self
                .pool
                .create_buffer(buf_w, buf_h, stride, wl_shm::Format::Abgr8888)
            {
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
                // Base layer: the frozen-screen frame if measure mode
                // set one (and it matches our buffer dims), otherwise
                // the flat HUD background tint (pre-existing behavior).
                let painted_freeze_frame = match inst.background_frame.as_ref() {
                    Some(f) if f.pixels.len() == inst.combined_bg_static_pixmap.len() => {
                        blit_frame_as_opaque_base(&mut inst.combined_bg_static_pixmap, f);
                        true
                    }
                    Some(f) => {
                        log::warn!(
                            "freeze background {}×{} doesn't match overlay buffer \
                             {}×{} — painting tint instead",
                            f.width,
                            f.height,
                            buf_w,
                            buf_h,
                        );
                        false
                    }
                    None => false,
                };
                if !painted_freeze_frame {
                    let bg = rgba8888_premul(hud.background);
                    if bg == [0, 0, 0, 0] {
                        inst.combined_bg_static_pixmap.fill(0);
                    } else {
                        for chunk in inst.combined_bg_static_pixmap.chunks_exact_mut(4) {
                            chunk.copy_from_slice(&bg);
                        }
                    }
                }
                render_static_onto(
                    &mut inst.combined_bg_static_pixmap,
                    buf_w as u32,
                    buf_h as u32,
                    scale_f,
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
            render_dynamic_onto(canvas, buf_w as u32, buf_h as u32, scale_f, hud);
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
        // With a viewport active the buffer is at fractional physical
        // resolution; map it to the surface's logical size so the
        // compositor scales it 1:1 to native pixels.
        if let Some(vp) = inst.viewport.as_ref() {
            vp.set_destination(inst.width as i32, inst.height as i32);
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
            let id = *self.output_to_id.entry(info.id).or_insert_with(|| {
                let id = MonitorId(self.next_monitor_id);
                self.next_monitor_id += 1;
                id
            });
            let logical = info.logical_size.map(|(w, h)| (w as u32, h as u32));
            let current_mode = info.modes.iter().find(|m| m.current);
            let (lw, lh) = logical.unwrap_or_else(|| {
                current_mode
                    .map(|m| (m.dimensions.0 as u32, m.dimensions.1 as u32))
                    .unwrap_or((0, 0))
            });
            let (lx, ly) = info.logical_position.unwrap_or((0, 0));
            // `wl_output`'s integer `scale_factor` rounds a fractional
            // scale (1.6 → 2). When both the physical mode size and the
            // logical size are known, derive the true (fractional)
            // scale from their ratio; otherwise fall back to the
            // rounded integer.
            let scale_factor = match (logical, current_mode) {
                (Some((log_w, _)), Some(m)) if log_w > 0 && m.dimensions.0 > 0 => {
                    m.dimensions.0 as f64 / log_w as f64
                }
                _ => info.scale_factor as f64,
            };
            vec.push(MonitorInfo {
                id,
                name: info
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{} {}", info.make, info.model)),
                bounds: Rect::new(lx, ly, lw, lh),
                scale_factor,
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
            let (monitor, capturing, overlay_key) = match self
                .overlays
                .iter()
                .find(|(_, inst)| inst.layer.wl_surface().id() == surf_id)
            {
                Some((k, inst)) => (inst.monitor, inst.input_capturing, *k),
                None => continue,
            };
            // On Enter, cache the serial/pointer so later Cmd::SetSystemPointer
            // can apply visibility. Default behavior (capturing, no
            // override yet) hides the cursor — we draw our own.
            if let PointerEventKind::Enter { serial } = ev.kind {
                self.last_pointer_enter = Some((pointer.clone(), serial));
                if let Some(mgr) = &self.cursor_shape_manager {
                    let pid = pointer.id();
                    self.pointer_shape_devices
                        .entry(pid)
                        .or_insert_with(|| mgr.get_shape_device(pointer, _qh));
                }
                let kind = self
                    .overlay_pointer_kind
                    .get(&overlay_key)
                    .copied()
                    .unwrap_or(if capturing {
                        SystemPointerKind::Hidden
                    } else {
                        SystemPointerKind::Default
                    });
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

// --- Fractional-scale + viewporter Dispatch ------------------------------
//
// The two manager globals and the viewport object carry no events, so
// their Dispatch impls are empty. Only `wp_fractional_scale_v1`
// delivers an event (`preferred_scale`), handled below.

impl Dispatch<WpFractionalScaleManagerV1, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: <WpFractionalScaleManagerV1 as Proxy>::Event,
        _data: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewporter, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: <WpViewporter as Proxy>::Event,
        _data: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: <WpViewport as Proxy>::Event,
        _data: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// `preferred_scale` carries the compositor's suggested scale as the
/// numerator of a fraction over 120 (so 192 ⇒ 1.6×). When it differs
/// from our current `frac_scale` we adopt it, drop the bg+static cache
/// (the buffer dims change), and redraw.
impl Dispatch<WpFractionalScaleV1, OverlayKey> for WaylandState {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        data: &OverlayKey,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        let new = (scale as f32 / 120.0).max(1.0);
        let mut redraw_now = false;
        if let Some(inst) = state.overlays.get_mut(data) {
            if (inst.frac_scale - new).abs() > f32::EPSILON {
                inst.frac_scale = new;
                // Buffer dims change ⇒ the pre-baked bg+static
                // composite is stale.
                inst.combined_cache_key = None;
                redraw_now = inst.visible_intent;
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
    ) {
    }
    fn unlocked(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<Self>,
        _locked_pointer: &ZwpLockedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {
    }
}
// (delegate_pointer! already wires Dispatch for the wp_cursor_shape
// manager + device through SCTK's CursorShapeManager.)

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

/// Blit a captured [`Frame`] (tightly-packed straight RGBA) into the
/// overlay's premultiplied SHM pixmap as an opaque base layer. The
/// source alpha is discarded: a freeze-screen background is opaque by
/// definition, and forcing `A = 255` makes premultiplied identical to
/// straight, so the RGB bytes carry over unchanged — and it can't be
/// tripped up by a compositor whose screencast leaves alpha at 0. The
/// caller guarantees `dst.len() == frame.pixels.len()`.
fn blit_frame_as_opaque_base(dst: &mut [u8], frame: &Frame) {
    for (d, s) in dst.chunks_exact_mut(4).zip(frame.pixels.chunks_exact(4)) {
        d[0] = s[0];
        d[1] = s[1];
        d[2] = s[2];
        d[3] = 0xFF;
    }
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
        _other => Err(PlatformError::Unsupported {
            what: "unrecognized PipeWire video format",
        }),
    }
}
