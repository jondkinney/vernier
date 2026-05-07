use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use vernier_core::{
    classify_aspect, detect_edges, shrink_to_content, AspectMode, EdgeQuad, FrameView,
    InteractionMode, Measurement, Px, SnapPoint, Tolerance,
};
use vernier_platform::{
    Accelerator, Frame, Hud, HudAxis, HudEdge, HudKind, MonitorId, NativeFrame, Platform,
    PlatformEvent, TrayMenu,
};
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(name = "vernier", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Toggle the running vernier daemon's overlay via the IPC socket.
    /// Use this in compositor configs that bind the key directly when the
    /// `GlobalShortcuts` portal is unavailable (e.g. Hyprland: `bind = ALT
    /// SHIFT, P, exec, vernier toggle`).
    Toggle,
    /// Tell the running vernier daemon to quit.
    Quit,
    /// Ask the running daemon to capture the primary monitor and write a PNG.
    Capture {
        /// Output PNG path.
        path: PathBuf,
    },
    /// Run the edge detector on the latest captured frame at the given pixel
    /// coordinates and print the four cardinal candidates.
    DetectEdges {
        /// X coordinate in the frame's pixel space.
        x: i32,
        /// Y coordinate in the frame's pixel space.
        y: i32,
        /// Color tolerance (sum-of-channel difference, 0..=765). Default 30.
        #[arg(long, default_value_t = 30)]
        tolerance: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(
            "info,zbus=warn,zbus_router=warn,tracing=warn,async_io=warn,polling=warn",
        ),
    )
    .init();
    match cli.command {
        Some(Cmd::Toggle) => run_client_command("toggle"),
        Some(Cmd::Quit) => run_client_command("quit"),
        Some(Cmd::Capture { path }) => run_client_command(&format!(
            "capture {}",
            path.canonicalize().unwrap_or(path).display()
        )),
        Some(Cmd::DetectEdges { x, y, tolerance }) => {
            run_client_command(&format!("detect-edges {x} {y} {tolerance}"))
        }
        None => run_daemon(),
    }
}

fn run_client_command(cmd: &str) -> Result<()> {
    let path = ipc_socket_path()?;
    let mut stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("connect to {} (is the daemon running?)", path.display()))?;
    use std::io::{Read, Write};
    stream.write_all(cmd.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.shutdown(std::net::Shutdown::Write)
        .with_context(|| "shutdown write half of ipc socket")?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).ok();
    if !response.is_empty() {
        print!("{}", String::from_utf8_lossy(&response));
    }
    Ok(())
}

fn run_daemon() -> Result<()> {
    log::info!("vernier {} — daemon", env!("CARGO_PKG_VERSION"));

    let (platform, platform_events) = vernier_platform::init()?;
    let monitors = platform.monitors()?;
    log::info!("monitors detected: {}", monitors.len());
    for m in &monitors {
        log::info!(
            "  id={:?} name={:?} {}x{}+{},{} scale={}",
            m.id, m.name, m.bounds.w, m.bounds.h, m.bounds.x, m.bounds.y, m.scale_factor
        );
    }

    let primary = monitors
        .iter()
        .find(|m| m.is_primary)
        .or_else(|| monitors.first())
        .context("no monitors available")?;
    let mut overlay = platform.create_overlay(primary.id)?;
    let _tray = platform.create_tray(TrayMenu::minimal("vernier"))?;

    let _hotkey = match platform.register_hotkey(Accelerator::default(), "Toggle vernier") {
        Ok(id) => {
            log::info!(
                "global hotkey registered (the user may be prompted by xdg-desktop-portal-hyprland to confirm the binding)"
            );
            Some(id)
        }
        Err(e) => {
            log::warn!(
                "hotkey registration failed: {e}; falling back to CLI/IPC. \
                 Bind a key in Hyprland: bind = ALT SHIFT, P, exec, vernier toggle"
            );
            None
        }
    };

    let (combined_tx, combined_rx) = std::sync::mpsc::channel::<MainEvent>();

    // Drain platform events into the combined channel.
    let combined_for_plat = combined_tx.clone();
    std::thread::Builder::new()
        .name("vernier-platform-drain".into())
        .spawn(move || {
            while let Ok(ev) = platform_events.recv() {
                if combined_for_plat.send(MainEvent::Platform(ev)).is_err() {
                    break;
                }
            }
        })?;

    // IPC socket for `vernier toggle` / `vernier quit`.
    let socket_path = ipc_socket_path()?;
    let _ = std::fs::remove_file(&socket_path);
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("bind ipc socket at {}", socket_path.display()))?;
    log::info!("ipc socket: {}", socket_path.display());

    let combined_for_ipc = combined_tx.clone();
    std::thread::Builder::new()
        .name("vernier-ipc".into())
        .spawn(move || ipc_loop(listener, combined_for_ipc))?;

    log::info!(
        "running. Hotkey toggles measurement; tray Quit or `vernier quit` exits."
    );

    let mut mode = InteractionMode::Idle;
    // Rate-limit overlay redraws driven by pointer-move events. Wayland
    // pointer events arrive at ~120Hz, but committing a fresh wl_buffer
    // that often overwhelms the compositor and gets us disconnected.
    let mut last_hud_redraw = Instant::now() - Duration::from_secs(1);
    // ~120Hz cap. Faster than the typical display refresh, which is
    // intentional during the brief measurement session: we want a fresh
    // frame ready whenever the compositor pulls one. Outside of
    // measurement mode we don't redraw at all.
    const REDRAW_INTERVAL: Duration = Duration::from_millis(8);
    // Edge-detection tolerance, adjustable live with +/-. Range matches
    // vernier-core's Tolerance (0..=765 sum-of-channel delta).
    let mut tolerance: u32 = Tolerance::DEFAULT.0;
    const TOLERANCE_STEP: u32 = 8;
    const TOLERANCE_MAX: u32 = 200;
    let mut last_pointer_xy: Option<(f64, f64)> = None;
    // Snapshot taken when measurement mode is entered. Edge detection
    // runs against this frozen frame so the HUD strokes we draw don't
    // appear in subsequent captures (the Wayland screencast portal
    // captures our own overlay surface; without freezing, our own lines
    // would be detected as edges on the next frame).
    let mut frozen_frame: Option<NativeFrame> = None;

    while let Ok(event) = combined_rx.recv() {
        match event {
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) if id == "quit" => {
                log::info!("quit requested via tray");
                break;
            }
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) if id == "toggle_overlay" => {
                toggle_measurement(&mut mode, &mut overlay, &*platform, primary.id, &mut frozen_frame);
            }
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) => {
                log::info!("unhandled tray menu id: {id}");
            }
            MainEvent::Platform(PlatformEvent::HotkeyPressed(_)) => {
                toggle_measurement(&mut mode, &mut overlay, &*platform, primary.id, &mut frozen_frame);
            }
            MainEvent::Platform(PlatformEvent::TrayIconLeftClicked) => {
                log::info!("tray icon left-clicked");
            }
            MainEvent::Platform(PlatformEvent::PointerEnter { x, y, .. })
            | MainEvent::Platform(PlatformEvent::PointerMove {
                monitor: _, x, y, ..
            }) => {
                let cursor_px = Px::new(x as i32, y as i32);
                update_cursor_in_mode(&mut mode, cursor_px);
                last_pointer_xy = Some((x, y));
                if last_hud_redraw.elapsed() >= REDRAW_INTERVAL {
                    last_hud_redraw = Instant::now();
                    refresh_hud(&mode, &mut overlay, frozen_frame.as_ref(), x, y, tolerance);
                }
            }
            MainEvent::Platform(PlatformEvent::PointerButton {
                button, pressed, x, y, ..
            }) => {
                if button == BTN_LEFT {
                    handle_pointer_button(
                        &mut mode,
                        &mut overlay,
                        pressed,
                        x,
                        y,
                        frozen_frame.as_ref(),
                        tolerance,
                    );
                    last_hud_redraw = Instant::now();
                }
            }
            MainEvent::Platform(PlatformEvent::PointerLeave { .. }) => {}
            MainEvent::Platform(PlatformEvent::KeyboardKey {
                keysym, pressed, ..
            }) => {
                if !pressed || matches!(mode, InteractionMode::Idle) {
                    // Idle daemon ignores keyboard; the layer surface
                    // doesn't even hold focus then.
                } else if keysym == 0xff1b {
                    // XKB_KEY_Escape = 0xff1b. Exit on press (not
                    // release, to avoid double-toggling).
                    log::info!("esc — exiting measurement mode");
                    toggle_measurement(
                        &mut mode,
                        &mut overlay,
                        &*platform,
                        primary.id,
                        &mut frozen_frame,
                    );
                } else if keysym_increases_tolerance(keysym) {
                    tolerance = (tolerance + TOLERANCE_STEP).min(TOLERANCE_MAX);
                    log::info!("tolerance ↑ → {tolerance}");
                    if let Some((x, y)) = last_pointer_xy {
                        last_hud_redraw = Instant::now();
                        refresh_hud(&mode, &mut overlay, frozen_frame.as_ref(), x, y, tolerance);
                    }
                } else if keysym_decreases_tolerance(keysym) {
                    tolerance = tolerance.saturating_sub(TOLERANCE_STEP);
                    log::info!("tolerance ↓ → {tolerance}");
                    if let Some((x, y)) = last_pointer_xy {
                        last_hud_redraw = Instant::now();
                        refresh_hud(&mode, &mut overlay, frozen_frame.as_ref(), x, y, tolerance);
                    }
                } else if keysym == 0x0072 || keysym == 0x0052 {
                    // 'r' / 'R' — re-capture the screen so the user can
                    // refresh after the underlying content changed.
                    match platform.capture_screen_native(primary.id) {
                        Ok(f) => {
                            log::info!("frame refreshed");
                            frozen_frame = Some(f);
                            if let Some((x, y)) = last_pointer_xy {
                                last_hud_redraw = Instant::now();
                                refresh_hud(
                                    &mode,
                                    &mut overlay,
                                    frozen_frame.as_ref(),
                                    x,
                                    y,
                                    tolerance,
                                );
                            }
                        }
                        Err(e) => log::warn!("refresh capture failed: {e}"),
                    }
                }
            }
            MainEvent::Platform(other) => log::debug!("platform event: {other:?}"),
            MainEvent::Ipc(IpcCmd::Toggle) => {
                toggle_measurement(
                    &mut mode,
                    &mut overlay,
                    &*platform,
                    primary.id,
                    &mut frozen_frame,
                );
            }
            MainEvent::Ipc(IpcCmd::Quit) => {
                log::info!("ipc: quit");
                break;
            }
            MainEvent::Ipc(IpcCmd::Capture(path)) => {
                log::info!("ipc: capture → {}", path.display());
                match platform.capture_screen(primary.id) {
                    Ok(frame) => match save_frame_png(&path, &frame) {
                        Ok(_) => log::info!(
                            "capture saved: {} ({}x{})",
                            path.display(),
                            frame.width,
                            frame.height
                        ),
                        Err(e) => log::error!("save_frame_png: {e:#}"),
                    },
                    Err(e) => log::error!("capture_screen: {e}"),
                }
            }
            MainEvent::Ipc(IpcCmd::DetectEdges {
                x,
                y,
                tolerance,
                reply,
            }) => {
                log::info!("ipc: detect-edges ({x},{y}) tol={tolerance}");
                let resp = match platform.capture_screen(primary.id) {
                    Ok(frame) => format_edges(&frame, x, y, tolerance),
                    Err(e) => format!("error: capture_screen: {e}\n"),
                };
                let _ = reply.send(resp);
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

#[derive(Debug)]
enum MainEvent {
    Platform(PlatformEvent),
    Ipc(IpcCmd),
}

#[derive(Debug)]
enum IpcCmd {
    Toggle,
    Quit,
    Capture(PathBuf),
    DetectEdges {
        x: i32,
        y: i32,
        tolerance: u32,
        reply: SyncSender<String>,
    },
}

fn ipc_loop(listener: std::os::unix::net::UnixListener, sender: std::sync::mpsc::Sender<MainEvent>) {
    use std::io::{BufRead, BufReader, Write};
    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                log::warn!("ipc accept: {e}");
                continue;
            }
        };
        let mut writer = match stream.try_clone() {
            Ok(w) => w,
            Err(e) => {
                log::warn!("ipc clone: {e}");
                continue;
            }
        };
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            let (cmd, arg) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
            match cmd {
                "toggle" => {
                    if sender.send(MainEvent::Ipc(IpcCmd::Toggle)).is_err() {
                        return;
                    }
                }
                "quit" => {
                    if sender.send(MainEvent::Ipc(IpcCmd::Quit)).is_err() {
                        return;
                    }
                }
                "capture" if !arg.is_empty() => {
                    if sender
                        .send(MainEvent::Ipc(IpcCmd::Capture(PathBuf::from(arg))))
                        .is_err()
                    {
                        return;
                    }
                }
                "detect-edges" => {
                    let parts: Vec<&str> = arg.split_whitespace().collect();
                    let x = parts.first().and_then(|s| s.parse::<i32>().ok());
                    let y = parts.get(1).and_then(|s| s.parse::<i32>().ok());
                    let tol = parts.get(2).and_then(|s| s.parse::<u32>().ok()).unwrap_or(30);
                    let (Some(x), Some(y)) = (x, y) else {
                        let _ = writer.write_all(b"error: detect-edges X Y [tolerance]\n");
                        continue;
                    };
                    let (tx, rx) = std::sync::mpsc::sync_channel::<String>(1);
                    if sender
                        .send(MainEvent::Ipc(IpcCmd::DetectEdges {
                            x,
                            y,
                            tolerance: tol,
                            reply: tx,
                        }))
                        .is_err()
                    {
                        return;
                    }
                    match rx.recv() {
                        Ok(resp) => {
                            let _ = writer.write_all(resp.as_bytes());
                        }
                        Err(_) => {
                            let _ = writer.write_all(b"error: daemon dropped reply\n");
                        }
                    }
                }
                other => log::debug!("ipc unknown command: {other:?}"),
            }
        }
    }
}

/// Linux input event code for the left mouse button (BTN_LEFT).
const BTN_LEFT: u32 = 0x110;

fn toggle_measurement(
    mode: &mut InteractionMode,
    overlay: &mut vernier_platform::OverlayHandle,
    platform: &dyn Platform,
    monitor: MonitorId,
    frozen_frame: &mut Option<NativeFrame>,
) {
    if matches!(mode, InteractionMode::Idle) {
        // Capture BEFORE showing the overlay so our own surface isn't
        // in the snapshot used for edge detection.
        match platform.capture_screen_native(monitor) {
            Ok(frame) => {
                log::info!(
                    "measurement mode: ON (frozen {}×{} {:?})",
                    frame.width, frame.height, frame.format
                );
                *frozen_frame = Some(frame);
            }
            Err(e) => {
                log::warn!(
                    "measurement mode: ON (no frame yet — capture failed: {e}). \
                     Press 'r' once a frame is available."
                );
                *frozen_frame = None;
            }
        }
        *mode = InteractionMode::Hover { cursor: Px::default() };
        overlay.set_input_capturing(true);
        overlay.set_hud(Some(Hud::hover((-100.0, -100.0))));
        overlay.show();
    } else {
        log::info!("measurement mode: OFF");
        *mode = InteractionMode::Idle;
        *frozen_frame = None;
        overlay.hide();
        overlay.set_input_capturing(false);
        overlay.set_hud(None);
    }
}

fn update_cursor_in_mode(mode: &mut InteractionMode, cursor_px: Px) {
    match mode {
        InteractionMode::Idle => {}
        InteractionMode::Hover { cursor }
        | InteractionMode::Drawing { cursor, .. }
        | InteractionMode::Held { cursor, .. } => {
            *cursor = cursor_px;
        }
    }
}

/// Build the HUD that matches the current `mode` and ship it to the
/// overlay. The caller is responsible for rate-limiting calls so we
/// don't flood the compositor with buffer commits.
///
/// In Hover mode this also runs live edge detection at the cursor
/// pixel, producing the four cardinal snap candidates that drive the
/// extending HUD lines.
fn refresh_hud(
    mode: &InteractionMode,
    overlay: &mut vernier_platform::OverlayHandle,
    frozen_frame: Option<&NativeFrame>,
    x: f64,
    y: f64,
    tolerance: u32,
) {
    match mode {
        InteractionMode::Idle => {}
        InteractionMode::Hover { .. } => {
            let edges = frozen_frame
                .and_then(|f| detect_hud_edges(f, x, y, tolerance))
                .unwrap_or([None; 4]);
            let hud = Hud {
                kind: HudKind::Hover { cursor: (x, y), edges },
                ..Hud::hover((x, y))
            };
            overlay.set_hud(Some(hud));
        }
        InteractionMode::Drawing { start, .. } => {
            let start_pos = (start.pixel.x as f64, start.pixel.y as f64);
            let mut hud = Hud::hover((x, y));
            hud.kind = HudKind::Drawing { start: start_pos, cursor: (x, y) };
            overlay.set_hud(Some(hud));
        }
        InteractionMode::Held { measurement, .. } => {
            // After release, the held rectangle persists but the live
            // crosshair tracks the cursor on top of it. Edge detection
            // continues against the frozen frame so the user can keep
            // measuring nearby UI without re-triggering measurement mode.
            let edges = frozen_frame
                .and_then(|f| detect_hud_edges(f, x, y, tolerance))
                .unwrap_or([None; 4]);
            let rect_start = (measurement.start.pixel.x as f64, measurement.start.pixel.y as f64);
            let rect_end = (measurement.end.pixel.x as f64, measurement.end.pixel.y as f64);
            let mut hud = Hud::hover((x, y));
            hud.kind = HudKind::Held {
                rect_start,
                rect_end,
                cursor: (x, y),
                edges,
            };
            overlay.set_hud(Some(hud));
        }
    }
}

/// XKB keysyms that should bump tolerance up: `+`, `=` (unshifted +),
/// keypad `+`.
fn keysym_increases_tolerance(keysym: u32) -> bool {
    matches!(
        keysym,
        0x002b // plus
        | 0x003d // equal
        | 0xffab // KP_Add
    )
}

/// XKB keysyms that should bump tolerance down: `-`, `_` (shifted -),
/// keypad `-`.
fn keysym_decreases_tolerance(keysym: u32) -> bool {
    matches!(
        keysym,
        0x002d // minus
        | 0x005f // underscore
        | 0xffad // KP_Subtract
    )
}

/// Capture the latest screen frame and run edge detection at the
/// surface-local cursor `(x, y)`. The result is in surface coordinates
/// so the HUD can render directly.
fn detect_hud_edges(
    frame: &NativeFrame,
    surface_x: f64,
    surface_y: f64,
    tolerance: u32,
) -> Option<[Option<HudEdge>; 4]> {
    let surface_w = frame.bounds.w as f64;
    let surface_h = frame.bounds.h as f64;
    if surface_w <= 0.0 || surface_h <= 0.0 {
        return None;
    }
    // Surface (logical) px → frame (physical) px.
    let scale_x = frame.width as f64 / surface_w;
    let scale_y = frame.height as f64 / surface_h;
    let frame_cursor = Px::new(
        (surface_x * scale_x).round() as i32,
        (surface_y * scale_y).round() as i32,
    );
    let view = FrameView {
        pixels: &frame.pixels,
        width: frame.width,
        height: frame.height,
        stride: frame.stride,
    };
    let edges = detect_edges(&view, frame_cursor, Tolerance(tolerance));
    Some(convert_edges_to_surface(&edges, scale_x, scale_y))
}

fn convert_edges_to_surface(
    edges: &EdgeQuad,
    scale_x: f64,
    scale_y: f64,
) -> [Option<HudEdge>; 4] {
    use vernier_core::Direction;
    let inv_x = 1.0 / scale_x;
    let inv_y = 1.0 / scale_y;
    let mut out: [Option<HudEdge>; 4] = [None; 4];
    for (slot, candidate) in out.iter_mut().zip(edges.iter()) {
        if let Some(c) = candidate {
            // detect_edges returns the FIRST pixel that exceeds tolerance
            // — i.e., the first different-color pixel ACROSS the boundary.
            // For visual snap, step back one pixel into the anchor region
            // so the line stops on the last same-color pixel. Matches
            //
            let (dx, dy, axis) = match c.direction {
                Direction::Left => (1, 0, HudAxis::Left),
                Direction::Right => (-1, 0, HudAxis::Right),
                Direction::Up => (0, 1, HudAxis::Up),
                Direction::Down => (0, -1, HudAxis::Down),
            };
            let adj_x = c.position.x + dx;
            let adj_y = c.position.y + dy;
            *slot = Some(HudEdge {
                axis,
                position: (adj_x as f64 * inv_x, adj_y as f64 * inv_y),
                distance_px: c.distance.saturating_sub(1),
            });
        }
    }
    out
}

fn handle_pointer_button(
    mode: &mut InteractionMode,
    overlay: &mut vernier_platform::OverlayHandle,
    pressed: bool,
    x: f64,
    y: f64,
    frozen_frame: Option<&vernier_platform::NativeFrame>,
    tolerance: u32,
) {
    let cursor_px = Px::new(x as i32, y as i32);
    if pressed {
        match mode {
            InteractionMode::Hover { .. } | InteractionMode::Held { .. } => {
                let snap = SnapPoint::loose(cursor_px);
                log::info!("drag started at ({},{})", cursor_px.x, cursor_px.y);
                *mode = InteractionMode::Drawing { start: snap, cursor: cursor_px };
                let mut hud = Hud::hover((x, y));
                hud.kind = HudKind::Drawing { start: (x, y), cursor: (x, y) };
                overlay.set_hud(Some(hud));
            }
            _ => {}
        }
    } else if let InteractionMode::Drawing { start, .. } = mode {
        let raw_start = (start.pixel.x as f64, start.pixel.y as f64);
        let raw_end = (x, y);
        // Snap-shrink: walk inward from each side of the dragged rect
        // until we hit content. Matches macOS shrink-to-fit on
        // release. Falls back to the raw rect if no frame or no
        // content boundary was found.
        let (snapped_start, snapped_end) =
            snap_shrink_logical_rect(frozen_frame, raw_start, raw_end, tolerance);
        let snapped_start_px = Px::new(snapped_start.0.round() as i32, snapped_start.1.round() as i32);
        let snapped_end_px = Px::new(snapped_end.0.round() as i32, snapped_end.1.round() as i32);
        let measurement = Measurement::new(
            SnapPoint::loose(snapped_start_px),
            SnapPoint::loose(snapped_end_px),
        );
        let aspect = if measurement.width() > 0 && measurement.height() > 0 {
            classify_aspect(
                measurement.width(),
                measurement.height(),
                AspectMode::Automatic,
                0.02,
            )
        } else {
            None
        };
        log::info!(
            "measurement: {}×{}px (drag was {}×{}px) aspect={:?}",
            measurement.width(),
            measurement.height(),
            (raw_end.0 - raw_start.0).abs() as i32,
            (raw_end.1 - raw_start.1).abs() as i32,
            aspect,
        );
        *mode = InteractionMode::Held { measurement, cursor: cursor_px };
        let mut hud = Hud::hover((x, y));
        hud.kind = HudKind::Held {
            rect_start: snapped_start,
            rect_end: snapped_end,
            cursor: (x, y),
            edges: [None; 4],
        };
        overlay.set_hud(Some(hud));
    }
}

/// Apply [`shrink_to_content`] to a rect given in surface (logical)
/// coords. Maps logical → frame coords, runs the shrink, maps back.
fn snap_shrink_logical_rect(
    frozen_frame: Option<&vernier_platform::NativeFrame>,
    a: (f64, f64),
    b: (f64, f64),
    tolerance: u32,
) -> ((f64, f64), (f64, f64)) {
    let Some(frame) = frozen_frame else {
        return (a, b);
    };
    let surface_w = frame.bounds.w as f64;
    let surface_h = frame.bounds.h as f64;
    if surface_w <= 0.0 || surface_h <= 0.0 {
        return (a, b);
    }
    let scale_x = frame.width as f64 / surface_w;
    let scale_y = frame.height as f64 / surface_h;
    let view = FrameView {
        pixels: &frame.pixels,
        width: frame.width,
        height: frame.height,
        stride: frame.stride,
    };
    let fx0 = (a.0 * scale_x).round() as i32;
    let fy0 = (a.1 * scale_y).round() as i32;
    let fx1 = (b.0 * scale_x).round() as i32;
    let fy1 = (b.1 * scale_y).round() as i32;
    let (sx0, sy0, sx1, sy1) =
        shrink_to_content(&view, fx0, fy0, fx1, fy1, Tolerance(tolerance));
    let inv_x = 1.0 / scale_x;
    let inv_y = 1.0 / scale_y;
    (
        (sx0 as f64 * inv_x, sy0 as f64 * inv_y),
        (sx1 as f64 * inv_x, sy1 as f64 * inv_y),
    )
}

fn format_edges(frame: &Frame, x: i32, y: i32, tolerance: u32) -> String {
    let view = match FrameView::packed(&frame.pixels, frame.width, frame.height) {
        Some(v) => v,
        None => {
            return format!(
                "error: frame buffer {} bytes is shorter than {}x{}*4\n",
                frame.pixels.len(),
                frame.width,
                frame.height
            );
        }
    };
    let edges = detect_edges(&view, Px::new(x, y), Tolerance(tolerance));
    let mut out = String::new();
    out.push_str(&format!(
        "frame: {}x{} cursor: ({},{}) tolerance: {}\n",
        frame.width, frame.height, x, y, tolerance
    ));
    let labels = ["Left  ", "Right ", "Up    ", "Down  "];
    for (slot, label) in edges.iter().zip(labels.iter()) {
        match slot {
            Some(c) => out.push_str(&format!(
                "  {label} dist={:4}px pos=({:5},{:5}) Δ={:3} edge=#{:02x}{:02x}{:02x}\n",
                c.distance,
                c.position.x,
                c.position.y,
                c.strength,
                c.edge_color.r,
                c.edge_color.g,
                c.edge_color.b,
            )),
            None => out.push_str(&format!("  {label} no edge before frame boundary\n")),
        }
    }
    out
}

fn save_frame_png(path: &Path, frame: &Frame) -> Result<()> {
    let img = image::RgbaImage::from_raw(frame.width, frame.height, frame.pixels.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "frame pixel buffer size {} doesn't match {}x{}*4",
                frame.pixels.len(),
                frame.width,
                frame.height
            )
        })?;
    img.save(path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn ipc_socket_path() -> Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(runtime_dir.join("vernier.sock"))
}
