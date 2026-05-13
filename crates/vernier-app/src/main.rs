use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use vernier_core::{
    classify_aspect, detect_edges, shrink_to_content, shrink_to_content_with_bg,
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
use std::sync::Arc;
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

mod capture_worker;
use capture_worker::CaptureWorker;

/// Minimum gap between HUD redraws / wl_buffer commits.
/// ~120Hz — enough headroom over typical display refresh that the
/// compositor always has a fresh frame, but not so high that we
/// flood the Wayland connection. Used by both the live cursor
/// redraws and the nudge auto-repeat throttle.
// 16ms (~60 Hz) caps surface commits at a rate Hyprland tolerates
// without `wl_surface.frame()` callback backpressure. Sustained
// commits faster than this (we used to throttle at 8ms / 125 Hz)
// caused the compositor to close the wayland socket — surfacing as
// "Broken pipe (os error 32)" and a dead overlay — when the user
// held an arrow key long enough for nudge auto-repeat to accumulate.
const HUD_REDRAW_INTERVAL: Duration = Duration::from_millis(16);

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
                // macOS: NSApp must own the main thread for the
                // tray + overlay windows to receive events. Push
                // the daemon body onto a worker and run NSApp.run()
                // here. Never returns.
                #[cfg(target_os = "macos")]
                {
                    vernier_platform::bootstrap_main(|| {
                        if let Err(e) = run_daemon() {
                            log::error!("daemon exited with error: {e:#}");
                        }
                    });
                }
                #[cfg(not(target_os = "macos"))]
                {
                    run_daemon()
                }
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
/// reloads `settings.toml` without restart. The Quit button in
/// the prefs window dispatches the same `vernier quit` IPC the
/// CLI uses.
///
/// Singleton: a Unix-socket lockfile in `$XDG_RUNTIME_DIR` ensures
/// only one prefs window can be open at a time. When a second
/// `vernier prefs` is invoked, we ask Hyprland (best-effort) to
/// focus the existing window and exit immediately.
///
/// If no daemon is responsive when the launcher invokes `vernier
/// prefs` (the desktop entry's Exec line), we spawn one as a
/// detached child so the user gets the tray icon + global toggle
/// hotkey alongside the prefs window. Without this, clicking the
/// launcher after a previous Quit only opens prefs and leaves the
/// hotkey dead.
fn run_prefs_window() -> Result<()> {
    // Sequoia gotcha: a subprocess spawned by an `.accessory` daemon
    // can't self-activate via AppKit APIs (every
    // `setActivationPolicy(.Regular)` / `activate()` call silently
    // no-ops). The kernel-level `TransformProcessType` does work —
    // call it BEFORE eframe initializes AppKit so the window appears
    // in the Dock and the activate-from-daemon path actually surfaces
    // the window. No-op on non-macOS.
    #[cfg(target_os = "macos")]
    vernier_platform::promote_to_foreground_application();
    let lock_path = prefs_lock_path()?;
    let _prefs_lock = match acquire_prefs_singleton_lock(&lock_path) {
        Some(l) => l,
        None => {
            log::info!(
                "prefs window already open (lock at {}); focusing existing one",
                lock_path.display()
            );
            // Best-effort raise: only meaningful on Hyprland, but
            // safe to ignore failures elsewhere — the existing
            // window is already on screen.
            let _ = std::process::Command::new("hyprctl")
                .args(["dispatch", "focuswindow", "class:vernier-prefs"])
                .output();
            return Ok(());
        }
    };
    let static_bind = static_vernier_bind_in_hypr_config();
    if !existing_daemon_responsive() {
        if let Ok(exe) = std::env::current_exe() {
            match std::process::Command::new(&exe).spawn() {
                Ok(c) => log::info!("daemon auto-spawned by prefs launcher (pid {})", c.id()),
                Err(e) => log::warn!("could not spawn daemon: {e:#}"),
            }
            // Brief pause so the daemon binds the IPC socket before
            // the prefs window starts pinging it on Save.
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    let on_saved: Box<dyn FnMut() + Send> = Box::new(|| {
        // Best-effort: if no daemon is running, the prefs window
        // still works — settings just take effect on next launch.
        if let Err(e) = run_client_command("reload-settings") {
            log::debug!("daemon reload ping failed (ok if not running): {e:#}");
        }
    });
    let on_quit: Box<dyn FnMut() + Send> = Box::new(|| {
        if let Err(e) = run_client_command("quit") {
            log::warn!("daemon quit IPC failed: {e:#}");
        }
    });
    vernier_ui::run_prefs(on_saved, on_quit, static_bind)
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
    // Capture build_id NOW, before anything else might rebuild the
    // on-disk binary. Stored inside vernier_core via OnceLock; the
    // `version` IPC handler reads the cached value rather than
    // re-stat'ing the path.
    let _ = vernier_core::build_id();
    log::info!(
        "vernier {} — daemon (build {})",
        env!("CARGO_PKG_VERSION"),
        vernier_core::build_id()
    );

    // Race-free singleton claim via flock — must happen before any
    // portal work. If two daemons start simultaneously (e.g. the
    // prefs auto-spawn racing with a fresh launcher click), only one
    // gets the lock; the loser exits before kicking off a screencast
    // handshake that would prompt the user a second time.
    let _daemon_lock = match acquire_daemon_singleton_lock()? {
        Some(f) => f,
        None => {
            log::info!("another vernier daemon is already running; exiting");
            return Ok(());
        }
    };

    // Block SIGTERM/SIGINT process-wide so the dedicated signal
    // thread (spawned later, once `combined_tx` exists) can convert
    // them into a graceful Quit. Must run before any thread spawns —
    // the block is inherited.
    block_quit_signals()?;

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
    // Wrap in Arc so the capture worker thread can hold its own
    // reference. Everything else still goes through &*platform via
    // deref coercion — the trait-object API is unchanged.
    let platform: Arc<dyn Platform> = Arc::from(platform);
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
        match platform.create_tray(TrayMenu::minimal("Vernier")) {
            Ok(t) => Some(t),
            Err(e) => {
                log::warn!(
                    "tray icon registration failed: {e:#}. Continuing without a tray; \
                     drive the daemon via the global hotkey or `vernier toggle`."
                );
                None
            }
        }
    } else {
        log::info!("tray icon hidden via settings.general.hide_tray_icon");
        None
    };

    // Toggle hotkey is fully driven by `settings.shortcuts.toggle`.
    // An empty / unparseable string registers nothing — no
    // hardcoded fallback. On Hyprland, any static `bind = …, exec,
    // vernier toggle` in the user's hyprland.conf still fires;
    // we surface a warning so the user knows to clean it up.
    let initial_accel_opt = if initial_settings.shortcuts.toggle.trim().is_empty() {
        log::info!("settings.shortcuts.toggle is empty — no toggle hotkey will be registered");
        None
    } else {
        match Accelerator::parse(&initial_settings.shortcuts.toggle) {
            Some(a) => Some(a),
            None => {
                log::warn!(
                    "could not parse settings.shortcuts.toggle = {:?}; no toggle hotkey will be registered",
                    initial_settings.shortcuts.toggle,
                );
                None
            }
        }
    };
    let on_hyprland = is_hyprland_session();
    if on_hyprland {
        if let Some(path) = static_vernier_bind_in_hypr_config() {
            log::warn!(
                "static `vernier toggle` binding detected in {} — \
                 remove it so the prefs-managed shortcut is the only one",
                path.display(),
            );
        }
    }
    if on_hyprland {
        // Active-window watcher backs the Figma plugin integration:
        // we need to know when a Figma tab is focused so we apply
        // the zoom-correction divisor only there.
        spawn_active_window_watcher();
    }
    if initial_settings.integrations.figma_zoom_correction {
        vernier_platform::figma_bridge::spawn(
            initial_settings.integrations.figma_bridge_port,
        );
    }

    let mut current_hotkey: Option<HotkeyId> = None;
    if let Some(accel) = initial_accel_opt {
        if on_hyprland {
            // Clear any stale runtime bind from a previous daemon
            // run before registering — otherwise hyprctl stacks
            // duplicates and a single key press fires `vernier
            // toggle` multiple times, flickering measure mode.
            let _ = unregister_hyprland_toggle(&accel);
            if !register_hyprland_toggle(&accel) {
                log::warn!("hyprctl bind for toggle failed");
            }
            // Keep the bind alive across `hyprctl reload` (which
            // wipes runtime keyword binds) and Hyprland restarts
            // (which spin up a new instance signature). The watcher
            // re-derives the accel from settings each time, so prefs
            // edits are picked up automatically.
            spawn_hyprland_bind_watcher();
        } else {
            current_hotkey = match platform.register_hotkey(accel, "Toggle Vernier") {
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
        }
    }
    let mut current_accel: Option<Accelerator> = initial_accel_opt;

    // Global "open preferences" hotkey: Cmd+, on macOS (the universal
    // Mac shortcut for app preferences) and Ctrl+, on every other
    // platform. Fires regardless of whether measure mode is open;
    // the handler tears measure mode down, clears persisted state,
    // and brings the prefs window to front. Not user-configurable
    // yet — convention is strong enough that "the comma shortcut"
    // doesn't need a setting.
    let prefs_hotkey_accel_str = if cfg!(target_os = "macos") {
        "META+,"
    } else {
        "CTRL+,"
    };
    let prefs_hotkey: Option<HotkeyId> = Accelerator::parse(prefs_hotkey_accel_str)
        .and_then(|accel| {
            if on_hyprland {
                // Hyprland routes hotkeys through hyprctl, not the
                // platform's register_hotkey. Skip for now; Hyprland
                // users can bind `vernier prefs` themselves.
                None
            } else {
                match platform.register_hotkey(accel, "Vernier Preferences") {
                    Ok(id) => {
                        log::info!(
                            "prefs hotkey registered as {prefs_hotkey_accel_str}",
                        );
                        Some(id)
                    }
                    Err(e) => {
                        log::warn!(
                            "prefs hotkey registration ({prefs_hotkey_accel_str}) failed: {e}"
                        );
                        None
                    }
                }
            }
        });

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

    // IPC socket for `vernier toggle` / `vernier quit`. The
    // daemon flock above already guarantees we're the only daemon,
    // so we can unconditionally remove a stale socket file and bind.
    let socket_path = ipc_socket_path()?;
    let _ = std::fs::remove_file(&socket_path);
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("bind ipc socket at {}", socket_path.display()))?;
    log::info!("ipc socket: {}", socket_path.display());

    // Convert SIGTERM/SIGINT into a clean quit through the same
    // event channel the IPC `quit` command uses. Doing this here
    // (after `combined_tx` exists) keeps shutdown ordering identical
    // to a `vernier quit`: the loop breaks, platform drops, the
    // ashpd D-Bus connection closes, xdg-desktop-portal flushes the
    // screencast restore token to its GVariant DB.
    spawn_signal_quit_thread(combined_tx.clone())?;

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
    const REDRAW_INTERVAL: Duration = HUD_REDRAW_INTERVAL;
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
    // is the in-flight BASE axis the next click will stick to the
    // cursor; the effective axis can be flipped by holding SHIFT
    // post-entry (see `effective_pending_axis`). `guides` are
    // committed lines. Once entered, pending mode is sticky: clicks
    // place a guide and stay in pending; ESC exits.
    let mut guides: Vec<Guide> = Vec::new();
    let mut pending_guide: Option<GuideAxis> = None;
    // Has the user released SHIFT at least once since entering pending
    // mode? Entry via SHIFT+H / SHIFT+V starts with `false` (the trigger
    // is still held); the first release flips this to `true`. After
    // that, holding SHIFT means "flip the axis for the next click".
    // Entries from a shift-less binding start with `true` immediately.
    let mut pending_guide_shift_acked: bool = false;
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
    // measurements suppressed). Alt held → "precise" mode: hide the
    // measurement crosshair / pointer so the user can read pixels under
    // the cursor, and skip the snap-to-detected-edge guide placement.
    // (Originally Super, but Hyprland's default `bindm = SUPER` traps
    // the click for `movewindow` before it reaches our layer surface.)
    // Super is still tracked separately so user-configured shortcuts
    // that include SUPER as a modifier still match.
    let mut shift_held: bool = false;
    let mut alt_held: bool = false;
    let mut ctrl_held: bool = false;
    let mut super_held: bool = false;
    // Cached parsed accelerators for the user's configured
    // shortcuts. Refreshed on startup + on each `reload-settings`
    // so a bare keypress can match against the live config rather
    // than against a hardcoded Esc / Shift+R / Enter table.
    let mut shortcut_accels = parse_shortcut_accels(&initial_settings);
    log_shortcut_accels(&shortcut_accels);
    // Crosshair (alignment) mode is "on while the configured modifier
    // is held". Recomputed on every modifier change and on each
    // settings reload so a re-bound modifier takes effect immediately.
    let mut align_mode: bool = false;
    // Index into `guides` of the guide currently being dragged via
    // pointer down on the line — None when not dragging.
    let mut dragging_guide: Option<usize> = None;
    // Index into `guides` of the "last selected" guide for arrow-key
    // nudging. Set when a guide is freshly placed and when a drag
    // ends without deletion. Cleared on remove / clear-all. Arrow
    // keys nudge this guide by 1px (10px with SHIFT) when no held
    // rect is the active target.
    let mut last_selected_guide: Option<usize> = None;
    // When the most-recent arrow-key press nudged a guide (as opposed
    // to a held rect), this records which guide so the repeat-timer
    // NudgeTick events know to keep nudging it. Cleared when a rect
    // nudge takes over or the guide goes away.
    let mut nudge_guide_idx: Option<usize> = None;
    // Stuck-measurement pill drag state. Press over a pill enters
    // tracking mode; release-with-no-movement removes the
    // measurement (the click path), release-with-movement keeps the
    // new offset. `stuck_press_pos` is the cursor position at press
    // time and `stuck_initial_offset` is the pill_offset the
    // measurement had at press, so the running offset is
    // initial + (cursor - press) clamped to ±100 each axis.
    let mut dragging_stuck_pill: Option<usize> = None;
    let mut stuck_press_pos: Option<(f64, f64)> = None;
    let mut stuck_initial_offset: (f64, f64) = (0.0, 0.0);
    // True once a stuck-pill drag has moved past STUCK_DRAG_THRESHOLD
    // since press. While set:
    //  - the pill renders its value (not the × delete indicator), so
    //    we don't flash the value on a click-to-remove that hasn't
    //    actually become a drag yet,
    //  - the system cursor hides, since the pill is now slaved to the
    //    pointer and the cursor itself just gets in the way.
    let mut stuck_pill_drag_committed: bool = false;
    const STUCK_PILL_DRAG_MAX: f64 = 50.0;
    const STUCK_DRAG_THRESHOLD: f64 = 2.0;
    // Cursor position at the press that started a guide drag — used
    // to tell a click (no movement) from a drag on release.
    let mut guide_press_pos: Option<Px> = None;
    // Last single-click on a guide line (idx + time). A second click
    // on the same guide within DOUBLE_CLICK_WINDOW deletes it.
    let mut last_guide_click: Option<(usize, Instant)> = None;
    const GUIDE_DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);
    // Last clear-and-hide press time. When the user has opted
    // into double-press confirmation, a second press within the
    // configured window (`clear_and_hide_double_press_window_ms`)
    // fires the action. Otherwise the first press fires it.
    let mut last_esc_at: Option<Instant> = None;
    // True between a 1st ESC press and either: (a) a 2nd ESC within
    // the close-and-clear window (full clear+exit), or (b) the
    // window expiring (true freeze via toggle_measurement). While
    // set, refresh_hud renders as if Idle (no live measurement),
    // but the layer surface keeps its input grab so the 2nd ESC
    // can still land.
    let mut pre_clear_freeze: bool = false;
    // Currently-held nudge direction (if any) and a generation
    // counter that the spawned timer thread checks before firing.
    // Bumping the generation invalidates the timer for that
    // direction so it stops without needing inter-thread cancel
    // signals.
    let mut active_nudge: Option<(NudgeDir, u64, u32)> = None; // (dir, generation, keysym)
    let mut nudge_generation: u64 = 0;
    // Sticky nudge target — once a rect is "selected" via a nudge
    // press while the cursor was inside it, subsequent nudges
    // (auto-repeat ticks AND fresh presses) keep moving the same
    // rect even if it slides out from under the cursor. The
    // selection releases when the mouse moves ≥ NUDGE_RELEASE_PX
    // from `anchor`, when the rect is removed, or when measure
    // mode exits.
    let mut nudge_selection: Option<NudgeSelection> = None;
    const NUDGE_RELEASE_PX: f64 = 10.0;
    // Shared with the spawned nudge-timer threads. Each thread
    // captures its own assigned generation; on every tick it checks
    // that the atomic still equals it before sending a NudgeTick,
    // so a key-release / new-direction press invalidates the
    // previous thread without an explicit cancel signal.
    let nudge_active_gen = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    const NUDGE_INITIAL_DELAY_MS: u64 = 225;
    const NUDGE_INTERVAL_MS: u64 = 16; // ~60 Hz — matches HUD_REDRAW_INTERVAL
    // Live resize op against a held rect — set on press over an
    // edge/corner, cleared on release.
    let mut resizing: Option<ResizeOp> = None;
    // Debounce for tray-icon Activate. waybar (and some other SNI
    // hosts) fires Activate twice per click — once on press and
    // once on release, both with the same coordinates. Without
    // this guard, every click on the tray icon spawns two prefs
    // windows.
    let mut last_tray_click: Option<Instant> = None;
    const TRAY_CLICK_DEDUPE: Duration = Duration::from_millis(500);
    // Handle to the in-flight prefs subprocess, if any. Tray click
    // toggles it (open if not running, close if running); the
    // OpenPrefs IPC opens-if-closed (no double window when the
    // user runs `vernier` from a terminal while prefs is up).
    let mut prefs_child: Option<std::process::Child> = None;
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
    // Live mode only: background-thread screen-capture worker. `Some`
    // when measure mode is ON AND freeze_screen is OFF. The daemon's
    // hot path pulls the latest frame via try_latest_frame() without
    // blocking on `CGWindowListCreateImage`. None means either
    // measure mode is off or freeze is on (capture is a one-shot in
    // freeze mode, not a stream).
    let mut capture_worker: Option<CaptureWorker> = None;

    while let Ok(event) = combined_rx.recv() {
        match event {
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) if id == "quit" => {
                log::info!("quit requested via tray");
                break;
            }
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) if id == "toggle_overlay" => {
                // Wipe transient state so an explicit toggle doesn't
                // leave us in a half-frozen / pending-guide limbo.
                pre_clear_freeze = false;
                pending_guide = None;
                pending_guide_shift_acked = false;
                last_esc_at = None;
                toggle_measurement(&mut mode, &mut overlay, &platform, primary.id, &mut frozen_frame, &mut capture_worker, &held_rects, &guides, &stuck_measurements, color_alternate);
            }
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) if id == "open_prefs" => {
                ensure_prefs_window(&mut prefs_child);
            }
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) => {
                log::info!("unhandled tray menu id: {id}");
            }
            MainEvent::Platform(PlatformEvent::HotkeyPressed(id))
                if prefs_hotkey == Some(id) =>
            {
                // Cmd+, (Ctrl+, elsewhere) — universal "open
                // preferences" shortcut. Treat it as an explicit exit
                // from measurement mode: save persisted content,
                // wipe held rects / guides / stuck so the user lands
                // on a clean prefs window with no leftover overlay,
                // then surface prefs.
                log::info!("prefs hotkey: clearing state and opening prefs");
                if let Err(e) = save_session(&held_rects, &guides, &stuck_measurements) {
                    log::warn!("save session before prefs hotkey: {e:#}");
                }
                pre_clear_freeze = false;
                pending_guide = None;
                pending_guide_shift_acked = false;
                last_esc_at = None;
                nudge_selection = None;
                last_selected_guide = None;
                active_toast = None;
                toast_until = None;
                held_rects.clear();
                guides.clear();
                stuck_measurements.clear();
                if !matches!(mode, InteractionMode::Idle) {
                    // In measure mode → toggle off. With the vecs
                    // cleared above, toggle_measurement takes the
                    // clean-hide branch and the overlay disappears.
                    toggle_measurement(
                        &mut mode,
                        &mut overlay,
                        &platform,
                        primary.id,
                        &mut frozen_frame,
                        &mut capture_worker,
                        &held_rects,
                        &guides,
                        &stuck_measurements,
                        color_alternate,
                    );
                } else {
                    // Already idle but the overlay may still be in
                    // passthrough mode showing previously-persisted
                    // rects/guides/stuck. Force the overlay closed so
                    // the cleared state is visible immediately.
                    overlay.set_background_frame(None);
                    overlay.hide();
                    overlay.set_hud(None);
                }
                ensure_prefs_window(&mut prefs_child);
            }
            MainEvent::Platform(PlatformEvent::HotkeyPressed(_)) => {
                // Same reset as the tray toggle path — explicit
                // toggle is the user's "get me out of any sub-mode".
                pre_clear_freeze = false;
                pending_guide = None;
                pending_guide_shift_acked = false;
                last_esc_at = None;
                toggle_measurement(&mut mode, &mut overlay, &platform, primary.id, &mut frozen_frame, &mut capture_worker, &held_rects, &guides, &stuck_measurements, color_alternate);
            }
            MainEvent::Platform(PlatformEvent::TrayIconLeftClicked { .. }) => {
                let now = Instant::now();
                if last_tray_click
                    .map(|t| now.duration_since(t) < TRAY_CLICK_DEDUPE)
                    .unwrap_or(false)
                {
                    log::debug!("tray click within dedupe window — ignoring duplicate");
                    continue;
                }
                last_tray_click = Some(now);
                toggle_prefs_window(&mut prefs_child);
            }
            MainEvent::Platform(PlatformEvent::PointerEnter { x, y, .. })
            | MainEvent::Platform(PlatformEvent::PointerMove {
                monitor: _, x, y, ..
            }) => {
                let cursor_px = Px::new(x as i32, y as i32);
                update_cursor_in_mode(&mut mode, cursor_px);
                last_pointer_xy = Some((x, y));
                // Release a sticky nudge selection once the user
                // has actively moved the mouse — small jitter
                // shouldn't drop it, but a real cursor move
                // (≥ NUDGE_RELEASE_PX) means the user is choosing
                // a new target.
                if let Some(sel) = nudge_selection {
                    let dx = x - sel.anchor.0;
                    let dy = y - sel.anchor.1;
                    if (dx * dx + dy * dy).sqrt() >= NUDGE_RELEASE_PX {
                        nudge_selection = None;
                    }
                }
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
                        refresh_frame_if_live(capture_worker.as_ref(), &mut frozen_frame);
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
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
                // While dragging a stuck-measurement pill, each move
                // updates the pill's offset relative to the press.
                // Clamped to ±STUCK_PILL_DRAG_MAX in each axis so it
                // can't be flung off-screen.
                if let Some(idx) = dragging_stuck_pill {
                    if let (Some(press), Some(m)) = (
                        stuck_press_pos,
                        stuck_measurements.get_mut(idx),
                    ) {
                        let raw_dx = stuck_initial_offset.0 + (x - press.0);
                        let raw_dy = stuck_initial_offset.1 + (y - press.1);
                        m.pill_offset = (
                            raw_dx.clamp(-STUCK_PILL_DRAG_MAX, STUCK_PILL_DRAG_MAX),
                            raw_dy.clamp(-STUCK_PILL_DRAG_MAX, STUCK_PILL_DRAG_MAX),
                        );
                        // First motion past the click/drag threshold
                        // "commits" the drag: the renderer switches
                        // from × back to the value text, the OS
                        // cursor hides, and the compositor confines
                        // the pointer to a 100×100-px box around the
                        // press point so the cursor physically stops
                        // at the same bound the pill_offset clamps to.
                        if !stuck_pill_drag_committed
                            && ((x - press.0).abs() > STUCK_DRAG_THRESHOLD
                                || (y - press.1).abs() > STUCK_DRAG_THRESHOLD)
                        {
                            stuck_pill_drag_committed = true;
                            // Center the confine region on the press
                            // point, but shift it by the pre-existing
                            // pill_offset so the cursor's reachable
                            // range mirrors the pill's ±50 clamp from
                            // its default anchor.
                            let rx = (press.0
                                - STUCK_PILL_DRAG_MAX
                                - stuck_initial_offset.0)
                                .round() as i32;
                            let ry = (press.1
                                - STUCK_PILL_DRAG_MAX
                                - stuck_initial_offset.1)
                                .round() as i32;
                            let side = (2.0 * STUCK_PILL_DRAG_MAX) as i32;
                            overlay.confine_pointer(rx, ry, side, side);
                        }
                    }
                }
                if let Some(op) = resizing {
                    if let Some(rect) = held_rects.get_mut(op.rect_idx) {
                        apply_resize(rect, &op, (x, y), &guides, alt_held);
                    }
                }
                if last_hud_redraw.elapsed() >= REDRAW_INTERVAL {
                    last_hud_redraw = Instant::now();
                    refresh_frame_if_live(capture_worker.as_ref(), &mut frozen_frame);
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
                        alt_held,
                        stuck_pill_drag_committed,
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
                        current_tol_value(tol_level),
                        toast,
                        &guides,
                        pending_guide,
                        &stuck_measurements,
                        &held_rects,
                        color_alternate,
                        align_mode,
                        alt_held,
                        pre_clear_freeze,
                        stuck_pill_drag_committed,
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
                // During the close-and-clear window we look visually
                // idle — drop pointer events so a stray click during
                // the freeze doesn't drop a new held rect / context
                // menu that ghost-appears after the window expires.
                if pre_clear_freeze {
                    continue;
                }
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
                        pending_guide_shift_acked = false;
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
                        current_tol_value(tol_level),
                        toast,
                        &guides,
                        pending_guide,
                        &stuck_measurements,
                        &held_rects,
                        color_alternate,
                        align_mode,
                        alt_held,
                        pre_clear_freeze,
                        stuck_pill_drag_committed,
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
                                        current_tol_value(tol_level),
                                        &guides,
                                    );
                                    let m = freeze_axis_measurement(
                                        GuideAxis::Horizontal,
                                        cx,
                                        cy,
                                        &edges,
                                        primary.bounds.w,
                                        primary.bounds.h,
                                        color_alternate,
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
                                        current_tol_value(tol_level),
                                        &guides,
                                    );
                                    let m = freeze_axis_measurement(
                                        GuideAxis::Vertical,
                                        cx,
                                        cy,
                                        &edges,
                                        primary.bounds.w,
                                        primary.bounds.h,
                                        color_alternate,
                                    );
                                    stuck_measurements.push(m);
                                }
                            }
                            MenuAction::OpenScreenshotTool => {
                                do_take_normal_screenshot(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
                                    &mut held_rects,
                                    &mut guides,
                                    &mut stuck_measurements,
                                    &mut nudge_selection,
                                    &mut last_selected_guide,
                                    &mut pending_guide,
                                    &mut pending_guide_shift_acked,
                                    &mut pre_clear_freeze,
                                    &mut active_toast,
                                    &mut toast_until,
                                    &mut last_esc_at,
                                    color_alternate,
                                );
                            }
                            MenuAction::EnterBackgroundMode => {
                                log::info!("entering background mode (toggle off)");
                                toggle_measurement(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
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
                                        nudge_selection = None;
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
                            MenuAction::OpenPrefs => {
                                log::info!(
                                    "preferences (menu) — clearing all drawings + exiting measure \
                                     mode + opening prefs"
                                );
                                // The user wants a clean transition into
                                // prefs: drop every drawing (held rects,
                                // guides, stuck measurements) so the
                                // overlay isn't left in passthrough with
                                // stale content, then exit measure mode,
                                // then spawn / activate the prefs window.
                                pre_clear_freeze = false;
                                pending_guide = None;
                                pending_guide_shift_acked = false;
                                last_esc_at = None;
                                held_rects.clear();
                                guides.clear();
                                stuck_measurements.clear();
                                nudge_selection = None;
                                last_selected_guide = None;
                                if !matches!(mode, InteractionMode::Idle) {
                                    toggle_measurement(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
                                    &held_rects,
                                        &guides,
                                        &stuck_measurements,
                                        color_alternate,
                                    );
                                }
                                ensure_prefs_window(&mut prefs_child);
                                focus_prefs_window(prefs_child.as_ref());
                            }
                            MenuAction::ClearAll => {
                                log::info!("clear all (menu)");
                                guides.clear();
                                stuck_measurements.clear();
                                held_rects.clear();
                                nudge_selection = None;
                                last_selected_guide = None;
                            }
                            MenuAction::CloseVernier => {
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
                        current_tol_value(tol_level),
                        toast,
                        &guides,
                        pending_guide,
                        &stuck_measurements,
                        &held_rects,
                        color_alternate,
                        align_mode,
                        alt_held,
                        pre_clear_freeze,
                        stuck_pill_drag_committed,
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
                        if !alt_held {
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
                                    current_tol_value(tol_level),
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
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                        continue;
                    }
                    // Release ends a stuck-pill drag if one is active.
                    // No movement → click → remove the measurement.
                    // Movement → keep the new pill_offset (already
                    // updated by PointerMove handler above).
                    if !pressed && dragging_stuck_pill.is_some() {
                        let idx = dragging_stuck_pill.take().unwrap();
                        let press_pos = stuck_press_pos.take();
                        let was_click = press_pos
                            .map(|(px, py)| {
                                (x - px).abs() <= 2.0 && (y - py).abs() <= 2.0
                            })
                            .unwrap_or(false);
                        if was_click {
                            if idx < stuck_measurements.len() {
                                log::info!("removing stuck measurement at idx {idx} (click)");
                                stuck_measurements.remove(idx);
                            }
                        } else {
                            log::info!(
                                "stuck pill drag released at idx {idx} (offset kept)"
                            );
                        }
                        stuck_initial_offset = (0.0, 0.0);
                        if stuck_pill_drag_committed {
                            overlay.release_pointer_confine();
                        }
                        stuck_pill_drag_committed = false;
                        last_hud_redraw = Instant::now();
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                        continue;
                    }
                    // Press over a stuck-measurement pill → start a
                    // pill drag (click without movement will remove
                    // it on release; movement repositions the pill
                    // up to ±100 logical px in each axis).
                    if pressed {
                        let stuck_bboxes = vernier_platform::placement::stuck_pill_bboxes(
                            &stuck_measurements,
                            &held_rects,
                            &current_measurement_format(),
                            primary.bounds.w as f64,
                            primary.bounds.h as f64,
                        );
                        if let Some(idx) = stuck_bboxes
                            .iter()
                            .position(|b| cursor_over_stuck_pill_at(cursor_px, *b))
                        {
                            log::info!("stuck pill press at idx {idx}");
                            dragging_stuck_pill = Some(idx);
                            stuck_press_pos = Some((x, y));
                            stuck_initial_offset = stuck_measurements[idx].pill_offset;
                            continue;
                        }
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
                                    if last_selected_guide == Some(idx) {
                                        last_selected_guide = None;
                                    } else if let Some(sel) = last_selected_guide {
                                        if sel > idx {
                                            last_selected_guide = Some(sel - 1);
                                        }
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
                            // Either a real drag (move) or a single
                            // click — both count as "interacting with
                            // this guide", so it becomes the
                            // arrow-key nudge target.
                            last_selected_guide = Some(idx);
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
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
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
                            if last_selected_guide == Some(idx) {
                                last_selected_guide = None;
                            } else if let Some(sel) = last_selected_guide {
                                if sel > idx {
                                    last_selected_guide = Some(sel - 1);
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
                                current_tol_value(tol_level),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
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
                                current_tol_value(tol_level),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
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
                                current_tol_value(tol_level),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
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
                    // starting a measurement drag. Pending mode is
                    // sticky — the click places a guide but leaves
                    // pending_guide set so the user can drop several
                    // (and toggle axis via SHIFT). ESC exits.
                    if pressed {
                        if let Some(axis) = pending_guide {
                            // Use the snapped position (matches what
                            // the user saw under the move cursor),
                            // unless Alt is held for free-place.
                            let position = if alt_held {
                                match axis {
                                    GuideAxis::Horizontal => y as i32,
                                    GuideAxis::Vertical => x as i32,
                                }
                            } else {
                                let edges = edges_for_hud(
                                    frozen_frame.as_ref(),
                                    x,
                                    y,
                                    current_tol_value(tol_level),
                                    &guides,
                                );
                                match axis {
                                    GuideAxis::Horizontal => snap_to_nearest_y_edge(y, &edges) as i32,
                                    GuideAxis::Vertical => snap_to_nearest_x_edge(x, &edges) as i32,
                                }
                            };
                            guides.push(Guide {
                                axis,
                                position,
                                color_alternate,
                                hovered: false,
                            });
                            last_selected_guide = Some(guides.len() - 1);
                            log::info!("guide stuck: {:?} @ {}", axis, position);
                            last_hud_redraw = Instant::now();
                            let toast = current_toast(&active_toast, toast_until);
                            refresh_hud(
                                &mode,
                                &mut overlay,
                                frozen_frame.as_ref(),
                                x,
                                y,
                                current_tol_value(tol_level),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
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
                        current_tol_value(tol_level),
                        &mut guides,
                        &mut stuck_measurements,
                        &mut held_rects,
                        &mut nudge_selection,
                        color_alternate,
                        alt_held,
                    );
                    last_hud_redraw = Instant::now();
                    if let ButtonOutcome::ScreenshotPillClicked { rs, re } = outcome {
                        // Hide Vernier's overlay before capture. grim
                        // reads layer-shell surfaces too, so the
                        // camera-pill icon AND the surface's background
                        // tint (a subtle blue) would otherwise show up
                        // in the output. `overlay.hide()` paints fully
                        // transparent (vs. `set_hud(None)` which falls
                        // through to drawing the bare tint and causes
                        // the blue flash). Surface stays mapped so we
                        // can re-show it after the capture without a
                        // full reconfig round-trip.
                        overlay.hide();
                        // Wait long enough for the compositor to commit
                        // the transparent surface before grim samples
                        // the framebuffer. ~150ms is a safe single-
                        // vsync margin on Hyprland.
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        #[cfg(target_os = "macos")]
                        let shot_outcome = take_held_screenshot_via_screencapture(rs, re);
                        #[cfg(not(target_os = "macos"))]
                        let shot_outcome = take_held_screenshot_via_grim(rs, re);
                        let handed_off = match shot_outcome {
                            Ok(o) => matches!(o, CaptureOutcome::HandedOff),
                            Err(e) => {
                                log::error!("screenshot failed: {e:#}");
                                false
                            }
                        };
                        if handed_off {
                            // Handoff path: the external annotation app
                            // (Satty etc.) is now opening with the
                            // captured PNG. Persist the session, wipe
                            // every held rect / guide / stuck so a
                            // later toggle-on starts clean, and drop
                            // out of measure mode so the user can
                            // focus on the annotator without our
                            // overlay's content lingering on screen.
                            // Mirrors the Esc clear-and-hide path.
                            log::info!(
                                "handoff complete — clearing {} rect(s), {} guide(s), {} stuck",
                                held_rects.len(),
                                guides.len(),
                                stuck_measurements.len(),
                            );
                            if let Err(e) =
                                save_session(&held_rects, &guides, &stuck_measurements)
                            {
                                log::warn!("save session: {e:#}");
                            }
                            held_rects.clear();
                            nudge_selection = None;
                            last_selected_guide = None;
                            guides.clear();
                            stuck_measurements.clear();
                            pending_guide = None;
                            pending_guide_shift_acked = false;
                            active_toast = None;
                            toast_until = None;
                            last_esc_at = None;
                            pre_clear_freeze = false;
                            toggle_measurement(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
                                    &held_rects,
                                &guides,
                                &stuck_measurements,
                                color_alternate,
                            );
                        } else {
                            // Local-save path: stay in measure mode so
                            // the user can re-shoot the same region
                            // (or any other held rect) without
                            // re-toggling. Toast just confirms the
                            // capture; it elapses without exiting
                            // measure mode.
                            let toast = HudToast { text: "Screenshot taken".into() };
                            active_toast = Some(toast.clone());
                            toast_until =
                                Some(Instant::now() + Duration::from_millis(TOAST_SCREENSHOT_MS));
                            spawn_toast_timer(
                                &combined_tx,
                                Duration::from_millis(TOAST_SCREENSHOT_MS),
                                false,
                            );
                            let toast_ref = current_toast(&active_toast, toast_until);
                            // Re-set the HUD first, *then* show the
                            // overlay. `set_hud` is a no-op redraw while
                            // visible_intent is false (set by our
                            // earlier `overlay.hide()`); calling `show`
                            // after that flips visible_intent back on
                            // and paints the new HUD in one frame.
                            // Doing it the other way around would
                            // briefly repaint the *old* HUD (camera
                            // pill still over the rect) before the
                            // toast version lands.
                            refresh_hud(
                                &mode,
                                &mut overlay,
                                frozen_frame.as_ref(),
                                x,
                                y,
                                current_tol_value(tol_level),
                                toast_ref,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                            overlay.show();
                        }
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
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
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
                keysym, pressed, is_repeat, ..
            }) => {
                log::debug!(
                    "key event: keysym=0x{:x} pressed={} repeat={}",
                    keysym, pressed, is_repeat
                );
                // Track modifiers regardless of mode so they're
                // current when the next non-modifier action fires.
                let is_shift = keysym == 0xffe1 || keysym == 0xffe2;
                let is_super = keysym == 0xffeb || keysym == 0xffec;
                let is_ctrl = keysym == 0xffe3 || keysym == 0xffe4;
                let is_alt = keysym == 0xffe9 || keysym == 0xffea;
                if is_shift || is_ctrl || is_alt || is_super {
                    let shift_was = shift_held;
                    let alt_was = alt_held;
                    if is_shift { shift_held = pressed; }
                    if is_ctrl { ctrl_held = pressed; }
                    if is_alt { alt_held = pressed; }
                    if is_super { super_held = pressed; }
                    // First SHIFT release after entering pending guide
                    // mode "acknowledges" the trigger keypress — from
                    // here on, each SHIFT press toggles the pending
                    // axis (latching). The release-after-trigger gate
                    // keeps the keypress that started pending mode
                    // (e.g. SHIFT+H) from immediately flipping itself.
                    if pending_guide.is_some()
                        && !pending_guide_shift_acked
                        && shift_was
                        && !shift_held
                    {
                        pending_guide_shift_acked = true;
                    }
                    // Latching axis toggle: rising edge of SHIFT while
                    // acknowledged flips the pending guide axis in
                    // place — the user can drop a horizontal guide,
                    // tap SHIFT, drop a vertical one, etc.
                    let shift_pressed_edge = !shift_was && shift_held;
                    let pending_flipped = pending_guide.is_some()
                        && pending_guide_shift_acked
                        && shift_pressed_edge;
                    if pending_flipped {
                        pending_guide = pending_guide.map(flip_axis);
                        log::info!("guide toggled via SHIFT: now {:?}", pending_guide);
                    }
                    let new_align = shortcut_accels
                        .crosshair
                        .map(|m| modifier_held(m, shift_held, ctrl_held, alt_held, super_held))
                        .unwrap_or(false);
                    let align_changed = new_align != align_mode;
                    // Repaint as soon as ALT toggles so the
                    // momentary cursor-hide kicks in / clears without
                    // waiting for the next PointerMove. Also flip the
                    // system pointer right here, since
                    // `want_system_pointer` is otherwise only
                    // re-evaluated on pointer events.
                    let alt_changed = alt_was != alt_held;
                    if align_changed {
                        align_mode = new_align;
                    }
                    if (align_changed || alt_changed || pending_flipped)
                        && !matches!(mode, InteractionMode::Idle)
                    {
                        if let Some((px_x, px_y)) = last_pointer_xy {
                            if alt_changed {
                                let cursor_px = Px::new(px_x as i32, px_y as i32);
                                let active_handle =
                                    resizing.map(|op| op.handle).or_else(|| {
                                        held_rects.iter().find_map(|r| {
                                            let rs = Px::new(
                                                r.rect_start.0 as i32,
                                                r.rect_start.1 as i32,
                                            );
                                            let re = Px::new(
                                                r.rect_end.0 as i32,
                                                r.rect_end.1 as i32,
                                            );
                                            if cursor_over_pill(cursor_px, rs, re) {
                                                None
                                            } else {
                                                cursor_over_rect_handle(
                                                    cursor_px, rs, re,
                                                )
                                            }
                                        })
                                    });
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
                                    alt_held,
                                    stuck_pill_drag_committed,
                                    primary.bounds.w as i32,
                                    primary.bounds.h as i32,
                                );
                                if want != system_pointer_visible {
                                    overlay.set_system_pointer_visible(want);
                                    system_pointer_visible = want;
                                }
                            }
                            last_hud_redraw = Instant::now();
                            let toast = current_toast(&active_toast, toast_until);
                            refresh_hud(
                                &mode,
                                &mut overlay,
                                frozen_frame.as_ref(),
                                px_x,
                                px_y,
                                current_tol_value(tol_level),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                        }
                    }
                    continue;
                }
                let pressed_accel =
                    xkb_to_accelerator(keysym, shift_held, ctrl_held, alt_held, super_held);
                // Auto-repeat events (key held down): only allow
                // through for nudge and tolerance ±. Every other
                // shortcut is one-shot — letting repeats fire
                // would, e.g., make a held Esc instantly trigger
                // the clear-and-hide double-tap.
                if is_repeat
                    && matches_nudge(&pressed_accel, &shortcut_accels).is_none()
                    && pressed_accel != shortcut_accels.tolerance_up
                    && pressed_accel != shortcut_accels.tolerance_down
                {
                    continue;
                }
                // Release of the active nudge key cancels the
                // auto-repeat timer thread by invalidating its
                // generation.
                if !pressed {
                    if let Some((_, _, active_keysym)) = active_nudge {
                        if active_keysym == keysym {
                            nudge_active_gen
                                .store(0, std::sync::atomic::Ordering::Relaxed);
                            active_nudge = None;
                        }
                    }
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
                                current_tol_value(tol_level),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                        }
                    }
                } else if {
                    // Open Prefs binding. macOS expects Cmd+, (Apple's
                    // long-standing "open preferences" convention);
                    // Linux/Windows use Ctrl+, — Super+, would be the
                    // direct port of macOS but Omarchy already binds
                    // that combo at the compositor level (mako's
                    // "Dismiss last notification") so the keypress
                    // never reaches our layer surface.
                    #[cfg(target_os = "macos")]
                    { super_held && keysym == 0x002c }
                    #[cfg(not(target_os = "macos"))]
                    { ctrl_held && keysym == 0x002c }
                } {
                    log::info!("prefs hotkey → opening prefs");
                    // If a measurement is in progress, exit it cleanly
                    // before the prefs window opens. Otherwise the user
                    // is left with the overlay grabbing the cursor /
                    // keyboard while they're trying to interact with
                    // prefs — a frustrating modal lockout. The
                    // `toggle_measurement` call below the matches!
                    // check handles every state (Hover, Drawing, Held)
                    // by transitioning to Idle.
                    if !matches!(mode, InteractionMode::Idle) {
                        pre_clear_freeze = false;
                        pending_guide = None;
                        pending_guide_shift_acked = false;
                        last_esc_at = None;
                        toggle_measurement(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
                                    &held_rects,
                            &guides,
                            &stuck_measurements,
                            color_alternate,
                        );
                    }
                    ensure_prefs_window(&mut prefs_child);
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.clear_and_hide
                    && pending_guide.is_some()
                {
                    // ESC while a guide is pending: leave guide mode
                    // (clear the pending axis), but DON'T enter the
                    // clear-and-hide flow. Guide mode is sticky for
                    // multi-guide placement; ESC is the single exit.
                    log::info!("guide pending: exit (Esc)");
                    pending_guide = None;
                    pending_guide_shift_acked = false;
                    if let Some((x, y)) = last_pointer_xy {
                        last_hud_redraw = Instant::now();
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.clear_and_hide
                {
                    // Configured clear-and-hide shortcut (default
                    // Esc). When `clear_and_hide_double_press` is
                    // on (default), the action requires a second
                    // press within the configured window; the first
                    // press shows a confirmation toast and visually
                    // freezes the overlay (no live measurement). When
                    // off, a single press fires immediately. Either
                    // way the action saves session (if persistence
                    // is on), wipes every held rect / guide / stuck
                    // measurement, and toggles measure mode off.
                    let s = current_settings();
                    // Skip the two-press confirmation when there's
                    // nothing to lose — no held rects, no guides, no
                    // stuck measurements means a single Esc just
                    // exits measure mode cleanly. The double-press
                    // exists to protect persisted content from being
                    // wiped by an accidental Esc; with nothing on
                    // screen there's no content to protect, and the
                    // "Press Esc again" toast is just friction.
                    let has_persisted_content = !held_rects.is_empty()
                        || !guides.is_empty()
                        || !stuck_measurements.is_empty();
                    let need_double =
                        s.shortcuts.clear_and_hide_double_press && has_persisted_content;
                    let window = Duration::from_millis(
                        s.shortcuts.clear_and_hide_double_press_window_ms.clamp(100, 3000) as u64,
                    );
                    let now = Instant::now();
                    let is_double = last_esc_at
                        .map(|t| now.duration_since(t) <= window)
                        .unwrap_or(false);
                    let fire = !need_double || is_double;
                    if fire {
                        if let Err(e) =
                            save_session(&held_rects, &guides, &stuck_measurements)
                        {
                            log::warn!("save session: {e:#}");
                        }
                        log::info!(
                            "clear-and-hide×2 — clearing {} rect(s), {} guide(s), {} stuck",
                            held_rects.len(),
                            guides.len(),
                            stuck_measurements.len(),
                        );
                        last_esc_at = None;
                        pre_clear_freeze = false;
                        held_rects.clear();
                        nudge_selection = None;
                        last_selected_guide = None;
                        guides.clear();
                        stuck_measurements.clear();
                        pending_guide = None;
                        pending_guide_shift_acked = false;
                        active_toast = None;
                        toast_until = None;
                        toggle_measurement(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
                                    &held_rects,
                            &guides,
                            &stuck_measurements,
                            color_alternate,
                        );
                    } else {
                        last_esc_at = Some(now);
                        // Visually freeze immediately: refresh_hud will
                        // render only persisted content (no live
                        // measurement crosshair) for the duration of
                        // the close-and-clear window. Input grab stays
                        // so the 2nd ESC can land. The
                        // ClearAndHideWindowExpired event fires after
                        // `window` to drop the grab if no 2nd press
                        // arrives.
                        pre_clear_freeze = true;
                        let key_label = shortcut_accels
                            .clear_and_hide
                            .as_ref()
                            .map(|a| a.to_string_key())
                            .unwrap_or_else(|| "Esc".into());
                        active_toast = Some(HudToast {
                            text: format!("Press {key_label} again to clear and exit"),
                        });
                        toast_until = Some(now + window);
                        spawn_toast_timer(&combined_tx, window, false);
                        spawn_clear_and_hide_window_timer(&combined_tx, window, now);
                        if let Some((x, y)) = last_pointer_xy {
                            last_hud_redraw = Instant::now();
                            let toast = current_toast(&active_toast, toast_until);
                            refresh_hud(
                                &mode,
                                &mut overlay,
                                frozen_frame.as_ref(),
                                x,
                                y,
                                current_tol_value(tol_level),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                        }
                    }
                } else if pressed_accel.is_some()
                    && (pressed_accel == shortcut_accels.guide_horizontal
                        || pressed_accel == shortcut_accels.guide_vertical)
                {
                    // Configured guide-placement shortcuts (default
                    // SHIFT+H = horizontal, SHIFT+V = vertical). The
                    // overlay enters "pending guide" mode and sticks
                    // it on each click — sticky, ESC to exit. Holding
                    // SHIFT (after the trigger is released) flips the
                    // axis for the next click.
                    let axis = if pressed_accel == shortcut_accels.guide_vertical {
                        GuideAxis::Vertical
                    } else {
                        GuideAxis::Horizontal
                    };
                    pending_guide = Some(axis);
                    // If SHIFT was the trigger (the default binds use
                    // SHIFT+H / SHIFT+V), wait for it to be released
                    // before treating SHIFT as the axis-flip modifier.
                    // Otherwise the trigger keypress would immediately
                    // flip the axis on itself.
                    pending_guide_shift_acked = !shift_held;
                    log::info!("guide pending: {:?} (click to stick, Esc to exit)", axis);
                    if let Some((x, y)) = last_pointer_xy {
                        last_hud_redraw = Instant::now();
                        let toast = current_toast(&active_toast, toast_until);
                        refresh_hud(
                            &mode,
                            &mut overlay,
                            frozen_frame.as_ref(),
                            x,
                            y,
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.color_toggle
                {
                    // Configured color-toggle shortcut (default `X`).
                    // Swaps the live HUD foreground (and pending
                    // guide preview) between primary and alternate.
                    // Already-placed rects / stucks / guides keep
                    // whichever color they had at placement.
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
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if pressed_accel.is_some()
                    && (pressed_accel == shortcut_accels.stuck_horizontal
                        || pressed_accel == shortcut_accels.stuck_vertical)
                {
                    // Configured stuck-axis shortcuts (default `H` =
                    // horizontal, `V` = vertical). Freezes the current
                    // crosshair's extent in that axis with the pixel
                    // value pinned.
                    if let Some((x, y)) = last_pointer_xy {
                        let axis = if pressed_accel == shortcut_accels.stuck_vertical {
                            GuideAxis::Vertical
                        } else {
                            GuideAxis::Horizontal
                        };
                        let edges = edges_for_hud(
                            frozen_frame.as_ref(),
                            x,
                            y,
                            current_tol_value(tol_level),
                            &guides,
                        );
                        let measurement = freeze_axis_measurement(
                            axis,
                            x,
                            y,
                            &edges,
                            primary.bounds.w,
                            primary.bounds.h,
                            color_alternate,
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
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.tolerance_up
                {
                    tol_level = tol_level.higher();
                    log::info!("tolerance → {} ({})", tol_level.label(), current_tol_value(tol_level));
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
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.tolerance_down
                {
                    tol_level = tol_level.lower();
                    log::info!("tolerance → {} ({})", tol_level.label(), current_tol_value(tol_level));
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
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.refresh_capture
                {
                    // Configured refresh-capture shortcut (default
                    // `R`) — recapture the screen so subsequent
                    // edge detection sees the current content.
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
                                    current_tol_value(tol_level),
                                    toast,
                                    &guides,
                                    pending_guide,
                                    &stuck_measurements,
                                    &held_rects,
                                    color_alternate,
                                    align_mode,
                                    alt_held,
                                    pre_clear_freeze,
                                    stuck_pill_drag_committed,
                                    primary.bounds.w as i32,
                                    primary.bounds.h as i32,
                                    None,
                                    context_menu.as_ref(),
                                );
                            }
                        }
                        Err(e) => log::warn!("refresh capture failed: {e}"),
                    }
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.take_normal_screenshot
                {
                    // Configured take-normal-screenshot shortcut
                    // (default `Ctrl+S`). Same teardown + detached
                    // spawn as the right-click menu's
                    // "Take Normal Screenshot" row.
                    do_take_normal_screenshot(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
                                    &mut held_rects,
                        &mut guides,
                        &mut stuck_measurements,
                        &mut nudge_selection,
                        &mut last_selected_guide,
                        &mut pending_guide,
                        &mut pending_guide_shift_acked,
                        &mut pre_clear_freeze,
                        &mut active_toast,
                        &mut toast_until,
                        &mut last_esc_at,
                        color_alternate,
                    );
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.restore
                {
                    // Configured restore-session shortcut (default
                    // Shift+R). Restores held rects / guides /
                    // stuck measurements saved automatically on
                    // Esc-exit.
                    let toast_text = match load_session() {
                        Some((r, g, s)) => {
                            log::info!(
                                "session restored: {} rect(s), {} guide(s), {} stuck",
                                r.len(),
                                g.len(),
                                s.len(),
                            );
                            held_rects = r;
                            nudge_selection = None;
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
                            current_tol_value(tol_level),
                            toast,
                            &guides,
                            pending_guide,
                            &stuck_measurements,
                            &held_rects,
                            color_alternate,
                            align_mode,
                            alt_held,
                            pre_clear_freeze,
                            stuck_pill_drag_committed,
                            primary.bounds.w as i32,
                            primary.bounds.h as i32,
                            None,
                            context_menu.as_ref(),
                        );
                    }
                } else if pressed_accel.is_some()
                    && pressed_accel == shortcut_accels.capture
                {
                    // Configured capture shortcut (default Enter) —
                    // copy the dimensions of the hovered held rect
                    // (or the only rect if just one exists) using
                    // the configured CopyFormat.
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
                                    current_tol_value(tol_level),
                                    toast,
                                    &guides,
                                    pending_guide,
                                    &stuck_measurements,
                                    &held_rects,
                                    color_alternate,
                                    align_mode,
                                    alt_held,
                                    pre_clear_freeze,
                                    stuck_pill_drag_committed,
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
                } else if matches_nudge(&pressed_accel, &shortcut_accels).is_some() {
                    // Configured nudge shortcuts (default arrow
                    // keys). SHIFT is treated as a 10× step
                    // multiplier independent of the bound key, so a
                    // user who binds e.g. SHIFT+H+W would still get
                    // the multiplier on top.
                    let dir = matches_nudge(&pressed_accel, &shortcut_accels)
                        .expect("matches_nudge guarded above");
                    // If a selection is already pinned (cursor
                    // hasn't moved ≥ NUDGE_RELEASE_PX since pin
                    // time), keep operating on it. Otherwise try
                    // to grab a fresh rect under the cursor.
                    let idx = nudge_selection
                        .filter(|s| s.rect_idx < held_rects.len())
                        .map(|s| s.rect_idx)
                        .or_else(|| {
                            last_pointer_xy.and_then(|(x, y)| {
                                let c = Px::new(x as i32, y as i32);
                                held_rects.iter().position(|r| {
                                    let rs = Px::new(
                                        r.rect_start.0 as i32,
                                        r.rect_start.1 as i32,
                                    );
                                    let re = Px::new(
                                        r.rect_end.0 as i32,
                                        r.rect_end.1 as i32,
                                    );
                                    cursor_in_held_rect(c, rs, re)
                                        || cursor_over_pill(c, rs, re)
                                })
                            })
                        });
                    // Fallback: when no held rect is the target, nudge
                    // the last-selected guide instead (the one the user
                    // just placed or just dragged). Each press = 1px
                    // (10px with SHIFT). Perpendicular arrows are no-ops
                    // — a horizontal guide only moves Up/Down, a
                    // vertical guide only moves Left/Right.
                    if idx.is_none() {
                        if let Some(g_idx) = last_selected_guide
                            .filter(|i| *i < guides.len())
                        {
                            let step: i32 = if shift_held { 10 } else { 1 };
                            let nudged = apply_guide_nudge(
                                &mut guides, g_idx, dir, step,
                            );
                            if nudged {
                                nudge_guide_idx = Some(g_idx);
                                nudge_selection = None;
                                log::debug!(
                                    "guide nudge {:?} by {} px → {}",
                                    dir, step, guides[g_idx].position
                                );
                                if let Some((px_x, px_y)) = last_pointer_xy {
                                    last_hud_redraw = Instant::now();
                                    let toast = current_toast(&active_toast, toast_until);
                                    refresh_hud(
                                        &mode,
                                        &mut overlay,
                                        frozen_frame.as_ref(),
                                        px_x,
                                        px_y,
                                        current_tol_value(tol_level),
                                        toast,
                                        &guides,
                                        pending_guide,
                                        &stuck_measurements,
                                        &held_rects,
                                        color_alternate,
                                        align_mode,
                                        alt_held,
                                        pre_clear_freeze,
                                        stuck_pill_drag_committed,
                                        primary.bounds.w as i32,
                                        primary.bounds.h as i32,
                                        None,
                                        context_menu.as_ref(),
                                    );
                                }
                                // Spawn (or restart) the repeat timer
                                // so holding the arrow key continues to
                                // nudge — same pattern as held-rect
                                // nudges, just routed via
                                // `nudge_guide_idx` in the tick handler.
                                if !is_repeat {
                                    nudge_generation = nudge_generation.wrapping_add(1);
                                    let this_gen = nudge_generation;
                                    nudge_active_gen
                                        .store(this_gen, std::sync::atomic::Ordering::Relaxed);
                                    active_nudge = Some((dir, this_gen, keysym));
                                    let tx = combined_tx.clone();
                                    let atomic = nudge_active_gen.clone();
                                    std::thread::Builder::new()
                                        .name("vernier-nudge-repeat".into())
                                        .spawn(move || {
                                            std::thread::sleep(Duration::from_millis(
                                                NUDGE_INITIAL_DELAY_MS,
                                            ));
                                            loop {
                                                if atomic
                                                    .load(std::sync::atomic::Ordering::Relaxed)
                                                    != this_gen
                                                {
                                                    return;
                                                }
                                                if tx
                                                    .send(MainEvent::NudgeTick {
                                                        dir,
                                                        generation: this_gen,
                                                    })
                                                    .is_err()
                                                {
                                                    return;
                                                }
                                                std::thread::sleep(Duration::from_millis(
                                                    NUDGE_INTERVAL_MS,
                                                ));
                                            }
                                        })
                                        .ok();
                                }
                            }
                            continue;
                        }
                        continue;
                    }
                    let idx = idx.expect("guarded by is_none check");
                    // Switching to a held-rect nudge — clear any
                    // lingering guide-nudge target so the tick handler
                    // doesn't keep moving an unrelated guide.
                    nudge_guide_idx = None;
                    // Pin / refresh the selection on every fresh
                    // press so the anchor tracks the most recent
                    // mouse position the user committed to.
                    if let Some((x, y)) = last_pointer_xy {
                        nudge_selection = Some(NudgeSelection {
                            rect_idx: idx,
                            anchor: (x, y),
                        });
                    }
                    apply_nudge_step(
                        dir,
                        idx,
                        shift_held,
                        alt_held,
                        align_mode,
                        color_alternate,
                        last_pointer_xy,
                        &mut held_rects,
                        &mode,
                        &mut overlay,
                        frozen_frame.as_ref(),
                        current_tol_value(tol_level),
                        &active_toast,
                        toast_until,
                        &guides,
                        pending_guide,
                        pre_clear_freeze,
                        stuck_pill_drag_committed,
                        &stuck_measurements,
                        primary.bounds.w as i32,
                        primary.bounds.h as i32,
                        context_menu.as_ref(),
                        &mut last_hud_redraw,
                    );
                    // Start (or restart) the auto-repeat timer for
                    // this direction. Bumping the generation
                    // invalidates any previously-spawned thread.
                    if !is_repeat {
                        nudge_generation = nudge_generation.wrapping_add(1);
                        let this_gen = nudge_generation;
                        nudge_active_gen
                            .store(this_gen, std::sync::atomic::Ordering::Relaxed);
                        active_nudge = Some((dir, this_gen, keysym));
                        let tx = combined_tx.clone();
                        let atomic = nudge_active_gen.clone();
                        std::thread::Builder::new()
                            .name("vernier-nudge-repeat".into())
                            .spawn(move || {
                                std::thread::sleep(Duration::from_millis(
                                    NUDGE_INITIAL_DELAY_MS,
                                ));
                                loop {
                                    if atomic.load(std::sync::atomic::Ordering::Relaxed)
                                        != this_gen
                                    {
                                        return;
                                    }
                                    if tx
                                        .send(MainEvent::NudgeTick {
                                            dir,
                                            generation: this_gen,
                                        })
                                        .is_err()
                                    {
                                        return;
                                    }
                                    std::thread::sleep(Duration::from_millis(
                                        NUDGE_INTERVAL_MS,
                                    ));
                                }
                            })
                            .ok();
                    }
                }
            }
            MainEvent::Platform(other) => log::debug!("platform event: {other:?}"),
            MainEvent::Ipc(IpcCmd::Toggle) => {
                toggle_measurement(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
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
                        shortcut_accels = parse_shortcut_accels(&s);
                        align_mode = shortcut_accels
                            .crosshair
                            .map(|m| modifier_held(m, shift_held, ctrl_held, alt_held, super_held))
                            .unwrap_or(false);
                        log_shortcut_accels(&shortcut_accels);
                        // Re-register the toggle hotkey if the user
                        // changed it. An empty / unparseable
                        // setting unregisters without re-binding.
                        let new_accel_opt = if s.shortcuts.toggle.trim().is_empty() {
                            None
                        } else {
                            Accelerator::parse(&s.shortcuts.toggle)
                        };
                        if new_accel_opt != current_accel {
                            // Tear down the previous bind no matter
                            // what the new one is.
                            if let Some(old) = current_accel {
                                if on_hyprland {
                                    let _ = unregister_hyprland_toggle(&old);
                                } else if let Some(prev) = current_hotkey.take() {
                                    if let Err(e) = platform.unregister_hotkey(prev) {
                                        log::warn!("unregister old hotkey: {e:#}");
                                    }
                                }
                            }
                            // Register the new bind only if there is one.
                            if let Some(accel) = new_accel_opt {
                                if on_hyprland {
                                    if register_hyprland_toggle(&accel) {
                                        log::info!(
                                            "toggle hotkey changed to {}",
                                            accel.to_string_key(),
                                        );
                                    } else {
                                        log::warn!(
                                            "hyprctl bind for new toggle {} failed",
                                            accel.to_string_key(),
                                        );
                                    }
                                } else {
                                    match platform.register_hotkey(accel, "Toggle Vernier") {
                                        Ok(id) => {
                                            log::info!(
                                                "toggle hotkey changed to {}",
                                                accel.to_string_key(),
                                            );
                                            current_hotkey = Some(id);
                                        }
                                        Err(e) => log::warn!(
                                            "register new hotkey {}: {e:#}",
                                            accel.to_string_key(),
                                        ),
                                    }
                                }
                            } else {
                                log::info!("toggle hotkey cleared (no shortcut configured)");
                            }
                            current_accel = new_accel_opt;
                        }
                        let was_frozen = current_settings().general.freeze_screen;
                        replace_settings(s);
                        let is_frozen = current_settings().general.freeze_screen;
                        // freeze_screen toggled mid-session while
                        // measure mode is on: spin the capture worker
                        // up or down to match. Freeze → live needs a
                        // fresh capture worker; live → freeze must
                        // stop the existing worker so it isn't still
                        // pegging CPU producing frames nobody reads.
                        if was_frozen != is_frozen && !matches!(mode, InteractionMode::Idle) {
                            if is_frozen {
                                if let Some(w) = capture_worker.take() {
                                    w.stop();
                                }
                            } else {
                                capture_worker = Some(CaptureWorker::start(
                                    Arc::clone(&platform),
                                    primary.id,
                                    LIVE_CAPTURE_INTERVAL,
                                ));
                            }
                        }
                        // Push a fresh HUD frame so prefs that
                        // affect overlay rendering (show_cursor,
                        // display_units, wh_indicators, guide
                        // color, etc.) take effect immediately
                        // rather than on the next pointer move.
                        // Also re-run refresh_frame_if_live so
                        // toggling freeze_screen ↔ live picks up
                        // a current frame on save instead of
                        // waiting for the next pointer event.
                        if !matches!(mode, InteractionMode::Idle) {
                            refresh_frame_if_live(capture_worker.as_ref(), &mut frozen_frame);
                            if let Some((x, y)) = last_pointer_xy {
                                last_hud_redraw = Instant::now();
                                let toast = current_toast(&active_toast, toast_until);
                                refresh_hud(
                                    &mode,
                                    &mut overlay,
                                    frozen_frame.as_ref(),
                                    x,
                                    y,
                                    current_tol_value(tol_level),
                                    toast,
                                    &guides,
                                    pending_guide,
                                    &stuck_measurements,
                                    &held_rects,
                                    color_alternate,
                                    align_mode,
                                    alt_held,
                                    pre_clear_freeze,
                                    stuck_pill_drag_committed,
                                    primary.bounds.w as i32,
                                    primary.bounds.h as i32,
                                    None,
                                    context_menu.as_ref(),
                                );
                            }
                        }
                    }
                    Err(e) => log::warn!("reload settings: {e:#}"),
                }
            }
            MainEvent::Ipc(IpcCmd::OpenPrefs) => {
                ensure_prefs_window(&mut prefs_child);
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
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
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
                        current_tol_value(tol_level),
                        None,
                        &guides,
                        pending_guide,
                        &stuck_measurements,
                        &held_rects,
                        color_alternate,
                        align_mode,
                        alt_held,
                        pre_clear_freeze,
                        stuck_pill_drag_committed,
                        primary.bounds.w as i32,
                        primary.bounds.h as i32,
                        None,
                        context_menu.as_ref(),
                    );
                }
            }
            MainEvent::NudgeTick { dir, generation } => {
                // Stale ticks (a newer key was pressed or the user
                // released the held key) bump the active generation
                // so we ignore them here.
                if active_nudge.map(|(_, g, _)| g) != Some(generation) {
                    continue;
                }
                if matches!(mode, InteractionMode::Idle) {
                    continue;
                }
                // Guide repeat takes precedence: if the initial press
                // nudged a guide, every subsequent tick should keep
                // nudging that same guide until the key releases.
                if let Some(g_idx) = nudge_guide_idx
                    .filter(|i| *i < guides.len())
                {
                    let step: i32 = if shift_held { 10 } else { 1 };
                    if apply_guide_nudge(&mut guides, g_idx, dir, step) {
                        if let Some((px_x, px_y)) = last_pointer_xy {
                            last_hud_redraw = Instant::now();
                            let toast = current_toast(&active_toast, toast_until);
                            refresh_hud(
                                &mode,
                                &mut overlay,
                                frozen_frame.as_ref(),
                                px_x,
                                px_y,
                                current_tol_value(tol_level),
                                toast,
                                &guides,
                                pending_guide,
                                &stuck_measurements,
                                &held_rects,
                                color_alternate,
                                align_mode,
                                alt_held,
                                pre_clear_freeze,
                                stuck_pill_drag_committed,
                                primary.bounds.w as i32,
                                primary.bounds.h as i32,
                                None,
                                context_menu.as_ref(),
                            );
                        }
                    }
                    continue;
                }
                let Some(sel) = nudge_selection else { continue };
                if sel.rect_idx >= held_rects.len() {
                    nudge_selection = None;
                    continue;
                }
                apply_nudge_step(
                    dir,
                    sel.rect_idx,
                    shift_held,
                    alt_held,
                    align_mode,
                    color_alternate,
                    last_pointer_xy,
                    &mut held_rects,
                    &mode,
                    &mut overlay,
                    frozen_frame.as_ref(),
                    current_tol_value(tol_level),
                    &active_toast,
                    toast_until,
                    &guides,
                    pending_guide,
                    pre_clear_freeze,
                    stuck_pill_drag_committed,
                    &stuck_measurements,
                    primary.bounds.w as i32,
                    primary.bounds.h as i32,
                    context_menu.as_ref(),
                    &mut last_hud_redraw,
                );
            }
            MainEvent::ClearAndHideWindowExpired { esc_at } => {
                // The 2s close-and-clear window from a 1st ESC press
                // has elapsed without a 2nd ESC. A 2nd ESC would have
                // cleared `last_esc_at` (and called toggle_measurement
                // itself for the full clear+exit), so seeing the
                // original timestamp still set is the cancel-token
                // signaling "no 2nd press came".
                if last_esc_at != Some(esc_at) {
                    continue;
                }
                last_esc_at = None;
                pre_clear_freeze = false;
                active_toast = None;
                toast_until = None;
                if !matches!(mode, InteractionMode::Idle) {
                    // Drop input capture + transition to true Idle
                    // with content preserved (or hide if empty).
                    toggle_measurement(
                                    &mut mode,
                                    &mut overlay,
                                    &platform,
                                    primary.id,
                                    &mut frozen_frame,
                                    &mut capture_worker,
                                    &held_rects,
                        &guides,
                        &stuck_measurements,
                        color_alternate,
                    );
                }
            }
        }
    }

    // Clean up the runtime hyprctl bind so the next daemon launch
    // doesn't stack duplicates (and so a stale `vernier toggle`
    // bind doesn't keep firing into a dead IPC socket).
    if on_hyprland {
        if let Some(accel) = current_accel {
            let _ = unregister_hyprland_toggle(&accel);
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

fn current_tol_value(level: vernier_core::ToleranceLevel) -> u32 {
    current_settings().tolerance.value_for(level)
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

/// Build the [`HudMeasurementFormat`] that matches whatever the
/// renderer will pick up from `current_settings()`. Used by the
/// hit-test path so pill placement comes out the same as on screen.
fn current_measurement_format() -> HudMeasurementFormat {
    let s = current_settings();
    let unit_suffix = if s.general.display_units {
        match s.appearance.units {
            Units::Px => "px".to_string(),
            Units::Pt => "pt".to_string(),
        }
    } else {
        String::new()
    };
    let (dimension_divisor, _) = current_figma_correction(&s);
    HudMeasurementFormat {
        unit_suffix,
        rounding: match s.appearance.rounding_mode {
            RoundingMode::Points => HudRounding::Points,
            RoundingMode::PointsRounded => HudRounding::PointsRounded,
            RoundingMode::ScreenPixels => HudRounding::ScreenPixels,
        },
        scale_factor: primary_scale_factor(),
        wh_indicators: s.general.display_wh_indicators,
        aspect_in_area: s.general.aspect_in_area_tool,
        aspect_mode: s.general.aspect_mode,
        dimension_divisor,
    }
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

/// When `general.freeze_screen` is off, pull the latest frame from
/// the platform so edge detection follows live content. No-op in the
/// default frozen mode (the user explicitly refreshes via the R key).
/// Errors are logged at debug — a transient capture miss just leaves
/// the previous frame in place for this redraw.
///
/// Pull the latest frame from the background capture worker into
/// `frozen_frame` if one is ready. Non-blocking — when the worker
/// hasn't produced a new frame since the last pull, the call is
/// effectively free and `frozen_frame` keeps its previous value
/// (which edge detection then uses against a slightly older snapshot,
/// invisible to the user during normal measuring).
///
/// In freeze-screen mode `capture_worker` is `None`; the call is a
/// no-op and `frozen_frame` stays pinned to the snapshot captured at
/// measurement-mode entry. The R-shortcut handler and
/// `toggle_measurement`'s entry path still use synchronous
/// `capture_screen_native` calls — those are explicit user-initiated
/// captures, not the hot path the worker exists to unblock.
fn refresh_frame_if_live(
    capture_worker: Option<&CaptureWorker>,
    frozen_frame: &mut Option<NativeFrame>,
) {
    if let Some(worker) = capture_worker {
        if let Some(frame) = worker.try_latest_frame() {
            *frozen_frame = Some(frame);
        }
    }
}

/// Cadence at which the live-mode capture worker calls
/// `CGWindowListCreateImage`. 100 ms keeps edge detection's input
/// fresh within ~one cursor-trail distance while leaving the
/// capture thread spending most of its time idle. The capture itself
/// is 30–60 ms on a 2× Retina display, so 100 ms is also the natural
/// floor — going lower would just back-pressure on the previous
/// capture's tail.
const LIVE_CAPTURE_INTERVAL: Duration = Duration::from_millis(100);

/// Pull the currently-configured guide color + measurement format
/// from settings and write them into a freshly-built `Hud`. Called
/// at every refresh_hud branch so the live HUD reflects prefs
/// changes the moment the daemon's IPC reload finishes.
fn populate_hud_appearance(hud: &mut Hud, alt_held: bool) {
    let s = current_settings();
    let g = s.appearance.guide_color;
    hud.guide_color = PlatColor::rgba(g.r, g.g, g.b, g.a);
    let ag = s.appearance.alternative_guide_color;
    hud.alternative_guide_color = PlatColor::rgba(ag.r, ag.g, ag.b, ag.a);
    let p = s.appearance.primary_color;
    hud.primary_fg = PlatColor::rgba(p.r, p.g, p.b, p.a);
    let a = s.appearance.alternative_color;
    hud.alternate_fg = PlatColor::rgba(a.r, a.g, a.b, a.a);
    let unit_suffix = if s.general.display_units {
        match s.appearance.units {
            Units::Px => "px".to_string(),
            Units::Pt => "pt".to_string(),
        }
    } else {
        String::new()
    };
    let (dimension_divisor, corner_indicator) = current_figma_correction(&s);
    hud.measurement_format = HudMeasurementFormat {
        unit_suffix,
        rounding: match s.appearance.rounding_mode {
            RoundingMode::Points => HudRounding::Points,
            RoundingMode::PointsRounded => HudRounding::PointsRounded,
            RoundingMode::ScreenPixels => HudRounding::ScreenPixels,
        },
        scale_factor: primary_scale_factor(),
        wh_indicators: s.general.display_wh_indicators,
        aspect_in_area: s.general.aspect_in_area_tool,
        aspect_mode: s.general.aspect_mode,
        dimension_divisor,
    };
    // Momentary cursor-hide: holding ALT suppresses Vernier's own
    // crosshair so the user can read the pixels under it when
    // measuring very small things. The system pointer hide is
    // handled separately (see `want_system_pointer` in the pointer
    // handler so the OS cursor goes away too).
    hud.show_cursor = s.general.show_cursor && !alt_held;
    hud.corner_indicator = corner_indicator;
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
         Name=Vernier\n\
         GenericName=Measurement Overlay\n\
         Comment=Cross-platform measurement overlay\n\
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
            "[Desktop Entry]\nType=Application\nName=Vernier\n\
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

/// True when launched inside a Hyprland session (env var set by
/// the compositor on each spawned client).
fn is_hyprland_session() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

/// Recursively search the user's Hyprland config tree
/// (`$XDG_CONFIG_HOME/hypr` with `~/.config/hypr` fallback) for a
/// `bind*` line that runs `vernier toggle`. Returns the path of
/// the first match. Used to warn the user that a static line is
/// shadowing the prefs-managed shortcut.
fn static_vernier_bind_in_hypr_config() -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .map(|d| d.join("hypr"))?;
    if !dir.is_dir() {
        return None;
    }
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ty = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ty.is_dir() {
                stack.push(path);
                continue;
            }
            if ty.is_file()
                && path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s == "conf")
                    .unwrap_or(false)
            {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    for line in text.lines() {
                        let trimmed = line.trim_start();
                        if !trimmed.starts_with("bind") || trimmed.starts_with('#') {
                            continue;
                        }
                        if trimmed.contains("vernier toggle")
                            || trimmed.contains("vernier")
                        {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Convert an `Accelerator` to the `(MODS, KEY)` pair Hyprland's
/// `bind`/`unbind` keywords expect. Modifier order is the
/// canonical Hyprland one (`SUPER CTRL ALT SHIFT`).
fn accel_to_hyprland(accel: &Accelerator) -> (String, String) {
    use vernier_platform::{Key, Modifiers};
    let mut mods: Vec<&str> = Vec::new();
    if accel.modifiers.contains(Modifiers::META) {
        mods.push("SUPER");
    }
    if accel.modifiers.contains(Modifiers::CTRL) {
        mods.push("CTRL");
    }
    if accel.modifiers.contains(Modifiers::ALT) {
        mods.push("ALT");
    }
    if accel.modifiers.contains(Modifiers::SHIFT) {
        mods.push("SHIFT");
    }
    let key = match accel.key {
        Key::Char(c) => c.to_ascii_uppercase().to_string(),
        Key::F(n) => format!("F{n}"),
        Key::Escape => "Escape".to_string(),
        Key::Enter => "Return".to_string(),
        Key::Space => "Space".to_string(),
        Key::Tab => "Tab".to_string(),
        Key::Backspace => "BackSpace".to_string(),
        Key::Delete => "Delete".to_string(),
        Key::Up => "Up".to_string(),
        Key::Down => "Down".to_string(),
        Key::Left => "Left".to_string(),
        Key::Right => "Right".to_string(),
    };
    (mods.join(" "), key)
}

/// Register the toggle accelerator as a runtime Hyprland bind
/// that runs `vernier toggle` (the IPC client command). Returns
/// `false` if the spawn or hyprctl call failed.
fn register_hyprland_toggle(accel: &Accelerator) -> bool {
    register_hyprland_toggle_for(accel, None)
}

/// Register the toggle bind, optionally targeting a specific Hyprland
/// instance via `hyprctl --instance <sig>`. The bind watcher uses
/// this to address a freshly-spawned Hyprland whose instance signature
/// differs from the one in our process's environment.
fn register_hyprland_toggle_for(accel: &Accelerator, instance: Option<&str>) -> bool {
    let exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "vernier".to_string());
    let (mods, key) = accel_to_hyprland(accel);
    // Unbind first so successive registers (initial + watcher
    // reconnect + configreloaded re-apply) collapse to exactly one
    // bind instead of stacking. `keyword unbind` is a no-op when
    // nothing's bound.
    let unbind_arg = format!("unbind = {mods}, {key}");
    {
        let mut prev = std::process::Command::new("hyprctl");
        if let Some(sig) = instance {
            prev.args(["-i", sig]);
        }
        prev.args(["keyword", &unbind_arg]);
        let _ = prev.output();
    }
    let arg = format!("bind = {mods}, {key}, exec, {exe} toggle");
    let mut cmd = std::process::Command::new("hyprctl");
    if let Some(sig) = instance {
        cmd.args(["-i", sig]);
    }
    cmd.args(["keyword", &arg]);
    match cmd.output() {
        Ok(out) => {
            if !out.status.success() {
                log::warn!(
                    "hyprctl bind exit={:?} stdout={} stderr={}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr),
                );
                return false;
            }
            log::info!(
                "hyprctl bind: {mods}, {key} → {exe} toggle ({})",
                accel.to_string_key()
            );
            true
        }
        Err(e) => {
            log::warn!("hyprctl bind spawn: {e:#}");
            false
        }
    }
}

/// Drop a previously-registered runtime bind so it doesn't pile up
/// across reloads. Best-effort — Hyprland tolerates duplicates and
/// unbind-on-not-bound is a no-op.
fn unregister_hyprland_toggle(accel: &Accelerator) -> bool {
    let (mods, key) = accel_to_hyprland(accel);
    let arg = format!("unbind = {mods}, {key}");
    match std::process::Command::new("hyprctl")
        .args(["keyword", &arg])
        .output()
    {
        Ok(_) => true,
        Err(e) => {
            log::warn!("hyprctl unbind spawn: {e:#}");
            false
        }
    }
}

/// Resolve the toggle accelerator from the cached settings. Returns
/// `None` when `shortcuts.toggle` is empty or unparseable, which is
/// the same as "no hotkey registered" at startup.
fn current_toggle_accel() -> Option<Accelerator> {
    let s = current_settings();
    if s.shortcuts.toggle.trim().is_empty() {
        return None;
    }
    Accelerator::parse(&s.shortcuts.toggle)
}

/// Locate the live Hyprland instance: prefer `HYPRLAND_INSTANCE_SIGNATURE`
/// from our process env when its `.socket2.sock` still exists; otherwise
/// fall back to the most recently-modified instance directory under
/// `$XDG_RUNTIME_DIR/hypr/`. The fallback handles the case where Hyprland
/// restarted while our daemon kept running, leaving a stale env var.
/// Returns `(signature, path/to/.socket2.sock)`.
fn current_hyprland_instance() -> Option<(String, PathBuf)> {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
    let hypr_dir = xdg.join("hypr");
    if let Some(sig_os) = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let sock = hypr_dir.join(&sig_os).join(".socket2.sock");
        if sock.exists() {
            return Some((sig_os.to_string_lossy().into_owned(), sock));
        }
    }
    let mut newest: Option<(std::time::SystemTime, String, PathBuf)> = None;
    for entry in std::fs::read_dir(&hypr_dir).ok()?.flatten() {
        let dir = entry.path();
        let sock = dir.join(".socket2.sock");
        if !sock.exists() {
            continue;
        }
        let mtime = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let sig = entry.file_name().to_string_lossy().into_owned();
        if newest.as_ref().map_or(true, |(t, _, _)| mtime > *t) {
            newest = Some((mtime, sig, sock));
        }
    }
    newest.map(|(_, sig, sock)| (sig, sock))
}

/// Background thread that follows Hyprland's event socket and
/// re-applies our `hyprctl bind` whenever the bind would otherwise
/// be lost: on every successful socket connect (covers fresh
/// daemon-launch and Hyprland restart), and on every `configreloaded`
/// event (covers `hyprctl reload`, which wipes runtime binds).
///
/// The thread loops forever. On socket EOF / read error / disconnect
/// it sleeps briefly and tries to discover the current Hyprland
/// instance again — so a Hyprland restart that changes the
/// `HYPRLAND_INSTANCE_SIGNATURE` is followed transparently as long as
/// our process can read `$XDG_RUNTIME_DIR/hypr/`.
fn spawn_hyprland_bind_watcher() {
    use std::io::{BufRead, BufReader};
    use std::time::Duration;
    std::thread::Builder::new()
        .name("vernier-hypr-bind-watcher".into())
        .spawn(|| {
            let mut last_sig = String::new();
            loop {
                let (sig, sock_path) = match current_hyprland_instance() {
                    Some(v) => v,
                    None => {
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                let stream = match std::os::unix::net::UnixStream::connect(&sock_path) {
                    Ok(s) => s,
                    Err(e) => {
                        log::debug!("hypr socket2 connect ({}): {e}", sock_path.display());
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                let new_instance = sig != last_sig;
                if new_instance {
                    log::info!("hypr socket2 connected (instance {sig})");
                    last_sig = sig.clone();
                }
                // Fresh connect = good moment to (re)apply the bind.
                // Either we just started and need it, or Hyprland came
                // back from a restart and lost everything.
                if let Some(accel) = current_toggle_accel() {
                    register_hyprland_toggle_for(&accel, Some(&sig));
                }
                let reader = BufReader::new(stream);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.starts_with("configreloaded>>") {
                        log::info!(
                            "hyprland configreloaded — re-applying vernier toggle bind"
                        );
                        if let Some(accel) = current_toggle_accel() {
                            register_hyprland_toggle_for(&accel, Some(&sig));
                        }
                    }
                }
                // Reader returned EOF / error — Hyprland likely
                // restarted or the socket was closed. Force a fresh
                // instance lookup on the next iteration.
                last_sig.clear();
                std::thread::sleep(Duration::from_millis(500));
            }
        })
        .ok();
}

#[derive(Debug, Clone, Default)]
struct ActiveWindow {
    class: String,
    title: String,
}

/// Last-known focused window from Hyprland's `activewindow>>` event
/// stream. Updated by `spawn_active_window_watcher`; read by the HUD
/// code path to decide whether Figma zoom-correction should fire.
fn active_window_lock() -> &'static std::sync::RwLock<ActiveWindow> {
    static SLOT: std::sync::OnceLock<std::sync::RwLock<ActiveWindow>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::RwLock::new(ActiveWindow::default()))
}

fn current_active_window() -> ActiveWindow {
    active_window_lock().read().map(|g| g.clone()).unwrap_or_default()
}

/// Subscribe to Hyprland's `socket2.sock` event stream and keep
/// `active_window_lock()` in sync with `activewindow>>` events. The
/// daemon uses this read-only — there's no main-thread blocking on
/// the cache, so a sluggish Hyprland event stream just delays the
/// Figma-detection decision by one frame.
fn spawn_active_window_watcher() {
    use std::io::{BufRead, BufReader};
    use std::time::Duration;
    std::thread::Builder::new()
        .name("vernier-active-window".into())
        .spawn(|| loop {
            let path = match current_hyprland_instance() {
                Some((_, p)) => p,
                None => {
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            let stream = match std::os::unix::net::UnixStream::connect(&path) {
                Ok(s) => s,
                Err(_) => {
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            // Prime the cache on (re)connect — without this, the
            // first poll after Hyprland restart still sees the old
            // window class.
            if let Some(initial) = read_active_window_via_hyprctl() {
                if let Ok(mut g) = active_window_lock().write() {
                    *g = initial;
                }
            }
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                // `activewindow>>CLASS,TITLE` — note that titles
                // can contain commas, so split on the first one
                // only and treat the rest as title.
                if let Some(rest) = line.strip_prefix("activewindow>>") {
                    let (class, title) = match rest.split_once(',') {
                        Some((c, t)) => (c.to_string(), t.to_string()),
                        None => (rest.to_string(), String::new()),
                    };
                    if let Ok(mut g) = active_window_lock().write() {
                        *g = ActiveWindow { class, title };
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(500));
        })
        .ok();
}

/// One-shot query of the focused window via `hyprctl activewindow`.
/// Used to prime the active-window cache on watcher startup so we
/// don't need a focus event before Figma-detection works.
fn read_active_window_via_hyprctl() -> Option<ActiveWindow> {
    let out = std::process::Command::new("hyprctl")
        .args(["activewindow"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut class = String::new();
    let mut title = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("class:") {
            class = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("title:") {
            title = v.trim().to_string();
        }
    }
    Some(ActiveWindow { class, title })
}

/// Resolve the current Figma-correction state from the active window
/// + the bridge's cached zoom + the user's settings. Returns the
/// divisor to apply (1.0 means no correction) and an indicator
/// string for the corner pill (`None` if no correction).
fn current_figma_correction(settings: &Settings) -> (f64, Option<String>) {
    if !settings.integrations.figma_zoom_correction {
        return (1.0, None);
    }
    let zoom = match vernier_platform::figma_bridge::current_figma_zoom() {
        Some(z) if z > 0.0 => z,
        _ => return (1.0, None),
    };
    let win = current_active_window();
    let class_match = settings
        .integrations
        .figma_browser_classes
        .iter()
        .any(|c| c.eq_ignore_ascii_case(&win.class));
    let title_match = win.title.contains(&settings.integrations.figma_title_suffix);
    if !(class_match && title_match) {
        return (1.0, None);
    }
    let pct = (zoom * 100.0).round() as i64;
    (zoom, Some(format!("F \u{00B7} {pct}%")))
}

#[derive(Debug, Clone, Default)]
struct ParsedShortcuts {
    clear_and_hide: Option<Accelerator>,
    restore: Option<Accelerator>,
    capture: Option<Accelerator>,
    /// Single-modifier "press-and-hold" binding for Crosshair
    /// (alignment) mode. Stored as `Modifiers` (single bit) rather
    /// than `Accelerator` because there's no key — the daemon just
    /// watches whether that modifier is currently held.
    crosshair: Option<vernier_platform::Modifiers>,
    guide_horizontal: Option<Accelerator>,
    guide_vertical: Option<Accelerator>,
    color_toggle: Option<Accelerator>,
    stuck_horizontal: Option<Accelerator>,
    stuck_vertical: Option<Accelerator>,
    refresh_capture: Option<Accelerator>,
    tolerance_up: Option<Accelerator>,
    tolerance_down: Option<Accelerator>,
    nudge_left: Option<Accelerator>,
    nudge_right: Option<Accelerator>,
    nudge_up: Option<Accelerator>,
    nudge_down: Option<Accelerator>,
    take_normal_screenshot: Option<Accelerator>,
}

fn parse_modifier_only(s: &str) -> Option<vernier_platform::Modifiers> {
    use vernier_platform::Modifiers;
    match s.trim().to_ascii_lowercase().as_str() {
        "shift" => Some(Modifiers::SHIFT),
        "ctrl" | "control" => Some(Modifiers::CTRL),
        "alt" | "opt" | "option" => Some(Modifiers::ALT),
        "super" | "meta" | "cmd" | "command" | "win" => Some(Modifiers::META),
        _ => None,
    }
}

fn modifier_held(m: vernier_platform::Modifiers, shift: bool, ctrl: bool, alt: bool, sup: bool) -> bool {
    use vernier_platform::Modifiers;
    (m == Modifiers::SHIFT && shift)
        || (m == Modifiers::CTRL && ctrl)
        || (m == Modifiers::ALT && alt)
        || (m == Modifiers::META && sup)
}

#[derive(Debug, Clone, Copy)]
enum NudgeDir {
    Left,
    Right,
    Up,
    Down,
}

/// Sticky nudge target — pinned when a nudge key is first pressed
/// while the cursor was over a held rect. `anchor` is the cursor
/// position at pin time; the selection releases when the mouse
/// moves further than `NUDGE_RELEASE_PX` from it.
#[derive(Debug, Clone, Copy)]
struct NudgeSelection {
    rect_idx: usize,
    anchor: (f64, f64),
}

/// Match `pressed` against any of the four nudge bindings, ignoring
/// SHIFT (which is reserved as the step-multiplier modifier — the
/// caller still reads `shift_held` separately).
fn matches_nudge(
    pressed: &Option<Accelerator>,
    accels: &ParsedShortcuts,
) -> Option<NudgeDir> {
    use vernier_platform::Modifiers;
    let Some(a) = pressed else { return None };
    let stripped = Accelerator {
        modifiers: Modifiers(a.modifiers.0 & !Modifiers::SHIFT.0),
        key: a.key,
    };
    let s = Some(stripped);
    if s == accels.nudge_left {
        Some(NudgeDir::Left)
    } else if s == accels.nudge_right {
        Some(NudgeDir::Right)
    } else if s == accels.nudge_up {
        Some(NudgeDir::Up)
    } else if s == accels.nudge_down {
        Some(NudgeDir::Down)
    } else {
        None
    }
}

/// Nudge a guide by `step` logical px in `dir`. Returns true if the
/// direction matches the guide's free axis (vertical guide → L/R,
/// horizontal guide → U/D); perpendicular nudges are no-ops. Used by
/// both the initial keypress and the repeat-timer NudgeTick handler.
fn apply_guide_nudge(
    guides: &mut [Guide],
    idx: usize,
    dir: NudgeDir,
    step: i32,
) -> bool {
    let Some(g) = guides.get_mut(idx) else { return false };
    match (dir, g.axis) {
        (NudgeDir::Left, GuideAxis::Vertical) => { g.position -= step; true }
        (NudgeDir::Right, GuideAxis::Vertical) => { g.position += step; true }
        (NudgeDir::Up, GuideAxis::Horizontal) => { g.position -= step; true }
        (NudgeDir::Down, GuideAxis::Horizontal) => { g.position += step; true }
        _ => false,
    }
}

/// One nudge increment: shift the held rect at `idx` 1 px in the
/// given direction (10 px when Shift is held) and repaint the HUD.
/// Used both for the initial press and for the follow-up
/// `NudgeTick` events the repeat timer sends.
#[allow(clippy::too_many_arguments)]
fn apply_nudge_step(
    dir: NudgeDir,
    idx: usize,
    shift_held: bool,
    alt_held: bool,
    align_mode: bool,
    color_alternate: bool,
    last_pointer_xy: Option<(f64, f64)>,
    held_rects: &mut Vec<HeldRect>,
    mode: &InteractionMode,
    overlay: &mut vernier_platform::OverlayHandle,
    frozen_frame: Option<&NativeFrame>,
    tol_value: u32,
    active_toast: &Option<HudToast>,
    toast_until: Option<Instant>,
    guides: &[Guide],
    pending_guide: Option<GuideAxis>,
    pre_clear_freeze: bool,
    stuck_pill_drag_committed: bool,
    stuck_measurements: &[StuckMeasurement],
    screen_w: i32,
    screen_h: i32,
    context_menu: Option<&ContextMenuState>,
    last_hud_redraw: &mut Instant,
) {
    if idx >= held_rects.len() {
        return;
    }
    let step = if shift_held { 10.0 } else { 1.0 };
    let (dx, dy) = match dir {
        NudgeDir::Left => (-step, 0.0),
        NudgeDir::Right => (step, 0.0),
        NudgeDir::Up => (0.0, -step),
        NudgeDir::Down => (0.0, step),
    };
    if let Some(rect) = held_rects.get_mut(idx) {
        rect.rect_start.0 += dx;
        rect.rect_start.1 += dy;
        rect.rect_end.0 += dx;
        rect.rect_end.1 += dy;
    }
    // Rate-limit buffer commits to ~60 Hz. Hyprland disconnects
    // clients that sustain faster commit rates without
    // `wl_surface.frame()` callback backpressure (broken pipe →
    // dead overlay). Position math still applies every tick; only
    // the redraw is throttled.
    if last_hud_redraw.elapsed() < HUD_REDRAW_INTERVAL {
        return;
    }
    if let Some((x, y)) = last_pointer_xy {
        *last_hud_redraw = Instant::now();
        let toast = current_toast(active_toast, toast_until);
        refresh_hud(
            mode,
            overlay,
            frozen_frame,
            x,
            y,
            tol_value,
            toast,
            guides,
            pending_guide,
            stuck_measurements,
            held_rects,
            color_alternate,
            align_mode,
            alt_held,
            pre_clear_freeze,
            stuck_pill_drag_committed,
            screen_w,
            screen_h,
            None,
            context_menu,
        );
    }
}

fn log_shortcut_accels(p: &ParsedShortcuts) {
    use vernier_platform::Modifiers;
    let fmt = |a: &Option<Accelerator>| {
        a.as_ref()
            .map(|x| x.to_string_key())
            .unwrap_or_else(|| "<unset>".into())
    };
    let fmt_mod = |m: &Option<Modifiers>| -> String {
        match m {
            Some(x) if *x == Modifiers::SHIFT => "SHIFT".into(),
            Some(x) if *x == Modifiers::CTRL => "CTRL".into(),
            Some(x) if *x == Modifiers::ALT => "ALT".into(),
            Some(x) if *x == Modifiers::META => "SUPER".into(),
            _ => "<unset>".into(),
        }
    };
    log::info!(
        "shortcuts reloaded — clear_and_hide={} restore={} capture={} crosshair={} \
         guide_h={} guide_v={} color_toggle={} stuck_h={} stuck_v={} \
         refresh={} tol_up={} tol_down={} nudge_l={} nudge_r={} nudge_u={} nudge_d={} \
         screenshot={}",
        fmt(&p.clear_and_hide),
        fmt(&p.restore),
        fmt(&p.capture),
        fmt_mod(&p.crosshair),
        fmt(&p.guide_horizontal),
        fmt(&p.guide_vertical),
        fmt(&p.color_toggle),
        fmt(&p.stuck_horizontal),
        fmt(&p.stuck_vertical),
        fmt(&p.refresh_capture),
        fmt(&p.tolerance_up),
        fmt(&p.tolerance_down),
        fmt(&p.nudge_left),
        fmt(&p.nudge_right),
        fmt(&p.nudge_up),
        fmt(&p.nudge_down),
        fmt(&p.take_normal_screenshot),
    );
}

fn parse_shortcut_accels(s: &Settings) -> ParsedShortcuts {
    ParsedShortcuts {
        clear_and_hide: Accelerator::parse(&s.shortcuts.clear_and_hide),
        restore: Accelerator::parse(&s.shortcuts.restore_session),
        capture: Accelerator::parse(&s.shortcuts.capture),
        crosshair: parse_modifier_only(&s.shortcuts.crosshair_mode),
        guide_horizontal: Accelerator::parse(&s.shortcuts.guide_horizontal),
        guide_vertical: Accelerator::parse(&s.shortcuts.guide_vertical),
        color_toggle: Accelerator::parse(&s.shortcuts.color_toggle),
        stuck_horizontal: Accelerator::parse(&s.shortcuts.stuck_horizontal),
        stuck_vertical: Accelerator::parse(&s.shortcuts.stuck_vertical),
        refresh_capture: Accelerator::parse(&s.shortcuts.refresh_capture),
        tolerance_up: Accelerator::parse(&s.shortcuts.tolerance_up),
        tolerance_down: Accelerator::parse(&s.shortcuts.tolerance_down),
        nudge_left: Accelerator::parse(&s.shortcuts.nudge_left),
        nudge_right: Accelerator::parse(&s.shortcuts.nudge_right),
        nudge_up: Accelerator::parse(&s.shortcuts.nudge_up),
        nudge_down: Accelerator::parse(&s.shortcuts.nudge_down),
        take_normal_screenshot: Accelerator::parse(&s.shortcuts.take_normal_screenshot),
    }
}

/// Translate the XKB keysym + currently-held modifier state into a
/// platform `Accelerator` so the daemon's keyboard handler can
/// match against `settings.shortcuts.*`. Returns `None` for keys
/// the user can't reasonably bind (lone modifiers, dead keys, …).
fn xkb_to_accelerator(
    keysym: u32,
    shift_held: bool,
    ctrl_held: bool,
    alt_held: bool,
    super_held: bool,
) -> Option<Accelerator> {
    use vernier_platform::{Key, Modifiers};
    let key = match keysym {
        0xff1b => Key::Escape,
        0xff0d | 0xff8d => Key::Enter,
        0xff09 => Key::Tab,
        0xff08 => Key::Backspace,
        0xffff => Key::Delete,
        0x0020 => Key::Space,
        0xff51 => Key::Left,
        0xff52 => Key::Up,
        0xff53 => Key::Right,
        0xff54 => Key::Down,
        // Function keys F1..F12 = 0xffbe..0xffc9
        0xffbe..=0xffc9 => Key::F((keysym - 0xffbe + 1) as u8),
        // Letters: lowercase a-z = 0x0061..0x007a, uppercase A-Z =
        // 0x0041..0x005a. Normalize to lowercase Char — the
        // SHIFT modifier is carried separately so "shift+f" and
        // "F" both round-trip to Key::Char('f') + shift.
        0x0061..=0x007a => Key::Char(char::from_u32(keysym)?),
        0x0041..=0x005a => Key::Char(char::from_u32(keysym + 0x20)?),
        // Digits 0-9 (no modifier).
        0x0030..=0x0039 => Key::Char(char::from_u32(keysym)?),
        // Punctuation that the prefs UI spells out (PLUS / MINUS
        // / EQUAL / UNDERSCORE). Keypad variants normalize to the
        // same Char so a single binding catches both.
        0x002b | 0xffab => Key::Char('+'),
        0x002d | 0xffad => Key::Char('-'),
        0x003d => Key::Char('='),
        0x005f => Key::Char('_'),
        _ => return None,
    };
    let mut modifiers = Modifiers::NONE;
    if shift_held {
        modifiers |= Modifiers::SHIFT;
    }
    if ctrl_held {
        modifiers |= Modifiers::CTRL;
    }
    if alt_held {
        modifiers |= Modifiers::ALT;
    }
    if super_held {
        modifiers |= Modifiers::META;
    }
    // Shifted-only punctuation (`+` and `_` on US layouts) already
    // implies SHIFT in the keysym itself — strip it so the
    // accelerator round-trips with the stored "PLUS" / "UNDERSCORE"
    // (which parse as Char with NO modifier).
    if matches!(key, Key::Char('+') | Key::Char('_')) {
        modifiers = Modifiers(modifiers.0 & !Modifiers::SHIFT.0);
    }
    Some(Accelerator { modifiers, key })
}

/// Spawn `vernier prefs` and return the `Child` handle so the
/// caller can track whether the window is still open. Used by the
/// toggle (tray click) and the open-if-closed (IPC `open-prefs`)
/// paths. Returns `None` if the spawn failed.
fn spawn_prefs_window() -> Option<std::process::Child> {
    let exe = std::env::current_exe().ok();
    let mut cmd = match exe {
        Some(p) => std::process::Command::new(p),
        None => std::process::Command::new("vernier"),
    };
    cmd.arg("prefs");
    match cmd.spawn() {
        Ok(child) => {
            log::info!("prefs window spawned (pid {})", child.id());
            Some(child)
        }
        Err(e) => {
            log::warn!("spawn prefs: {e:#}");
            None
        }
    }
}

/// Tray click semantics: open prefs if not running, close it if
/// running. Closing kills the subprocess (egui shuts down cleanly
/// on SIGKILL — there's no editable state besides the in-memory
/// settings copy, which the user already had a chance to save).
fn toggle_prefs_window(handle: &mut Option<std::process::Child>) {
    if let Some(child) = handle.as_mut() {
        // try_wait returns Ok(None) while the child is still alive.
        if matches!(child.try_wait(), Ok(None)) {
            if let Some(mut c) = handle.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            log::info!("prefs window closed via tray toggle");
            return;
        }
    }
    // Either no child or it has already exited — replace with a
    // fresh one.
    *handle = spawn_prefs_window();
}

/// Open prefs, or bring the existing window to the front when one is
/// already up. Keeps the `vernier` (no-args) IPC path from spawning a
/// duplicate, and ensures every "Preferences..." invocation (tray
/// menu, in-overlay menu, Cmd+, hotkey) lands the prefs window in
/// front of whatever the user was looking at instead of silently
/// no-op'ing when prefs was already open behind another app.
fn ensure_prefs_window(handle: &mut Option<std::process::Child>) {
    if let Some(child) = handle.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            log::info!("prefs window already open — focusing existing window");
            focus_prefs_window(handle.as_ref());
            return;
        }
    }
    *handle = spawn_prefs_window();
}

/// Bring the prefs window to the foreground when it's already open.
/// macOS only: routes through `NSRunningApplication
/// .runningApplicationWithProcessIdentifier(...)
/// .activateWithOptions(...)`, which raises every window owned by
/// the prefs process and gives them focus. On Linux/Windows this is
/// a no-op — eframe / winit handles focus on creation, and the
/// existing "already running, do nothing" branch is fine since
/// users typically expect the existing window to surface via the
/// window manager.
#[allow(unused_variables)]
fn focus_prefs_window(child: Option<&std::process::Child>) {
    #[cfg(target_os = "macos")]
    {
        let Some(pid) = child.map(|c| c.id() as i32) else { return };
        vernier_platform::focus_macos_app_by_pid(pid);
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

/// Spawn a detached thread that sleeps for the close-and-clear window
/// and then enqueues `MainEvent::ClearAndHideWindowExpired`. The
/// `esc_at` is the timestamp of the 1st ESC; the handler will compare
/// it against `last_esc_at` and ignore the event if a 2nd ESC has
/// already cleared the marker.
fn spawn_clear_and_hide_window_timer(
    tx: &std::sync::mpsc::Sender<MainEvent>,
    delay: Duration,
    esc_at: Instant,
) {
    let tx = tx.clone();
    std::thread::Builder::new()
        .name("vernier-clear-window".into())
        .spawn(move || {
            std::thread::sleep(delay);
            let _ = tx.send(MainEvent::ClearAndHideWindowExpired { esc_at });
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
    /// Internal: nudge auto-repeat tick. Fired by a worker thread
    /// while the user holds a nudge key — SCTK's software repeat
    /// wasn't reliably scheduling on Hyprland, so we drive our
    /// own timer with a generation counter for cancellation.
    NudgeTick { dir: NudgeDir, generation: u64 },
    /// Internal: the close-and-clear confirmation window has lapsed
    /// without a 2nd ESC, so the visually-frozen overlay should now
    /// commit to a true freeze (drop input capture, mode → Idle).
    /// `esc_at` is the timestamp of the 1st ESC press: the handler
    /// only acts on this event when `last_esc_at == Some(esc_at)`,
    /// which doubles as the cancel-token if a 2nd ESC arrived first.
    ClearAndHideWindowExpired { esc_at: Instant },
}

#[derive(Debug, Clone, Copy)]
enum ButtonOutcome {
    None,
    /// User clicked the camera pill on a held rect. The handler
    /// itself can't safely capture: in live (`freeze_screen = false`)
    /// mode the most recent PipeWire frame includes Vernier's own
    /// HUD (camera icon, custom crosshair), so cropping it directly
    /// bakes those decorations into the saved PNG. The caller does
    /// the clean-capture dance: blank the HUD, wait for a fresh
    /// PipeWire frame, capture, save, restore.
    ScreenshotPillClicked { rs: Px, re: Px },
}

/// Result of [`take_held_screenshot`]. Propagated through
/// [`ButtonOutcome::ScreenshotTaken`] so the main loop knows
/// whether to clear-and-hide (handoff: user has moved on to the
/// external annotation app) or stay in measure mode for follow-up
/// shots (local save: file already on disk, nothing else to do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureOutcome {
    /// The image was written to `output_dir` (or printed) and
    /// vernier retains control.
    SavedLocal,
    /// The image was written to a temp PNG and spawned in the
    /// configured handoff app. Vernier should get out of the
    /// way.
    HandedOff,
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
                "version" => {
                    // Answered directly here — no main-loop roundtrip
                    // needed since the build_id is a process-lifetime
                    // constant captured at daemon startup.
                    let _ = writer
                        .write_all(format!("{}\n", vernier_core::build_id()).as_bytes());
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
    OpenPrefs,
    ClearAll,
    CloseVernier,
}

struct MenuItemDef {
    label: &'static str,
    /// Shortcut hint as a list of segments (modifiers + key). Joined
    /// with a single space at render time so each modifier sits
    /// uniformly apart from the next one and from the trailing key —
    /// regardless of whether the SUPER token resolves to a single
    /// glyph (⌘ / Omarchy logo) or a multi-char word ("Super" /
    /// "Win"). Use the literal `"SUPER"` sentinel here; it gets
    /// substituted by `super_glyph_for_menu()` per-platform.
    shortcut: Option<&'static [&'static str]>,
    icon: HudContextMenuIcon,
    action: MenuAction,
    divider_after: bool,
}

const MENU_ITEMS: &[MenuItemDef] = &[
    MenuItemDef {
        label: "Add Horizontal Guide",
        shortcut: Some(&["\u{21E7}", "H"]),
        icon: HudContextMenuIcon::GuideH,
        action: MenuAction::AddHorizontalGuide,
        divider_after: false,
    },
    MenuItemDef {
        label: "Add Vertical Guide",
        shortcut: Some(&["\u{21E7}", "V"]),
        icon: HudContextMenuIcon::GuideV,
        action: MenuAction::AddVerticalGuide,
        divider_after: true,
    },
    MenuItemDef {
        label: "Hold Horizontal Distance",
        shortcut: Some(&["H"]),
        icon: HudContextMenuIcon::StuckH,
        action: MenuAction::HoldHorizontalDistance,
        divider_after: false,
    },
    MenuItemDef {
        label: "Hold Vertical Distance",
        shortcut: Some(&["V"]),
        icon: HudContextMenuIcon::StuckV,
        action: MenuAction::HoldVerticalDistance,
        divider_after: true,
    },
    MenuItemDef {
        label: "Take Normal Screenshot",
        shortcut: Some(&["\u{2303}", "S"]),
        icon: HudContextMenuIcon::Camera,
        action: MenuAction::OpenScreenshotTool,
        divider_after: false,
    },
    MenuItemDef {
        label: "Enter Background Mode",
        shortcut: Some(&["\u{2303}", "\u{21E7}", "SUPER", "F"]),
        icon: HudContextMenuIcon::Background,
        action: MenuAction::EnterBackgroundMode,
        divider_after: false,
    },
    MenuItemDef {
        label: "Restore Last Session",
        shortcut: Some(&["\u{21E7}", "R"]),
        icon: HudContextMenuIcon::Restore,
        action: MenuAction::RestoreLastSession,
        divider_after: true,
    },
    MenuItemDef {
        // Shortcut hint is intentionally None — the actual binding
        // is Cmd+, on macOS and Ctrl+, elsewhere; the MENU_ITEMS
        // const can't easily express that platform-conditional. The
        // user gets the label only; the keyboard shortcut still
        // works system-wide regardless.
        label: "Preferences…",
        shortcut: None,
        icon: HudContextMenuIcon::Settings,
        action: MenuAction::OpenPrefs,
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
        label: "Close Vernier",
        shortcut: None,
        icon: HudContextMenuIcon::Close,
        action: MenuAction::CloseVernier,
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
/// Capture the held rect using `grim` (wlr-screencopy). PipeWire's
/// frames include the OS cursor on Hyprland regardless of the
/// portal's `CursorMode::Hidden` setting; grim reads the surface
/// buffer directly, which excludes the cursor by default. Vernier's
/// own overlay is hidden by the caller (overlay.set_hud(None) + a
/// brief wait) before this fires, so grim sees a clean desktop too.
#[cfg(not(target_os = "macos"))]
fn take_held_screenshot_via_grim(rect_start: Px, rect_end: Px) -> Result<CaptureOutcome> {
    let s = current_settings();
    let prefs = s.screenshots.clone();
    let pad = prefs.padding_px as i32;
    let lo_x = rect_start.x.min(rect_end.x) - pad;
    let lo_y = rect_start.y.min(rect_end.y) - pad;
    let hi_x = rect_start.x.max(rect_end.x) + pad;
    let hi_y = rect_start.y.max(rect_end.y) + pad;
    let w = hi_x - lo_x;
    let h = hi_y - lo_y;
    if w <= 0 || h <= 0 {
        anyhow::bail!("empty screenshot region");
    }
    let region = format!("{},{} {}x{}", lo_x, lo_y, w, h);
    let tmp_path = std::env::temp_dir()
        .join(format!("vernier-grim-{}.png", current_timestamp()));
    let mut cmd = std::process::Command::new("grim");
    cmd.args(["-g", &region]);
    // grim -s downscales the output by the given factor. retina_downscale
    // wants the PNG at logical px rather than the raw HiDPI buffer.
    if prefs.retina_downscale {
        cmd.args(["-s", "1"]);
    }
    cmd.arg(&tmp_path);
    let status = cmd
        .status()
        .with_context(|| "grim spawn failed (is grim installed?)")?;
    if !status.success() {
        anyhow::bail!("grim exited with status {status}");
    }
    let img = image::open(&tmp_path)
        .with_context(|| format!("decode grim output {}", tmp_path.display()))?
        .to_rgba8();
    finish_held_screenshot(img, None, &prefs)
}

/// macOS counterpart to [`take_held_screenshot_via_grim`]: shells out
/// to `/usr/sbin/screencapture` (always installed) for the actual
/// pixel grab, then funnels the decoded PNG through the shared
/// `finish_held_screenshot` post-pipeline so handoff / save / sound
/// behavior matches Linux.
///
/// `screencapture -R x,y,w,h` captures a screen-space region in
/// display *points* (logical px) — the same coordinate space Vernier
/// already uses for `Px` — origin top-left of primary display.
/// `-x` suppresses the system shutter sound (Vernier plays its own
/// when `capture_sound` is on). `-t png` forces PNG output regardless
/// of filename heuristics. The captured frame omits the cursor
/// (screencapture's default for region mode), matching the no-cursor
/// behavior of the Wayland grim path.
///
/// Note: `screencapture` always writes the image at the source
/// display's native pixel resolution (i.e., HiDPI / "retina"); the
/// `retina_downscale` preference is currently honored only on Linux.
/// Add post-decode resizing here when wiring it up on macOS.
#[cfg(target_os = "macos")]
fn take_held_screenshot_via_screencapture(
    rect_start: Px,
    rect_end: Px,
) -> Result<CaptureOutcome> {
    let s = current_settings();
    let prefs = s.screenshots.clone();
    let pad = prefs.padding_px as i32;
    let lo_x = rect_start.x.min(rect_end.x) - pad;
    let lo_y = rect_start.y.min(rect_end.y) - pad;
    let hi_x = rect_start.x.max(rect_end.x) + pad;
    let hi_y = rect_start.y.max(rect_end.y) + pad;
    let w = hi_x - lo_x;
    let h = hi_y - lo_y;
    if w <= 0 || h <= 0 {
        anyhow::bail!("empty screenshot region");
    }
    let region = format!("{},{},{},{}", lo_x, lo_y, w, h);
    let tmp_path = std::env::temp_dir()
        .join(format!("vernier-screencapture-{}.png", current_timestamp()));
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .args(["-R", &region, "-x", "-t", "png"])
        .arg(&tmp_path)
        .status()
        .with_context(|| "screencapture spawn failed")?;
    if !status.success() {
        anyhow::bail!("screencapture exited with status {status}");
    }
    // `screencapture` writes the capture at the display's native
    // pixel density (2x on Retina) but tags the PNG with DPI=72,
    // which causes DPI-aware viewers (CleanShot X, Preview, Quick
    // Look, Safari) to render it at the doubled pixel grid — so the
    // image visually appears 2x larger than the region the user
    // measured.
    //
    // Always rewrite the PNG's `pHYs` chunk to advertise the
    // display's actual DPI (typically 144 on Retina). DPI-aware
    // viewers then render the 2x physical-px frame at half size:
    // same logical dimensions as the captured region, every pixel
    // of detail preserved. This is strictly better than downscaling
    // (which would lose detail through interpolation), so we apply
    // it unconditionally on macOS — the prefs `retina_downscale`
    // toggle still gates the Linux-side `grim -s 1` downscale where
    // the pHYs trick doesn't help (annotation tools like Satty
    // ignore PNG DPI metadata).
    //
    // Scale factor is computed from the ratio of captured pixels to
    // requested logical px so a 1x external display gets DPI=72
    // (no rewrite), a 2x Retina gets 144, a hypothetical 3x gets
    // 216 — never wrong even when displays mix.
    let captured_dims = image::image_dimensions(&tmp_path)
        .with_context(|| format!("read dims {}", tmp_path.display()))?;
    let scale_x = captured_dims.0 as f64 / w as f64;
    let dpi = (72.0 * scale_x).round() as i64;
    if dpi != 72 {
        let dpi_str = dpi.to_string();
        let dpi_status = std::process::Command::new("/usr/bin/sips")
            .args([
                "-s", "dpiWidth", &dpi_str,
                "-s", "dpiHeight", &dpi_str,
            ])
            .arg(&tmp_path)
            .arg("--out")
            .arg(&tmp_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .with_context(|| "sips spawn failed")?;
        if !dpi_status.success() {
            anyhow::bail!("sips exited with status {dpi_status}");
        }
    }
    let img = image::open(&tmp_path)
        .with_context(|| format!("decode screencapture output {}", tmp_path.display()))?
        .to_rgba8();
    // Pass the source PNG so the handoff / save-to-disk path copies
    // it byte-for-byte instead of going through `image::save`, which
    // would strip the DPI metadata we just wrote.
    finish_held_screenshot(img, Some(&tmp_path), &prefs)
}

/// Post-capture pipeline: shutter sound, handoff to external editor
/// (Satty etc.) OR save to disk + clipboard + notification. Shared
/// between the PipeWire-frame path (legacy, currently unused for
/// the camera-pill click but still wired for tests) and the
/// grim-based path.
///
/// `source_png`, when provided, is the path of a fully-encoded PNG
/// on disk that should be copied (not re-encoded) for any output
/// file. Used on macOS where `screencapture`'s native PNG carries a
/// `pHYs` DPI=144 chunk that we *don't* want `image::save` to strip
/// — DPI-aware viewers (CleanShot X, Preview) use that chunk to
/// render the 2x physical-px frame at logical (point) size, so the
/// captured pixels appear sharp at their measurement dimensions
/// instead of either lossy (downscaled) or doubled (raw).
fn finish_held_screenshot(
    img: image::RgbaImage,
    source_png: Option<&Path>,
    prefs: &vernier_core::ScreenshotSettings,
) -> Result<CaptureOutcome> {
    let final_w = img.width();
    let final_h = img.height();
    if prefs.capture_sound {
        play_shutter_sound();
    }
    if prefs.handoff_enabled && !prefs.handoff_command.is_empty() {
        let app_label = if !prefs.handoff_app_name.is_empty() {
            prefs.handoff_app_name.clone()
        } else {
            prefs.handoff_command.clone()
        };
        let args_template = if prefs.handoff_args.is_empty() {
            "{file}".to_string()
        } else {
            prefs.handoff_args.clone()
        };
        let cmd = prefs.handoff_command.clone();
        let temp_path = std::env::temp_dir()
            .join(format!("vernier-handoff-{}.png", current_timestamp()));
        if let Some(src) = source_png {
            std::fs::copy(src, &temp_path).with_context(|| {
                format!("copy {} → {}", src.display(), temp_path.display())
            })?;
        } else {
            img.save(&temp_path)
                .with_context(|| format!("write {}", temp_path.display()))?;
        }
        let path_str = temp_path.to_string_lossy().into_owned();
        let argv = vernier_core::render_args(&args_template, &path_str);
        let spawned = std::process::Command::new(&cmd).args(&argv).spawn();
        match spawned {
            Ok(_) => {
                log::info!(
                    "screenshot handed off to {}: {} ({}×{})",
                    app_label, path_str, final_w, final_h
                );
                return Ok(CaptureOutcome::HandedOff);
            }
            Err(e) => {
                log::warn!(
                    "handoff spawn failed (cmd={cmd:?}, file={path_str}): {e:#} — \
                     temp file kept for inspection"
                );
                return Ok(CaptureOutcome::SavedLocal);
            }
        }
    }
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
    if let Some(src) = source_png {
        std::fs::copy(src, &path)
            .with_context(|| format!("copy {} → {}", src.display(), path.display()))?;
    } else {
        img.save(&path)
            .with_context(|| format!("write {}", path.display()))?;
    }
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
    let path_str = path.to_string_lossy().into_owned();
    // Resolve the handoff app once, on the daemon thread, so the
    // notification thread closure owns simple Strings — no Settings
    // borrow held across the notify-send wait. Only fires when both
    // edit_action is on AND the user actually picked an app.
    let handoff_for_action = if prefs.handoff_edit_action
        && !prefs.handoff_command.is_empty()
    {
        let label = if !prefs.handoff_app_name.is_empty() {
            prefs.handoff_app_name.clone()
        } else {
            prefs.handoff_command.clone()
        };
        let args = if prefs.handoff_args.is_empty() {
            "{file}".to_string()
        } else {
            prefs.handoff_args.clone()
        };
        Some((prefs.handoff_command.clone(), args, label))
    } else {
        None
    };
    std::thread::spawn(move || {
        let edit_label;
        let mut args: Vec<&str> = vec![
            "-i",
            &path_str,
            "-t",
            "10000",
            "Screenshot saved",
        ];
        if let Some((_, _, ref name)) = handoff_for_action {
            edit_label = format!("Click to edit with {name}");
            args.insert(0, "default=Edit");
            args.insert(0, "-A");
            args.push(&edit_label);
        } else {
            args.push(&path_str);
        }
        let result = std::process::Command::new("notify-send").args(&args).output();
        let Some((cmd, args_template, _)) = handoff_for_action else {
            return;
        };
        if let Ok(out) = result {
            let action = String::from_utf8_lossy(&out.stdout);
            if action.trim() == "default" {
                let argv = vernier_core::render_args(&args_template, &path_str);
                let _ = std::process::Command::new(&cmd).args(&argv).spawn();
            }
        }
    });
    Ok(CaptureOutcome::SavedLocal)
}

/// Shared body for the "Take Normal Screenshot" action — fires from
/// both the right-click menu and the `CTRL+S` shortcut. Runs the
/// same teardown as Esc clear-and-hide, then spawns the user's
/// `external_screenshot_command` detached on a 250ms timer (so the
/// overlay-hide commit lands first) and a watchdog that SIGKILLs
/// `hyprpicker` once `slurp` closes.
fn do_take_normal_screenshot(
    mode: &mut InteractionMode,
    overlay: &mut vernier_platform::OverlayHandle,
    platform: &Arc<dyn Platform>,
    monitor: MonitorId,
    frozen_frame: &mut Option<NativeFrame>,
    capture_worker: &mut Option<CaptureWorker>,
    held_rects: &mut Vec<HeldRect>,
    guides: &mut Vec<Guide>,
    stuck_measurements: &mut Vec<StuckMeasurement>,
    nudge_selection: &mut Option<NudgeSelection>,
    last_selected_guide: &mut Option<usize>,
    pending_guide: &mut Option<GuideAxis>,
    pending_guide_shift_acked: &mut bool,
    pre_clear_freeze: &mut bool,
    active_toast: &mut Option<HudToast>,
    toast_until: &mut Option<Instant>,
    last_esc_at: &mut Option<Instant>,
    color_alternate: bool,
) {
    let cmd = current_settings()
        .screenshots
        .external_screenshot_command
        .clone();
    log::info!(
        "external screenshot: running clear-and-hide, then spawning {cmd:?}"
    );
    if let Err(e) = save_session(held_rects, guides, stuck_measurements) {
        log::warn!("save session: {e:#}");
    }
    *last_esc_at = None;
    *pre_clear_freeze = false;
    held_rects.clear();
    *nudge_selection = None;
    *last_selected_guide = None;
    guides.clear();
    stuck_measurements.clear();
    *pending_guide = None;
    *pending_guide_shift_acked = false;
    *active_toast = None;
    *toast_until = None;
    toggle_measurement(
        mode,
        overlay,
        platform,
        monitor,
        frozen_frame,
        capture_worker,
        held_rects,
        guides,
        stuck_measurements,
        color_alternate,
    );
    std::thread::sleep(std::time::Duration::from_millis(250));
    let _ = std::process::Command::new("setsid")
        .arg("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let watchdog = r#"
for _ in $(seq 1 100); do
  pgrep -x slurp >/dev/null 2>&1 && break
  sleep 0.05
done
while pgrep -x slurp >/dev/null 2>&1; do
  sleep 0.02
done
pkill -KILL -x hyprpicker 2>/dev/null
"#;
    let _ = std::process::Command::new("setsid")
        .arg("sh")
        .arg("-c")
        .arg(watchdog)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn build_hud_menu_items() -> Vec<HudContextMenuItem> {
    let sup = super_glyph_for_menu();
    MENU_ITEMS
        .iter()
        .map(|it| HudContextMenuItem {
            label: it.label.into(),
            shortcut: it.shortcut.map(|tokens| {
                tokens
                    .iter()
                    .map(|t| if *t == "SUPER" { sup } else { *t })
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
            icon: it.icon,
            divider_after: it.divider_after,
        })
        .collect()
}

/// Glyph (or short text) used for the SUPER / META / "command" key in
/// context-menu shortcut hints. Computed once per process:
/// - macOS: `\u{2318}` (⌘)
/// - Omarchy (Linux with `~/.local/share/fonts/omarchy.ttf`): `\u{e900}`,
///   the Omarchy logo glyph the prefs Shortcuts pane already uses for SUPER.
/// - Other Linux: `Super`
/// - Windows: `Win`
fn super_glyph_for_menu() -> &'static str {
    use std::sync::OnceLock;
    static GLYPH: OnceLock<&'static str> = OnceLock::new();
    GLYPH.get_or_init(|| {
        if cfg!(target_os = "macos") {
            "\u{2318}"
        } else if cfg!(target_os = "windows") {
            "Win"
        } else if cfg!(target_os = "linux") {
            if omarchy_font_present() {
                "\u{e900}"
            } else {
                "Super"
            }
        } else {
            "Super"
        }
    })
}

fn omarchy_font_present() -> bool {
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".local/share/fonts/omarchy.ttf"))
        .map(|p| p.exists())
        .unwrap_or(false)
}

fn toggle_measurement(
    mode: &mut InteractionMode,
    overlay: &mut vernier_platform::OverlayHandle,
    platform: &Arc<dyn Platform>,
    monitor: MonitorId,
    frozen_frame: &mut Option<NativeFrame>,
    capture_worker: &mut Option<CaptureWorker>,
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
                // Push the captured frame to the overlay as its
                // background so the user sees a literal snapshot —
                // anything moving underneath (browser scroll, video)
                // becomes invisible while measuring. Backends that
                // don't implement set_background_frame fall through to
                // the default no-op (transparent overlay, live content
                // visible), which is functionally fine since edge
                // detection still uses the frozen NativeFrame.
                if current_settings().general.freeze_screen {
                    if let Ok(packed) = platform.capture_screen(monitor) {
                        overlay.set_background_frame(Some(packed));
                    }
                }
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
        // Live mode → spawn the background capture worker so cursor
        // moves don't stall behind `CGWindowListCreateImage`. Freeze
        // mode → leave it `None`; the frozen frame captured above is
        // all edge detection needs until the user toggles off.
        if !current_settings().general.freeze_screen {
            *capture_worker = Some(CaptureWorker::start(
                Arc::clone(platform),
                monitor,
                LIVE_CAPTURE_INTERVAL,
            ));
        }
        *mode = InteractionMode::Hover { cursor: Px::default() };
        overlay.set_input_capturing(true);
        let mut hud = Hud::hover((-100.0, -100.0));
        hud.foreground = fg;
        // `toggle_measurement` runs at mode transitions only; Alt's
        // momentary cursor-hide kicks in on the next PointerMove redraw.
        populate_hud_appearance(&mut hud, false);
        hud.held_rects = held_rects.to_vec();
        hud.guides = guides.to_vec();
        hud.stuck_measurements = stuck_measurements.to_vec();
        overlay.set_hud(Some(hud));
        overlay.show();
        return;
    }
    // Stop the capture worker on every measure-mode OFF transition,
    // both passthrough-with-content and clean-exit. The thread joins
    // on the next iteration of its loop, which is at most
    // LIVE_CAPTURE_INTERVAL away.
    if let Some(w) = capture_worker.take() {
        w.stop();
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
        // Drop the snapshot so the desktop is visible again in
        // passthrough mode — the user explicitly wanted to interact
        // with their underlying apps but still see the persisted
        // measurement overlay.
        overlay.set_background_frame(None);
        let mut hud = Hud::hover((-1000.0, -1000.0));
        hud.kind = HudKind::None;
        hud.foreground = fg;
        populate_hud_appearance(&mut hud, false);
        hud.held_rects = held_rects.to_vec();
        hud.guides = guides.to_vec();
        hud.stuck_measurements = stuck_measurements.to_vec();
        overlay.set_hud(Some(hud));
    } else {
        // Going OFF clean: hide the overlay and detach all state.
        log::info!("measurement mode: OFF");
        *mode = InteractionMode::Idle;
        *frozen_frame = None;
        overlay.set_background_frame(None);
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
    alt_held: bool,
    pre_clear_freeze: bool,
    stuck_drag_committed: bool,
    screen_w: i32,
    screen_h: i32,
    resize_handle: Option<ResizeHandle>,
    context_menu: Option<&ContextMenuState>,
) {
    let fg = hud_foreground(color_alternate);

    // 1st ESC has visually frozen the overlay: content stays but the
    // live measurement crosshair / pills don't draw. Keyboard input is
    // still captured (so a 2nd ESC can land); the close-and-clear
    // window timer drops the input grab on expiry.
    if pre_clear_freeze {
        let mut hud = Hud::hover((-1000.0, -1000.0));
        hud.kind = HudKind::None;
        hud.foreground = fg;
        populate_hud_appearance(&mut hud, alt_held);
        hud.toast = toast.cloned();
        hud.guides = guides.to_vec();
        hud.stuck_measurements = stuck_measurements.to_vec();
        hud.held_rects = held_rects.to_vec();
        overlay.set_hud(Some(hud));
        return;
    }
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
    // detected pixel edge on the relevant axis — unless Alt is
    // held, which falls back to free placement at the cursor.
    let (pending_x, pending_y) = if let Some(axis) = pending_guide {
        if alt_held {
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
    let mut composed_guides =
        compose_guides(guides, pending_guide, pending_x, pending_y, color_alternate);
    if pending_guide.is_none() {
        let mut found = false;
        for g in composed_guides.iter_mut() {
            if !found && cursor_over_guide_line(cursor_px, g) {
                g.hovered = true;
                found = true;
            }
        }
    }
    // Same hover detection for stuck measurements. Suppressed while
    // a stuck-pill drag is committed: the pill is slaved to the
    // cursor, so the hit-box is trivially true; keeping hovered=false
    // here makes the renderer show the value text (not the × delete
    // glyph) for the duration of the drag.
    let mut composed_stuck: Vec<StuckMeasurement> = stuck_measurements.to_vec();
    if pending_guide.is_none() && !stuck_drag_committed {
        let stuck_bboxes = vernier_platform::placement::stuck_pill_bboxes(
            stuck_measurements,
            held_rects,
            &current_measurement_format(),
            screen_w as f64,
            screen_h as f64,
        );
        let mut found = false;
        for (i, s) in composed_stuck.iter_mut().enumerate() {
            if !found {
                if let Some(b) = stuck_bboxes.get(i) {
                    if cursor_over_stuck_pill_at(cursor_px, *b) {
                        s.hovered = true;
                        found = true;
                    }
                }
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
            color_alternate: r.color_alternate,
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
    let stuck_bboxes_here = vernier_platform::placement::stuck_pill_bboxes(
        stuck_measurements,
        held_rects,
        &current_measurement_format(),
        screen_w as f64,
        screen_h as f64,
    );
    let any_stuck_hover = stuck_bboxes_here
        .iter()
        .any(|b| cursor_over_stuck_pill_at(cursor_px, *b));
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
        populate_hud_appearance(&mut hud, alt_held);
        hud.toast = toast.cloned();
        hud.guides = composed_guides;
        hud.stuck_measurements = composed_stuck;
        hud.held_rects = composed_rects;
        hud.cursor_in_rect = cursor_in_rect;
        // Resize cursor matching the axis the new guide will move
        // along. Suppressed when ALT is held so the user can read
        // pixels under the cursor (matches the cursor-hide in Hover
        // / Held modes).
        if !alt_held {
            hud.move_cursor_at = Some((pending_x, pending_y));
            hud.cursor_kind = match axis {
                GuideAxis::Horizontal => CursorKind::ResizeNS,
                GuideAxis::Vertical => CursorKind::ResizeEW,
            };
        }
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
                populate_hud_appearance(&mut hud, alt_held);
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
            populate_hud_appearance(&mut hud, alt_held);
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
            populate_hud_appearance(&mut hud, alt_held);
            if has_drag_distance(start.pixel, cursor_px) {
                let start_pos = (start.pixel.x as f64, start.pixel.y as f64);
                // Snap the moving end of the rect to nearby guides on
                // each axis. Alt disables snap for free placement.
                let (cx, cy) = if alt_held {
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
/// Flip a guide axis. Horizontal ↔ Vertical. Used by the SHIFT-flip
/// behavior in pending guide mode.
fn flip_axis(axis: GuideAxis) -> GuideAxis {
    match axis {
        GuideAxis::Horizontal => GuideAxis::Vertical,
        GuideAxis::Vertical => GuideAxis::Horizontal,
    }
}

fn compose_guides(
    committed: &[Guide],
    pending: Option<GuideAxis>,
    x: f64,
    y: f64,
    pending_color_alternate: bool,
) -> Vec<Guide> {
    let mut out: Vec<Guide> = committed.to_vec();
    if let Some(axis) = pending {
        let position = match axis {
            GuideAxis::Horizontal => y as i32,
            GuideAxis::Vertical => x as i32,
        };
        out.push(Guide {
            axis,
            position,
            color_alternate: pending_color_alternate,
            hovered: false,
        });
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

/// Default snap threshold for mid-drag and end-of-drag guide snap.
/// Tight enough that the moving corner doesn't feel "stuck" when the
/// user is dragging past a guide intentionally.
const SNAP_PX_DEFAULT: f64 = 8.0;

/// Wider snap threshold used at the *start* of a drag — when the user
/// is committing to a corner position with no visual mid-drag
/// feedback yet, a generous magnet helps them land on the guide
/// intersection without precise aim. 30 px is the working value
/// (matches Figma/Sketch's "snap zone" for object creation).
const SNAP_PX_START_DRAG: f64 = 30.0;

/// Snap an x coordinate to the nearest vertical guide within
/// `threshold_px` logical px. No-op when `general.snap_to_guides`
/// is off. Used while drawing or resizing held rects so edges align
/// cleanly with reference guides.
fn snap_x_to_guides_within(x: f64, guides: &[Guide], threshold_px: f64) -> f64 {
    if !current_settings().general.snap_to_guides {
        return x;
    }
    let mut best = x;
    let mut best_d = threshold_px;
    for g in guides.iter().filter(|g| g.axis == GuideAxis::Vertical) {
        let d = (x - g.position as f64).abs();
        if d < best_d {
            best_d = d;
            best = g.position as f64;
        }
    }
    best
}

/// Mirror of [`snap_x_to_guides_within`] for horizontal guides.
fn snap_y_to_guides_within(y: f64, guides: &[Guide], threshold_px: f64) -> f64 {
    if !current_settings().general.snap_to_guides {
        return y;
    }
    let mut best = y;
    let mut best_d = threshold_px;
    for g in guides.iter().filter(|g| g.axis == GuideAxis::Horizontal) {
        let d = (y - g.position as f64).abs();
        if d < best_d {
            best_d = d;
            best = g.position as f64;
        }
    }
    best
}

/// Convenience: default-threshold (`SNAP_PX_DEFAULT`) snap. Used by
/// the mid-drag / end-of-drag / resize paths.
fn snap_x_to_guides(x: f64, guides: &[Guide]) -> f64 {
    snap_x_to_guides_within(x, guides, SNAP_PX_DEFAULT)
}

fn snap_y_to_guides(y: f64, guides: &[Guide]) -> f64 {
    snap_y_to_guides_within(y, guides, SNAP_PX_DEFAULT)
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
    color_alternate: bool,
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
                pill_offset: (0.0, 0.0),
                color_alternate,
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
                pill_offset: (0.0, 0.0),
                color_alternate,
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
fn cursor_over_stuck_pill_at(cursor: Px, bbox: vernier_platform::placement::PillRect) -> bool {
    let cx = cursor.x as f64;
    let cy = cursor.y as f64;
    cx >= bbox.x && cx <= bbox.x + bbox.w && cy >= bbox.y && cy <= bbox.y + bbox.h
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
/// Whether the compositor should draw its theme pointer over the
/// overlay. Returns `true` when the cursor sits on an interactive
/// affordance (held rect, guide badge, stuck pill, menu) and `false`
/// when Vernier draws its own custom cursor instead. Holding ALT
/// also returns `false` so the OS pointer hides momentarily for
/// precise reads (paired with `populate_hud_appearance` suppressing
/// Vernier's own crosshair).
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
    alt_held: bool,
    stuck_drag_committed: bool,
    screen_w: i32,
    screen_h: i32,
) -> bool {
    // Holding ALT hides everything cursor-related so the user can
    // read the pixels under their cursor. The menu still gets the
    // pointer (the row hover otherwise becomes invisible).
    if alt_held && !menu_open {
        return false;
    }
    // The context menu always wants the system arrow, even when it
    // overlaps clickable elements underneath.
    if menu_open {
        return true;
    }
    // Mid-drag on a stuck pill: the pill is the visual feedback, so
    // a cursor on top of it would just be clutter.
    if stuck_drag_committed {
        return false;
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
    let on_stuck = vernier_platform::placement::stuck_pill_bboxes(
        stuck_measurements,
        held_rects,
        &current_measurement_format(),
        screen_w as f64,
        screen_h as f64,
    )
    .iter()
    .any(|b| cursor_over_stuck_pill_at(cursor_px, *b));
    on_held || on_stuck || on_guide_x
}

/// Apply a live resize: re-anchor the rect's appropriate edges to
/// `cursor` based on which handle is being dragged.
fn apply_resize(
    rect: &mut HeldRect,
    op: &ResizeOp,
    cursor: (f64, f64),
    guides: &[Guide],
    alt_held: bool,
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
    // both axes, side handles only move one. Alt disables snap.
    if !alt_held {
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
    nudge_selection: &mut Option<NudgeSelection>,
    color_alternate: bool,
    alt_held: bool,
) -> ButtonOutcome {
    let fg = hud_foreground(color_alternate);
    let cursor_px = Px::new(x as i32, y as i32);
    if pressed {
        // (Stuck-measurement pill click→remove and drag→reposition
        // are handled at the main loop level — same pattern as guide
        // dragging — because they need a press/release state machine
        // that this single-call helper can't model. Guide removal and
        // drag-to-move are handled there too.)
        // Pressing on any held rect's W×H pill takes a screenshot of
        // that region. Otherwise the press starts a new measurement
        // drag — held rects accumulate, the new draw doesn't replace
        // them.
        // Pill click on a held rect → take a screenshot of that rect.
        for rect in held_rects.iter() {
            let rs = Px::new(rect.rect_start.0 as i32, rect.rect_start.1 as i32);
            let re = Px::new(rect.rect_end.0 as i32, rect.rect_end.1 as i32);
            if cursor_over_pill(cursor_px, rs, re) {
                return ButtonOutcome::ScreenshotPillClicked { rs, re };
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
            // Drop any sticky nudge selection — the index it
            // referenced is about to disappear (or shift).
            *nudge_selection = None;
            held_rects.remove(idx);
            return ButtonOutcome::None;
        }
        if matches!(mode, InteractionMode::Hover { .. }) {
            // Snap the start corner to a nearby guide on press, mirroring
            // the end-snap on release. Without this, only the trailing
            // corner aligns mid-drag and the user has to reach back to
            // pull the leading corner onto its guide via a resize handle.
            //
            // Uses a wider threshold (SNAP_PX_START_DRAG = 30 px) than
            // mid-drag/end-of-drag: at press time the user has no
            // visual feedback yet — they're committing to a corner with
            // a single click — so a generous magnet makes "draw a box
            // around these guides" forgiving without feeling sticky
            // during the drag.
            //
            // Alt disables snap (same modifier as the release-snap).
            let snapped_start = if alt_held {
                cursor_px
            } else {
                Px::new(
                    snap_x_to_guides_within(x, guides, SNAP_PX_START_DRAG).round() as i32,
                    snap_y_to_guides_within(y, guides, SNAP_PX_START_DRAG).round() as i32,
                )
            };
            let snap = SnapPoint::loose(snapped_start);
            log::info!(
                "drag started at ({},{}) (raw cursor ({},{}))",
                snapped_start.x, snapped_start.y, cursor_px.x, cursor_px.y
            );
            *mode = InteractionMode::Drawing { start: snap, cursor: cursor_px };
            // Don't paint the rect yet — wait for the user to actually
            // move past `DRAG_THRESHOLD_PX`. A bare click should look
            // like a hover, not a 1×1 box.
            let edges = edges_for_hud(frozen_frame, x, y, tolerance, guides);
            let mut hud = Hud::hover((x, y));
            hud.foreground = fg;
            populate_hud_appearance(&mut hud, alt_held);
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
            populate_hud_appearance(&mut hud, alt_held);
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
        // saw it snap to mid-drag. Alt disables snap.
        let raw_end = if alt_held {
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
            classify_aspect(
                measurement.width(),
                measurement.height(),
                current_settings().general.aspect_mode,
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
        held_rects.push(HeldRect {
            rect_start: snapped_start,
            rect_end: snapped_end,
            camera_armed: false,
            color_alternate,
        });
        *mode = InteractionMode::Hover { cursor: cursor_px };
        let mut hud = Hud::hover((x, y));
        hud.foreground = fg;
        populate_hud_appearance(&mut hud, alt_held);
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


/// Pipe `text` into `wl-copy`. Used by the Enter-to-copy-dimensions
/// path; the screenshot capture has its own image-mode call.
/// Best-effort shutter-sound playback. Spawns a detached process so
/// the daemon's main loop doesn't block while the audio plays.
/// Tries canberra-gtk-play (with the standard `screen-capture`
/// theme name) first, falls back to paplay against the freedesktop
/// sound file. Silent if neither is installed.
fn play_shutter_sound() {
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
            "rect {} {} {} {} {}\n",
            r.rect_start.0,
            r.rect_start.1,
            r.rect_end.0,
            r.rect_end.1,
            r.color_alternate as u8,
        ));
    }
    for g in guides {
        let axis = match g.axis {
            GuideAxis::Horizontal => "h",
            GuideAxis::Vertical => "v",
        };
        s.push_str(&format!(
            "guide {axis} {} {}\n",
            g.position, g.color_alternate as u8
        ));
    }
    for m in stuck_measurements {
        let axis = match m.axis {
            GuideAxis::Horizontal => "h",
            GuideAxis::Vertical => "v",
        };
        s.push_str(&format!(
            "stuck {axis} {} {} {} {} {} {}\n",
            m.at,
            m.start,
            m.end,
            m.pill_offset.0,
            m.pill_offset.1,
            m.color_alternate as u8,
        ));
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
                        color_alternate: false,
                    });
                }
            }
            // v2 rect line: trailing color-alt flag (0 / 1). Pre-v0.1.5
            // saves drop through to the 5-token arm above.
            ["rect", a, b, c, d, alt] => {
                if let (Ok(ax), Ok(ay), Ok(bx), Ok(by), Ok(alt)) = (
                    a.parse::<f64>(),
                    b.parse::<f64>(),
                    c.parse::<f64>(),
                    d.parse::<f64>(),
                    alt.parse::<u8>(),
                ) {
                    rects.push(HeldRect {
                        rect_start: (ax, ay),
                        rect_end: (bx, by),
                        camera_armed: false,
                        color_alternate: alt != 0,
                    });
                }
            }
            ["guide", "h", pos] => {
                if let Ok(p) = pos.parse() {
                    guides.push(Guide {
                        axis: GuideAxis::Horizontal,
                        position: p,
                        color_alternate: false,
                        hovered: false,
                    });
                }
            }
            ["guide", "v", pos] => {
                if let Ok(p) = pos.parse() {
                    guides.push(Guide {
                        axis: GuideAxis::Vertical,
                        position: p,
                        color_alternate: false,
                        hovered: false,
                    });
                }
            }
            // v2 guide line: trailing color-alt flag.
            ["guide", ax_s, pos, alt] => {
                let ax = match *ax_s {
                    "h" => GuideAxis::Horizontal,
                    "v" => GuideAxis::Vertical,
                    _ => continue,
                };
                if let (Ok(p), Ok(alt)) = (pos.parse::<i32>(), alt.parse::<u8>()) {
                    guides.push(Guide {
                        axis: ax,
                        position: p,
                        color_alternate: alt != 0,
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
                        pill_offset: (0.0, 0.0),
                        color_alternate: false,
                        hovered: false,
                    });
                }
            }
            // v2 stuck-line format: extra pill_offset (ox, oy) at end.
            // Pre-v0.1.5 sessions don't have these — they fall through
            // to the 5-token arm above with a default (0, 0) offset.
            ["stuck", axis, at, start, end, ox, oy] => {
                let ax = match *axis {
                    "h" => GuideAxis::Horizontal,
                    "v" => GuideAxis::Vertical,
                    _ => continue,
                };
                if let (Ok(at), Ok(start), Ok(end), Ok(ox), Ok(oy)) =
                    (at.parse(), start.parse(), end.parse(), ox.parse(), oy.parse())
                {
                    stuck.push(StuckMeasurement {
                        axis: ax,
                        at,
                        start,
                        end,
                        pill_offset: (ox, oy),
                        color_alternate: false,
                        hovered: false,
                    });
                }
            }
            // v3 stuck-line format: pill_offset + color-alt flag.
            ["stuck", axis, at, start, end, ox, oy, alt] => {
                let ax = match *axis {
                    "h" => GuideAxis::Horizontal,
                    "v" => GuideAxis::Vertical,
                    _ => continue,
                };
                if let (Ok(at), Ok(start), Ok(end), Ok(ox), Ok(oy), Ok(alt)) = (
                    at.parse::<f64>(),
                    start.parse::<f64>(),
                    end.parse::<f64>(),
                    ox.parse::<f64>(),
                    oy.parse::<f64>(),
                    alt.parse::<u8>(),
                ) {
                    stuck.push(StuckMeasurement {
                        axis: ax,
                        at,
                        start,
                        end,
                        pill_offset: (ox, oy),
                        color_alternate: alt != 0,
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

/// Lockfile path used to enforce single-instance for the prefs
/// window. A `UnixListener` bound here proves ownership; the OS
/// releases the bind when the prefs process exits.
fn prefs_lock_path() -> Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    Ok(runtime_dir.join("vernier.prefs.lock"))
}

/// Try to claim the prefs singleton lock. Returns `Some(listener)`
/// when this process is the sole prefs window — keep it alive for
/// the lifetime of the prefs UI. Returns `None` when another
/// prefs is already running (a connect to the lock socket
/// succeeds), in which case the caller should focus the existing
/// window and exit.
fn acquire_prefs_singleton_lock(path: &Path) -> Option<std::os::unix::net::UnixListener> {
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return None;
    }
    let _ = std::fs::remove_file(path);
    std::os::unix::net::UnixListener::bind(path).ok()
}

/// Race-free daemon singleton via `flock(LOCK_EX|LOCK_NB)`. The lock
/// is released by the kernel when the returned `File`'s last fd is
/// closed — so it survives panics and clean exits, and is reclaimable
/// even after a SIGKILL. Holding this before any portal work prevents
/// two racing daemons from each prompting xdph for screencast consent.
fn acquire_daemon_singleton_lock() -> Result<Option<std::fs::File>> {
    use std::os::fd::AsRawFd;
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let lock_path = runtime_dir.join("vernier.daemon.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("open daemon lock at {}", lock_path.display()))?;
    let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        return Ok(Some(f));
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(None),
        _ => Err(anyhow::anyhow!("flock {}: {err}", lock_path.display())),
    }
}

/// Block SIGTERM/SIGINT for the calling thread so freshly-spawned
/// threads inherit the block. A dedicated `vernier-signal` thread
/// (see `spawn_signal_quit_thread`) then accepts these signals via
/// `sigwait` and converts them into `IpcCmd::Quit`. Without this,
/// SIGTERM would default-kill the daemon mid-screencast — xdg-desktop-
/// portal never gets to flush the restore token to its GVariant DB,
/// and the next launch re-prompts the user.
fn block_quit_signals() -> Result<()> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err(anyhow::anyhow!(
                "pthread_sigmask: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// Spawn the signal-handler thread that turns SIGTERM/SIGINT into a
/// graceful `IpcCmd::Quit` on the main event channel. Must be called
/// after `block_quit_signals` so this thread is the only one that
/// receives the signals.
fn spawn_signal_quit_thread(combined_tx: std::sync::mpsc::Sender<MainEvent>) -> Result<()> {
    std::thread::Builder::new()
        .name("vernier-signal".into())
        .spawn(move || {
            let mut sig: libc::c_int = 0;
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::sigaddset(&mut set, libc::SIGINT);
                let _ = libc::sigwait(&set, &mut sig);
            }
            log::info!("received signal {sig}; shutting down cleanly");
            let _ = combined_tx.send(MainEvent::Ipc(IpcCmd::Quit));
        })
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("spawn signal thread: {e}"))
}
