use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use vernier_core::{detect_edges, FrameView, Px, Tolerance};
use vernier_platform::{Accelerator, Frame, PlatformEvent, TrayMenu};
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;

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
        "running. Hotkey, tray Preferences, or `vernier toggle` all toggle the overlay; tray Quit or `vernier quit` exits."
    );

    while let Ok(event) = combined_rx.recv() {
        match event {
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) if id == "quit" => {
                log::info!("quit requested via tray");
                break;
            }
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) if id == "toggle_overlay" => {
                overlay.toggle();
                log::info!(
                    "tray: overlay now {}",
                    if overlay.is_visible() { "visible" } else { "hidden" }
                );
            }
            MainEvent::Platform(PlatformEvent::TrayMenuActivated { id }) => {
                log::info!("unhandled tray menu id: {id}");
            }
            MainEvent::Platform(PlatformEvent::HotkeyPressed(_)) => {
                log::info!("global hotkey pressed");
                overlay.toggle();
                log::info!(
                    "overlay now {}",
                    if overlay.is_visible() { "visible" } else { "hidden" }
                );
            }
            MainEvent::Platform(PlatformEvent::TrayIconLeftClicked) => {
                log::info!("tray icon left-clicked");
            }
            MainEvent::Platform(other) => log::debug!("platform event: {other:?}"),
            MainEvent::Ipc(IpcCmd::Toggle) => {
                log::info!("ipc: toggle");
                overlay.toggle();
                log::info!(
                    "overlay now {}",
                    if overlay.is_visible() { "visible" } else { "hidden" }
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
