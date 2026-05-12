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
    delegate_registry, delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    reexports::protocols::wp::cursor_shape::v1::client::{
        wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    },
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager},
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
                    inst.hud = hud;
                    if inst.visible_intent {
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
        } else if let Some(hud) = inst.hud.as_ref() {
            render_hud_into(canvas, buf_w as u32, buf_h as u32, scale as u32, hud);
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
                    if !self.pointer_shape_devices.contains_key(&pid) {
                        let device = mgr.get_shape_device(pointer, _qh);
                        self.pointer_shape_devices.insert(pid, device);
                    }
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

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_layer!(WaylandState);
delegate_shm!(WaylandState);
delegate_seat!(WaylandState);
delegate_pointer!(WaylandState);
delegate_keyboard!(WaylandState);
delegate_registry!(WaylandState);
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

/// Layout for the dimension-readout pill. Computed alongside the line
/// strokes so the pill background can be drawn via tiny-skia, and the
/// glyph rasterization can run after pixmap drops its &mut canvas.
struct PillLayout {
    text: String,
    /// Pen X position of the first glyph in BUFFER coords.
    text_x: f32,
    /// Baseline Y position in BUFFER coords (descenders go below).
    baseline_y: f32,
    /// fontdue rasterization size in BUFFER pixels.
    px_size: f32,
}

/// Pixel size of the dimension-readout text in LOGICAL pixels. Sized
/// to fill the pill comfortably against a 2 physical-px stroke.
const TEXT_LOGICAL_PX: f32 = 12.5;
/// Smaller text size for "stuck" measurement pills — keeps the
/// frozen readouts visually subordinate to the live W×H pill.
const TEXT_STUCK_LOGICAL_PX: f32 = 10.0;
/// Toast pills get their own (larger) text size so status messages
/// stay readable at a distance. Kept independent so tweaking the
/// measurement-pill text doesn't shrink the toast.
const TOAST_TEXT_LOGICAL_PX: f32 = 18.0;

/// Lazily-loaded TTF font for the dimension readout. We try a few
/// well-known system paths; if none are available we fall back to no
/// text (the pill stays empty).
fn hud_font() -> Option<&'static fontdue::Font> {
    use std::sync::OnceLock;
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        const CANDIDATES: &[&str] = &[
            "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        ];
        for path in CANDIDATES {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(font) = fontdue::Font::from_bytes(
                    bytes.as_slice(),
                    fontdue::FontSettings::default(),
                ) {
                    log::info!("hud font: {path}");
                    return Some(font);
                }
            }
        }
        log::warn!("hud font: no system TTF found; pill text will be blank");
        None
    })
    .as_ref()
}

/// Fallback font for glyphs the primary HUD font doesn't carry —
/// notably the macOS modifier symbols (⇧⌃⌘⌥). Adwaita Sans includes
/// them; we also try DejaVu / Noto as additional fallbacks for less
/// common Linux distros.
fn hud_symbol_font() -> Option<&'static fontdue::Font> {
    use std::sync::OnceLock;
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        const CANDIDATES: &[&str] = &[
            "/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        ];
        for path in CANDIDATES {
            if let Ok(bytes) = std::fs::read(path) {
                if let Ok(f) = fontdue::Font::from_bytes(
                    bytes.as_slice(),
                    fontdue::FontSettings::default(),
                ) {
                    log::info!("hud symbol font: {path}");
                    return Some(f);
                }
            }
        }
        None
    })
    .as_ref()
}

/// Pick the best font for `c`: primary if it carries the glyph,
/// otherwise the symbol fallback, otherwise the Omarchy font (carries
/// the U+E900 SUPER logo the right-click menu uses on Omarchy hosts).
/// Used so per-glyph rendering can substitute for missing characters
/// without leaving tofu boxes.
fn font_for_char<'a>(primary: &'a fontdue::Font, c: char) -> &'a fontdue::Font {
    if primary.lookup_glyph_index(c) != 0 {
        return primary;
    }
    if let Some(symbol) = hud_symbol_font() {
        if symbol.lookup_glyph_index(c) != 0 {
            return symbol;
        }
    }
    if let Some(omarchy) = omarchy_font() {
        if omarchy.lookup_glyph_index(c) != 0 {
            return omarchy;
        }
    }
    primary
}

/// Lazily load `~/.local/share/fonts/omarchy.ttf` so the right-click
/// menu can render the U+E900 SUPER glyph on Omarchy hosts. Returns
/// `None` if the font isn't installed or fails to parse — in which
/// case the SUPER hint falls back to the literal text "Super".
fn omarchy_font() -> Option<&'static fontdue::Font> {
    use std::sync::OnceLock;
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        let home = std::env::var_os("HOME")?;
        let path = std::path::PathBuf::from(home).join(".local/share/fonts/omarchy.ttf");
        let bytes = std::fs::read(&path).ok()?;
        let font = fontdue::Font::from_bytes(
            bytes.as_slice(),
            fontdue::FontSettings::default(),
        )
        .ok()?;
        log::info!("hud omarchy font: {}", path.display());
        Some(font)
    })
    .as_ref()
}

fn measure_text_width(font: &fontdue::Font, text: &str, px_size: f32) -> f32 {
    text.chars()
        .map(|c| font_for_char(font, c).metrics(c, px_size).advance_width)
        .sum()
}

/// Pill bg dimensions for `text` at `text_logical_px`. Padding is
/// proportional to the text size (matches push_pill).
fn pill_dims_at(text: &str, text_logical_px: f32, scale_f: f32) -> (f32, f32, f32, f32) {
    let px_size = text_logical_px * scale_f;
    let (text_w, ascent, descent) = if let Some(font) = hud_font() {
        let w = measure_text_width(font, text, px_size);
        let (a, d) = font
            .horizontal_line_metrics(px_size)
            .map(|m| (m.ascent, -m.descent))
            .unwrap_or((px_size * 0.8, px_size * 0.2));
        (w, a, d)
    } else {
        (
            text.len() as f32 * px_size * 0.55,
            px_size * 0.8,
            px_size * 0.2,
        )
    };
    let pad_x = 0.8 * text_logical_px * scale_f;
    let pad_y = 0.4 * text_logical_px * scale_f;
    let pill_w = text_w.ceil() + pad_x * 2.0;
    let pill_h = (ascent + descent).ceil() + pad_y * 2.0;
    (pill_w, pill_h, ascent, descent)
}

/// Draw the dark pill background only — caller is responsible for
/// pushing the centered text glyph layout afterwards. Useful when
/// the displayed glyph is a different size from the bg's nominal
/// content (e.g. hover-X overlay on a stuck-measurement pill).
fn draw_pill_bg(pixmap: &mut tiny_skia::PixmapMut, x: f32, y: f32, w: f32, h: f32) {
    use tiny_skia::*;
    let mut bg_paint = Paint::default();
    bg_paint.set_color_rgba8(40, 40, 40, 230);
    bg_paint.anti_alias = true;
    if let Some(path) = pill_path(x, y, w, h) {
        pixmap.fill_path(&path, &bg_paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Push a glyph layout centered in the rectangle `(x, y, w, h)` at
/// `text_logical_px`. The text may be larger than the box (e.g.
/// stuck-pill hover X overflows).
fn push_text_in_box(
    pills: &mut Vec<PillLayout>,
    text: String,
    box_x: f32,
    box_y: f32,
    box_w: f32,
    box_h: f32,
    text_logical_px: f32,
    scale_f: f32,
) {
    let Some(font) = hud_font() else { return };
    let px_size = text_logical_px * scale_f;
    let text_w = measure_text_width(font, &text, px_size);
    let (ascent, descent) = font
        .horizontal_line_metrics(px_size)
        .map(|m| (m.ascent, -m.descent))
        .unwrap_or((px_size * 0.8, px_size * 0.2));
    let cx = box_x + box_w * 0.5;
    let cy = box_y + box_h * 0.5;
    pills.push(PillLayout {
        text,
        text_x: (cx - text_w * 0.5).round(),
        baseline_y: (cy + (ascent - descent) * 0.5).round(),
        px_size,
    });
}

/// Render a [`Hud`] into a wl_shm Abgr8888 buffer at the given buffer
/// dimensions and HiDPI scale factor. Cursor / edge coords are in
/// surface (logical) pixels and get multiplied by `scale` internally.
fn render_hud_into(canvas: &mut [u8], buf_w: u32, buf_h: u32, scale: u32, hud: &Hud) {
    let bg = rgba8888_premul(hud.background);
    if bg == [0, 0, 0, 0] {
        canvas.fill(0);
    } else {
        for chunk in canvas.chunks_exact_mut(4) {
            chunk.copy_from_slice(&bg);
        }
    }

    // tiny-skia phase scoped so its &mut borrow on canvas is released
    // before we rasterize glyphs into it.
    let pills = render_hud_strokes(canvas, buf_w, buf_h, scale, hud);

    if !pills.is_empty() {
        if let Some(font) = hud_font() {
            for layout in &pills {
                render_pill_text(canvas, buf_w, buf_h, font, layout);
            }
        }
    }
}

fn render_hud_strokes(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    scale: u32,
    hud: &Hud,
) -> Vec<PillLayout> {
    use tiny_skia::*;
    let Some(mut pixmap) = PixmapMut::from_bytes(canvas, buf_w, buf_h) else {
        return Vec::new();
    };

    let fg = hud.foreground;
    let mut paint = Paint::default();
    paint.set_color_rgba8(fg.r, fg.g, fg.b, fg.a);
    // Anti-aliasing is the bulk of tiny-skia's per-frame cost. Crisp
    // 1px lines are also closer to a clean minimal aesthetic.
    paint.anti_alias = false;
    // Axis lines: 1 LOGICAL pixel = `scale` buffer pixels.
    let stroke = Stroke {
        // Hard 2 physical pixels regardless of buffer scale — narrow
        // enough not to obscure the pixel boundary being measured,
        // wide enough to stay legible against busy backgrounds.
        width: 2.0,
        ..Default::default()
    };
    // Tick caps: ~2 LOGICAL pixels so they read as filled bars over the
    // thinner axis lines. Hard 2
    // physical pixels — distinct enough from the 1 px axis lines to
    // read as caps without being chunky.
    let tick_stroke = Stroke {
        width: 2.0,
        ..Default::default()
    };

    let mut pills: Vec<PillLayout> = Vec::new();

    // Held rects are additive — drawn first so the live HUD sits on
    // top. Each accumulated drag stays visible.
    if !hud.held_rects.is_empty() {
        for rect in &hud.held_rects {
            // Held rect's W×H pill sits inside only if the rect is
            // at least 70 logical px wide and 35 tall. Smaller rects
            // get the pill anchored below to keep the readout legible.
            let rw_logical = (rect.rect_end.0 - rect.rect_start.0).abs() as i32;
            let rh_logical = (rect.rect_end.1 - rect.rect_start.1).abs() as i32;
            let pill_below = rw_logical < 70 || rh_logical < 35;
            draw_area_rect(
                &mut pixmap,
                &mut pills,
                &rect.rect_start,
                &rect.rect_end,
                buf_w as f32,
                buf_h as f32,
                scale,
                fg,
                &hud.measurement_format,
                &stroke,
                &paint,
                rect.camera_armed,
                pill_below,
            );
        }
    }

    // Live drag rect, BEFORE the cursor — the cursor (arrow or
    // crosshair) is rendered last so it sits on top of every other
    // overlay element including hover X badges.
    if let HudKind::Drawing { start, cursor } = &hud.kind {
        let rw_logical = (cursor.0 - start.0).abs() as i32;
        let rh_logical = (cursor.1 - start.1).abs() as i32;
        let pill_below = rw_logical < 70 || rh_logical < 35;
        draw_area_rect(
            &mut pixmap,
            &mut pills,
            start,
            cursor,
            buf_w as f32,
            buf_h as f32,
            scale,
            fg,
            &hud.measurement_format,
            &stroke,
            &paint,
            false,
            pill_below,
        );
    }
    if let HudKind::Held {
        rect_start,
        rect_end,
        camera_armed,
        ..
    } = &hud.kind
    {
        draw_area_rect(
            &mut pixmap,
            &mut pills,
            rect_start,
            rect_end,
            buf_w as f32,
            buf_h as f32,
            scale,
            fg,
            &hud.measurement_format,
            &stroke,
            &paint,
            *camera_armed,
            false,
        );
    }
    if !hud.stuck_measurements.is_empty() {
        draw_stuck_measurements(
            &mut pixmap,
            &mut pills,
            &hud.stuck_measurements,
            fg,
            &hud.measurement_format,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }
    if !hud.guides.is_empty() {
        // Cursor extracted from kind so the guide-hover X badge can
        // render at the actual cursor position on the line.
        let cursor = match &hud.kind {
            HudKind::Hover { cursor, .. } => Some(*cursor),
            HudKind::Drawing { cursor, .. } => Some(*cursor),
            HudKind::Held { cursor, .. } => Some(*cursor),
            HudKind::None => None,
        };
        draw_guides(
            &mut pixmap,
            &mut pills,
            &hud.guides,
            cursor,
            hud.align_mode,
            hud.guide_color,
            &hud.measurement_format,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }
    // Cursor crosshair / arrow goes on top of EVERYTHING so the
    // user's pointer indicator never disappears behind a pill, X
    // badge, or guide. Toast comes after so it sits above the
    // cursor too — fine, toast is a transient status message and
    // the cursor isn't the focus when it's up.
    // The live measurement crosshair (axis lines + tick caps + W×H
    // pill) always renders — that's the actual measurement, not the
    // cursor. Inside `draw_hover_indicators`, the white-outlined `+`
    // marker (the cursor itself) is gated by `hud.show_cursor`.
    if let HudKind::Hover { cursor, edges } = &hud.kind {
        if hud.align_mode {
            draw_hover_indicators(
                &mut pixmap,
                &mut pills,
                cursor,
                edges,
                buf_w as f32,
                buf_h as f32,
                scale,
                &paint,
                &stroke,
                &tick_stroke,
                hud.measurement_format.wh_indicators,
                &hud.measurement_format.unit_suffix,
                hud.measurement_format.dimension_divisor,
                hud.show_cursor,
            );
        } else if hud.move_cursor_at.is_some() {
            // The dedicated draw_move_cursor block at the end of
            // this function paints it.
        } else if hud.cursor_in_rect {
            // System cursor is shown via wp_cursor_shape from main.rs
            // when cursor_in_rect is true — no custom drawing here.
            let _ = cursor;
        } else {
            draw_hover_indicators(
                &mut pixmap,
                &mut pills,
                cursor,
                edges,
                buf_w as f32,
                buf_h as f32,
                scale,
                &paint,
                &stroke,
                &tick_stroke,
                hud.measurement_format.wh_indicators,
                &hud.measurement_format.unit_suffix,
                hud.measurement_format.dimension_divisor,
                hud.show_cursor,
            );
        }
    }
    if let HudKind::Held {
        cursor,
        edges,
        cursor_in_rect,
        ..
    } = &hud.kind
    {
        if *cursor_in_rect {
            draw_arrow_cursor(
                &mut pixmap,
                cursor.0 as f32 * scale as f32,
                cursor.1 as f32 * scale as f32,
                scale as f32,
            );
        } else {
            draw_hover_indicators(
                &mut pixmap,
                &mut pills,
                cursor,
                edges,
                buf_w as f32,
                buf_h as f32,
                scale,
                &paint,
                &stroke,
                &tick_stroke,
                hud.measurement_format.wh_indicators,
                &hud.measurement_format.unit_suffix,
                hud.measurement_format.dimension_divisor,
                hud.show_cursor,
            );
        }
    }
    if let Some((cx, cy)) = hud.move_cursor_at {
        let bx = cx as f32 * scale as f32;
        let by = cy as f32 * scale as f32;
        match hud.cursor_kind {
            crate::CursorKind::Move => {
                draw_move_cursor(&mut pixmap, bx, by, scale as f32);
            }
            crate::CursorKind::ResizeNS => {
                draw_resize_cursor(&mut pixmap, bx, by, scale as f32, 0.0);
            }
            crate::CursorKind::ResizeEW => {
                draw_resize_cursor(&mut pixmap, bx, by, scale as f32, 90.0);
            }
            crate::CursorKind::ResizeNWSE => {
                draw_resize_cursor(&mut pixmap, bx, by, scale as f32, -45.0);
            }
            crate::CursorKind::ResizeNESW => {
                draw_resize_cursor(&mut pixmap, bx, by, scale as f32, 45.0);
            }
        }
    }
    if let Some(toast) = &hud.toast {
        draw_toast(
            &mut pixmap,
            &mut pills,
            &toast.text,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }
    if let Some(menu) = &hud.context_menu {
        draw_context_menu(
            &mut pixmap,
            &mut pills,
            menu,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }
    if let Some(text) = hud.corner_indicator.as_deref() {
        draw_corner_indicator(
            &mut pixmap,
            &mut pills,
            text,
            buf_w as f32,
            buf_h as f32,
            scale as f32,
        );
    }
    pills
}

/// Top-right pill that signals an active integration is rewriting
/// the on-screen values (e.g. `F · 200%` while the Figma plugin is
/// connected and a Figma tab is focused). Drawn last so it sits
/// above measurement HUD elements but below the context menu.
fn draw_corner_indicator(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    text: &str,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    let _ = buf_h;
    let margin = 12.0 * scale_f;
    push_pill(
        pixmap,
        pills,
        text.to_string(),
        buf_w - margin,
        margin,
        PillAnchor::AnchorTopRight,
        buf_w,
        buf_h,
        scale_f,
        TEXT_LOGICAL_PX,
    );
}

/// 2-direction resize cursor — black bar with arrowheads at both
/// ends and a white halo. `rotate_deg` orients the arrows: 0 = NS
/// (vertical), 90 = EW (horizontal), -45 = NWSE, 45 = NESW.
fn draw_resize_cursor(
    pixmap: &mut tiny_skia::PixmapMut,
    cx: f32,
    cy: f32,
    scale_f: f32,
    rotate_deg: f32,
) {
    use tiny_skia::*;
    let l = 7.5 * scale_f;   // half-length of arms
    let a = 3.5 * scale_f;   // arrowhead extent — smaller arrowheads
    let t = 1.0 * scale_f;   // arm half-thickness
    let s = a + 4.0 * scale_f; // serif half-width — 4 px wider than the arrowhead on each side
    let sh = 1.0 * scale_f;  // serif half-height (along arm axis)
    let mut pb = PathBuilder::new();
    // I-beam style: NS double-arrow with a horizontal serif at the
    // center. Trace outer boundary clockwise from top tip.
    pb.move_to(0.0, -l);
    pb.line_to(-a, -l + a);
    pb.line_to(-t, -l + a);
    pb.line_to(-t, -sh);
    pb.line_to(-s, -sh);
    pb.line_to(-s, sh);
    pb.line_to(-t, sh);
    pb.line_to(-t, l - a);
    pb.line_to(-a, l - a);
    pb.line_to(0.0, l);
    pb.line_to(a, l - a);
    pb.line_to(t, l - a);
    pb.line_to(t, sh);
    pb.line_to(s, sh);
    pb.line_to(s, -sh);
    pb.line_to(t, -sh);
    pb.line_to(t, -l + a);
    pb.line_to(a, -l + a);
    pb.close();
    let path = match pb.finish() {
        Some(p) => p,
        None => return,
    };
    let transform = Transform::from_rotate(rotate_deg).post_translate(cx, cy);
    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 255);
    white.anti_alias = true;
    pixmap.stroke_path(
        &path,
        &white,
        &Stroke {
            // Lighter white outline so the cursor stays slim against
            // smaller geometry.
            width: 4.0,
            line_join: LineJoin::Miter,
            ..Default::default()
        },
        transform,
        None,
    );
    let mut black = Paint::default();
    black.set_color_rgba8(0, 0, 0, 255);
    black.anti_alias = true;
    pixmap.fill_path(&path, &black, FillRule::Winding, transform, None);
}

/// 4-direction "move" cursor — black diamond/plus with arrowheads on
/// each tip and a white halo, drawn while the user is placing a
/// guide. Centered on `(cx, cy)` in BUFFER pixels.
fn draw_move_cursor(pixmap: &mut tiny_skia::PixmapMut, cx: f32, cy: f32, scale_f: f32) {
    use tiny_skia::*;
    let l = 11.0 * scale_f; // half-length of each arm (tip distance from center)
    let a = 5.0 * scale_f;  // arrowhead extent
    let t = 1.5 * scale_f;  // arm half-thickness
    // Center square matches the arm thickness so the arms flow into
    // each other without a notch — gives the cleaner +-with-arrows
    // shape of a standard move cursor.
    let c = t;
    let mut pb = PathBuilder::new();
    pb.move_to(cx, cy - l);
    pb.line_to(cx - a, cy - l + a);
    pb.line_to(cx - t, cy - l + a);
    pb.line_to(cx - t, cy - c);
    pb.line_to(cx - c, cy - c);
    pb.line_to(cx - c, cy - t);
    pb.line_to(cx - l + a, cy - t);
    pb.line_to(cx - l + a, cy - a);
    pb.line_to(cx - l, cy);
    pb.line_to(cx - l + a, cy + a);
    pb.line_to(cx - l + a, cy + t);
    pb.line_to(cx - c, cy + t);
    pb.line_to(cx - c, cy + c);
    pb.line_to(cx - t, cy + c);
    pb.line_to(cx - t, cy + l - a);
    pb.line_to(cx - a, cy + l - a);
    pb.line_to(cx, cy + l);
    pb.line_to(cx + a, cy + l - a);
    pb.line_to(cx + t, cy + l - a);
    pb.line_to(cx + t, cy + c);
    pb.line_to(cx + c, cy + c);
    pb.line_to(cx + c, cy + t);
    pb.line_to(cx + l - a, cy + t);
    pb.line_to(cx + l - a, cy + a);
    pb.line_to(cx + l, cy);
    pb.line_to(cx + l - a, cy - a);
    pb.line_to(cx + l - a, cy - t);
    pb.line_to(cx + c, cy - t);
    pb.line_to(cx + c, cy - c);
    pb.line_to(cx + t, cy - c);
    pb.line_to(cx + t, cy - l + a);
    pb.line_to(cx + a, cy - l + a);
    pb.close();
    if let Some(path) = pb.finish() {
        // White halo first, then black fill on top — same contrast
        // affordance as the regular cross marker.
        let mut white = Paint::default();
        white.set_color_rgba8(255, 255, 255, 255);
        white.anti_alias = true;
        pixmap.stroke_path(
            &path,
            &white,
            &Stroke {
                width: 4.0,
                line_join: LineJoin::Miter,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
        let mut black = Paint::default();
        black.set_color_rgba8(0, 0, 0, 255);
        black.anti_alias = true;
        pixmap.fill_path(&path, &black, FillRule::Winding, Transform::identity(), None);
    }
}

/// Draw frozen single-axis measurements — coral line + tick caps +
/// pill with the pixel count. Same visual language as the live
/// crosshair so the user reads them as "stuck" measurements.
fn draw_stuck_measurements(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    measurements: &[crate::StuckMeasurement],
    fg: Color,
    fmt: &crate::HudMeasurementFormat,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    use crate::GuideAxis;
    let mut paint = Paint::default();
    paint.set_color_rgba8(fg.r, fg.g, fg.b, fg.a);
    paint.anti_alias = false;
    let line_stroke = Stroke { width: 2.0, ..Default::default() };
    let tick_stroke = Stroke { width: 2.0, ..Default::default() };
    let tick_half = 5.0 * scale_f; // tick reach in buffer px
    // Approximate pill height in buffer px — text + proportional pad.
    // Used to decide "is the line long enough to fit the value pill
    // comfortably inside?" Threshold = 3 × pill height.
    let est_pill_h = TEXT_STUCK_LOGICAL_PX * scale_f * 1.8;

    for m in measurements {
        // Snap endpoints to the physical pixel grid (same `floor`
        // step the live crosshair uses), subtract in buffer px, and
        // divide back by scale. Rounded to an integer so the pill
        // matches the live W×H readout exactly — without it, HiDPI
        // half-pixel offsets and fractional rounding modes can drift
        // the displayed length relative to the live pill.
        let start_buf = (m.start as f32 * scale_f).floor();
        let end_buf = (m.end as f32 * scale_f).floor();
        let length = ((end_buf - start_buf).abs() / scale_f).round() as f64;
        let value_text = format_value(length, fmt);
        // Pill bg is ALWAYS sized for the value text so the size
        // doesn't change when hovering. The displayed glyph may be
        // larger (× at 1.5×) and overflow the bg slightly — that's
        // the visual cue.
        let (pill_w, pill_h, _, _) =
            pill_dims_at(&value_text, TEXT_STUCK_LOGICAL_PX, scale_f);
        let display_text = if m.hovered {
            "\u{00D7}".to_string()
        } else {
            value_text.clone()
        };
        let display_size = if m.hovered {
            TEXT_STUCK_LOGICAL_PX * 1.5
        } else {
            TEXT_STUCK_LOGICAL_PX
        };
        let half = 1.0; // half of the 2px stroke for pixel-grid snap
        match m.axis {
            GuideAxis::Vertical => {
                let x = (m.at as f32 * scale_f).floor() + half;
                let y0 = (m.start as f32 * scale_f).floor() + half;
                let y1 = (m.end as f32 * scale_f).floor() + half;
                // Main vertical line.
                let mut pb = PathBuilder::new();
                pb.move_to(x, y0);
                pb.line_to(x, y1);
                if let Some(p) = pb.finish() {
                    pixmap.stroke_path(&p, &paint, &line_stroke, Transform::identity(), None);
                }
                // Horizontal tick caps at start and end.
                for ty in [y0, y1] {
                    let mut pb = PathBuilder::new();
                    pb.move_to(x - tick_half, ty);
                    pb.line_to(x + tick_half, ty);
                    if let Some(p) = pb.finish() {
                        pixmap.stroke_path(&p, &paint, &tick_stroke, Transform::identity(), None);
                    }
                }
                let mid_y = (y0 + y1) * 0.5;
                let line_len = (y1 - y0).abs();
                let (anchor_x, anchor_y, anchor) = if line_len >= 3.0 * est_pill_h {
                    (x, mid_y, PillAnchor::Centered)
                } else {
                    (x + tick_half + 4.0 * scale_f, mid_y, PillAnchor::LeftCenter)
                };
                let (mut pill_x, mut pill_y) = match anchor {
                    PillAnchor::Centered => (anchor_x - pill_w * 0.5, anchor_y - pill_h * 0.5),
                    PillAnchor::LeftCenter => (anchor_x, anchor_y - pill_h * 0.5),
                    _ => (anchor_x, anchor_y),
                };
                pill_x = pill_x.floor().min(buf_w - pill_w - 1.0).max(0.0);
                pill_y = pill_y.floor().min(buf_h - pill_h - 1.0).max(0.0);
                draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
                push_text_in_box(
                    pills,
                    display_text.clone(),
                    pill_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    display_size,
                    scale_f,
                );
            }
            GuideAxis::Horizontal => {
                let y = (m.at as f32 * scale_f).floor() + half;
                let x0 = (m.start as f32 * scale_f).floor() + half;
                let x1 = (m.end as f32 * scale_f).floor() + half;
                // Main horizontal line.
                let mut pb = PathBuilder::new();
                pb.move_to(x0, y);
                pb.line_to(x1, y);
                if let Some(p) = pb.finish() {
                    pixmap.stroke_path(&p, &paint, &line_stroke, Transform::identity(), None);
                }
                // Vertical tick caps at left and right.
                for tx in [x0, x1] {
                    let mut pb = PathBuilder::new();
                    pb.move_to(tx, y - tick_half);
                    pb.line_to(tx, y + tick_half);
                    if let Some(p) = pb.finish() {
                        pixmap.stroke_path(&p, &paint, &tick_stroke, Transform::identity(), None);
                    }
                }
                let mid_x = (x0 + x1) * 0.5;
                let line_len = (x1 - x0).abs();
                let (anchor_x, anchor_y, anchor) = if line_len >= 3.0 * est_pill_h {
                    (mid_x, y, PillAnchor::Centered)
                } else {
                    (mid_x, y + tick_half + 4.0 * scale_f, PillAnchor::AnchorTop)
                };
                let (mut pill_x, mut pill_y) = match anchor {
                    PillAnchor::Centered => (anchor_x - pill_w * 0.5, anchor_y - pill_h * 0.5),
                    PillAnchor::AnchorTop => (anchor_x - pill_w * 0.5, anchor_y),
                    _ => (anchor_x, anchor_y),
                };
                pill_x = pill_x.floor().min(buf_w - pill_w - 1.0).max(0.0);
                pill_y = pill_y.floor().min(buf_h - pill_h - 1.0).max(0.0);
                draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
                push_text_in_box(
                    pills,
                    display_text.clone(),
                    pill_x,
                    pill_y,
                    pill_w,
                    pill_h,
                    display_size,
                    scale_f,
                );
            }
        }
    }
}

/// Draw persistent reference guides — 1 physical-pixel blue lines
/// spanning the full buffer along each guide's axis. Drawn after the
/// rest of the HUD so the guides sit on top of measurement strokes.
/// When a guide is `hovered` and we have a `cursor`, draw a small dark
/// "X" badge on the line at the cursor's free axis to signal removal.
fn draw_guides(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    guides: &[crate::Guide],
    cursor: Option<(f64, f64)>,
    align_mode: bool,
    guide_color: crate::Color,
    fmt: &crate::HudMeasurementFormat,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    use crate::GuideAxis;
    let mut paint = Paint::default();
    paint.set_color_rgba8(guide_color.r, guide_color.g, guide_color.b, guide_color.a);
    paint.anti_alias = false;
    let stroke = Stroke {
        width: 1.0,
        ..Default::default()
    };
    for guide in guides {
        let pos = (guide.position as f32 * scale_f).floor() + 0.5;
        let mut pb = PathBuilder::new();
        match guide.axis {
            GuideAxis::Horizontal => {
                pb.move_to(0.0, pos);
                pb.line_to(buf_w, pos);
            }
            GuideAxis::Vertical => {
                pb.move_to(pos, 0.0);
                pb.line_to(pos, buf_h);
            }
        }
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
        if guide.hovered {
            // Anchor the X badge at the line's midpoint on the
            // perpendicular axis (screen center) — the cursor itself
            // becomes a drag handle instead of being the X target.
            let _ = cursor;
            let (badge_x, badge_y) = match guide.axis {
                GuideAxis::Horizontal => (buf_w * 0.5, pos),
                GuideAxis::Vertical => (pos, buf_h * 0.5),
            };
            draw_remove_x_badge(pixmap, pills, badge_x, badge_y, buf_w, buf_h, scale_f);
        }
    }

    let _ = align_mode;
    // Inter-guide distance pills. For each adjacent pair of guides
    // sharing an axis, render a small pill (same style as a stuck
    // measurement) showing the px gap between them, centered between
    // the two guides on the spanning axis.
    let mut horiz: Vec<i32> = guides
        .iter()
        .filter(|g| g.axis == GuideAxis::Horizontal)
        .map(|g| g.position)
        .collect();
    horiz.sort_unstable();
    horiz.dedup();
    for win in horiz.windows(2) {
        let dist = (win[1] - win[0]).abs();
        if dist == 0 {
            continue;
        }
        let value = format_value(dist as f64, fmt);
        let (pill_w, pill_h, _, _) =
            pill_dims_at(&value, TEXT_STUCK_LOGICAL_PX, scale_f);
        // Horizontal pair (gap is vertical) → label anchored at the
        // LEFT of the screen, vertically centered between the two
        // guide ys.
        let mid_y = (win[0] + win[1]) as f32 * 0.5 * scale_f;
        let pill_x = (50.0 * scale_f).floor().max(0.0);
        let pill_y = (mid_y - pill_h * 0.5)
            .floor()
            .min(buf_h - pill_h - 1.0)
            .max(0.0);
        draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
        push_text_in_box(
            pills,
            value,
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            TEXT_STUCK_LOGICAL_PX,
            scale_f,
        );
    }
    let mut vert: Vec<i32> = guides
        .iter()
        .filter(|g| g.axis == GuideAxis::Vertical)
        .map(|g| g.position)
        .collect();
    vert.sort_unstable();
    vert.dedup();
    for win in vert.windows(2) {
        let dist = (win[1] - win[0]).abs();
        if dist == 0 {
            continue;
        }
        let value = format_value(dist as f64, fmt);
        let (pill_w, pill_h, _, _) =
            pill_dims_at(&value, TEXT_STUCK_LOGICAL_PX, scale_f);
        // Vertical pair (gap is horizontal) → label anchored at the
        // TOP of the screen, horizontally centered between the two
        // guide xs.
        let mid_x = (win[0] + win[1]) as f32 * 0.5 * scale_f;
        let pill_x = (mid_x - pill_w * 0.5)
            .floor()
            .min(buf_w - pill_w - 1.0)
            .max(0.0);
        let pill_y = (50.0 * scale_f).floor().max(0.0);
        draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
        push_text_in_box(
            pills,
            value,
            pill_x,
            pill_y,
            pill_w,
            pill_h,
            TEXT_STUCK_LOGICAL_PX,
            scale_f,
        );
    }
}

/// Small oval "remove" pill with a `×` glyph, drawn on a hovered
/// guide. Same visual treatment as a hovered stuck-measurement pill
/// — bg sized for a single digit at TEXT_STUCK_LOGICAL_PX, × glyph
/// rendered at 1.5× that size and overflowing slightly.
fn draw_remove_x_badge(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    cx: f32,
    cy: f32,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    let (pill_w, pill_h, _, _) = pill_dims_at("0", TEXT_STUCK_LOGICAL_PX, scale_f);
    let pill_x = (cx - pill_w * 0.5)
        .floor()
        .min(buf_w - pill_w - 1.0)
        .max(0.0);
    let pill_y = (cy - pill_h * 0.5)
        .floor()
        .min(buf_h - pill_h - 1.0)
        .max(0.0);
    draw_pill_bg(pixmap, pill_x, pill_y, pill_w, pill_h);
    push_text_in_box(
        pills,
        "\u{00D7}".to_string(),
        pill_x,
        pill_y,
        pill_w,
        pill_h,
        TEXT_STUCK_LOGICAL_PX * 1.5,
        scale_f,
    );
}

/// Draw the live measure crosshair: axis lines through the cursor with
/// tick caps where edges were detected, plus the white `+` cursor
/// marker on top, and a W×H pill in the lower-right of the cursor.
#[allow(clippy::too_many_arguments)]
fn draw_hover_indicators(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    cursor: &(f64, f64),
    edges: &[Option<crate::HudEdge>; 4],
    buf_w: f32,
    buf_h: f32,
    scale: u32,
    paint: &tiny_skia::Paint,
    stroke: &tiny_skia::Stroke,
    tick_stroke: &tiny_skia::Stroke,
    wh_indicators: bool,
    unit_suffix: &str,
    dimension_divisor: f64,
    show_cursor: bool,
) {
    use tiny_skia::*;
    let scale_f = scale as f32;
    {
            // Convert surface-logical coords to buffer-physical, snap
            // to the pixel grid, offset by stroke half-width so non-AA
            // strokes land cleanly on integer columns / rows. Without
            // this, integer positions sit on the boundary between two
            // pixels and the rasterizer's tie-break rule picks one or
            // the other, giving uneven tick lengths and shimmer.
            let half = stroke.width * 0.5;
            let snap = |v: f64| (v * scale as f64).floor() as f32 + half;
            let cx = snap(cursor.0);
            let cy = snap(cursor.1);
            let surface_w = buf_w;
            let surface_h = buf_h;

            // Horizontal axis line: spans from left snap edge (or screen
            // left) to right snap edge (or screen right), through cursor.
            let left = edges
                .iter()
                .filter_map(|e| e.as_ref())
                .find(|e| e.axis == HudAxis::Left);
            let right = edges
                .iter()
                .filter_map(|e| e.as_ref())
                .find(|e| e.axis == HudAxis::Right);
            let left_x = left.map(|e| snap(e.position.0)).unwrap_or(half);
            let right_x = right
                .map(|e| snap(e.position.0))
                .unwrap_or(surface_w - half);
            let mut pb = PathBuilder::new();
            pb.move_to(left_x, cy);
            pb.line_to(right_x, cy);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }

            // Vertical axis line.
            let up = edges
                .iter()
                .filter_map(|e| e.as_ref())
                .find(|e| e.axis == HudAxis::Up);
            let down = edges
                .iter()
                .filter_map(|e| e.as_ref())
                .find(|e| e.axis == HudAxis::Down);
            let up_y = up.map(|e| snap(e.position.1)).unwrap_or(half);
            let down_y = down
                .map(|e| snap(e.position.1))
                .unwrap_or(surface_h - half);
            let mut pb = PathBuilder::new();
            pb.move_to(cx, up_y);
            pb.line_to(cx, down_y);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }

            // Tick marks. Anchor the tick CENTER on the matching axis
            // line (cy for left/right ticks, cx for up/down ticks) so
            // they sit exactly on the main lines.
            // Tick half-length = 5 LOGICAL pixels. Drawn with the
            // thicker `tick_stroke` so caps look like filled bars.
            let tick = 5.0 * scale_f;
            for edge in edges.iter().flatten() {
                let ex = snap(edge.position.0);
                let ey = snap(edge.position.1);
                let (px, py, tdx, tdy) = match edge.axis {
                    HudAxis::Left | HudAxis::Right => (ex, cy, 0.0, tick),
                    HudAxis::Up | HudAxis::Down => (cx, ey, tick, 0.0),
                };
                let mut pb = PathBuilder::new();
                pb.move_to(px - tdx, py - tdy);
                pb.line_to(px + tdx, py + tdy);
                if let Some(path) = pb.finish() {
                    pixmap.stroke_path(&path, &paint, &tick_stroke, Transform::identity(), None);
                }
            }

            // Cursor `+` marker: black interior with a white outline,
            // The white outline keeps the
            // mark visible against dark UI; the black core makes it
            // pop on light UI. Drawn after the axis lines so it sits
            // on top of their crossing point.
            //
            // Gated by `show_cursor` (the prefs "Show cursor" toggle)
            // — the rest of this function (axis lines, tick caps,
            // W×H pill) is the measurement HUD itself and stays
            // visible either way.
            if show_cursor {
                let arm = 6.0 * scale_f;
                let mut pb = PathBuilder::new();
                pb.move_to(cx - arm, cy);
                pb.line_to(cx + arm, cy);
                pb.move_to(cx, cy - arm);
                pb.line_to(cx, cy + arm);
                if let Some(path) = pb.finish() {
                    // Hard physical-pixel widths: 4 px white outline,
                    // 2 px black core, regardless of buffer scale.
                    let mut outline = Paint::default();
                    outline.set_color_rgba8(255, 255, 255, 255);
                    outline.anti_alias = true;
                    pixmap.stroke_path(
                        &path,
                        &outline,
                        &Stroke {
                            // Total stroke = black core 2 + 3 px white
                            // halo on each side.
                            width: 8.0,
                            line_cap: tiny_skia::LineCap::Round,
                            ..Default::default()
                        },
                        Transform::identity(),
                        None,
                    );
                    let mut fill = Paint::default();
                    fill.set_color_rgba8(0, 0, 0, 255);
                    fill.anti_alias = true;
                    pixmap.stroke_path(
                        &path,
                        &fill,
                        &Stroke {
                            width: 2.0,
                            line_cap: tiny_skia::LineCap::Round,
                            ..Default::default()
                        },
                        Transform::identity(),
                        None,
                    );
                }
            }

            // Width / height in LOGICAL pixels. Buffer span / scale,
            // then divided by the configured dimension divisor (1.0
            // by default, > 1.0 when Figma zoom-correction is active).
            let div = if dimension_divisor > 0.0 { dimension_divisor as f32 } else { 1.0 };
            let w_px = (((right_x - left_x) / scale_f) / div).round() as u32;
            let h_px = (((down_y - up_y) / scale_f) / div).round() as u32;

            // "W × H" with the Unicode multiplication sign. The
            // optional unit suffix (e.g. "px") trails the second
            // number, or each number when `wh_indicators` is on —
            // matches the held-rect pill so the live and committed
            // readouts agree.
            let text = if wh_indicators {
                format!(
                    "W: {}{} \u{00D7} H: {}{}",
                    w_px, unit_suffix, h_px, unit_suffix
                )
            } else {
                format!("{} \u{00D7} {}{}", w_px, h_px, unit_suffix)
            };
            let px_size = TEXT_LOGICAL_PX * scale_f;
            // Measure text via fontdue. If the font is missing we still
            // render the pill (just empty) at a sensible width using the
            // average glyph metric.
            let (text_w, ascent, descent) = if let Some(font) = hud_font() {
                let w = measure_text_width(font, &text, px_size);
                let lm = font.horizontal_line_metrics(px_size);
                let (a, d) = lm
                    .map(|m| (m.ascent, -m.descent))
                    .unwrap_or((px_size * 0.8, px_size * 0.2));
                (w, a, d)
            } else {
                (text.len() as f32 * px_size * 0.55, px_size * 0.8, px_size * 0.2)
            };
            let pad_x = 10.0 * scale_f;
            let pad_y = 5.0 * scale_f;
            let pill_w = text_w.ceil() + pad_x * 2.0;
            let pill_h = (ascent + descent).ceil() + pad_y * 2.0;
            // Lower-right of cursor by 14 LOGICAL px each axis.
            let cursor_buf_x = (cursor.0 * scale as f64) as f32;
            let cursor_buf_y = (cursor.1 * scale as f64) as f32;
            let offset = 14.0 * scale_f;
            let mut pill_x = (cursor_buf_x + offset).floor();
            let mut pill_y = (cursor_buf_y + offset).floor();
            pill_x = pill_x.min(surface_w - pill_w - 1.0).max(0.0);
            pill_y = pill_y.min(surface_h - pill_h - 1.0).max(0.0);

            // Slightly translucent dark gray (not pure black). The background still shows through a
            // little, which keeps the pill from looking overweight.
            let mut bg_paint = Paint::default();
            bg_paint.set_color_rgba8(40, 40, 40, 230);
            bg_paint.anti_alias = true;
            if let Some(path) = pill_path(pill_x, pill_y, pill_w, pill_h) {
                pixmap.fill_path(&path, &bg_paint, FillRule::Winding, Transform::identity(), None);
            }

            pills.push(PillLayout {
                text,
                text_x: pill_x + pad_x,
                baseline_y: pill_y + pad_y + ascent,
                px_size,
            });
    }
}

/// Draw the rectangle for an in-progress drag (Drawing) or a committed
/// measurement (Held), plus the W×H and aspect-ratio pills. When
/// `camera_armed` is true, the W×H pill renders a camera icon instead
/// of the dimension text — that signals to the user that clicking will
/// capture the held region as a screenshot.
#[allow(clippy::too_many_arguments)]
fn draw_area_rect(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    a: &(f64, f64),
    b: &(f64, f64),
    buf_w: f32,
    buf_h: f32,
    scale: u32,
    fg: Color,
    fmt: &crate::HudMeasurementFormat,
    stroke: &tiny_skia::Stroke,
    line_paint: &tiny_skia::Paint,
    camera_armed: bool,
    pill_below: bool,
) {
    use tiny_skia::*;
    let scale_f = scale as f32;
    let half = scale_f * 0.5;
    let snap = |v: f64| (v * scale as f64).floor() as f32 + half;
    let ax = snap(a.0);
    let ay = snap(a.1);
    let bx = snap(b.0);
    let by = snap(b.1);
    let rx = ax.min(bx);
    let ry = ay.min(by);
    let rw = (ax - bx).abs();
    let rh = (ay - by).abs();
    if rw < scale_f || rh < scale_f {
        return;
    }
    if let Some(rect) = Rect::from_xywh(rx, ry, rw, rh) {
        // Translucent fill — keeps the underlying content readable.
        let mut fill_paint = Paint::default();
        fill_paint.set_color_rgba8(fg.r, fg.g, fg.b, 40);
        pixmap.fill_rect(rect, &fill_paint, Transform::identity(), None);
        // Solid border at the same stroke as axis lines.
        let mut pb = PathBuilder::new();
        pb.push_rect(rect);
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, line_paint, stroke, Transform::identity(), None);
        }
    }
    let w_logical_f = (rw / scale_f) as f64;
    let h_logical_f = (rh / scale_f) as f64;
    let w_logical = w_logical_f.round() as u32;
    let h_logical = h_logical_f.round() as u32;

    // W × H pill, centered inside the rectangle. When the cursor is
    // over it (camera_armed=true), swap the text for a camera icon
    // while keeping the same pill bounds so the visible chip doesn't
    // jump as you hover in/out.
    let dim_text = if fmt.wh_indicators {
        format!(
            "W: {}{} \u{00D7} H: {}{}",
            format_number(w_logical_f, fmt),
            fmt.unit_suffix,
            format_number(h_logical_f, fmt),
            fmt.unit_suffix,
        )
    } else {
        format!(
            "{} \u{00D7} {}{}",
            format_number(w_logical_f, fmt),
            format_number(h_logical_f, fmt),
            fmt.unit_suffix
        )
    };
    // Drawing-mode pills sit below the rect so the user can see what
    // they're highlighting. Held rects keep the centered position
    // (after snap-shrink they're tight to content, less obscuring).
    let dim_anchor_x = rx + rw * 0.5;
    let (dim_anchor_y, dim_anchor) = if pill_below {
        (ry + rh + 8.0 * scale_f, PillAnchor::AnchorTop)
    } else {
        (ry + rh * 0.5, PillAnchor::Centered)
    };
    if camera_armed {
        // Use the SAME pill bounds and position as the text version
        // would have — when the pill is below the rect, the camera
        // icon goes below too. Then just swap the content (icon
        // instead of text) inside that pill.
        let (pill_w, pill_h) = pill_dimensions_for_text(&dim_text, scale_f);
        let (mut pill_x, mut pill_y) = match dim_anchor {
            PillAnchor::Centered => {
                (dim_anchor_x - pill_w * 0.5, dim_anchor_y - pill_h * 0.5)
            }
            PillAnchor::AnchorTop => (dim_anchor_x - pill_w * 0.5, dim_anchor_y),
            _ => (dim_anchor_x - pill_w * 0.5, dim_anchor_y - pill_h * 0.5),
        };
        pill_x = pill_x.floor().min(buf_w - pill_w - 1.0).max(0.0);
        pill_y = pill_y.floor().min(buf_h - pill_h - 1.0).max(0.0);
        let mut bg_paint = Paint::default();
        bg_paint.set_color_rgba8(40, 40, 40, 230);
        bg_paint.anti_alias = true;
        if let Some(path) = pill_path(pill_x, pill_y, pill_w, pill_h) {
            pixmap.fill_path(&path, &bg_paint, FillRule::Winding, Transform::identity(), None);
        }
        draw_camera_icon(
            pixmap,
            pill_x + pill_w * 0.5,
            pill_y + pill_h * 0.5,
            scale_f,
        );
    } else {
        push_pill(
            pixmap,
            pills,
            dim_text,
            dim_anchor_x,
            dim_anchor_y,
            dim_anchor,
            buf_w,
            buf_h,
            scale_f,
            TEXT_LOGICAL_PX,
        );
    }

    // Aspect ratio pill — sits just below the dimension pill when
    // both are below the rect, otherwise just below the rect.
    let aspect_text = if fmt.aspect_in_area {
        estimate_aspect_text(w_logical, h_logical, fmt.aspect_mode)
    } else {
        None
    };
    if let Some(aspect_text) = aspect_text {
        let aspect_y = if pill_below {
            ry + rh + 8.0 * scale_f + (TEXT_LOGICAL_PX + 2.0 * 5.0) * scale_f + 6.0 * scale_f
        } else {
            ry + rh + 24.0 * scale_f
        };
        push_pill(
            pixmap,
            pills,
            aspect_text,
            rx + rw * 0.5,
            aspect_y,
            PillAnchor::AnchorTop,
            buf_w,
            buf_h,
            scale_f,
            TEXT_LOGICAL_PX,
        );
    }
}

/// Format the aspect-ratio pill for the area tool. Delegates to the
/// shared `vernier_core::aspect` classifier so the pill respects the
/// user's configured `AspectMode` (Automatic / Standard / Reduced /
/// CommonOnly). Returns `None` when the configured mode declines to
/// report a ratio (CommonOnly with no curated match).
fn estimate_aspect_text(
    width: u32,
    height: u32,
    mode: vernier_core::AspectMode,
) -> Option<String> {
    use vernier_core::{CommonRatio, Ratio};
    if width == 0 || height == 0 {
        return None;
    }
    let ratio = vernier_core::classify_aspect(width, height, mode, 0.02)?;
    let (n, d) = match ratio {
        Ratio::Common(c) => match c {
            CommonRatio::R16x9 => (16, 9),
            CommonRatio::R4x3 => (4, 3),
            CommonRatio::R1x1 => (1, 1),
            CommonRatio::R21x9 => (21, 9),
            CommonRatio::R16x10 => (16, 10),
            CommonRatio::R5x4 => (5, 4),
            CommonRatio::R3x2 => (3, 2),
            CommonRatio::R2x1 => (2, 1),
            CommonRatio::R9x16 => (9, 16),
            CommonRatio::R3x4 => (3, 4),
        },
        Ratio::Reduced { num, den } => (num, den),
    };
    Some(format!("{} : {}", n, d))
}

#[derive(Copy, Clone)]
enum PillAnchor {
    /// Position pill so its center lands at (anchor_x, anchor_y).
    Centered,
    /// Position pill so its top-center lands at (anchor_x, anchor_y).
    AnchorTop,
    /// Position pill so its top-right lands at (anchor_x, anchor_y).
    AnchorTopRight,
    /// Position pill so its left edge sits at `anchor_x` and its
    /// vertical center sits at `anchor_y`.
    LeftCenter,
    /// Lower-right of the anchor by the given buffer-pixel offset.
    #[allow(dead_code)]
    BelowRight(f32),
}

#[allow(clippy::too_many_arguments)]
fn push_pill(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    text: String,
    anchor_x: f32,
    anchor_y: f32,
    anchor: PillAnchor,
    surface_w: f32,
    surface_h: f32,
    scale_f: f32,
    text_logical_px: f32,
) {
    use tiny_skia::*;
    let Some(font) = hud_font() else { return; };
    let px_size = text_logical_px * scale_f;
    let text_w = measure_text_width(font, &text, px_size);
    let (ascent, descent) = font
        .horizontal_line_metrics(px_size)
        .map(|m| (m.ascent, -m.descent))
        .unwrap_or((px_size * 0.8, px_size * 0.2));
    // Padding scales with the chosen text size so smaller pills stay
    // visually balanced (8/4 ratio matches the active pill's 10/5 at
    // 12.5 px).
    let pad_x = 0.8 * text_logical_px * scale_f;
    let pad_y = 0.4 * text_logical_px * scale_f;
    let pill_w = text_w.ceil() + pad_x * 2.0;
    let pill_h = (ascent + descent).ceil() + pad_y * 2.0;

    let (mut pill_x, mut pill_y) = match anchor {
        PillAnchor::Centered => (anchor_x - pill_w * 0.5, anchor_y - pill_h * 0.5),
        PillAnchor::AnchorTop => (anchor_x - pill_w * 0.5, anchor_y),
        PillAnchor::AnchorTopRight => (anchor_x - pill_w, anchor_y),
        PillAnchor::LeftCenter => (anchor_x, anchor_y - pill_h * 0.5),
        PillAnchor::BelowRight(off) => (anchor_x + off, anchor_y + off),
    };
    pill_x = pill_x.floor().min(surface_w - pill_w - 1.0).max(0.0);
    pill_y = pill_y.floor().min(surface_h - pill_h - 1.0).max(0.0);

    let mut bg_paint = Paint::default();
    bg_paint.set_color_rgba8(40, 40, 40, 230);
    bg_paint.anti_alias = true;
    if let Some(path) = pill_path(pill_x, pill_y, pill_w, pill_h) {
        pixmap.fill_path(&path, &bg_paint, FillRule::Winding, Transform::identity(), None);
    }
    pills.push(PillLayout {
        text,
        text_x: pill_x + pad_x,
        baseline_y: pill_y + pad_y + ascent,
        px_size,
    });
}

/// Rasterize the dimension-readout text into the buffer using fontdue.
/// Each glyph's grayscale alpha bitmap is alpha-blended onto the pill
/// background that `render_hud_strokes` already drew. The buffer is
/// premultiplied RGBA, and the source is fully-opaque white at the
/// glyph's per-pixel alpha — so premul source = (a, a, a, a) and
/// `out = src + dst * (1 - src.a)` reduces to the inner block here.
fn render_pill_text(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    font: &fontdue::Font,
    layout: &PillLayout,
) {
    let mut pen_x = layout.text_x;
    let baseline = layout.baseline_y;
    for ch in layout.text.chars() {
        let active = font_for_char(font, ch);
        let (metrics, bitmap) = active.rasterize(ch, layout.px_size);
        let glyph_origin_x = pen_x + metrics.xmin as f32;
        // The Omarchy SUPER logo (U+E900) is drawn to the top of the em
        // box rather than the cap height, so at the same baseline it
        // floats noticeably above neighbouring letters. Nudge it down
        // ~1 logical px (≈ 10% of the px size, scale-aware via the
        // font size already being in physical px) so it sits on the
        // shared visual baseline of the shortcut row.
        let y_bias = if ch == '\u{e900}' {
            layout.px_size * 0.10
        } else {
            0.0
        };
        let glyph_origin_y =
            baseline - metrics.ymin as f32 - metrics.height as f32 + y_bias;
        composite_glyph(
            canvas,
            buf_w,
            buf_h,
            &bitmap,
            metrics.width as u32,
            metrics.height as u32,
            glyph_origin_x,
            glyph_origin_y,
        );
        pen_x += metrics.advance_width;
    }
}

fn composite_glyph(
    canvas: &mut [u8],
    buf_w: u32,
    buf_h: u32,
    bitmap: &[u8],
    glyph_w: u32,
    glyph_h: u32,
    pos_x: f32,
    pos_y: f32,
) {
    if glyph_w == 0 || glyph_h == 0 {
        return;
    }
    let base_x = pos_x.round() as i32;
    let base_y = pos_y.round() as i32;
    for j in 0..glyph_h as i32 {
        let y = base_y + j;
        if y < 0 || y as u32 >= buf_h {
            continue;
        }
        for i in 0..glyph_w as i32 {
            let x = base_x + i;
            if x < 0 || x as u32 >= buf_w {
                continue;
            }
            let alpha = bitmap[(j as u32 * glyph_w + i as u32) as usize];
            if alpha == 0 {
                continue;
            }
            let idx = (y as u32 * buf_w + x as u32) as usize * 4;
            let inv = 255u16 - alpha as u16;
            // Source is opaque white at `alpha`; premultiplied = (a,a,a,a).
            // out = src + dst * (1 - src.a)
            //     = alpha + dst * inv / 255 (per channel, including alpha)
            canvas[idx] = (alpha as u16 + (canvas[idx] as u16 * inv) / 255) as u8;
            canvas[idx + 1] = (alpha as u16 + (canvas[idx + 1] as u16 * inv) / 255) as u8;
            canvas[idx + 2] = (alpha as u16 + (canvas[idx + 2] as u16 * inv) / 255) as u8;
            canvas[idx + 3] = (alpha as u16 + (canvas[idx + 3] as u16 * inv) / 255) as u8;
        }
    }
}

/// Compute the pill dimensions (in buffer pixels) that would house
/// `text` at the HUD's standard text size. Used by both the text-pill
/// path and the camera-icon path so the pill bounds stay stable when
/// the cursor hovers in / out of the held rect.
fn pill_dimensions_for_text(text: &str, scale_f: f32) -> (f32, f32) {
    let px_size = TEXT_LOGICAL_PX * scale_f;
    let (text_w, ascent, descent) = if let Some(font) = hud_font() {
        let w = measure_text_width(font, text, px_size);
        let (a, d) = font
            .horizontal_line_metrics(px_size)
            .map(|m| (m.ascent, -m.descent))
            .unwrap_or((px_size * 0.8, px_size * 0.2));
        (w, a, d)
    } else {
        (text.len() as f32 * px_size * 0.55, px_size * 0.8, px_size * 0.2)
    };
    let pad_x = 10.0 * scale_f;
    let pad_y = 5.0 * scale_f;
    (text_w.ceil() + pad_x * 2.0, (ascent + descent).ceil() + pad_y * 2.0)
}

/// Tiny line-art camera icon centered at `(cx, cy)`. Sized in LOGICAL
/// pixels and multiplied by `scale_f` so it's crisp at HiDPI.
fn draw_camera_icon(pixmap: &mut tiny_skia::PixmapMut, cx: f32, cy: f32, scale_f: f32) {
    use tiny_skia::*;
    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 245);
    white.anti_alias = true;
    let mut dark = Paint::default();
    dark.set_color_rgba8(35, 35, 35, 255);
    dark.anti_alias = true;

    // Body geometry — sized smaller than the pill so it sits with
    // visible margin around it.
    let body_w = 17.0 * scale_f;
    let body_h = 10.0 * scale_f;
    let body_x = cx - body_w * 0.5;
    let body_y = cy - body_h * 0.5 + 0.75 * scale_f;
    let radius = 1.25 * scale_f;

    // Bump (small viewfinder/hot-shoe bar) just above the body, slightly
    // offset to one side for a less-symmetrical, more iconic camera shape.
    let bump_w = 5.0 * scale_f;
    let bump_h = 1.6 * scale_f;
    let bump_x = cx - body_w * 0.5 + 2.0 * scale_f;
    let bump_y = body_y - bump_h + 0.4 * scale_f;
    if let Some(rect) = Rect::from_xywh(bump_x, bump_y, bump_w, bump_h) {
        pixmap.fill_rect(rect, &white, Transform::identity(), None);
    }

    // Body — rounded rect built from cubic-free quad corners.
    let bx2 = body_x + body_w;
    let by2 = body_y + body_h;
    let mut pb = PathBuilder::new();
    pb.move_to(body_x + radius, body_y);
    pb.line_to(bx2 - radius, body_y);
    pb.quad_to(bx2, body_y, bx2, body_y + radius);
    pb.line_to(bx2, by2 - radius);
    pb.quad_to(bx2, by2, bx2 - radius, by2);
    pb.line_to(body_x + radius, by2);
    pb.quad_to(body_x, by2, body_x, by2 - radius);
    pb.line_to(body_x, body_y + radius);
    pb.quad_to(body_x, body_y, body_x + radius, body_y);
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
    }

    // Lens (dark filled circle) and a small highlight for liveliness.
    let lens_cx = cx;
    let lens_cy = body_y + body_h * 0.5;
    let lens_r = 2.7 * scale_f;
    let mut pb = PathBuilder::new();
    pb.push_circle(lens_cx, lens_cy, lens_r);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &dark, FillRule::Winding, Transform::identity(), None);
    }
    let hi_r = 0.8 * scale_f;
    let mut pb = PathBuilder::new();
    pb.push_circle(lens_cx + 0.8 * scale_f, lens_cy - 0.8 * scale_f, hi_r);
    if let Some(path) = pb.finish() {
        pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
    }
}

/// Standard left-pointer arrow drawn at `(cx, cy)` (top-left tip).
/// Rendered ourselves because we hide the system pointer for the whole
/// measurement session — when the user is inside the held region we
/// want them to see a click-affordance pointer in software.
fn draw_arrow_cursor(pixmap: &mut tiny_skia::PixmapMut, cx: f32, cy: f32, scale_f: f32) {
    use tiny_skia::*;
    let s = scale_f;
    // Slimmer and slightly taller — closer to the Hyprland default
    // silhouette in image #30 (sharp tip, refined tail).
    let pts: [(f32, f32); 7] = [
        (0.0, 0.0),    // sharp tip
        (0.0, 17.0),   // bottom of left edge
        (4.5, 13.5),   // notch where tail starts
        (7.5, 18.0),   // tail bottom-left
        (9.0, 17.5),   // tail bottom-right
        (5.5, 11.5),   // right notch
        (10.5, 11.0),  // top of right edge
    ];
    let mut pb = PathBuilder::new();
    pb.move_to(cx + pts[0].0 * s, cy + pts[0].1 * s);
    for p in &pts[1..] {
        pb.line_to(cx + p.0 * s, cy + p.1 * s);
    }
    pb.close();
    let path = match pb.finish() {
        Some(p) => p,
        None => return,
    };
    // Hyprland-style pointer: black body with a thin white halo.
    // Stroke white first (forms the outline), then fill black on top
    // so the halo is visible only along the arrow's edge.
    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 255);
    white.anti_alias = true;
    let mut black = Paint::default();
    black.set_color_rgba8(0, 0, 0, 255);
    black.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = 2.0;
    stroke.line_join = LineJoin::Miter;
    pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
    pixmap.fill_path(&path, &black, FillRule::Winding, Transform::identity(), None);
}

/// Toast pill ("Tolerance: High" / "Screenshot taken"). Anchored in
/// the lower third of the buffer — far enough below the cursor that
/// it doesn't visually fight the measurement crosshair, close enough
/// to bottom that the user's gaze doesn't have to leave the work.
fn draw_toast(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    text: &str,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    let px_size = TOAST_TEXT_LOGICAL_PX * scale_f;
    let (text_w, ascent, descent) = if let Some(font) = hud_font() {
        let w = measure_text_width(font, text, px_size);
        let (a, d) = font
            .horizontal_line_metrics(px_size)
            .map(|m| (m.ascent, -m.descent))
            .unwrap_or((px_size * 0.8, px_size * 0.2));
        (w, a, d)
    } else {
        (text.len() as f32 * px_size * 0.55, px_size * 0.8, px_size * 0.2)
    };
    let pad_x = 22.0 * scale_f;
    let pad_y = 12.0 * scale_f;
    let pill_w = text_w.ceil() + pad_x * 2.0;
    let pill_h = (ascent + descent).ceil() + pad_y * 2.0;
    let pill_x = ((buf_w - pill_w) * 0.5).floor().max(0.0);
    // Lower-third anchor: pill top at ~2/3 of the buffer height so the
    // pill body sits inside the bottom third regardless of resolution.
    let pill_y = (buf_h * 2.0 / 3.0).floor().max(0.0);

    let mut bg = Paint::default();
    bg.set_color_rgba8(20, 20, 20, 235);
    bg.anti_alias = true;
    if let Some(path) = pill_path(pill_x, pill_y, pill_w, pill_h) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, Transform::identity(), None);
    }
    pills.push(PillLayout {
        text: text.to_string(),
        text_x: (pill_x + pad_x).round(),
        baseline_y: (pill_y + pad_y + ascent).round(),
        px_size,
    });
}

/// Render a logical-pixel measurement value with the user's
/// configured rounding mode. No unit suffix is appended — callers
/// add it for single-value pills and omit it for W×H pills.
fn format_number(value_logical: f64, fmt: &crate::HudMeasurementFormat) -> String {
    use crate::HudRounding::*;
    let divisor = if fmt.dimension_divisor > 0.0 {
        fmt.dimension_divisor
    } else {
        1.0
    };
    let value = value_logical / divisor;
    match fmt.rounding {
        Points => {
            let r = (value * 10.0).round() / 10.0;
            if (r - r.round()).abs() < f64::EPSILON {
                format!("{}", r as i64)
            } else {
                format!("{r:.1}")
            }
        }
        PointsRounded => format!("{}", value.round() as i64),
        ScreenPixels => format!("{}", (value * fmt.scale_factor).round() as i64),
    }
}

fn format_value(value_logical: f64, fmt: &crate::HudMeasurementFormat) -> String {
    format!("{}{}", format_number(value_logical, fmt), fmt.unit_suffix)
}

/// Right-click context menu — floating list of actions anchored at
/// the cursor where the right-click happened. Drawn last (on top of
/// every other HUD layer including the toast). Hovered row gets a
/// lighter bg; each row is icon + label + optional shortcut hint.
fn draw_context_menu(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    menu: &crate::HudContextMenu,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    let Some(font) = hud_font() else { return };

    const ROW_H: f32 = 32.0;
    const RADIUS: f32 = 12.0;
    const PAD_X: f32 = 14.0;
    const PAD_Y: f32 = 10.0;
    const ICON_COL_W: f32 = 32.0;
    const SHORTCUT_GAP: f32 = 16.0;
    const DIV_PAD_V: f32 = 8.0;
    const DIV_HEIGHT: f32 = 1.0;

    let label_px = TEXT_LOGICAL_PX * scale_f;
    let shortcut_px = TEXT_STUCK_LOGICAL_PX * scale_f;

    let icon_col = ICON_COL_W * scale_f;
    let pad_x = PAD_X * scale_f;
    let pad_y = PAD_Y * scale_f;
    let row_h = ROW_H * scale_f;
    let radius = RADIUS * scale_f;
    let div_pad_v = DIV_PAD_V * scale_f;
    let div_h = DIV_HEIGHT * scale_f;
    let _ = SHORTCUT_GAP; // kept for parity with hit-tester

    let inner_label_x = pad_x + icon_col;
    let menu_w = (menu.width as f32) * scale_f;

    let mut content_h = pad_y * 2.0;
    for (i, it) in menu.items.iter().enumerate() {
        content_h += row_h;
        if it.divider_after && i + 1 < menu.items.len() {
            content_h += 2.0 * div_pad_v + div_h;
        }
    }

    let mx = (menu.origin.0 as f32) * scale_f;
    let my = (menu.origin.1 as f32) * scale_f;
    let mx = mx.min(buf_w - menu_w - 1.0).max(0.0);
    let my = my.min(buf_h - content_h - 1.0).max(0.0);

    // Drop any pre-existing (measurement) pills whose text would
    // bleed through the menu — the menu sits on top, so its area
    // should be clean. Menu pills themselves are pushed below this
    // filter, so they're not affected.
    pills.retain(|p| !pill_text_overlaps_rect(p, mx, my, menu_w, content_h, font));

    let mut bg = Paint::default();
    bg.set_color_rgba8(22, 22, 22, 248);
    bg.anti_alias = true;
    if let Some(path) = rounded_rect_path(mx, my, menu_w, content_h, radius) {
        pixmap.fill_path(&path, &bg, FillRule::Winding, Transform::identity(), None);
    }

    let mut row_y = my + pad_y;
    for (i, it) in menu.items.iter().enumerate() {
        if menu.hovered == Some(i) {
            let mut hbg = Paint::default();
            hbg.set_color_rgba8(48, 48, 48, 235);
            hbg.anti_alias = true;
            let inset = pad_x * 0.5;
            if let Some(path) =
                rounded_rect_path(mx + inset, row_y, menu_w - inset * 2.0, row_h, radius * 0.5)
            {
                pixmap.fill_path(&path, &hbg, FillRule::Winding, Transform::identity(), None);
            }
        }

        let icon_cx = mx + pad_x + icon_col * 0.5;
        let icon_cy = row_y + row_h * 0.5;
        draw_menu_icon(pixmap, it.icon, icon_cx, icon_cy, scale_f);

        let (l_asc, l_desc) = font
            .horizontal_line_metrics(label_px)
            .map(|m| (m.ascent, -m.descent))
            .unwrap_or((label_px * 0.8, label_px * 0.2));
        pills.push(PillLayout {
            text: it.label.clone(),
            text_x: (mx + inner_label_x).round(),
            baseline_y: (icon_cy + (l_asc - l_desc) * 0.5).round(),
            px_size: label_px,
        });

        if let Some(s) = &it.shortcut {
            let sw = measure_text_width(font, s, shortcut_px);
            let (s_asc, s_desc) = font
                .horizontal_line_metrics(shortcut_px)
                .map(|m| (m.ascent, -m.descent))
                .unwrap_or((shortcut_px * 0.8, shortcut_px * 0.2));
            let shortcut_x_end = mx + menu_w - pad_x;
            pills.push(PillLayout {
                text: s.clone(),
                text_x: (shortcut_x_end - sw).round(),
                baseline_y: (icon_cy + (s_asc - s_desc) * 0.5).round(),
                px_size: shortcut_px,
            });
        }

        row_y += row_h;
        if it.divider_after && i + 1 < menu.items.len() {
            row_y += div_pad_v;
            let mut dpaint = Paint::default();
            dpaint.set_color_rgba8(60, 60, 60, 235);
            dpaint.anti_alias = false;
            let dx0 = mx + pad_x;
            let dx1 = mx + menu_w - pad_x;
            let mut dpb = PathBuilder::new();
            dpb.move_to(dx0, row_y);
            dpb.line_to(dx1, row_y);
            dpb.line_to(dx1, row_y + div_h);
            dpb.line_to(dx0, row_y + div_h);
            dpb.close();
            if let Some(path) = dpb.finish() {
                pixmap.fill_path(&path, &dpaint, FillRule::Winding, Transform::identity(), None);
            }
            row_y += div_h + div_pad_v;
        }
    }
}

/// True when `pill`'s rasterized text region intersects the rect
/// `(mx, my, mw, mh)`. Used by the context menu to suppress
/// underlying measurement pill text from bleeding through.
fn pill_text_overlaps_rect(
    pill: &PillLayout,
    mx: f32,
    my: f32,
    mw: f32,
    mh: f32,
    font: &fontdue::Font,
) -> bool {
    let text_w = measure_text_width(font, &pill.text, pill.px_size);
    let p_left = pill.text_x;
    let p_right = pill.text_x + text_w;
    let p_top = pill.baseline_y - pill.px_size;
    let p_bot = pill.baseline_y + pill.px_size * 0.3;
    p_right > mx && p_left < mx + mw && p_bot > my && p_top < my + mh
}

/// Build a path for a rectangle with all four corners rounded by
/// radius `r`. `r` is clamped to `min(w/2, h/2)`.
fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    use tiny_skia::PathBuilder;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
    let k = r * 0.5523;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Render the small (~16 logical px) icon for a context-menu row.
/// `cx`/`cy` are the icon's center in BUFFER pixels.
fn draw_menu_icon(
    pixmap: &mut tiny_skia::PixmapMut,
    icon: crate::HudContextMenuIcon,
    cx: f32,
    cy: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    let mut accent = Paint::default();
    accent.set_color_rgba8(120, 180, 255, 240);
    accent.anti_alias = true;
    let mut coral = Paint::default();
    coral.set_color_rgba8(0xFF, 0x5C, 0x5C, 245);
    coral.anti_alias = true;
    let mut white = Paint::default();
    white.set_color_rgba8(220, 220, 220, 240);
    white.anti_alias = true;
    let stroke = Stroke {
        width: 1.5 * scale_f,
        line_cap: LineCap::Round,
        ..Default::default()
    };

    use crate::HudContextMenuIcon as I;
    match icon {
        I::GuideH => {
            let half = 8.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - half, cy);
            pb.line_to(cx + half, cy);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &accent, &stroke, Transform::identity(), None);
            }
        }
        I::GuideV => {
            let half = 8.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - half);
            pb.line_to(cx, cy + half);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &accent, &stroke, Transform::identity(), None);
            }
        }
        I::StuckH => {
            let len = 6.0 * scale_f;
            let cap = 4.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - len, cy);
            pb.line_to(cx + len, cy);
            pb.move_to(cx - len, cy - cap);
            pb.line_to(cx - len, cy + cap);
            pb.move_to(cx + len, cy - cap);
            pb.line_to(cx + len, cy + cap);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &coral, &stroke, Transform::identity(), None);
            }
        }
        I::StuckV => {
            let len = 6.0 * scale_f;
            let cap = 4.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - len);
            pb.line_to(cx, cy + len);
            pb.move_to(cx - cap, cy - len);
            pb.line_to(cx + cap, cy - len);
            pb.move_to(cx - cap, cy + len);
            pb.line_to(cx + cap, cy + len);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &coral, &stroke, Transform::identity(), None);
            }
        }
        I::Camera => {
            let bw = 12.0 * scale_f;
            let bh = 8.0 * scale_f;
            let bx = cx - bw * 0.5;
            let by = cy - bh * 0.5 + 1.0 * scale_f;
            if let Some(path) = rounded_rect_path(bx, by, bw, bh, 1.5 * scale_f) {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let mut pb = PathBuilder::new();
            pb.push_circle(cx, cy + 1.0 * scale_f, 2.0 * scale_f);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let bump_w = 4.0 * scale_f;
            let bump_h = 2.0 * scale_f;
            let bump_x = cx - bump_w * 0.5;
            let bump_y = by - bump_h;
            let mut pb = PathBuilder::new();
            pb.move_to(bump_x, bump_y);
            pb.line_to(bump_x + bump_w, bump_y);
            pb.line_to(bump_x + bump_w, bump_y + bump_h);
            pb.line_to(bump_x, bump_y + bump_h);
            pb.close();
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
        }
        I::Background => {
            let s = 12.0 * scale_f;
            let x = cx - s * 0.5;
            let y = cy - s * 0.5;
            if let Some(path) = rounded_rect_path(x, y, s, s, 2.0 * scale_f) {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let dot_r = 1.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.push_circle(cx - 2.5 * scale_f, cy + 1.0 * scale_f, dot_r);
            pb.push_circle(cx + 2.5 * scale_f, cy + 1.0 * scale_f, dot_r);
            if let Some(path) = pb.finish() {
                pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
            }
        }
        I::Restore => {
            let r = 5.0 * scale_f;
            let k = r * 0.5523;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - r, cy);
            pb.cubic_to(cx - r, cy + k, cx - k, cy + r, cx, cy + r);
            pb.cubic_to(cx + k, cy + r, cx + r, cy + k, cx + r, cy);
            pb.cubic_to(cx + r, cy - k, cx + k, cy - r, cx, cy - r);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let a = 2.5 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx, cy - r);
            pb.line_to(cx - a, cy - r - a);
            pb.move_to(cx, cy - r);
            pb.line_to(cx + a, cy - r - a);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
        }
        I::Clear => {
            let bw = 8.0 * scale_f;
            let bh = 9.0 * scale_f;
            let bx = cx - bw * 0.5;
            let by = cy - bh * 0.5 + 1.5 * scale_f;
            if let Some(path) = rounded_rect_path(bx, by, bw, bh, 1.5 * scale_f) {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
            let lid_w = 11.0 * scale_f;
            let lid_x = cx - lid_w * 0.5;
            let lid_y = by - 1.5 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(lid_x, lid_y);
            pb.line_to(lid_x + lid_w, lid_y);
            let h_w = 4.0 * scale_f;
            let h_x = cx - h_w * 0.5;
            pb.move_to(h_x, lid_y);
            pb.line_to(h_x, lid_y - 1.5 * scale_f);
            pb.line_to(h_x + h_w, lid_y - 1.5 * scale_f);
            pb.line_to(h_x + h_w, lid_y);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
        }
        I::Close => {
            let s = 5.0 * scale_f;
            let mut pb = PathBuilder::new();
            pb.move_to(cx - s, cy - s);
            pb.line_to(cx + s, cy + s);
            pb.move_to(cx + s, cy - s);
            pb.line_to(cx - s, cy + s);
            if let Some(path) = pb.finish() {
                pixmap.stroke_path(&path, &white, &stroke, Transform::identity(), None);
            }
        }
    }
}

/// Build a horizontal pill path (rectangle with fully-rounded ends).
/// `w` must be ≥ `h`; otherwise returns `None`.
fn pill_path(x: f32, y: f32, w: f32, h: f32) -> Option<tiny_skia::Path> {
    use tiny_skia::PathBuilder;
    if w < h {
        return None;
    }
    let r = h * 0.5;
    let cy = y + r;
    // Cubic Bezier circle approximation: control offset = r * 0.5523.
    let k = r * 0.5523;
    let mut pb = PathBuilder::new();
    // Top edge (left-corner end → right-corner start).
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    // Right cap as two cubic quarters.
    pb.cubic_to(x + w - r + k, y, x + w, cy - k, x + w, cy);
    pb.cubic_to(x + w, cy + k, x + w - r + k, y + h, x + w - r, y + h);
    // Bottom edge.
    pb.line_to(x + r, y + h);
    // Left cap.
    pb.cubic_to(x + r - k, y + h, x, cy + k, x, cy);
    pb.cubic_to(x, cy - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

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

/// Pre-multiplied RGBA, stored in memory as R G B A. Matches both
/// tiny-skia's `PremultipliedColorU8` byte layout and wl_shm's
/// `Abgr8888` format.
fn rgba8888_premul(c: Color) -> [u8; 4] {
    let a = c.a as u16;
    let r = (c.r as u16 * a / 255) as u8;
    let g = (c.g as u16 * a / 255) as u8;
    let b = (c.b as u16 * a / 255) as u8;
    [r, g, b, c.a]
}
