use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use vernier_core::{
    classify_aspect, detect_edges, shrink_to_content, AspectMode, EdgeQuad, FrameView,
    InteractionMode, Measurement, Px, SnapPoint, Tolerance,
};
use vernier_platform::{
    Accelerator, Frame, Guide, GuideAxis, HeldRect, Hud, HudAxis, HudEdge, HudKind, HudToast,
    MonitorId, NativeFrame, Platform, PlatformEvent, StuckMeasurement, TrayMenu,
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
    // Edge-detection tolerance — discrete levels (Zero / Low / Medium
    // / High) cycled with +/-. Each level maps to a sum-of-channel
    // delta that the edge-detection scan uses to ignore minor color
    // variation.
    let mut tol_level = TolLevel::DEFAULT;
    let mut last_pointer_xy: Option<(f64, f64)> = None;
    // Active toast (centered or bottom-center). While `toast_until` is
    // in the future we keep showing the toast on every redraw and
    // ignore tickless dismissal.
    let mut active_toast: Option<HudToast> = None;
    let mut toast_until: Option<Instant> = None;
    const TOAST_TOLERANCE_MS: u64 = 900;
    const TOAST_SCREENSHOT_MS: u64 = 1200;
    // Reference guides accumulate across keypresses. `pending_guide`
    // is the in-flight axis the next click will stick to the cursor;
    // `guides` are committed lines.
    let mut guides: Vec<Guide> = Vec::new();
    let mut pending_guide: Option<GuideAxis> = None;
    // Frozen single-axis measurements. Same lifecycle as `guides`:
    // accumulated with lower-h / lower-v key presses, cleared by Esc.
    let mut stuck_measurements: Vec<StuckMeasurement> = Vec::new();
    // Committed rectangle measurements. Each finished drag pushes
    // here; they all stay visible while new ones get drawn on top.
    // Esc clears the list.
    let mut held_rects: Vec<HeldRect> = Vec::new();
    // Toggled by `x` while measuring — swaps the measurement HUD
    // foreground (axis lines, tick caps, rect borders, stuck pills)
    // from coral red to near-black for the rare cases where red
    // disappears against the underlying UI.
    let mut color_alternate: bool = false;
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
                toggle_measurement(&mut mode, &mut overlay, &*platform, primary.id, &mut frozen_frame, &held_rects, &guides, &stuck_measurements, color_alternate);
            }
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) => {
                log::info!("unhandled tray menu id: {id}");
            }
            MainEvent::Platform(PlatformEvent::HotkeyPressed(_)) => {
                toggle_measurement(&mut mode, &mut overlay, &*platform, primary.id, &mut frozen_frame, &held_rects, &guides, &stuck_measurements, color_alternate);
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
                    let toast = current_toast(&active_toast, toast_until);
                    refresh_hud(
                        &mode,
                        &mut overlay,
                        frozen_frame.as_ref(),
                        x,
                        y,
                        tol_level.value(),
                        toast,
                        &guides,
                        pending_guide,
                        &stuck_measurements,
                        &held_rects,
                        color_alternate,
                    );
                }
            }
            MainEvent::Platform(PlatformEvent::PointerButton {
                button, pressed, x, y, ..
            }) => {
                if button == BTN_LEFT {
                    // While a guide is pending placement, the next
                    // press sticks it at the cursor instead of
                    // starting a measurement drag.
                    if pressed {
                        if let Some(axis) = pending_guide.take() {
                            let position = match axis {
                                GuideAxis::Horizontal => y as i32,
                                GuideAxis::Vertical => x as i32,
                            };
                            guides.push(Guide { axis, position, hovered: false });
                            log::info!("guide stuck: {:?} @ {}", axis, position);
                            last_hud_redraw = Instant::now();
                            let toast = current_toast(&active_toast, toast_until);
                            refresh_hud(
                                &mode,
                                &mut overlay,
                                frozen_frame.as_ref(),
                                x,
                                y,
                                tol_level.value(),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                            );
                            continue;
                        }
                    }
                    let outcome = handle_pointer_button(
                        &mut mode,
                        &mut overlay,
                        pressed,
                        x,
                        y,
                        frozen_frame.as_ref(),
                        tol_level.value(),
                        &mut guides,
                        &mut stuck_measurements,
                        &mut held_rects,
                        color_alternate,
                    );
                    last_hud_redraw = Instant::now();
                    if let ButtonOutcome::ScreenshotTaken = outcome {
                        let toast = HudToast { text: "Screenshot taken".into() };
                        let mut hud = Hud::hover((x, y));
                        hud.kind = HudKind::None;
                        hud.toast = Some(toast.clone());
                        overlay.set_hud(Some(hud));
                        active_toast = Some(toast);
                        toast_until =
                            Some(Instant::now() + Duration::from_millis(TOAST_SCREENSHOT_MS));
                        spawn_toast_timer(
                            &combined_tx,
                            Duration::from_millis(TOAST_SCREENSHOT_MS),
                            true,
                        );
                    } else {
                        // Push the latest HUD now so removals (held
                        // rect / guide / stuck) and other state
                        // changes appear immediately, without the
                        // user having to nudge the mouse to trigger
                        // the next redraw.
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            tol_level.value(),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                        );
                    }
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
                    // XKB_KEY_Escape = 0xff1b. Esc clears any guides
                    // and exits measurement mode in a single press —
                    // matches macOS "back to background daemon"
                    // semantics.
                    log::info!(
                        "esc — clearing {} guide(s), {} stuck, {} held rect(s) and exiting",
                        guides.len(),
                        stuck_measurements.len(),
                        held_rects.len(),
                    );
                    guides.clear();
                    pending_guide = None;
                    stuck_measurements.clear();
                    held_rects.clear();
                    toggle_measurement(
                        &mut mode,
                        &mut overlay,
                        &*platform,
                        primary.id,
                        &mut frozen_frame,
                        &held_rects,
                        &guides,
                        &stuck_measurements,
                        color_alternate,
                    );
                } else if keysym == 0x0048 || keysym == 0x0056 {
                    // Shift+H = 0x48 ('H'), Shift+V = 0x56 ('V').
                    // Begin placing a horizontal / vertical guide that
                    // tracks the cursor until the next click sticks it.
                    let axis = if keysym == 0x0048 {
                        GuideAxis::Horizontal
                    } else {
                        GuideAxis::Vertical
                    };
                    pending_guide = Some(axis);
                    log::info!("guide pending: {:?} (click to stick)", axis);
                    if let Some((x, y)) = last_pointer_xy {
                        last_hud_redraw = Instant::now();
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            tol_level.value(),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                        );
                    }
                } else if keysym == 0x0078 {
                    // Lowercase 'x' toggles the measurement HUD's
                    // foreground between coral red (default) and
                    // black (alternate). Useful when the underlying
                    // UI clashes with one of the two.
                    color_alternate = !color_alternate;
                    log::info!(
                        "color_alternate → {}",
                        if color_alternate { "black" } else { "red" }
                    );
                    if let Some((x, y)) = last_pointer_xy {
                        last_hud_redraw = Instant::now();
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            tol_level.value(),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                        );
                    }
                } else if keysym == 0x0068 || keysym == 0x0076 {
                    // Lowercase 'h' / 'v' freezes the current
                    // horizontal / vertical axis distance (with the
                    // pixel value pinned beside it). Stackable; Esc
                    // clears them along with any guides.
                    if let Some((x, y)) = last_pointer_xy {
                        let axis = if keysym == 0x0068 {
                            GuideAxis::Horizontal
                        } else {
                            GuideAxis::Vertical
                        };
                        let edges = edges_for_hud(
                            frozen_frame.as_ref(),
                            x,
                            y,
                            tol_level.value(),
                            &guides,
                        );
                        let measurement = freeze_axis_measurement(
                            axis,
                            x,
                            y,
                            &edges,
                            primary.bounds.w,
                            primary.bounds.h,
                        );
                        log::info!(
                            "stuck {:?} measurement: {} px @ {}",
                            axis,
                            (measurement.end - measurement.start).abs(),
                            measurement.at,
                        );
                        stuck_measurements.push(measurement);
                        last_hud_redraw = Instant::now();
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            tol_level.value(),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                        );
                    }
                } else if keysym_increases_tolerance(keysym) {
                    tol_level = tol_level.higher();
                    log::info!("tolerance → {} ({})", tol_level.label(), tol_level.value());
                    let toast = HudToast {
                        text: format!("Tolerance: {}", tol_level.label()),
                    };
                    active_toast = Some(toast);
                    toast_until = Some(Instant::now() + Duration::from_millis(TOAST_TOLERANCE_MS));
                    spawn_toast_timer(
                        &combined_tx,
                        Duration::from_millis(TOAST_TOLERANCE_MS),
                        false,
                    );
                    if let Some((x, y)) = last_pointer_xy {
                        last_hud_redraw = Instant::now();
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            tol_level.value(),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                        );
                    }
                } else if keysym_decreases_tolerance(keysym) {
                    tol_level = tol_level.lower();
                    log::info!("tolerance → {} ({})", tol_level.label(), tol_level.value());
                    let toast = HudToast {
                        text: format!("Tolerance: {}", tol_level.label()),
                    };
                    active_toast = Some(toast);
                    toast_until = Some(Instant::now() + Duration::from_millis(TOAST_TOLERANCE_MS));
                    spawn_toast_timer(
                        &combined_tx,
                        Duration::from_millis(TOAST_TOLERANCE_MS),
                        false,
                    );
                    if let Some((x, y)) = last_pointer_xy {
                        last_hud_redraw = Instant::now();
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            tol_level.value(),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                        );
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
                                let toast = current_toast(&active_toast, toast_until);
                                refresh_hud(
                                    &mode,
                                    &mut overlay,
                                    frozen_frame.as_ref(),
                                    x,
                                    y,
                                    tol_level.value(),
                                    toast,
                                    &guides,
                                    pending_guide,
                                    &stuck_measurements,
                                    &held_rects,
                                    color_alternate,
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
                    &held_rects,
                    &guides,
                    &stuck_measurements,
                    color_alternate,
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
            MainEvent::ToastElapsed { exit_measurement } => {
                // A timer thread fires when its toast duration elapses.
                // If a fresher toast is still active (user hit +/-
                // again, or the screenshot toast superseded a tolerance
                // toast), keep waiting — the newer timer's elapsed
                // event will handle the dismissal.
                let now = Instant::now();
                let still_active = toast_until.map_or(false, |t| now < t);
                if still_active {
                    continue;
                }
                active_toast = None;
                toast_until = None;
                if exit_measurement {
                    toggle_measurement(
                        &mut mode,
                        &mut overlay,
                        &*platform,
                        primary.id,
                        &mut frozen_frame,
                        &held_rects,
                        &guides,
                        &stuck_measurements,
                        color_alternate,
                    );
                } else if let Some((x, y)) = last_pointer_xy {
                    last_hud_redraw = Instant::now();
                    refresh_hud(
                        &mode,
                        &mut overlay,
                        frozen_frame.as_ref(),
                        x,
                        y,
                        tol_level.value(),
                        None,
                        &guides,
                        pending_guide,
                        &stuck_measurements,
                        &held_rects,
                        color_alternate,
                    );
                }
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Foreground color for HUD strokes and pills. Coral red by default;
/// black when the user has toggled the alternate palette with `x`.
fn hud_foreground(alt: bool) -> vernier_platform::Color {
    use vernier_platform::Color;
    if alt {
        Color::rgba(0x10, 0x10, 0x10, 0xF5)
    } else {
        Color::rgba(0xFF, 0x5C, 0x5C, 0xF5)
    }
}

/// Returns the active toast iff its dismissal time hasn't passed.
fn current_toast<'a>(toast: &'a Option<HudToast>, until: Option<Instant>) -> Option<&'a HudToast> {
    if until.map_or(false, |t| Instant::now() < t) {
        toast.as_ref()
    } else {
        None
    }
}

/// Spawn a detached thread that sleeps for `delay` and then enqueues
/// `MainEvent::ToastElapsed` so the main loop can dismiss the toast.
fn spawn_toast_timer(
    tx: &std::sync::mpsc::Sender<MainEvent>,
    delay: Duration,
    exit_measurement: bool,
) {
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("vernier-toast-timer".into())
        .spawn(move || {
            std::thread::sleep(delay);
            let _ = tx.send(MainEvent::ToastElapsed { exit_measurement });
        })
        .ok();
}

#[derive(Debug)]
enum MainEvent {
    Platform(PlatformEvent),
    Ipc(IpcCmd),
    /// Internal: a transient on-screen toast (tolerance feedback or
    /// post-screenshot confirmation) has elapsed and should be cleared.
    /// `exit_measurement` is true for the screenshot toast — the
    /// overlay closes after the toast fades.
    ToastElapsed { exit_measurement: bool },
}

#[derive(Debug, Clone, Copy)]
enum ButtonOutcome {
    None,
    /// The user clicked the camera pill on the held rect — caller
    /// should pop a "Screenshot taken" toast and exit measurement
    /// mode after a short delay.
    ScreenshotTaken,
}

/// Discrete tolerance levels the user cycles through with `+`/`-`.
/// Backed by a sum-of-channel-delta value the edge-detection scan
/// uses to ignore minor color variation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TolLevel { Zero, Low, Medium, High }

impl TolLevel {
    const DEFAULT: Self = Self::Medium;
    fn label(self) -> &'static str {
        match self {
            Self::Zero => "Zero",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
        }
    }
    fn value(self) -> u32 {
        match self {
            Self::Zero => 0,
            Self::Low => 16,
            Self::Medium => 48,
            Self::High => 96,
        }
    }
    fn higher(self) -> Self {
        match self {
            Self::Zero => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::High,
        }
    }
    fn lower(self) -> Self {
        match self {
            Self::Zero => Self::Zero,
            Self::Low => Self::Zero,
            Self::Medium => Self::Low,
            Self::High => Self::Medium,
        }
    }
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
    held_rects: &[HeldRect],
    guides: &[Guide],
    stuck_measurements: &[StuckMeasurement],
    color_alternate: bool,
) {
    let fg = hud_foreground(color_alternate);
    if matches!(mode, InteractionMode::Idle) {
        // Going ON — recapture the screen for edge detection, restore
        // input grab, and re-render any persisted content alongside.
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
        let mut hud = Hud::hover((-100.0, -100.0));
        hud.foreground = fg;
        hud.held_rects = held_rects.to_vec();
        hud.guides = guides.to_vec();
        hud.stuck_measurements = stuck_measurements.to_vec();
        overlay.set_hud(Some(hud));
        overlay.show();
        return;
    }
    let has_content = !held_rects.is_empty()
        || !guides.is_empty()
        || !stuck_measurements.is_empty();
    if has_content {
        // Going OFF with persisted content: drop the frozen frame and
        // switch the overlay into passthrough mode (no input grab, no
        // keyboard focus) so the desktop becomes interactive again
        // while the held content stays visible.
        log::info!(
            "measurement mode: OFF (persisting {} rect(s), {} guide(s), {} stuck)",
            held_rects.len(),
            guides.len(),
            stuck_measurements.len(),
        );
        *mode = InteractionMode::Idle;
        *frozen_frame = None;
        overlay.set_input_capturing(false);
        let mut hud = Hud::hover((-1000.0, -1000.0));
        hud.kind = HudKind::None;
        hud.foreground = fg;
        hud.held_rects = held_rects.to_vec();
        hud.guides = guides.to_vec();
        hud.stuck_measurements = stuck_measurements.to_vec();
        overlay.set_hud(Some(hud));
    } else {
        // Going OFF clean: hide the overlay and detach all state.
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
    toast: Option<&HudToast>,
    guides: &[Guide],
    pending_guide: Option<GuideAxis>,
    stuck_measurements: &[StuckMeasurement],
    held_rects: &[HeldRect],
    color_alternate: bool,
) {
    let fg = hud_foreground(color_alternate);
    let cursor_px = Px::new(x as i32, y as i32);
    // Compose guides + pending guide. Mark the FIRST committed guide
    // the cursor is over as hovered so the renderer shows an X badge
    // (only one removal target at a time, prevents accidental clicks).
    let mut composed_guides = compose_guides(guides, pending_guide, x, y);
    if pending_guide.is_none() {
        let mut found = false;
        for g in composed_guides.iter_mut() {
            if !found && cursor_over_guide_line(cursor_px, g) {
                g.hovered = true;
                found = true;
            }
        }
    }
    // Same hover detection for stuck measurements.
    let mut composed_stuck: Vec<StuckMeasurement> = stuck_measurements.to_vec();
    if pending_guide.is_none() {
        let mut found = false;
        for s in composed_stuck.iter_mut() {
            if !found && cursor_over_stuck_pill(cursor_px, s) {
                s.hovered = true;
                found = true;
            }
        }
    }
    // Per-rect transient state: which pill the cursor is over, and
    // whether the cursor sits inside any rect at all (suppresses the
    // crosshair).
    let composed_rects: Vec<HeldRect> = held_rects
        .iter()
        .map(|r| HeldRect {
            rect_start: r.rect_start,
            rect_end: r.rect_end,
            camera_armed: cursor_over_pill(
                cursor_px,
                Px::new(r.rect_start.0 as i32, r.rect_start.1 as i32),
                Px::new(r.rect_end.0 as i32, r.rect_end.1 as i32),
            ),
        })
        .collect();
    // Show the arrow cursor (suppress crosshair) when the pointer is
    // over any element that responds to a click — held rect interior,
    // a hovered stuck-measurement pill, or a hovered guide line.
    let cursor_in_held = held_rects.iter().any(|r| {
        let rs = Px::new(r.rect_start.0 as i32, r.rect_start.1 as i32);
        let re = Px::new(r.rect_end.0 as i32, r.rect_end.1 as i32);
        // Either inside the rect itself OR hovering its pill
        // (the pill may sit below the rect when the rect is small)
        // — both cases want the arrow cursor.
        cursor_in_held_rect(cursor_px, rs, re) || cursor_over_pill(cursor_px, rs, re)
    });
    let any_guide_hover = guides.iter().any(|g| cursor_over_guide_line(cursor_px, g));
    let any_stuck_hover = stuck_measurements
        .iter()
        .any(|m| cursor_over_stuck_pill(cursor_px, m));
    let cursor_in_rect = cursor_in_held || any_guide_hover || any_stuck_hover;

    // While placing a guide, suppress the measurement crosshair —
    // only the guide line(s) should be visible. Crosshairs return as
    // soon as the guide is committed (pending_guide → None).
    if pending_guide.is_some() {
        let mut hud = Hud::hover((x, y));
        hud.kind = HudKind::None;
        hud.foreground = fg;
        hud.toast = toast.cloned();
        hud.guides = composed_guides;
        hud.stuck_measurements = composed_stuck;
        hud.held_rects = composed_rects;
        hud.cursor_in_rect = cursor_in_rect;
        overlay.set_hud(Some(hud));
        return;
    }
    match mode {
        InteractionMode::Idle => {}
        InteractionMode::Hover { .. } | InteractionMode::Held { .. } => {
            let edges = edges_for_hud(frozen_frame, x, y, tolerance, guides);
            let mut hud = Hud {
                kind: HudKind::Hover { cursor: (x, y), edges },
                ..Hud::hover((x, y))
            };
            hud.foreground = fg;
            hud.toast = toast.cloned();
            hud.guides = composed_guides.clone();
            hud.stuck_measurements = composed_stuck.clone();
            hud.held_rects = composed_rects.clone();
            hud.cursor_in_rect = cursor_in_rect;
            overlay.set_hud(Some(hud));
        }
        InteractionMode::Drawing { start, .. } => {
            let mut hud = Hud::hover((x, y));
            hud.foreground = fg;
            if has_drag_distance(start.pixel, cursor_px) {
                let start_pos = (start.pixel.x as f64, start.pixel.y as f64);
                hud.kind = HudKind::Drawing { start: start_pos, cursor: (x, y) };
            } else {
                // Below the drag threshold the rect would just be a
                // 1×1 dot — fall back to the live measurement HUD so a
                // mis-click looks identical to hovering.
                let edges = edges_for_hud(frozen_frame, x, y, tolerance, guides);
                hud.kind = HudKind::Hover { cursor: (x, y), edges };
            }
            hud.toast = toast.cloned();
            hud.guides = composed_guides.clone();
            hud.stuck_measurements = composed_stuck;
            hud.held_rects = composed_rects;
            hud.cursor_in_rect = cursor_in_rect;
            overlay.set_hud(Some(hud));
        }
    }
}

/// Combine committed guides with the in-flight pending guide (if any)
/// into a single list for the renderer. The pending guide tracks the
/// cursor live until the user clicks to commit it.
fn compose_guides(
    committed: &[Guide],
    pending: Option<GuideAxis>,
    x: f64,
    y: f64,
) -> Vec<Guide> {
    let mut out: Vec<Guide> = committed.to_vec();
    if let Some(axis) = pending {
        let position = match axis {
            GuideAxis::Horizontal => y as i32,
            GuideAxis::Vertical => x as i32,
        };
        out.push(Guide { axis, position, hovered: false });
    }
    out
}

/// Run `detect_hud_edges` against the frozen frame, then fold any
/// committed guides in as additional edge candidates. Guides clamp
/// the axis lines: if a guide is nearer than the detected pixel edge
/// on a given side, the line snaps to the guide instead.
fn edges_for_hud(
    frozen_frame: Option<&NativeFrame>,
    x: f64,
    y: f64,
    tolerance: u32,
    guides: &[Guide],
) -> [Option<HudEdge>; 4] {
    let mut edges = frozen_frame
        .and_then(|f| detect_hud_edges(f, x, y, tolerance))
        .unwrap_or([None; 4]);
    apply_guides_to_edges(&mut edges, guides, x, y);
    edges
}

/// Snapshot the current axis distance into a [`StuckMeasurement`].
/// Uses whatever edges the cursor is sitting between (detected pixels
/// + guide-clamps); falls back to the surface bounds when an edge is
/// missing on a side so the user always gets a meaningful value.
fn freeze_axis_measurement(
    axis: GuideAxis,
    x: f64,
    y: f64,
    edges: &[Option<HudEdge>; 4],
    surface_w: u32,
    surface_h: u32,
) -> StuckMeasurement {
    match axis {
        GuideAxis::Vertical => {
            let up = edges[2].map(|e| e.position.1.round() as i32).unwrap_or(0);
            let down = edges[3]
                .map(|e| e.position.1.round() as i32)
                .unwrap_or(surface_h as i32);
            StuckMeasurement {
                axis,
                at: x.round() as i32,
                start: up,
                end: down,
                hovered: false,
            }
        }
        GuideAxis::Horizontal => {
            let left = edges[0].map(|e| e.position.0.round() as i32).unwrap_or(0);
            let right = edges[1]
                .map(|e| e.position.0.round() as i32)
                .unwrap_or(surface_w as i32);
            StuckMeasurement {
                axis,
                at: y.round() as i32,
                start: left,
                end: right,
                hovered: false,
            }
        }
    }
}

/// Mutate `edges` so each guide that lies between the cursor and an
/// existing edge takes that edge's slot — effectively making guides
/// behave like detected pixel boundaries. Slot order matches
/// [`detect_edges`]: 0=Left, 1=Right, 2=Up, 3=Down.
fn apply_guides_to_edges(
    edges: &mut [Option<HudEdge>; 4],
    guides: &[Guide],
    x: f64,
    y: f64,
) {
    for guide in guides {
        match guide.axis {
            GuideAxis::Vertical => {
                let dx = guide.position as f64 - x;
                if dx <= -1.0 {
                    let dist = (-dx) as u32;
                    if edges[0].map_or(true, |e| e.distance_px > dist) {
                        edges[0] = Some(HudEdge {
                            axis: HudAxis::Left,
                            position: (guide.position as f64, y),
                            distance_px: dist,
                        });
                    }
                } else if dx >= 1.0 {
                    let dist = dx as u32;
                    if edges[1].map_or(true, |e| e.distance_px > dist) {
                        edges[1] = Some(HudEdge {
                            axis: HudAxis::Right,
                            position: (guide.position as f64, y),
                            distance_px: dist,
                        });
                    }
                }
            }
            GuideAxis::Horizontal => {
                let dy = guide.position as f64 - y;
                if dy <= -1.0 {
                    let dist = (-dy) as u32;
                    if edges[2].map_or(true, |e| e.distance_px > dist) {
                        edges[2] = Some(HudEdge {
                            axis: HudAxis::Up,
                            position: (x, guide.position as f64),
                            distance_px: dist,
                        });
                    }
                } else if dy >= 1.0 {
                    let dist = dy as u32;
                    if edges[3].map_or(true, |e| e.distance_px > dist) {
                        edges[3] = Some(HudEdge {
                            axis: HudAxis::Down,
                            position: (x, guide.position as f64),
                            distance_px: dist,
                        });
                    }
                }
            }
        }
    }
}

/// True when `cursor` (logical pixels) sits inside the W×H pill area
/// of the held rectangle. The pill follows the same below-vs-inside
/// rule as the renderer (small rects → pill below; ≥70×35 → pill
/// centered inside) so the hit zone tracks the actual UI element
/// rather than a fixed offset from the rect center.
fn cursor_over_pill(cursor: Px, rect_start: Px, rect_end: Px) -> bool {
    let lo_x = rect_start.x.min(rect_end.x);
    let hi_x = rect_start.x.max(rect_end.x);
    let lo_y = rect_start.y.min(rect_end.y);
    let hi_y = rect_start.y.max(rect_end.y);
    let rw = hi_x - lo_x;
    let rh = hi_y - lo_y;
    let pill_below = rw < 70 || rh < 35;
    let pill_cx = (lo_x + hi_x) / 2;
    let pill_cy = if pill_below {
        // Pill is anchored 8 logical px below the rect, with the
        // pill itself ~18 logical px tall (text + padding) — center
        // sits ~17 px below the bottom edge.
        hi_y + 17
    } else {
        (lo_y + hi_y) / 2
    };
    const HALF_W: i32 = 50;
    const HALF_H: i32 = 11;
    cursor.x >= pill_cx - HALF_W
        && cursor.x <= pill_cx + HALF_W
        && cursor.y >= pill_cy - HALF_H
        && cursor.y <= pill_cy + HALF_H
}

/// Minimum drag distance (logical pixels) before a press becomes a
/// drawing rectangle. A bare click below this threshold collapses
/// back to Hover so the user doesn't get a 1×1 red dot from a
/// mis-click.
const DRAG_THRESHOLD_PX: i32 = 2;

fn has_drag_distance(start: Px, cursor: Px) -> bool {
    (start.x - cursor.x).abs() >= DRAG_THRESHOLD_PX
        || (start.y - cursor.y).abs() >= DRAG_THRESHOLD_PX
}

/// True when `cursor` is within a small distance of a guide line.
/// The hover threshold is generous (4 logical px) because the line
/// itself is only 1 physical px wide and we want clicks-to-remove to
/// feel forgiving.
fn cursor_over_guide_line(cursor: Px, g: &Guide) -> bool {
    const HOVER_PX: i32 = 4;
    match g.axis {
        GuideAxis::Horizontal => (cursor.y - g.position).abs() <= HOVER_PX,
        GuideAxis::Vertical => (cursor.x - g.position).abs() <= HOVER_PX,
    }
}

/// True when `cursor` is inside the bounding box of a stuck
/// measurement's value pill. Pill bounds are estimated from the
/// digit count of the value text and the constants used by the
/// renderer (TEXT_STUCK_LOGICAL_PX = 10, proportional padding).
fn cursor_over_stuck_pill(cursor: Px, m: &StuckMeasurement) -> bool {
    let value_text = format!("{}", (m.end - m.start).abs());
    let chars = value_text.len() as f64;
    // Approximation: avg glyph advance ≈ 0.55 × text size.
    let pill_w = (chars * 10.0 * 0.55 + 2.0 * 8.0).max(20.0);
    let pill_h = 10.0 * 1.8; // text + 2 × pad
    let est_pill_h = pill_h as f32;
    let inside_long = match m.axis {
        GuideAxis::Vertical => (m.end - m.start).abs() as f32 >= 3.0 * est_pill_h,
        GuideAxis::Horizontal => (m.end - m.start).abs() as f32 >= 3.0 * est_pill_h,
    };
    let (px, py) = match m.axis {
        GuideAxis::Vertical => {
            let mid = (m.start + m.end) as f64 * 0.5;
            if inside_long {
                (m.at as f64 - pill_w * 0.5, mid - pill_h * 0.5)
            } else {
                // LeftCenter at (m.at + tick_half + 4, mid)
                (m.at as f64 + 9.0, mid - pill_h * 0.5)
            }
        }
        GuideAxis::Horizontal => {
            let mid = (m.start + m.end) as f64 * 0.5;
            if inside_long {
                (mid - pill_w * 0.5, m.at as f64 - pill_h * 0.5)
            } else {
                // AnchorTop at (mid, m.at + tick_half + 4)
                (mid - pill_w * 0.5, m.at as f64 + 9.0)
            }
        }
    };
    let cx = cursor.x as f64;
    let cy = cursor.y as f64;
    cx >= px && cx <= px + pill_w && cy >= py && cy <= py + pill_h
}

/// True when `cursor` (logical pixels) is inside the held rectangle.
/// Excludes the rectangle's 1-px border so guides still draw when the
/// pointer sits exactly on the edge.
fn cursor_in_held_rect(cursor: Px, rect_start: Px, rect_end: Px) -> bool {
    let lo_x = rect_start.x.min(rect_end.x);
    let hi_x = rect_start.x.max(rect_end.x);
    let lo_y = rect_start.y.min(rect_end.y);
    let hi_y = rect_start.y.max(rect_end.y);
    cursor.x > lo_x && cursor.x < hi_x && cursor.y > lo_y && cursor.y < hi_y
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
    guides: &mut Vec<Guide>,
    stuck_measurements: &mut Vec<StuckMeasurement>,
    held_rects: &mut Vec<HeldRect>,
    color_alternate: bool,
) -> ButtonOutcome {
    let fg = hud_foreground(color_alternate);
    let cursor_px = Px::new(x as i32, y as i32);
    if pressed {
        // First: if cursor is over a stuck-measurement pill or a guide
        // line, the click removes that single item (the renderer is
        // showing an X badge to signal this).
        if let Some(idx) = stuck_measurements
            .iter()
            .position(|m| cursor_over_stuck_pill(cursor_px, m))
        {
            log::info!("removing stuck measurement at idx {idx}");
            stuck_measurements.remove(idx);
            return ButtonOutcome::None;
        }
        if let Some(idx) = guides
            .iter()
            .position(|g| cursor_over_guide_line(cursor_px, g))
        {
            log::info!("removing guide at idx {idx}");
            guides.remove(idx);
            return ButtonOutcome::None;
        }
        // Pressing on any held rect's W×H pill takes a screenshot of
        // that region. Otherwise the press starts a new measurement
        // drag — held rects accumulate, the new draw doesn't replace
        // them.
        // Pill click on a held rect → take a screenshot of that rect.
        for rect in held_rects.iter() {
            let rs = Px::new(rect.rect_start.0 as i32, rect.rect_start.1 as i32);
            let re = Px::new(rect.rect_end.0 as i32, rect.rect_end.1 as i32);
            if cursor_over_pill(cursor_px, rs, re) {
                if let Some(frame) = frozen_frame {
                    match take_held_screenshot(frame, rs, re) {
                        Ok(()) => return ButtonOutcome::ScreenshotTaken,
                        Err(e) => log::error!("screenshot failed: {e:#}"),
                    }
                } else {
                    log::warn!("screenshot requested but no frozen frame is available");
                }
                return ButtonOutcome::None;
            }
        }
        // Click inside a held rect (but NOT on its pill) → remove
        // that rect. Pill clicks were handled above.
        if let Some(idx) = held_rects.iter().position(|r| {
            let rs = Px::new(r.rect_start.0 as i32, r.rect_start.1 as i32);
            let re = Px::new(r.rect_end.0 as i32, r.rect_end.1 as i32);
            cursor_in_held_rect(cursor_px, rs, re)
        }) {
            log::info!("removing held rect at idx {idx}");
            held_rects.remove(idx);
            return ButtonOutcome::None;
        }
        if matches!(mode, InteractionMode::Hover { .. }) {
            let snap = SnapPoint::loose(cursor_px);
            log::info!("drag started at ({},{})", cursor_px.x, cursor_px.y);
            *mode = InteractionMode::Drawing { start: snap, cursor: cursor_px };
            // Don't paint the rect yet — wait for the user to actually
            // move past `DRAG_THRESHOLD_PX`. A bare click should look
            // like a hover, not a 1×1 box.
            let edges = edges_for_hud(frozen_frame, x, y, tolerance, guides);
            let mut hud = Hud::hover((x, y));
            hud.foreground = fg;
            hud.kind = HudKind::Hover { cursor: (x, y), edges };
            hud.guides = guides.to_vec();
            hud.stuck_measurements = stuck_measurements.to_vec();
            hud.held_rects = held_rects.to_vec();
            overlay.set_hud(Some(hud));
        }
    } else if let InteractionMode::Drawing { start, .. } = mode {
        // Click-without-drag: short-circuit back to Hover.
        if !has_drag_distance(start.pixel, cursor_px) {
            log::info!("click without drag — no measurement");
            *mode = InteractionMode::Hover { cursor: cursor_px };
            let edges = edges_for_hud(frozen_frame, x, y, tolerance, guides);
            let mut hud = Hud::hover((x, y));
            hud.foreground = fg;
            hud.kind = HudKind::Hover { cursor: (x, y), edges };
            hud.guides = guides.to_vec();
            hud.stuck_measurements = stuck_measurements.to_vec();
            hud.held_rects = held_rects.to_vec();
            overlay.set_hud(Some(hud));
            return ButtonOutcome::None;
        }
        let raw_start = (start.pixel.x as f64, start.pixel.y as f64);
        let raw_end = (x, y);
        // Snap-shrink to fit content.
        let (snapped_start, snapped_end) =
            snap_shrink_logical_rect(frozen_frame, raw_start, raw_end, tolerance);
        let measurement = Measurement::new(
            SnapPoint::loose(Px::new(snapped_start.0.round() as i32, snapped_start.1.round() as i32)),
            SnapPoint::loose(Px::new(snapped_end.0.round() as i32, snapped_end.1.round() as i32)),
        );
        let aspect = if measurement.width() > 0 && measurement.height() > 0 {
            classify_aspect(measurement.width(), measurement.height(), AspectMode::Automatic, 0.02)
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
        held_rects.push(HeldRect {
            rect_start: snapped_start,
            rect_end: snapped_end,
            camera_armed: false,
        });
        *mode = InteractionMode::Hover { cursor: cursor_px };
        let mut hud = Hud::hover((x, y));
        hud.foreground = fg;
        hud.kind = HudKind::Hover {
            cursor: (x, y),
            edges: [None; 4],
        };
        hud.guides = guides.to_vec();
        hud.stuck_measurements = stuck_measurements.to_vec();
        hud.held_rects = held_rects.to_vec();
        overlay.set_hud(Some(hud));
    }
    ButtonOutcome::None
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

/// Crop the held region out of the frozen capture, save it as a PNG to
/// `~/Pictures/screenshot-<timestamp>.png`, copy to the clipboard, and
/// fire a `notify-send` notification with an "Edit" action that opens
/// the saved file in Satty (Omarchy's default screenshot annotator).
/// The notification handler runs on a detached thread because
/// `notify-send -A` blocks until the user acts on or dismisses the
/// notification.
fn take_held_screenshot(
    frame: &vernier_platform::NativeFrame,
    rect_start: Px,
    rect_end: Px,
) -> Result<()> {
    use vernier_platform::PixelFormat;
    let surface_w = frame.bounds.w as f64;
    let surface_h = frame.bounds.h as f64;
    if surface_w <= 0.0 || surface_h <= 0.0 {
        anyhow::bail!("monitor has zero dimensions");
    }
    let scale_x = frame.width as f64 / surface_w;
    let scale_y = frame.height as f64 / surface_h;
    let fx0 = (rect_start.x.min(rect_end.x) as f64 * scale_x).round() as i32;
    let fy0 = (rect_start.y.min(rect_end.y) as f64 * scale_y).round() as i32;
    let fx1 = (rect_start.x.max(rect_end.x) as f64 * scale_x).round() as i32;
    let fy1 = (rect_start.y.max(rect_end.y) as f64 * scale_y).round() as i32;
    let cx0 = fx0.max(0).min(frame.width as i32) as u32;
    let cy0 = fy0.max(0).min(frame.height as i32) as u32;
    let cx1 = fx1.max(0).min(frame.width as i32) as u32;
    let cy1 = fy1.max(0).min(frame.height as i32) as u32;
    let w = cx1.saturating_sub(cx0);
    let h = cy1.saturating_sub(cy0);
    if w == 0 || h == 0 {
        anyhow::bail!("empty screenshot region");
    }

    // Crop + convert to packed RGBA8.
    let mut pixels = Vec::with_capacity((w as usize) * (h as usize) * 4);
    let stride = frame.stride as usize;
    for y in cy0..cy1 {
        let row_off = (y as usize) * stride;
        for x in cx0..cx1 {
            let i = row_off + (x as usize) * 4;
            let chunk = &frame.pixels[i..i + 4];
            match frame.format {
                PixelFormat::Bgra8 => pixels.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]),
                PixelFormat::Bgrx8 => pixels.extend_from_slice(&[chunk[2], chunk[1], chunk[0], 0xFF]),
                PixelFormat::Rgba8 => pixels.extend_from_slice(chunk),
                PixelFormat::Rgbx8 => pixels.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 0xFF]),
                PixelFormat::Xrgb8 => pixels.extend_from_slice(&[chunk[1], chunk[2], chunk[3], 0xFF]),
                PixelFormat::Xbgr8 => pixels.extend_from_slice(&[chunk[3], chunk[2], chunk[1], 0xFF]),
            }
        }
    }

    let img = image::RgbaImage::from_raw(w, h, pixels)
        .ok_or_else(|| anyhow::anyhow!("RgbaImage::from_raw"))?;
    let dir = pictures_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let timestamp = current_timestamp();
    let path = dir.join(format!("screenshot-{timestamp}.png"));
    img.save(&path)
        .with_context(|| format!("write {}", path.display()))?;
    log::info!("screenshot saved: {} ({}×{})", path.display(), w, h);

    // Copy to clipboard. `wl-copy -t image/png < FILE` matches what
    // omarchy-cmd-screenshot does.
    if let Ok(file) = std::fs::File::open(&path) {
        let _ = std::process::Command::new("wl-copy")
            .args(["-t", "image/png"])
            .stdin(file)
            .spawn();
    }

    // Notification with an "Edit" action. notify-send -A blocks until
    // the user acts; we run it on a detached thread and shell out to
    // satty if the action fires.
    let path_str = path.to_string_lossy().into_owned();
    std::thread::spawn(move || {
        let result = std::process::Command::new("notify-send")
            .args([
                "-A",
                "default=Edit",
                "-i",
                &path_str,
                "-t",
                "10000",
                "Screenshot saved",
                "Click to edit with Satty",
            ])
            .output();
        if let Ok(out) = result {
            let action = String::from_utf8_lossy(&out.stdout);
            if action.trim() == "default" {
                let _ = std::process::Command::new("satty")
                    .args([
                        "--filename",
                        &path_str,
                        "--output-filename",
                        &path_str,
                        "--actions-on-enter",
                        "save-to-clipboard",
                        "--save-after-copy",
                        "--copy-command",
                        "wl-copy",
                    ])
                    .spawn();
            }
        }
    });

    Ok(())
}

fn pictures_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("OMARCHY_SCREENSHOT_DIR") {
        return std::path::PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("XDG_PICTURES_DIR") {
        return std::path::PathBuf::from(d);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join("Pictures")
}

fn current_timestamp() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d_%H-%M-%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .to_string()
        })
}

fn ipc_socket_path() -> Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(runtime_dir.join("vernier.sock"))
}
