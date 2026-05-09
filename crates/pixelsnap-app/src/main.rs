use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use vernier_core::{
    classify_aspect, detect_edges, shrink_to_content, shrink_to_content_with_bg, AspectMode,
    EdgeQuad, FrameView, InteractionMode, Measurement, Px, RoundingMode, Settings, SnapPoint,
    Tolerance, Units,
};
use vernier_platform::{
    Accelerator, Color as PlatColor, CursorKind, Frame, Guide, GuideAxis, HeldRect, HotkeyId,
    Hud, HudAxis, HudContextMenu, HudContextMenuIcon, HudContextMenuItem, HudEdge, HudKind,
    HudMeasurementFormat, HudRounding, HudToast, MonitorId, NativeFrame, Platform, PlatformEvent,
    StuckMeasurement, TrayMenu,
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
    /// Open the preferences window. Reads the current settings from
    /// `~/.config/vernier/settings.toml`, lets the user edit them
    /// across the General / Screenshots / Tolerance / Appearance /
    /// Integrations / Shortcuts / About sections, and pings the
    /// running daemon to reload after each save.
    Prefs,
    /// Tell the running daemon to re-read its settings file. Sent
    /// automatically by the prefs window after each save.
    ReloadSettings,
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
        Some(Cmd::Prefs) => run_prefs_window(),
        Some(Cmd::ReloadSettings) => run_client_command("reload-settings"),
        None => {
            // If a daemon is already running, treat the bare invocation
            // as "open prefs" — matches the launcher / desktop-entry
            // expectation that double-launching surfaces the
            // preferences window rather than failing on a busy IPC
            // socket. If no daemon is up, fall through to start one.
            if existing_daemon_responsive() {
                log::info!("daemon already running; opening prefs window");
                let _ = run_client_command("open-prefs");
                Ok(())
            } else {
                run_daemon()
            }
        }
    }
}

/// Probe the IPC socket. Returns true if connecting succeeds —
/// proving a daemon owns the socket. A stale socket file from a
/// crashed daemon refuses connections, which we treat as "not
/// running" so the next launch can replace it.
fn existing_daemon_responsive() -> bool {
    let path = match ipc_socket_path() {
        Ok(p) => p,
        Err(_) => return false,
    };
    if !path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

/// Open the egui prefs window. After each successful save the
/// in-process callback pings the running daemon over IPC so it
/// reloads `settings.toml` without restart.
fn run_prefs_window() -> Result<()> {
    let on_saved: Box<dyn FnMut() + Send> = Box::new(|| {
        // Best-effort: if no daemon is running, the prefs window
        // still works — settings just take effect on next launch.
        if let Err(e) = run_client_command("reload-settings") {
            log::debug!("daemon reload ping failed (ok if not running): {e:#}");
        }
    });
    vernier_ui::run_prefs(on_saved)
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

    let initial_settings = match Settings::load() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("settings.toml: {e:#}; using defaults");
            Settings::default()
        }
    };
    let icon_path = match ensure_app_icon_png() {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!("app icon: {e:#}");
            None
        }
    };
    apply_autostart(&initial_settings.general).unwrap_or_else(|e| {
        log::warn!("autostart: {e:#}");
    });
    ensure_application_desktop_file(icon_path.as_deref()).unwrap_or_else(|e| {
        log::warn!("desktop entry: {e:#}");
    });
    replace_settings(initial_settings.clone());

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
    set_primary_scale_factor(primary.scale_factor);
    let mut overlay = platform.create_overlay(primary.id)?;
    let _tray = if !initial_settings.general.hide_tray_icon {
        Some(platform.create_tray(TrayMenu::minimal("vernier"))?)
    } else {
        log::info!("tray icon hidden via settings.general.hide_tray_icon");
        None
    };

    let initial_accel = Accelerator::parse(&initial_settings.shortcuts.toggle)
        .unwrap_or_else(|| {
            log::warn!(
                "could not parse settings.shortcuts.toggle = {:?}; using default {:?}",
                initial_settings.shortcuts.toggle,
                Accelerator::default().to_string_key(),
            );
            Accelerator::default()
        });
    let mut current_hotkey: Option<HotkeyId> =
        match platform.register_hotkey(initial_accel, "Toggle vernier") {
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
    let mut current_accel = initial_accel;

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
    let mut tol_level = initial_settings.tolerance.default_level;
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
    // Modifier state — tracked separately from the keysym handler so
    // we know it across non-key events too.
    // Shift held → "alignment crosshair" mode (full-screen axis lines,
    // measurements suppressed). Super held → place guides freely
    // (skip the snap-to-detected-edge default).
    let mut shift_held: bool = false;
    let mut super_held: bool = false;
    // Index into `guides` of the guide currently being dragged via
    // pointer down on the line — None when not dragging.
    let mut dragging_guide: Option<usize> = None;
    // Cursor position at the press that started a guide drag — used
    // to tell a click (no movement) from a drag on release.
    let mut guide_press_pos: Option<Px> = None;
    // Last single-click on a guide line (idx + time). A second click
    // on the same guide within DOUBLE_CLICK_WINDOW deletes it.
    let mut last_guide_click: Option<(usize, Instant)> = None;
    const GUIDE_DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);
    // Most recent Esc press time. Esc requires a double-press within
    // ESC_DOUBLE_WINDOW to actually exit measurement mode — single
    // press just shows a hint toast.
    let mut last_esc_at: Option<Instant> = None;
    const ESC_DOUBLE_WINDOW: Duration = Duration::from_millis(700);
    // Live resize op against a held rect — set on press over an
    // edge/corner, cleared on release.
    let mut resizing: Option<ResizeOp> = None;
    // Right-click context menu state. `Some` while open; the renderer
    // reads it to draw the menu, the pointer/keyboard handlers route
    // input to it.
    let mut context_menu: Option<ContextMenuState> = None;
    // Track whether the compositor's theme cursor is currently shown
    // over the overlay. We toggle on hover transitions instead of
    // every frame so set_cursor / set_shape calls don't spam.
    let mut system_pointer_visible: bool = false;
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
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) if id == "open_prefs" => {
                spawn_prefs_window();
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
                // Context menu open → only update its hover row and
                // refresh; suppress the regular crosshair / drag /
                // resize logic until the menu closes.
                if context_menu.is_some() {
                    if !system_pointer_visible {
                        overlay.set_system_pointer_visible(true);
                        system_pointer_visible = true;
                    }
                    let new_hovered = {
                        let m = context_menu.as_ref().unwrap();
                        menu_hit_row(m.origin, MENU_ITEMS, (x, y))
                    };
                    let needs_redraw = context_menu
                        .as_ref()
                        .map(|m| m.hovered != new_hovered)
                        .unwrap_or(false);
                    if let Some(m) = context_menu.as_mut() {
                        m.hovered = new_hovered;
                    }
                    if needs_redraw && last_hud_redraw.elapsed() >= REDRAW_INTERVAL {
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                    continue;
                }
                // While dragging a guide, every pointer-move slides
                // it to the new cursor position on its free axis.
                if let Some(idx) = dragging_guide {
                    if let Some(g) = guides.get_mut(idx) {
                        g.position = match g.axis {
                            GuideAxis::Horizontal => y as i32,
                            GuideAxis::Vertical => x as i32,
                        };
                    }
                }
                if let Some(op) = resizing {
                    if let Some(rect) = held_rects.get_mut(op.rect_idx) {
                        apply_resize(rect, &op, (x, y), &guides, super_held);
                    }
                }
                if last_hud_redraw.elapsed() >= REDRAW_INTERVAL {
                    last_hud_redraw = Instant::now();
                    // Active resize wins; otherwise compute the
                    // handle the cursor is hovering on any held rect.
                    let active_handle = resizing.map(|op| op.handle).or_else(|| {
                        held_rects.iter().find_map(|r| {
                            let rs = Px::new(r.rect_start.0 as i32, r.rect_start.1 as i32);
                            let re = Px::new(r.rect_end.0 as i32, r.rect_end.1 as i32);
                            if cursor_over_pill(cursor_px, rs, re) {
                                None
                            } else {
                                cursor_over_rect_handle(cursor_px, rs, re)
                            }
                        })
                    });
                    // Toggle the compositor's theme cursor on/off
                    // whenever the hover state crosses the "system
                    // pointer" boundary.
                    let want = want_system_pointer(
                        cursor_px,
                        &held_rects,
                        &guides,
                        &stuck_measurements,
                        pending_guide,
                        dragging_guide,
                        resizing,
                        active_handle,
                        context_menu.is_some(),
                        primary.bounds.w as i32,
                        primary.bounds.h as i32,
                    );
                    if want != system_pointer_visible {
                        overlay.set_system_pointer_visible(want);
                        system_pointer_visible = want;
                    }
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
                        shift_held,
                        super_held,
                        primary.bounds.w as i32,
                        primary.bounds.h as i32,
                        active_handle,
                        context_menu.as_ref(),
                    );
                }
            }
            MainEvent::Platform(PlatformEvent::PointerButton {
                button, pressed, x, y, ..
            }) => {
                // Right-click toggles the floating context menu. An
                // active drag / resize blocks it (don't disrupt
                // in-flight gestures).
                if button == BTN_RIGHT {
                    if !pressed {
                        continue;
                    }
                    if dragging_guide.is_some() || resizing.is_some() {
                        continue;
                    }
                    if context_menu.is_some() {
                        context_menu = None;
                    } else {
                        // Cancel any pending guide placement so we
                        // don't end up with both UI states fighting
                        // for the next click.
                        pending_guide = None;
                        let menu_h = menu_content_height_logical(MENU_ITEMS);
                        // Anchor the menu so it doesn't overlap the
                        // crosshair: 10 logical px right of the
                        // vertical axis line and below the horizontal
                        // one.
                        const MENU_CURSOR_GAP: f64 = 10.0;
                        let mut ox = x + MENU_CURSOR_GAP;
                        let mut oy = y + MENU_CURSOR_GAP;
                        ox = ox
                            .min(primary.bounds.w as f64 - MENU_WIDTH_LOGICAL - 1.0)
                            .max(0.0);
                        oy = oy.min(primary.bounds.h as f64 - menu_h - 1.0).max(0.0);
                        let hovered = menu_hit_row((ox, oy), MENU_ITEMS, (x, y));
                        context_menu = Some(ContextMenuState {
                            origin: (ox, oy),
                            cursor_at_open: (x, y),
                            hovered,
                        });
                        // Force the system arrow on so the user has a
                        // visible pointer to click rows with — the
                        // next PointerMove will recompute when needed.
                        if !system_pointer_visible {
                            overlay.set_system_pointer_visible(true);
                            system_pointer_visible = true;
                        }
                        log::info!("context menu opened at ({:.0},{:.0})", ox, oy);
                    }
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
                        shift_held,
                        super_held,
                        primary.bounds.w as i32,
                        primary.bounds.h as i32,
                        None,
                        context_menu.as_ref(),
                    );
                    continue;
                }
                // While the context menu is open, BTN_LEFT routes to
                // the menu — dispatch the hovered row, or close if
                // the click landed outside the menu bounds.
                if button == BTN_LEFT && context_menu.is_some() {
                    if !pressed {
                        // Absorb release so the underlying click logic
                        // doesn't fire after a row was dispatched.
                        continue;
                    }
                    let (origin, hit) = {
                        let m = context_menu.as_ref().unwrap();
                        (m.origin, menu_hit_row(m.origin, MENU_ITEMS, (x, y)))
                    };
                    let action = hit.map(|i| MENU_ITEMS[i].action);
                    let _was_inside = menu_contains(origin, MENU_ITEMS, (x, y));
                    context_menu = None;
                    if let Some(action) = action {
                        match action {
                            MenuAction::AddHorizontalGuide => {
                                pending_guide = Some(GuideAxis::Horizontal);
                                log::info!("guide pending: Horizontal (menu)");
                            }
                            MenuAction::AddVerticalGuide => {
                                pending_guide = Some(GuideAxis::Vertical);
                                log::info!("guide pending: Vertical (menu)");
                            }
                            MenuAction::HoldHorizontalDistance => {
                                if let Some((cx, cy)) = last_pointer_xy {
                                    let edges = edges_for_hud(
                                        frozen_frame.as_ref(),
                                        cx,
                                        cy,
                                        tol_level.value(),
                                        &guides,
                                    );
                                    let m = freeze_axis_measurement(
                                        GuideAxis::Horizontal,
                                        cx,
                                        cy,
                                        &edges,
                                        primary.bounds.w,
                                        primary.bounds.h,
                                    );
                                    stuck_measurements.push(m);
                                }
                            }
                            MenuAction::HoldVerticalDistance => {
                                if let Some((cx, cy)) = last_pointer_xy {
                                    let edges = edges_for_hud(
                                        frozen_frame.as_ref(),
                                        cx,
                                        cy,
                                        tol_level.value(),
                                        &guides,
                                    );
                                    let m = freeze_axis_measurement(
                                        GuideAxis::Vertical,
                                        cx,
                                        cy,
                                        &edges,
                                        primary.bounds.w,
                                        primary.bounds.h,
                                    );
                                    stuck_measurements.push(m);
                                }
                            }
                            MenuAction::OpenScreenshotTool => {
                                let cmd = current_settings()
                                    .integrations
                                    .external_screenshot_command
                                    .clone();
                                log::info!("opening external screenshot tool: {cmd}");
                                if let Err(e) = std::process::Command::new(&cmd).spawn() {
                                    log::warn!("spawn screenshot tool {cmd:?}: {e}");
                                }
                                // Step out of measure mode so the
                                // screenshot tool gets a clean desktop.
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
                            MenuAction::EnterBackgroundMode => {
                                log::info!("entering background mode (toggle off)");
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
                            MenuAction::RestoreLastSession => {
                                let toast_text = match load_session() {
                                    Some((r, g, s)) => {
                                        log::info!(
                                            "session restored (menu): {} rect(s), {} guide(s), {} stuck",
                                            r.len(),
                                            g.len(),
                                            s.len(),
                                        );
                                        held_rects = r;
                                        guides = g;
                                        stuck_measurements = s;
                                        "Session restored".to_string()
                                    }
                                    None => {
                                        log::info!("no saved session to restore");
                                        "No previous session".to_string()
                                    }
                                };
                                active_toast = Some(HudToast { text: toast_text });
                                toast_until = Some(
                                    Instant::now()
                                        + Duration::from_millis(TOAST_TOLERANCE_MS),
                                );
                                spawn_toast_timer(
                                    &combined_tx,
                                    Duration::from_millis(TOAST_TOLERANCE_MS),
                                    false,
                                );
                            }
                            MenuAction::ClearAll => {
                                log::info!("clear all (menu)");
                                guides.clear();
                                stuck_measurements.clear();
                                held_rects.clear();
                            }
                            MenuAction::ClosemacOS => {
                                log::info!("close requested via context menu");
                                break;
                            }
                        }
                    }
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
                        shift_held,
                        super_held,
                        primary.bounds.w as i32,
                        primary.bounds.h as i32,
                        None,
                        context_menu.as_ref(),
                    );
                    continue;
                }
                if button == BTN_LEFT {
                    let cursor_px = Px::new(x as i32, y as i32);
                    // Release ends a resize first if one is active —
                    // before falling through to other release paths.
                    if !pressed && resizing.is_some() {
                        log::info!("resize released");
                        let op = resizing.take().unwrap();
                        // Snap only the side(s) the handle dragged
                        // back to the nearest content boundary.
                        // snap_shrink_resize samples bg from outside
                        // the un-moved corner so the algorithm stays
                        // stable across repeated resizes (the rect's
                        // own top-left can land inside content after
                        // a few iterations and would otherwise pin
                        // the wrong reference color).
                        if !super_held {
                            if let Some(rect) = held_rects.get_mut(op.rect_idx) {
                                let lo_x = rect.rect_start.0.min(rect.rect_end.0);
                                let hi_x = rect.rect_start.0.max(rect.rect_end.0);
                                let lo_y = rect.rect_start.1.min(rect.rect_end.1);
                                let hi_y = rect.rect_start.1.max(rect.rect_end.1);
                                let (snapped_lo, snapped_hi) = snap_shrink_resize(
                                    frozen_frame.as_ref(),
                                    (lo_x, lo_y),
                                    (hi_x, hi_y),
                                    op.handle,
                                    tol_level.value(),
                                );
                                rect.rect_start = snapped_lo;
                                rect.rect_end = snapped_hi;
                            }
                        }
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                        continue;
                    }
                    // Release ends a guide drag if one is active.
                    if !pressed && dragging_guide.is_some() {
                        let idx = dragging_guide.take().unwrap();
                        let press_pos = guide_press_pos.take();
                        // A press+release with virtually no cursor
                        // movement counts as a "click" — track it for
                        // double-click-to-delete. A real drag wipes
                        // any pending click instead.
                        let was_click = press_pos
                            .map(|p| {
                                (cursor_px.x - p.x).abs() <= 2
                                    && (cursor_px.y - p.y).abs() <= 2
                            })
                            .unwrap_or(false);
                        let mut deleted = false;
                        if was_click {
                            let now = Instant::now();
                            if let Some((last_idx, last_t)) = last_guide_click {
                                if last_idx == idx
                                    && now.duration_since(last_t) <= GUIDE_DOUBLE_CLICK_WINDOW
                                {
                                    log::info!("double-click on guide {idx} — removing");
                                    if idx < guides.len() {
                                        guides.remove(idx);
                                    }
                                    last_guide_click = None;
                                    deleted = true;
                                }
                            }
                            if !deleted {
                                last_guide_click = Some((idx, now));
                            }
                        } else {
                            last_guide_click = None;
                        }
                        if !deleted {
                            log::info!("guide drag released at idx {idx}");
                        }
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                        continue;
                    }
                    // Press on a guide × badge → remove that guide.
                    // Press on a guide line (anywhere else) → start
                    // dragging it. Both checks happen BEFORE the
                    // pending-placement / rect-drag paths.
                    if pressed {
                        let screen_w = primary.bounds.w as i32;
                        let screen_h = primary.bounds.h as i32;
                        if let Some(idx) = guides.iter().position(|g| {
                            cursor_over_guide_x_badge(cursor_px, g, screen_w, screen_h)
                        }) {
                            log::info!("removing guide {idx} via X badge");
                            guides.remove(idx);
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
                                shift_held,
                                super_held,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                            continue;
                        }
                        if let Some(idx) =
                            guides.iter().position(|g| cursor_over_guide_line(cursor_px, g))
                        {
                            log::info!("guide drag started at idx {idx}");
                            dragging_guide = Some(idx);
                            guide_press_pos = Some(cursor_px);
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
                                shift_held,
                                super_held,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                            continue;
                        }
                    }
                    // Press on a held rect's edge or corner (and not
                    // on its W×H pill) starts a resize drag — which
                    // pre-empts both the rect-interior remove path
                    // and the new-drag path.
                    if pressed && pending_guide.is_none() {
                        let mut started_resize = false;
                        for (idx, rect) in held_rects.iter().enumerate() {
                            let rs = Px::new(
                                rect.rect_start.0 as i32,
                                rect.rect_start.1 as i32,
                            );
                            let re = Px::new(
                                rect.rect_end.0 as i32,
                                rect.rect_end.1 as i32,
                            );
                            if cursor_over_pill(cursor_px, rs, re) {
                                continue;
                            }
                            if let Some(handle) =
                                cursor_over_rect_handle(cursor_px, rs, re)
                            {
                                resizing = Some(ResizeOp {
                                    rect_idx: idx,
                                    handle,
                                    initial_start: rect.rect_start,
                                    initial_end: rect.rect_end,
                                    initial_cursor: (x, y),
                                });
                                log::info!(
                                    "resize start: rect={idx} handle={:?}",
                                    handle
                                );
                                started_resize = true;
                                break;
                            }
                        }
                        if started_resize {
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
                                shift_held,
                                super_held,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                            continue;
                        }
                    }
                    // While a guide is pending placement, the next
                    // press sticks it at the cursor instead of
                    // starting a measurement drag.
                    if pressed {
                        if let Some(axis) = pending_guide.take() {
                            // Use the snapped position (matches what
                            // the user saw under the move cursor),
                            // unless Super is held for free-place.
                            let position = if super_held {
                                match axis {
                                    GuideAxis::Horizontal => y as i32,
                                    GuideAxis::Vertical => x as i32,
                                }
                            } else {
                                let edges = edges_for_hud(
                                    frozen_frame.as_ref(),
                                    x,
                                    y,
                                    tol_level.value(),
                                    &guides,
                                );
                                match axis {
                                    GuideAxis::Horizontal => snap_to_nearest_y_edge(y, &edges) as i32,
                                    GuideAxis::Vertical => snap_to_nearest_x_edge(x, &edges) as i32,
                                }
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
                                shift_held,
                                super_held,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
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
                        super_held,
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                }
            }
            MainEvent::Platform(PlatformEvent::PointerLeave { .. }) => {}
            MainEvent::Platform(PlatformEvent::KeyboardKey {
                keysym, pressed, ..
            }) => {
                // Track modifiers regardless of mode so they're
                // current when the next non-modifier action fires.
                let is_shift = keysym == 0xffe1 || keysym == 0xffe2;
                let is_super = keysym == 0xffeb || keysym == 0xffec;
                if is_shift {
                    shift_held = pressed;
                    if !matches!(mode, InteractionMode::Idle) {
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
                                shift_held,
                                super_held,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                        }
                    }
                    continue;
                }
                if is_super {
                    super_held = pressed;
                    continue;
                }
                if !pressed || matches!(mode, InteractionMode::Idle) {
                    // Idle daemon ignores non-modifier keyboard;
                    // the layer surface doesn't even hold focus then.
                } else if context_menu.is_some() {
                    // Context menu absorbs keyboard input — only Esc
                    // closes it. Other keys are ignored so the menu
                    // doesn't fire surprise side-actions.
                    if keysym == 0xff1b {
                        log::info!("context menu closed via Esc");
                        context_menu = None;
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
                                shift_held,
                                super_held,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                        }
                    }
                } else if keysym == 0xff1b {
                    // XKB_KEY_Escape = 0xff1b. Two quick presses
                    // exit; a single press just hints. Lets the
                    // user "back out" of pending placements / drags
                    // without losing the whole session.
                    let now = Instant::now();
                    let is_double = last_esc_at
                        .map(|t| now.duration_since(t) <= ESC_DOUBLE_WINDOW)
                        .unwrap_or(false);
                    if is_double {
                        let persist = current_settings().general.session_persistence;
                        log::info!(
                            "esc×2 — {} session ({} rect(s), {} guide(s), {} stuck) and exiting",
                            if persist { "saving" } else { "discarding" },
                            held_rects.len(),
                            guides.len(),
                            stuck_measurements.len(),
                        );
                        if persist {
                            if let Err(e) = save_session(&held_rects, &guides, &stuck_measurements) {
                                log::warn!("save session: {e:#}");
                            }
                        }
                        last_esc_at = None;
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
                    } else {
                        last_esc_at = Some(now);
                        active_toast = Some(HudToast {
                            text: "Press Esc again to exit".into(),
                        });
                        toast_until = Some(now + ESC_DOUBLE_WINDOW);
                        spawn_toast_timer(
                            &combined_tx,
                            ESC_DOUBLE_WINDOW,
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
                                shift_held,
                                super_held,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                        }
                    }
                } else if keysym == 0x0048 || keysym == 0x0042 || keysym == 0x0056 {
                    // Capital H or B → horizontal guide; Capital V →
                    // vertical guide. (B sits next to V on the keyboard
                    // so the pair maps cleanly: B for horizontal, V
                    // for vertical.)
                    let axis = if keysym == 0x0056 {
                        GuideAxis::Vertical
                    } else {
                        GuideAxis::Horizontal
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if keysym == 0x0068 || keysym == 0x0062 || keysym == 0x0076 {
                    // Lowercase 'h' or 'b' → horizontal stuck axis;
                    // 'v' → vertical. Freezes the current crosshair's
                    // extent in that axis with the pixel value pinned.
                    if let Some((x, y)) = last_pointer_xy {
                        let axis = if keysym == 0x0076 {
                            GuideAxis::Vertical
                        } else {
                            GuideAxis::Horizontal
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if keysym == 0x0072 {
                    // Lowercase 'r' — re-capture the screen so the
                    // user can refresh after the underlying content
                    // changed. Capital R is reserved for restore.
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
                                    shift_held,
                                    super_held,
                                    primary.bounds.w as i32,
                                    primary.bounds.h as i32,
                                    None,
                                    context_menu.as_ref(),
                                );
                            }
                        }
                        Err(e) => log::warn!("refresh capture failed: {e}"),
                    }
                } else if keysym == 0x0052 {
                    // Capital R — restore the last saved session
                    // (held rects, guides, stuck measurements). Saved
                    // automatically on Esc-exit.
                    let toast_text = match load_session() {
                        Some((r, g, s)) => {
                            log::info!(
                                "session restored: {} rect(s), {} guide(s), {} stuck",
                                r.len(),
                                g.len(),
                                s.len(),
                            );
                            held_rects = r;
                            guides = g;
                            stuck_measurements = s;
                            "Session restored".to_string()
                        }
                        None => {
                            log::info!("no saved session to restore");
                            "No previous session".to_string()
                        }
                    };
                    active_toast = Some(HudToast { text: toast_text });
                    toast_until =
                        Some(Instant::now() + Duration::from_millis(TOAST_TOLERANCE_MS));
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
                            shift_held,
                            super_held,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if keysym == 0xff0d || keysym == 0xff8d {
                    // Enter / KP_Enter — copy the dimensions of the
                    // hovered held rect (or the only rect if just
                    // one exists) using the configured CopyFormat.
                    let cursor_px = last_pointer_xy
                        .map(|(x, y)| Px::new(x as i32, y as i32));
                    let target = cursor_px
                        .and_then(|c| {
                            held_rects.iter().find(|r| {
                                let rs = Px::new(r.rect_start.0 as i32, r.rect_start.1 as i32);
                                let re = Px::new(r.rect_end.0 as i32, r.rect_end.1 as i32);
                                cursor_in_held_rect(c, rs, re) || cursor_over_pill(c, rs, re)
                            })
                        })
                        .or_else(|| {
                            (held_rects.len() == 1).then(|| &held_rects[0])
                        });
                    if let Some(rect) = target {
                        let w = (rect.rect_end.0 - rect.rect_start.0).abs().round() as u32;
                        let h = (rect.rect_end.1 - rect.rect_start.1).abs().round() as u32;
                        let fmt = current_settings().integrations.copy_dimensions_format;
                        let text = fmt.render(w, h);
                        if let Err(e) = write_clipboard_text(&text) {
                            log::warn!("copy dimensions: {e:#}");
                        } else {
                            log::info!("copied dimensions: {text:?}");
                            active_toast = Some(HudToast {
                                text: format!("Copied: {text}"),
                            });
                            toast_until = Some(
                                Instant::now() + Duration::from_millis(TOAST_TOLERANCE_MS),
                            );
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
                                    shift_held,
                                    super_held,
                                    primary.bounds.w as i32,
                                    primary.bounds.h as i32,
                                    None,
                                    context_menu.as_ref(),
                                );
                            }
                        }
                    } else {
                        log::info!("Enter: no held rect under cursor — nothing to copy");
                    }
                } else if matches!(
                    keysym,
                    0xff51 | 0xff52 | 0xff53 | 0xff54
                ) {
                    // Arrow keys — nudge the hovered held rect.
                    // Shift = 10px step, plain = 1px.
                    let cursor_px = last_pointer_xy
                        .map(|(x, y)| Px::new(x as i32, y as i32));
                    let target_idx = cursor_px.and_then(|c| {
                        held_rects.iter().position(|r| {
                            let rs = Px::new(r.rect_start.0 as i32, r.rect_start.1 as i32);
                            let re = Px::new(r.rect_end.0 as i32, r.rect_end.1 as i32);
                            cursor_in_held_rect(c, rs, re) || cursor_over_pill(c, rs, re)
                        })
                    });
                    if let Some(idx) = target_idx {
                        let step = if shift_held { 10.0 } else { 1.0 };
                        let (dx, dy) = match keysym {
                            0xff51 => (-step, 0.0), // Left
                            0xff52 => (0.0, -step), // Up
                            0xff53 => (step, 0.0),  // Right
                            0xff54 => (0.0, step),  // Down
                            _ => (0.0, 0.0),
                        };
                        if let Some(rect) = held_rects.get_mut(idx) {
                            rect.rect_start.0 += dx;
                            rect.rect_start.1 += dy;
                            rect.rect_end.0 += dx;
                            rect.rect_end.1 += dy;
                        }
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
                                shift_held,
                                super_held,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                        }
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
            MainEvent::Ipc(IpcCmd::ReloadSettings) => {
                log::info!("ipc: reload-settings");
                match Settings::load() {
                    Ok(s) => {
                        if let Err(e) = apply_autostart(&s.general) {
                            log::warn!("autostart: {e:#}");
                        }
                        // Reset live tolerance to the new default so the
                        // user immediately sees their pref reflected.
                        tol_level = s.tolerance.default_level;
                        // Re-register the toggle hotkey if the user
                        // changed it.
                        if let Some(new_accel) = Accelerator::parse(&s.shortcuts.toggle) {
                            if new_accel != current_accel {
                                if let Some(prev) = current_hotkey.take() {
                                    if let Err(e) = platform.unregister_hotkey(prev) {
                                        log::warn!("unregister old hotkey: {e:#}");
                                    }
                                }
                                match platform.register_hotkey(new_accel, "Toggle vernier") {
                                    Ok(id) => {
                                        log::info!(
                                            "toggle hotkey changed to {}",
                                            new_accel.to_string_key(),
                                        );
                                        current_hotkey = Some(id);
                                        current_accel = new_accel;
                                    }
                                    Err(e) => log::warn!(
                                        "register new hotkey {}: {e:#}",
                                        new_accel.to_string_key(),
                                    ),
                                }
                            }
                        } else {
                            log::warn!(
                                "skipping hotkey re-register: could not parse {:?}",
                                s.shortcuts.toggle,
                            );
                        }
                        replace_settings(s);
                    }
                    Err(e) => log::warn!("reload settings: {e:#}"),
                }
            }
            MainEvent::Ipc(IpcCmd::OpenPrefs) => {
                spawn_prefs_window();
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
                        shift_held,
                        super_held,
                        primary.bounds.w as i32,
                        primary.bounds.h as i32,
                        None,
                        context_menu.as_ref(),
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
/// Process-wide settings cache. The daemon initialises it on
/// startup, the IPC `reload-settings` handler swaps in a fresh load
/// after the prefs UI saves, and rendering helpers read through it
/// to avoid threading `&Settings` through every call site.
fn settings_lock() -> &'static std::sync::Mutex<Settings> {
    use std::sync::{Mutex, OnceLock};
    static CELL: OnceLock<Mutex<Settings>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Settings::load().unwrap_or_default()))
}

fn current_settings() -> Settings {
    settings_lock().lock().unwrap().clone()
}

fn replace_settings(s: Settings) {
    *settings_lock().lock().unwrap() = s;
}

fn hud_foreground(alt: bool) -> vernier_platform::Color {
    use vernier_platform::Color;
    let s = current_settings();
    let c = if alt {
        s.appearance.alternative_color
    } else {
        s.appearance.primary_color
    };
    Color::rgba(c.r, c.g, c.b, c.a)
}


fn scale_factor_lock() -> &'static std::sync::Mutex<f64> {
    use std::sync::{Mutex, OnceLock};
    static CELL: OnceLock<Mutex<f64>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(1.0))
}

fn primary_scale_factor() -> f64 {
    *scale_factor_lock().lock().unwrap()
}

fn set_primary_scale_factor(s: f64) {
    *scale_factor_lock().lock().unwrap() = s;
}

/// Pull the currently-configured guide color + measurement format
/// from settings and write them into a freshly-built `Hud`. Called
/// at every refresh_hud branch so the live HUD reflects prefs
/// changes the moment the daemon's IPC reload finishes.
fn populate_hud_appearance(hud: &mut Hud) {
    let s = current_settings();
    let g = s.appearance.guide_color;
    hud.guide_color = PlatColor::rgba(g.r, g.g, g.b, g.a);
    hud.measurement_format = HudMeasurementFormat {
        unit_suffix: match s.appearance.units {
            Units::Px => "px".to_string(),
            Units::Pt => "pt".to_string(),
        },
        rounding: match s.appearance.rounding_mode {
            RoundingMode::Points => HudRounding::Points,
            RoundingMode::PointsRounded => HudRounding::PointsRounded,
            RoundingMode::ScreenPixels => HudRounding::ScreenPixels,
        },
        scale_factor: primary_scale_factor(),
    };
}

/// XDG data dir (`$XDG_DATA_HOME` with `~/.local/share` fallback).
fn xdg_data_dir() -> Result<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .context("no XDG_DATA_HOME or HOME")
}

/// Drop the procedural app icon onto disk under the XDG hicolor
/// theme (256×256 PNG). Both desktop entries reference it as
/// `Icon=vernier`, so launchers (walker, rofi, GNOME activities)
/// resolve it via the standard icon-theme lookup. Returns the
/// installed path so callers can also use it as an absolute Icon=
/// fallback.
fn ensure_app_icon_png() -> Result<PathBuf> {
    let dir = xdg_data_dir()?.join("icons/hicolor/256x256/apps");
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("vernier.png");
    let rgba = vernier_platform::render_app_icon_rgba(256);
    let img = image::RgbaImage::from_raw(256, 256, rgba)
        .ok_or_else(|| anyhow::anyhow!("RgbaImage::from_raw"))?;
    img.save(&path).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Write the application desktop entry (`vernier.desktop`) to
/// `$XDG_DATA_HOME/applications` so app launchers (walker, rofi,
/// the GNOME activity overview, …) find it. Idempotent — overwritten
/// each daemon start so changes to the binary path show up. Exec
/// runs `vernier prefs` so launching from a UI surfaces the
/// preferences window rather than starting a second daemon. Icon
/// uses the absolute path to the PNG we just dropped, so launchers
/// pick it up even on systems without an `index.theme`.
fn ensure_application_desktop_file(icon_path: Option<&Path>) -> Result<()> {
    let dir = xdg_data_dir()?.join("applications");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("vernier.desktop");
    let exe = std::env::current_exe()
        .context("current_exe")?
        .display()
        .to_string();
    let icon = icon_path
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "vernier".into());
    let body = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=macOS\n\
         GenericName=Measurement Overlay\n\
         Comment=Cross-platform measurement overlay (macOS clone)\n\
         Icon={icon}\n\
         Exec={exe} prefs\n\
         Terminal=false\n\
         Categories=Utility;Graphics;\n\
         Keywords=measure;ruler;pixel;design;screenshot;\n\
         StartupNotify=false\n"
    );
    std::fs::write(&path, body)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Helper: read the PNG path the daemon just installed, fall back
/// to `Icon=vernier` (XDG name lookup) if it isn't present.
fn icon_path_for_desktop_entries() -> Option<PathBuf> {
    let p = xdg_data_dir().ok()?.join("icons/hicolor/256x256/apps/vernier.png");
    if p.exists() { Some(p) } else { None }
}

/// Write or remove `~/.config/autostart/vernier.desktop` depending
/// on the user's `general.launch_at_login` preference. Idempotent.
fn apply_autostart(general: &vernier_core::GeneralSettings) -> Result<()> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("no XDG_CONFIG_HOME or HOME")?
        .join("autostart");
    let path = dir.join("vernier.desktop");
    if general.launch_at_login {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let exe = std::env::current_exe()
            .context("current_exe")?
            .display()
            .to_string();
        let icon = icon_path_for_desktop_entries()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "vernier".into());
        let body = format!(
            "[Desktop Entry]\nType=Application\nName=macOS\n\
             Comment=Measurement overlay\n\
             Icon={icon}\n\
             Exec={exe}\nTerminal=false\n\
             Categories=Utility;\nX-GNOME-Autostart-enabled=true\n"
        );
        std::fs::write(&path, body)
            .with_context(|| format!("write {}", path.display()))?;
    } else if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("remove {}", path.display()))?;
    }
    Ok(())
}

/// Spawn `vernier prefs` so the tray "Preferences…" entry can
/// open the settings UI without the daemon hosting an egui window
/// inside its own event loop. If the current binary path can't be
/// resolved (very unusual), fall back to looking up `vernier` on
/// PATH.
fn spawn_prefs_window() {
    let exe = std::env::current_exe().ok();
    let mut cmd = match exe {
        Some(p) => std::process::Command::new(p),
        None => std::process::Command::new("vernier"),
    };
    cmd.arg("prefs");
    match cmd.spawn() {
        Ok(child) => log::info!("prefs window spawned (pid {})", child.id()),
        Err(e) => log::warn!("spawn prefs: {e:#}"),
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
    /// Re-read settings.toml and apply user-tunable behavior
    /// (default tolerance, foreground colors, screenshot dir, …).
    /// Sent by the prefs window after each save.
    ReloadSettings,
    /// Spawn `vernier prefs` so the tray menu can open the
    /// settings UI without the daemon embedding it.
    OpenPrefs,
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
                "reload-settings" => {
                    if sender
                        .send(MainEvent::Ipc(IpcCmd::ReloadSettings))
                        .is_err()
                    {
                        return;
                    }
                }
                "open-prefs" => {
                    if sender.send(MainEvent::Ipc(IpcCmd::OpenPrefs)).is_err() {
                        return;
                    }
                }
                other => log::debug!("ipc unknown command: {other:?}"),
            }
        }
    }
}

/// Linux input event code for the left mouse button (BTN_LEFT).
const BTN_LEFT: u32 = 0x110;
/// Linux input event code for the right mouse button (BTN_RIGHT).
const BTN_RIGHT: u32 = 0x111;

/// Width of the right-click context menu in logical pixels. Hard-coded
/// (rather than auto-fit) so the renderer and the main-loop hit-tester
/// stay in sync without sharing fontdue measurements across crates.
/// Sized to comfortably fit the longest label ("Hold Horizontal
/// Distance") + its shortcut hint with generous padding.
const MENU_WIDTH_LOGICAL: f64 = 340.0;

/// In-flight state of the right-click context menu.
#[derive(Clone, Copy, Debug)]
struct ContextMenuState {
    /// Top-left in logical pixels, already clamped at open-time so the
    /// menu fits on-screen.
    origin: (f64, f64),
    /// Cursor position at the moment the menu opened. While the menu
    /// is open, the live measurement crosshair / edge detection
    /// freezes here so the readout doesn't track the mouse as the
    /// user navigates rows.
    cursor_at_open: (f64, f64),
    hovered: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
enum MenuAction {
    AddHorizontalGuide,
    AddVerticalGuide,
    HoldHorizontalDistance,
    HoldVerticalDistance,
    OpenScreenshotTool,
    EnterBackgroundMode,
    RestoreLastSession,
    ClearAll,
    ClosemacOS,
}

struct MenuItemDef {
    label: &'static str,
    shortcut: Option<&'static str>,
    icon: HudContextMenuIcon,
    action: MenuAction,
    divider_after: bool,
}

const MENU_ITEMS: &[MenuItemDef] = &[
    MenuItemDef {
        label: "Add Horizontal Guide",
        shortcut: Some("\u{21E7}H"),
        icon: HudContextMenuIcon::GuideH,
        action: MenuAction::AddHorizontalGuide,
        divider_after: false,
    },
    MenuItemDef {
        label: "Add Vertical Guide",
        shortcut: Some("\u{21E7}V"),
        icon: HudContextMenuIcon::GuideV,
        action: MenuAction::AddVerticalGuide,
        divider_after: true,
    },
    MenuItemDef {
        label: "Hold Horizontal Distance",
        shortcut: Some("H"),
        icon: HudContextMenuIcon::StuckH,
        action: MenuAction::HoldHorizontalDistance,
        divider_after: false,
    },
    MenuItemDef {
        label: "Hold Vertical Distance",
        shortcut: Some("V"),
        icon: HudContextMenuIcon::StuckV,
        action: MenuAction::HoldVerticalDistance,
        divider_after: true,
    },
    MenuItemDef {
        label: "Open Screenshot Tool",
        shortcut: Some("\u{2318}S"),
        icon: HudContextMenuIcon::Camera,
        action: MenuAction::OpenScreenshotTool,
        divider_after: false,
    },
    MenuItemDef {
        label: "Enter Background Mode",
        shortcut: Some("\u{2303}\u{21E7}\u{2318}F"),
        icon: HudContextMenuIcon::Background,
        action: MenuAction::EnterBackgroundMode,
        divider_after: false,
    },
    MenuItemDef {
        label: "Restore Last Session",
        shortcut: Some("\u{21E7}R"),
        icon: HudContextMenuIcon::Restore,
        action: MenuAction::RestoreLastSession,
        divider_after: true,
    },
    MenuItemDef {
        label: "Clear All",
        shortcut: None,
        icon: HudContextMenuIcon::Clear,
        action: MenuAction::ClearAll,
        divider_after: false,
    },
    MenuItemDef {
        label: "Close macOS",
        shortcut: None,
        icon: HudContextMenuIcon::Close,
        action: MenuAction::ClosemacOS,
        divider_after: false,
    },
];

/// Layout constants shared with `draw_context_menu` in the renderer.
/// Keep these in sync — they're the source of truth for both the
/// hit-test here and the visual layout in wayland.rs.
const MENU_ROW_H: f64 = 32.0;
const MENU_PAD_Y: f64 = 10.0;
const MENU_DIV_PAD_V: f64 = 8.0;
const MENU_DIV_HEIGHT: f64 = 1.0;

/// Total height of the menu in logical px — pad + rows + dividers.
/// Identical formula to the renderer's so clamping stays in sync.
fn menu_content_height_logical(items: &[MenuItemDef]) -> f64 {
    let mut h = MENU_PAD_Y * 2.0;
    for (i, it) in items.iter().enumerate() {
        h += MENU_ROW_H;
        if it.divider_after && i + 1 < items.len() {
            h += 2.0 * MENU_DIV_PAD_V + MENU_DIV_HEIGHT;
        }
    }
    h
}

/// Hit-test the cursor against the menu. Returns the row index under
/// the cursor, or `None` if the cursor is on a divider gap or outside
/// the menu bounds.
fn menu_hit_row(
    origin: (f64, f64),
    items: &[MenuItemDef],
    cursor: (f64, f64),
) -> Option<usize> {
    let cx = cursor.0 - origin.0;
    let cy = cursor.1 - origin.1;
    if cx < 0.0 || cx >= MENU_WIDTH_LOGICAL {
        return None;
    }
    let mut row_y = MENU_PAD_Y;
    for (i, it) in items.iter().enumerate() {
        if cy >= row_y && cy < row_y + MENU_ROW_H {
            return Some(i);
        }
        row_y += MENU_ROW_H;
        if it.divider_after && i + 1 < items.len() {
            row_y += 2.0 * MENU_DIV_PAD_V + MENU_DIV_HEIGHT;
        }
    }
    None
}

/// Cursor inside the menu's outer bounding box (used to decide whether
/// a left-click means "click outside → close" or "click on a row").
fn menu_contains(origin: (f64, f64), items: &[MenuItemDef], cursor: (f64, f64)) -> bool {
    let cx = cursor.0 - origin.0;
    let cy = cursor.1 - origin.1;
    cx >= 0.0
        && cx < MENU_WIDTH_LOGICAL
        && cy >= 0.0
        && cy < menu_content_height_logical(items)
}

/// Convert the static items table to the renderer-friendly form.
fn build_hud_menu_items() -> Vec<HudContextMenuItem> {
    MENU_ITEMS
        .iter()
        .map(|it| HudContextMenuItem {
            label: it.label.into(),
            shortcut: it.shortcut.map(|s| s.into()),
            icon: it.icon,
            divider_after: it.divider_after,
        })
        .collect()
}

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
        populate_hud_appearance(&mut hud);
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
        populate_hud_appearance(&mut hud);
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
#[allow(clippy::too_many_arguments)]
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
    align_mode: bool,
    super_held: bool,
    screen_w: i32,
    screen_h: i32,
    resize_handle: Option<ResizeHandle>,
    context_menu: Option<&ContextMenuState>,
) {
    let fg = hud_foreground(color_alternate);
    // While the context menu is open, freeze the live measurement at
    // the cursor's position when the menu opened — the crosshair, edge
    // ticks, and any cursor-driven hover state stop tracking the mouse
    // so the user can navigate menu rows without the readout jumping
    // around. The actual mouse position is still used for menu hover
    // (handled separately in the PointerMove path).
    let (x, y) = if let Some(m) = context_menu {
        m.cursor_at_open
    } else {
        (x, y)
    };
    let cursor_px = Px::new(x as i32, y as i32);
    // For pending guide placement, snap the position to the nearest
    // detected pixel edge on the relevant axis — unless Super is
    // held, which falls back to free placement at the cursor.
    let (pending_x, pending_y) = if let Some(axis) = pending_guide {
        if super_held {
            (x, y)
        } else {
            let edges = edges_for_hud(frozen_frame, x, y, tolerance, guides);
            match axis {
                GuideAxis::Horizontal => (x, snap_to_nearest_y_edge(y, &edges)),
                GuideAxis::Vertical => (snap_to_nearest_x_edge(x, &edges), y),
            }
        }
    } else {
        (x, y)
    };
    // Compose guides + pending guide. Mark the FIRST committed guide
    // the cursor is over as hovered so the renderer shows an X badge
    // (only one removal target at a time, prevents accidental clicks).
    let mut composed_guides = compose_guides(guides, pending_guide, pending_x, pending_y);
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
    let hovered_guide = guides.iter().find(|g| cursor_over_guide_line(cursor_px, g));
    let over_guide_x = hovered_guide
        .map(|g| cursor_over_guide_x_badge(cursor_px, g, screen_w, screen_h))
        .unwrap_or(false);
    let any_stuck_hover = stuck_measurements
        .iter()
        .any(|m| cursor_over_stuck_pill(cursor_px, m));
    // Cursor swap: any X-to-remove element (held-rect pill, stuck
    // pill, guide X badge) becomes the arrow pointer. The guide line
    // body (between X and edges) becomes the matching resize cursor
    // — drag-to-relocate.
    // Resize cursor takes priority over arrow / move when active
    // (either a live drag is in progress or the cursor is hovering a
    // rect edge/corner). After that, guide-line hover gets a
    // direction-matching resize cursor; X-badge / pill / interior
    // hover get the arrow.
    let cursor_in_rect = (cursor_in_held || any_stuck_hover || over_guide_x)
        && resize_handle.is_none();
    let resize_cursor_kind = resize_handle
        .map(handle_to_cursor_kind)
        .or_else(|| {
            if !over_guide_x {
                hovered_guide.map(|g| match g.axis {
                    GuideAxis::Horizontal => CursorKind::ResizeNS,
                    GuideAxis::Vertical => CursorKind::ResizeEW,
                })
            } else {
                None
            }
        });

    // Right-click context menu: built once here and attached to
    // whichever HUD the regular branches end up producing. The menu
    // stacks on top of the live measurement crosshair / held content,
    // so the user can read measurements while picking actions.
    let menu_for_hud = context_menu.map(|m| HudContextMenu {
        origin: m.origin,
        width: MENU_WIDTH_LOGICAL,
        items: build_hud_menu_items(),
        hovered: m.hovered,
    });
    // While placing a guide, suppress the measurement crosshair —
    // only the guide line(s) should be visible. Crosshairs return as
    // soon as the guide is committed (pending_guide → None).
    if let Some(axis) = pending_guide {
        let mut hud = Hud::hover((x, y));
        hud.kind = HudKind::None;
        hud.foreground = fg;
        populate_hud_appearance(&mut hud);
        hud.toast = toast.cloned();
        hud.guides = composed_guides;
        hud.stuck_measurements = composed_stuck;
        hud.held_rects = composed_rects;
        hud.cursor_in_rect = cursor_in_rect;
        // Resize cursor matching the axis the new guide will move
        // along — same affordance as dragging an existing guide.
        hud.move_cursor_at = Some((pending_x, pending_y));
        hud.cursor_kind = match axis {
            GuideAxis::Horizontal => CursorKind::ResizeNS,
            GuideAxis::Vertical => CursorKind::ResizeEW,
        };
        hud.context_menu = menu_for_hud.clone();
        overlay.set_hud(Some(hud));
        return;
    }
    match mode {
        InteractionMode::Idle => {
            // Idle daemon shouldn't normally reach here, but if it
            // does (e.g. a stray PointerMove during teardown) just
            // push an empty HUD with the menu attached so the menu
            // still draws if someone opened it via tray.
            if menu_for_hud.is_some() {
                let mut hud = Hud::hover((x, y));
                hud.kind = HudKind::None;
                hud.foreground = fg;
                populate_hud_appearance(&mut hud);
                hud.toast = toast.cloned();
                hud.guides = composed_guides;
                hud.stuck_measurements = composed_stuck;
                hud.held_rects = composed_rects;
                hud.context_menu = menu_for_hud;
                overlay.set_hud(Some(hud));
            }
        }
        InteractionMode::Hover { .. } | InteractionMode::Held { .. } => {
            // Shift-held alignment mode: extend the live axis lines
            // to the screen edges (no edge-snap clamping), but keep
            // every other affordance — pills, X badges, hover text
            // swaps — fully interactive.
            let edges = if align_mode {
                [None; 4]
            } else {
                edges_for_hud(frozen_frame, x, y, tolerance, guides)
            };
            let mut hud = Hud {
                kind: HudKind::Hover { cursor: (x, y), edges },
                ..Hud::hover((x, y))
            };
            hud.foreground = fg;
            populate_hud_appearance(&mut hud);
            hud.toast = toast.cloned();
            hud.guides = composed_guides.clone();
            hud.stuck_measurements = composed_stuck.clone();
            hud.held_rects = composed_rects.clone();
            hud.cursor_in_rect = cursor_in_rect;
            hud.align_mode = align_mode;
            // Suppress custom move/resize cursors while the context
            // menu is open — the system arrow takes over so the menu
            // is comfortable to point at.
            if menu_for_hud.is_none() {
                if let Some(kind) = resize_cursor_kind {
                    hud.move_cursor_at = Some((x, y));
                    hud.cursor_kind = kind;
                }
            }
            hud.context_menu = menu_for_hud.clone();
            overlay.set_hud(Some(hud));
        }
        InteractionMode::Drawing { start, .. } => {
            let mut hud = Hud::hover((x, y));
            hud.foreground = fg;
            populate_hud_appearance(&mut hud);
            if has_drag_distance(start.pixel, cursor_px) {
                let start_pos = (start.pixel.x as f64, start.pixel.y as f64);
                // Snap the moving end of the rect to nearby guides on
                // each axis. Super disables snap for free placement.
                let (cx, cy) = if super_held {
                    (x, y)
                } else {
                    (snap_x_to_guides(x, guides), snap_y_to_guides(y, guides))
                };
                hud.kind = HudKind::Drawing { start: start_pos, cursor: (cx, cy) };
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
            hud.context_menu = menu_for_hud.clone();
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

/// Snap a horizontal-guide y position to the nearest detected
/// up/down edge if it's within 8 logical px; otherwise return the
/// raw cursor y.
fn snap_to_nearest_y_edge(cursor_y: f64, edges: &[Option<HudEdge>; 4]) -> f64 {
    const SNAP_PX: f64 = 8.0;
    let mut best = cursor_y;
    let mut best_d = SNAP_PX;
    if let Some(up) = edges[2] {
        let d = (cursor_y - up.position.1).abs();
        if d < best_d {
            best_d = d;
            best = up.position.1;
        }
    }
    if let Some(down) = edges[3] {
        let d = (cursor_y - down.position.1).abs();
        if d < best_d {
            best = down.position.1;
        }
    }
    best
}

/// Same as [`snap_to_nearest_y_edge`] but for vertical guides — snaps
/// the cursor's x to the nearest left/right edge within 8 logical px.
fn snap_to_nearest_x_edge(cursor_x: f64, edges: &[Option<HudEdge>; 4]) -> f64 {
    const SNAP_PX: f64 = 8.0;
    let mut best = cursor_x;
    let mut best_d = SNAP_PX;
    if let Some(left) = edges[0] {
        let d = (cursor_x - left.position.0).abs();
        if d < best_d {
            best_d = d;
            best = left.position.0;
        }
    }
    if let Some(right) = edges[1] {
        let d = (cursor_x - right.position.0).abs();
        if d < best_d {
            best = right.position.0;
        }
    }
    best
}

/// Snap an x coordinate to the nearest vertical guide within 8 logical
/// px. Used while drawing or resizing held rects so edges align cleanly
/// with reference guides.
fn snap_x_to_guides(x: f64, guides: &[Guide]) -> f64 {
    const SNAP_PX: f64 = 8.0;
    let mut best = x;
    let mut best_d = SNAP_PX;
    for g in guides.iter().filter(|g| g.axis == GuideAxis::Vertical) {
        let d = (x - g.position as f64).abs();
        if d < best_d {
            best_d = d;
            best = g.position as f64;
        }
    }
    best
}

/// Mirror of [`snap_x_to_guides`] for horizontal guides.
fn snap_y_to_guides(y: f64, guides: &[Guide]) -> f64 {
    const SNAP_PX: f64 = 8.0;
    let mut best = y;
    let mut best_d = SNAP_PX;
    for g in guides.iter().filter(|g| g.axis == GuideAxis::Horizontal) {
        let d = (y - g.position as f64).abs();
        if d < best_d {
            best_d = d;
            best = g.position as f64;
        }
    }
    best
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
    // Keep edge positions as floats so the renderer's pill text
    // matches the live W×H readout (subtract first, then round).
    // Rounding individually here loses the sub-pixel info detected
    // on HiDPI displays and was the source of an off-by-1 between
    // live and frozen values.
    match axis {
        GuideAxis::Vertical => {
            let up = edges[2].map(|e| e.position.1).unwrap_or(0.0);
            let down = edges[3]
                .map(|e| e.position.1)
                .unwrap_or(surface_h as f64);
            StuckMeasurement {
                axis,
                at: x,
                start: up,
                end: down,
                hovered: false,
            }
        }
        GuideAxis::Horizontal => {
            let left = edges[0].map(|e| e.position.0).unwrap_or(0.0);
            let right = edges[1]
                .map(|e| e.position.0)
                .unwrap_or(surface_w as f64);
            StuckMeasurement {
                axis,
                at: y,
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

/// True when `cursor` is over the X-to-remove badge on a hovered
/// guide. The badge sits at the line's midpoint on the perpendicular
/// axis (screen center along the guide's free axis) — same place the
/// renderer draws it.
fn cursor_over_guide_x_badge(cursor: Px, g: &Guide, screen_w: i32, screen_h: i32) -> bool {
    let (bx, by) = match g.axis {
        GuideAxis::Horizontal => (screen_w / 2, g.position),
        GuideAxis::Vertical => (g.position, screen_h / 2),
    };
    // Pill bounds: ~22 wide × ~14 tall logical (matches stuck pill
    // for "0"). Hit area generous enough for an easy click target.
    const HALF_W: i32 = 13;
    const HALF_H: i32 = 9;
    (cursor.x - bx).abs() <= HALF_W && (cursor.y - by).abs() <= HALF_H
}

/// True when `cursor` is inside the bounding box of a stuck
/// measurement's value pill. Pill bounds are estimated from the
/// digit count of the value text and the constants used by the
/// renderer (TEXT_STUCK_LOGICAL_PX = 10, proportional padding).
fn cursor_over_stuck_pill(cursor: Px, m: &StuckMeasurement) -> bool {
    let length = (m.end - m.start).abs().round() as i64;
    let value_text = format!("{length}");
    let chars = value_text.len() as f64;
    // Approximation: avg glyph advance ≈ 0.55 × text size.
    let pill_w = (chars * 10.0 * 0.55 + 2.0 * 8.0).max(20.0);
    let pill_h = 10.0 * 1.8; // text + 2 × pad
    let est_pill_h = pill_h;
    let inside_long = (m.end - m.start).abs() >= 3.0 * est_pill_h;
    let (px, py) = match m.axis {
        GuideAxis::Vertical => {
            let mid = (m.start + m.end) * 0.5;
            if inside_long {
                (m.at - pill_w * 0.5, mid - pill_h * 0.5)
            } else {
                // LeftCenter at (m.at + tick_half + 4, mid)
                (m.at + 9.0, mid - pill_h * 0.5)
            }
        }
        GuideAxis::Horizontal => {
            let mid = (m.start + m.end) * 0.5;
            if inside_long {
                (mid - pill_w * 0.5, m.at - pill_h * 0.5)
            } else {
                // AnchorTop at (mid, m.at + tick_half + 4)
                (mid - pill_w * 0.5, m.at + 9.0)
            }
        }
    };
    let cx = cursor.x as f64;
    let cy = cursor.y as f64;
    cx >= px && cx <= px + pill_w && cy >= py && cy <= py + pill_h
}

#[derive(Debug, Clone, Copy)]
enum ResizeHandle {
    Top,
    Right,
    Bottom,
    Left,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

#[derive(Debug, Clone, Copy)]
struct ResizeOp {
    rect_idx: usize,
    handle: ResizeHandle,
    initial_start: (f64, f64),
    initial_end: (f64, f64),
    initial_cursor: (f64, f64),
}

/// Hit-test the cursor against a held rect's resize handles. Corners
/// take priority over edges. Returns `None` if the cursor isn't on
/// any handle (in which case the click is interior — remove — or
/// outside the rect entirely).
fn cursor_over_rect_handle(
    cursor: Px,
    rect_start: Px,
    rect_end: Px,
) -> Option<ResizeHandle> {
    let lo_x = rect_start.x.min(rect_end.x);
    let hi_x = rect_start.x.max(rect_end.x);
    let lo_y = rect_start.y.min(rect_end.y);
    let hi_y = rect_start.y.max(rect_end.y);
    const CORNER_PX: i32 = 7;
    const EDGE_PX: i32 = 4;
    if cursor.x < lo_x - EDGE_PX
        || cursor.x > hi_x + EDGE_PX
        || cursor.y < lo_y - EDGE_PX
        || cursor.y > hi_y + EDGE_PX
    {
        return None;
    }
    let near_left = (cursor.x - lo_x).abs() <= CORNER_PX;
    let near_right = (cursor.x - hi_x).abs() <= CORNER_PX;
    let near_top = (cursor.y - lo_y).abs() <= CORNER_PX;
    let near_bottom = (cursor.y - hi_y).abs() <= CORNER_PX;
    if near_left && near_top {
        return Some(ResizeHandle::TopLeft);
    }
    if near_right && near_top {
        return Some(ResizeHandle::TopRight);
    }
    if near_left && near_bottom {
        return Some(ResizeHandle::BottomLeft);
    }
    if near_right && near_bottom {
        return Some(ResizeHandle::BottomRight);
    }
    let on_left = (cursor.x - lo_x).abs() <= EDGE_PX && cursor.y > lo_y && cursor.y < hi_y;
    let on_right = (cursor.x - hi_x).abs() <= EDGE_PX && cursor.y > lo_y && cursor.y < hi_y;
    let on_top = (cursor.y - lo_y).abs() <= EDGE_PX && cursor.x > lo_x && cursor.x < hi_x;
    let on_bottom = (cursor.y - hi_y).abs() <= EDGE_PX && cursor.x > lo_x && cursor.x < hi_x;
    if on_top {
        return Some(ResizeHandle::Top);
    }
    if on_bottom {
        return Some(ResizeHandle::Bottom);
    }
    if on_left {
        return Some(ResizeHandle::Left);
    }
    if on_right {
        return Some(ResizeHandle::Right);
    }
    None
}

fn handle_to_cursor_kind(handle: ResizeHandle) -> CursorKind {
    use ResizeHandle::*;
    match handle {
        Top | Bottom => CursorKind::ResizeNS,
        Left | Right => CursorKind::ResizeEW,
        TopLeft | BottomRight => CursorKind::ResizeNWSE,
        TopRight | BottomLeft => CursorKind::ResizeNESW,
    }
}

/// True when the compositor's theme cursor should show over the
/// overlay — i.e. the user is hovering a clickable element (X badge,
/// pill, rect interior) and we're NOT in a state that demands a
/// custom cursor (guide line drag, rect resize, guide placement).
#[allow(clippy::too_many_arguments)]
fn want_system_pointer(
    cursor_px: Px,
    held_rects: &[HeldRect],
    guides: &[Guide],
    stuck_measurements: &[StuckMeasurement],
    pending_guide: Option<GuideAxis>,
    dragging_guide: Option<usize>,
    resizing: Option<ResizeOp>,
    resize_handle: Option<ResizeHandle>,
    menu_open: bool,
    screen_w: i32,
    screen_h: i32,
) -> bool {
    // The context menu always wants the system arrow, even when it
    // overlaps clickable elements underneath.
    if menu_open {
        return true;
    }
    if pending_guide.is_some()
        || dragging_guide.is_some()
        || resizing.is_some()
        || resize_handle.is_some()
    {
        return false;
    }
    let on_guide_x = guides
        .iter()
        .any(|g| cursor_over_guide_x_badge(cursor_px, g, screen_w, screen_h));
    let on_guide_line = guides.iter().any(|g| cursor_over_guide_line(cursor_px, g));
    if on_guide_line && !on_guide_x {
        // Guide line body — drag handle (custom resize cursor).
        return false;
    }
    let on_held = held_rects.iter().any(|r| {
        let rs = Px::new(r.rect_start.0 as i32, r.rect_start.1 as i32);
        let re = Px::new(r.rect_end.0 as i32, r.rect_end.1 as i32);
        cursor_in_held_rect(cursor_px, rs, re) || cursor_over_pill(cursor_px, rs, re)
    });
    let on_stuck = stuck_measurements
        .iter()
        .any(|m| cursor_over_stuck_pill(cursor_px, m));
    on_held || on_stuck || on_guide_x
}

/// Apply a live resize: re-anchor the rect's appropriate edges to
/// `cursor` based on which handle is being dragged.
fn apply_resize(
    rect: &mut HeldRect,
    op: &ResizeOp,
    cursor: (f64, f64),
    guides: &[Guide],
    super_held: bool,
) {
    let initial_lo_x = op.initial_start.0.min(op.initial_end.0);
    let initial_hi_x = op.initial_start.0.max(op.initial_end.0);
    let initial_lo_y = op.initial_start.1.min(op.initial_end.1);
    let initial_hi_y = op.initial_start.1.max(op.initial_end.1);
    let dx = cursor.0 - op.initial_cursor.0;
    let dy = cursor.1 - op.initial_cursor.1;
    let mut lo_x = initial_lo_x;
    let mut hi_x = initial_hi_x;
    let mut lo_y = initial_lo_y;
    let mut hi_y = initial_hi_y;
    use ResizeHandle::*;
    match op.handle {
        Top => lo_y += dy,
        Bottom => hi_y += dy,
        Left => lo_x += dx,
        Right => hi_x += dx,
        TopLeft => {
            lo_x += dx;
            lo_y += dy;
        }
        TopRight => {
            hi_x += dx;
            lo_y += dy;
        }
        BottomLeft => {
            lo_x += dx;
            hi_y += dy;
        }
        BottomRight => {
            hi_x += dx;
            hi_y += dy;
        }
    }
    // Snap the moving edges to nearby guides — corner handles move
    // both axes, side handles only move one. Super disables snap.
    if !super_held {
        match op.handle {
            Top | TopLeft | TopRight => lo_y = snap_y_to_guides(lo_y, guides),
            Bottom | BottomLeft | BottomRight => hi_y = snap_y_to_guides(hi_y, guides),
            _ => {}
        }
        match op.handle {
            Left | TopLeft | BottomLeft => lo_x = snap_x_to_guides(lo_x, guides),
            Right | TopRight | BottomRight => hi_x = snap_x_to_guides(hi_x, guides),
            _ => {}
        }
    }
    if lo_x > hi_x {
        std::mem::swap(&mut lo_x, &mut hi_x);
    }
    if lo_y > hi_y {
        std::mem::swap(&mut lo_y, &mut hi_y);
    }
    rect.rect_start = (lo_x, lo_y);
    rect.rect_end = (hi_x, hi_y);
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
    super_held: bool,
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
        // (Guide removal and drag-to-move are handled at the main
        // loop level — see the PointerButton branch — because they
        // need access to the dragging_guide state machine.)
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
            populate_hud_appearance(&mut hud);
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
            populate_hud_appearance(&mut hud);
            hud.kind = HudKind::Hover { cursor: (x, y), edges };
            hud.guides = guides.to_vec();
            hud.stuck_measurements = stuck_measurements.to_vec();
            hud.held_rects = held_rects.to_vec();
            overlay.set_hud(Some(hud));
            return ButtonOutcome::None;
        }
        let raw_start = (start.pixel.x as f64, start.pixel.y as f64);
        // Snap the moving end of the rect to nearby guides on release
        // so the committed rect aligns with whatever guide the user
        // saw it snap to mid-drag. Super disables snap.
        let raw_end = if super_held {
            (x, y)
        } else {
            (snap_x_to_guides(x, guides), snap_y_to_guides(y, guides))
        };
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
        populate_hud_appearance(&mut hud);
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

/// Snap-shrink a held rect after a resize-release. Only the side(s)
/// the handle was dragging snap; the opposite side(s) stay where they
/// are. The bg reference for the shrink algorithm is sampled JUST
/// OUTSIDE the un-moved corner / edge — sampling the rect's own
/// top-left (the default) breaks down once the user drags the rect
/// so its top-left lands inside content.
fn snap_shrink_resize(
    frozen_frame: Option<&vernier_platform::NativeFrame>,
    rect_lo: (f64, f64),
    rect_hi: (f64, f64),
    handle: ResizeHandle,
    tolerance: u32,
) -> ((f64, f64), (f64, f64)) {
    let Some(frame) = frozen_frame else {
        return (rect_lo, rect_hi);
    };
    let surface_w = frame.bounds.w as f64;
    let surface_h = frame.bounds.h as f64;
    if surface_w <= 0.0 || surface_h <= 0.0 {
        return (rect_lo, rect_hi);
    }
    let scale_x = frame.width as f64 / surface_w;
    let scale_y = frame.height as f64 / surface_h;
    let view = FrameView {
        pixels: &frame.pixels,
        width: frame.width,
        height: frame.height,
        stride: frame.stride,
    };
    let fx0 = (rect_lo.0 * scale_x).round() as i32;
    let fy0 = (rect_lo.1 * scale_y).round() as i32;
    let fx1 = (rect_hi.0 * scale_x).round() as i32;
    let fy1 = (rect_hi.1 * scale_y).round() as i32;
    let pad_x = (2.0 * scale_x).round() as i32;
    let pad_y = (2.0 * scale_y).round() as i32;
    let mid_fx = (fx0 + fx1) / 2;
    let mid_fy = (fy0 + fy1) / 2;
    use ResizeHandle::*;
    let (bg_x, bg_y) = match handle {
        TopLeft => (fx1 + pad_x, fy1 + pad_y),
        TopRight => (fx0 - pad_x, fy1 + pad_y),
        BottomLeft => (fx1 + pad_x, fy0 - pad_y),
        BottomRight => (fx0 - pad_x, fy0 - pad_y),
        Top => (mid_fx, fy1 + pad_y),
        Bottom => (mid_fx, fy0 - pad_y),
        Left => (fx1 + pad_x, mid_fy),
        Right => (fx0 - pad_x, mid_fy),
    };
    let (sx0, sy0, sx1, sy1) = shrink_to_content_with_bg(
        &view,
        fx0,
        fy0,
        fx1,
        fy1,
        bg_x,
        bg_y,
        Tolerance(tolerance),
    );
    let inv_x = 1.0 / scale_x;
    let inv_y = 1.0 / scale_y;
    let snapped_lo_x = match handle {
        Left | TopLeft | BottomLeft => sx0 as f64 * inv_x,
        _ => rect_lo.0,
    };
    let snapped_hi_x = match handle {
        Right | TopRight | BottomRight => sx1 as f64 * inv_x,
        _ => rect_hi.0,
    };
    let snapped_lo_y = match handle {
        Top | TopLeft | TopRight => sy0 as f64 * inv_y,
        _ => rect_lo.1,
    };
    let snapped_hi_y = match handle {
        Bottom | BottomLeft | BottomRight => sy1 as f64 * inv_y,
        _ => rect_hi.1,
    };
    ((snapped_lo_x, snapped_lo_y), (snapped_hi_x, snapped_hi_y))
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

/// Crop the held region out of the frozen capture and save / copy /
/// notify per the user's screenshot prefs. The notification handler
/// runs on a detached thread because `notify-send -A` blocks until
/// the user acts on or dismisses the notification.
fn take_held_screenshot(
    frame: &vernier_platform::NativeFrame,
    rect_start: Px,
    rect_end: Px,
) -> Result<()> {
    use vernier_platform::PixelFormat;
    let s = current_settings();
    let prefs = s.screenshots.clone();
    let surface_w = frame.bounds.w as f64;
    let surface_h = frame.bounds.h as f64;
    if surface_w <= 0.0 || surface_h <= 0.0 {
        anyhow::bail!("monitor has zero dimensions");
    }
    let scale_x = frame.width as f64 / surface_w;
    let scale_y = frame.height as f64 / surface_h;
    let pad_logical = prefs.padding_px as f64;
    let lo_x_l = rect_start.x.min(rect_end.x) as f64 - pad_logical;
    let lo_y_l = rect_start.y.min(rect_end.y) as f64 - pad_logical;
    let hi_x_l = rect_start.x.max(rect_end.x) as f64 + pad_logical;
    let hi_y_l = rect_start.y.max(rect_end.y) as f64 + pad_logical;
    let fx0 = (lo_x_l * scale_x).round() as i32;
    let fy0 = (lo_y_l * scale_y).round() as i32;
    let fx1 = (hi_x_l * scale_x).round() as i32;
    let fy1 = (hi_y_l * scale_y).round() as i32;
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
    // Retina downscale: physical px → logical px so the file ends up
    // at the on-screen size rather than the raw HiDPI buffer size.
    let img = if prefs.retina_downscale && (scale_x > 1.0 || scale_y > 1.0) {
        let target_w = (w as f64 / scale_x).round() as u32;
        let target_h = (h as f64 / scale_y).round() as u32;
        if target_w > 0 && target_h > 0 {
            image::imageops::resize(
                &img,
                target_w,
                target_h,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            img
        }
    } else {
        img
    };
    let final_w = img.width();
    let final_h = img.height();
    let dir = prefs
        .output_dir
        .clone()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(pictures_dir);
    let dir = expand_user_path(&dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let timestamp = current_timestamp();
    let template = if prefs.filename_template.trim().is_empty() {
        "screenshot-{ts}.png".to_string()
    } else {
        prefs.filename_template.clone()
    };
    let filename = template
        .replace("{ts}", &timestamp)
        .replace("{w}", &final_w.to_string())
        .replace("{h}", &final_h.to_string());
    let path = dir.join(filename);
    img.save(&path)
        .with_context(|| format!("write {}", path.display()))?;
    log::info!(
        "screenshot saved: {} ({}×{}) padding={} retina_downscale={}",
        path.display(),
        final_w,
        final_h,
        prefs.padding_px,
        prefs.retina_downscale,
    );

    if prefs.copy_to_clipboard {
        if let Ok(file) = std::fs::File::open(&path) {
            let _ = std::process::Command::new("wl-copy")
                .args(["-t", "image/png"])
                .stdin(file)
                .spawn();
        }
    }

    if prefs.capture_sound {
        // Best-effort: rely on the user's `canberra-gtk-play` /
        // `paplay` setup for Omarchy's stock shutter sound.
        std::thread::spawn(|| {
            for cmd in ["canberra-gtk-play", "paplay"] {
                let mut c = std::process::Command::new(cmd);
                if cmd == "canberra-gtk-play" {
                    c.args(["-i", "screen-capture"]);
                } else {
                    c.arg("/usr/share/sounds/freedesktop/stereo/screen-capture.oga");
                }
                if c.spawn().is_ok() {
                    break;
                }
            }
        });
    }

    let path_str = path.to_string_lossy().into_owned();
    let satty_action = prefs.satty_edit_action;
    std::thread::spawn(move || {
        let mut args: Vec<&str> = vec![
            "-i",
            &path_str,
            "-t",
            "10000",
            "Screenshot saved",
        ];
        if satty_action {
            args.insert(0, "default=Edit");
            args.insert(0, "-A");
            args.push("Click to edit with Satty");
        } else {
            args.push(&path_str);
        }
        let result = std::process::Command::new("notify-send").args(&args).output();
        if !satty_action {
            return;
        }
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

/// Pipe `text` into `wl-copy`. Used by the Enter-to-copy-dimensions
/// path; the screenshot capture has its own image-mode call.
fn write_clipboard_text(text: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("wl-copy")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("spawn wl-copy")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .context("write to wl-copy stdin")?;
    }
    child.wait().context("wait wl-copy")?;
    Ok(())
}

/// Expand a leading `~` or `~/...` in a path against `$HOME`.
/// Settings persist whatever the user typed; this is the convenient
/// runtime equivalent.
fn expand_user_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        return PathBuf::from(home).join(rest);
    }
    if s == "~" {
        return PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()));
    }
    p.to_path_buf()
}

/// Where the last-session snapshot lives (held rects + guides +
/// stuck axis measurements). Restored on Capital R.
fn session_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = std::path::PathBuf::from(&home).join(".local/share/vernier");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("last-session.txt")
}

/// Save the current persistent state to disk in a human-readable
/// line-based format. Best-effort — failures are logged, not fatal.
fn save_session(
    rects: &[HeldRect],
    guides: &[Guide],
    stuck_measurements: &[StuckMeasurement],
) -> std::io::Result<()> {
    let path = session_path();
    let mut s = String::new();
    s.push_str("# vernier session v1\n");
    for r in rects {
        s.push_str(&format!(
            "rect {} {} {} {}\n",
            r.rect_start.0, r.rect_start.1, r.rect_end.0, r.rect_end.1
        ));
    }
    for g in guides {
        let axis = match g.axis {
            GuideAxis::Horizontal => "h",
            GuideAxis::Vertical => "v",
        };
        s.push_str(&format!("guide {axis} {}\n", g.position));
    }
    for m in stuck_measurements {
        let axis = match m.axis {
            GuideAxis::Horizontal => "h",
            GuideAxis::Vertical => "v",
        };
        s.push_str(&format!("stuck {axis} {} {} {}\n", m.at, m.start, m.end));
    }
    std::fs::write(&path, s)
}

/// Load whatever was last saved. Returns empty vecs if no session
/// file exists or it can't be parsed.
fn load_session() -> Option<(Vec<HeldRect>, Vec<Guide>, Vec<StuckMeasurement>)> {
    let path = session_path();
    let s = std::fs::read_to_string(&path).ok()?;
    let mut rects = Vec::new();
    let mut guides = Vec::new();
    let mut stuck = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["rect", a, b, c, d] => {
                if let (Ok(ax), Ok(ay), Ok(bx), Ok(by)) =
                    (a.parse::<f64>(), b.parse::<f64>(), c.parse::<f64>(), d.parse::<f64>())
                {
                    rects.push(HeldRect {
                        rect_start: (ax, ay),
                        rect_end: (bx, by),
                        camera_armed: false,
                    });
                }
            }
            ["guide", "h", pos] => {
                if let Ok(p) = pos.parse() {
                    guides.push(Guide {
                        axis: GuideAxis::Horizontal,
                        position: p,
                        hovered: false,
                    });
                }
            }
            ["guide", "v", pos] => {
                if let Ok(p) = pos.parse() {
                    guides.push(Guide {
                        axis: GuideAxis::Vertical,
                        position: p,
                        hovered: false,
                    });
                }
            }
            ["stuck", axis, at, start, end] => {
                let ax = match *axis {
                    "h" => GuideAxis::Horizontal,
                    "v" => GuideAxis::Vertical,
                    _ => continue,
                };
                if let (Ok(at), Ok(start), Ok(end)) =
                    (at.parse(), start.parse(), end.parse())
                {
                    stuck.push(StuckMeasurement {
                        axis: ax,
                        at,
                        start,
                        end,
                        hovered: false,
                    });
                }
            }
            _ => {
                log::warn!("session: skipping unparsable line `{line}`");
            }
        }
    }
    Some((rects, guides, stuck))
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
