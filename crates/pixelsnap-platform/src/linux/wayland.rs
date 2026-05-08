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
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
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
    Connection, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
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

    fn draw_overlay(&mut self, key: OverlayKey) {
        let Some(inst) = self.overlays.get_mut(&key) else {
            return;
        };
        log::debug!(
            "draw_overlay key={:?} configured={} {}x{} visible={}",
            key, inst.configured, inst.width, inst.height, inst.visible_intent
        );
        if !inst.configured || inst.width == 0 || inst.height == 0 {
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
            match self.seat_state.get_keyboard(qh, &seat, None) {
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
            let (monitor, capturing) = match self
                .overlays
                .values()
                .find(|inst| inst.layer.wl_surface().id() == surf_id)
            {
                Some(inst) => (inst.monitor, inst.input_capturing),
                None => continue,
            };
            // On Enter, hide the system cursor while measuring so the
            // user's actual mouse pointer doesn't obscure the snap
            // lines. The cursor is already excluded from the captured
            // PipeWire stream (CursorMode::Hidden), so this is purely a
            // display-side hide.
            if let PointerEventKind::Enter { serial } = ev.kind {
                if capturing {
                    pointer.set_cursor(serial, None, 0, 0);
                }
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
            });
        }
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
        // Auto-repeat events: ignore. We only care about discrete press/release
        // for ESC and friends.
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

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_layer!(WaylandState);
delegate_shm!(WaylandState);
delegate_seat!(WaylandState);
delegate_pointer!(WaylandState);
delegate_keyboard!(WaylandState);
delegate_registry!(WaylandState);

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

fn measure_text_width(font: &fontdue::Font, text: &str, px_size: f32) -> f32 {
    text.chars()
        .map(|c| font.metrics(c, px_size).advance_width)
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
    let scale_f = scale as f32;

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
    if let HudKind::Hover { cursor, edges } = &hud.kind {
        if hud.cursor_in_rect {
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
                fg,
                &paint,
                &stroke,
                &tick_stroke,
                true,
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
                fg,
                &paint,
                &stroke,
                &tick_stroke,
                false,
            );
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
    pills
}

/// Draw frozen single-axis measurements — coral line + tick caps +
/// pill with the pixel count. Same visual language as the live
/// crosshair so the user reads them as "stuck" measurements.
fn draw_stuck_measurements(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    measurements: &[crate::StuckMeasurement],
    fg: Color,
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
        let length = (m.end - m.start).abs();
        let value_text = format!("{}", length);
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
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    use tiny_skia::*;
    use crate::GuideAxis;
    let mut paint = Paint::default();
    paint.set_color_rgba8(0x42, 0x9C, 0xFF, 0xF5);
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
            if let Some((cx, cy)) = cursor {
                let (badge_x, badge_y) = match guide.axis {
                    GuideAxis::Horizontal => (cx as f32 * scale_f, pos),
                    GuideAxis::Vertical => (pos, cy as f32 * scale_f),
                };
                draw_remove_x_badge(pixmap, pills, badge_x, badge_y, buf_w, buf_h, scale_f);
            }
        }
    }
}

/// Small oval "remove" pill with a `×` glyph, drawn at the cursor
/// position on a hovered guide. Same size and styling as the
/// stuck-measurement pill so the affordance reads consistently.
fn draw_remove_x_badge(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    cx: f32,
    cy: f32,
    buf_w: f32,
    buf_h: f32,
    scale_f: f32,
) {
    push_pill(
        pixmap,
        pills,
        "\u{00D7}".to_string(),
        cx,
        cy,
        PillAnchor::Centered,
        buf_w,
        buf_h,
        scale_f,
        TEXT_STUCK_LOGICAL_PX,
    );
}

/// Draw the live measure crosshair: axis lines through the cursor with
/// tick caps where edges were detected, plus the white `+` cursor
/// marker on top. When `show_dim_pill` is true, also emits a W×H pill
/// in the lower-right of the cursor (Hover mode); Held mode passes
/// false because the held rectangle has its own central pill.
#[allow(clippy::too_many_arguments)]
fn draw_hover_indicators(
    pixmap: &mut tiny_skia::PixmapMut,
    pills: &mut Vec<PillLayout>,
    cursor: &(f64, f64),
    edges: &[Option<crate::HudEdge>; 4],
    buf_w: f32,
    buf_h: f32,
    scale: u32,
    fg: Color,
    paint: &tiny_skia::Paint,
    stroke: &tiny_skia::Stroke,
    tick_stroke: &tiny_skia::Stroke,
    show_dim_pill: bool,
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

            // Width / height in LOGICAL pixels. Buffer span / scale.
            let w_px = ((right_x - left_x) / scale_f).round() as u32;
            let h_px = ((down_y - up_y) / scale_f).round() as u32;

            // Match macOS format: "W × H" with the Unicode
            // multiplication sign, no "px" suffix.
            let text = format!("{} \u{00D7} {}", w_px, h_px);
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
    let w_logical = (rw / scale_f).round() as u32;
    let h_logical = (rh / scale_f).round() as u32;

    // W × H pill, centered inside the rectangle. When the cursor is
    // over it (camera_armed=true), swap the text for a camera icon
    // while keeping the same pill bounds so the visible chip doesn't
    // jump as you hover in/out.
    let dim_text = format!("{} \u{00D7} {}", w_logical, h_logical);
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
    if let Some(aspect_text) = estimate_aspect_text(w_logical, h_logical) {
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

/// Find the simplest fraction approximating `width : height`. Returns
/// the ratio formatted as either `A : B` (exact match against a curated
/// common ratio) or `~ A : B` (approximate). Returns `None` if nothing
/// within tolerance bounds is found.
///
/// Approach: first check a curated list of "real" display/photo ratios
/// (16:9, 4:3, etc.) within 2% — those get displayed as-is. If nothing
/// matches there, enumerate fractions by smallest denominator first and
/// return the first one within 4%, marked approximate.
fn estimate_aspect_text(width: u32, height: u32) -> Option<String> {
    if width == 0 || height == 0 {
        return None;
    }
    let target = width as f64 / height as f64;

    // Real-world display, photography, and print ratios. Both
    // orientations included so portrait rectangles match too.
    const CURATED: &[(u32, u32)] = &[
        (1, 1),
        (16, 9),
        (9, 16),
        (4, 3),
        (3, 4),
        (16, 10),
        (10, 16),
        (21, 9),
        (9, 21),
        (3, 2),
        (2, 3),
        (5, 4),
        (4, 5),
        (5, 3),
        (3, 5),
        (2, 1),
        (1, 2),
    ];

    let mut best_curated: Option<((u32, u32), f64)> = None;
    for &(n, d) in CURATED {
        let r = n as f64 / d as f64;
        let err = (r - target).abs() / target;
        if err <= 0.02 && best_curated.map_or(true, |(_, e)| err < e) {
            best_curated = Some(((n, d), err));
        }
    }
    if let Some(((n, d), err)) = best_curated {
        let prefix = if err < 0.005 { "" } else { "\u{223C} " };
        return Some(format!("{}{} : {}", prefix, n, d));
    }

    // Smallest-denominator approximation. 4% tolerance picks "simple"
    // fractions like 7 : 5 for slightly-off ratios where no curated
    // option fits.
    for b in 1..=10u32 {
        let a = (target * b as f64).round() as u32;
        if a == 0 {
            continue;
        }
        if gcd_u32(a, b) != 1 {
            continue;
        }
        let r = a as f64 / b as f64;
        let err = (r - target).abs() / target;
        if err <= 0.04 {
            return Some(format!("\u{223C} {} : {}", a, b));
        }
    }
    None
}

fn gcd_u32(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd_u32(b, a % b) }
}

#[derive(Copy, Clone)]
enum PillAnchor {
    /// Position pill so its center lands at (anchor_x, anchor_y).
    Centered,
    /// Position pill so its top-center lands at (anchor_x, anchor_y).
    AnchorTop,
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
        let (metrics, bitmap) = font.rasterize(ch, layout.px_size);
        let glyph_origin_x = pen_x + metrics.xmin as f32;
        let glyph_origin_y = baseline - metrics.ymin as f32 - metrics.height as f32;
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
    let pts: [(f32, f32); 7] = [
        (0.0, 0.0),
        (0.0, 14.0),
        (4.0, 11.0),
        (6.5, 16.5),
        (9.0, 15.5),
        (6.5, 10.0),
        (12.0, 10.0),
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
    let mut black = Paint::default();
    black.set_color_rgba8(0, 0, 0, 220);
    black.anti_alias = true;
    let mut white = Paint::default();
    white.set_color_rgba8(255, 255, 255, 255);
    white.anti_alias = true;
    let mut stroke = Stroke::default();
    stroke.width = 2.0 * s;
    stroke.line_join = LineJoin::Miter;
    pixmap.stroke_path(&path, &black, &stroke, Transform::identity(), None);
    pixmap.fill_path(&path, &white, FillRule::Winding, Transform::identity(), None);
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

fn stroke_circle(
    pixmap: &mut tiny_skia::PixmapMut,
    cx: f32,
    cy: f32,
    radius: f32,
    paint: &tiny_skia::Paint,
) {
    use tiny_skia::*;
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, radius);
    if let Some(path) = pb.finish() {
        pixmap.stroke_path(
            &path,
            paint,
            &Stroke {
                width: 1.5,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
    }
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
