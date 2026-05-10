//! Localhost WebSocket bridge for the macOS Figma plugin.
//!
//! The Figma plugin (in `figma-plugin/`) runs inside the user's
//! Figma tab and pushes the current viewport zoom over a WebSocket
//! to `127.0.0.1:<port>`. The daemon caches the latest value behind
//! a `RwLock` so the HUD can read it on every redraw without any
//! locks crossing the WebSocket I/O thread.
//!
//! The bridge is intentionally synchronous (one thread per accepted
//! connection) — there's at most one Figma tab active at a time and
//! traffic is a few JSON messages per second.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use tungstenite::{accept, Message};

/// Cached zoom value plus the wall-clock instant it landed. Reading
/// code rejects stale entries so a disconnected plugin doesn't keep
/// silently scaling measurements.
static FIGMA_ZOOM: RwLock<Option<(f64, Instant)>> = RwLock::new(None);

/// Maximum age for a cached zoom value. The plugin polls at ~100 ms,
/// so anything older than 2 s means the connection has gone away.
const FRESHNESS: Duration = Duration::from_secs(2);

/// Read the current Figma zoom factor (e.g. 2.0 = 200% zoom). Returns
/// `None` when no plugin is connected, when the cached value is stale,
/// or when the lock is poisoned.
pub fn current_figma_zoom() -> Option<f64> {
    let guard = FIGMA_ZOOM.read().ok()?;
    let (zoom, when) = guard.as_ref()?;
    if when.elapsed() <= FRESHNESS {
        Some(*zoom)
    } else {
        None
    }
}

/// Spawn the bridge on a background thread. Idempotent failure: if
/// the port is already bound (e.g. another vernier daemon is using
/// it) we log and exit quietly so the daemon's main flow isn't
/// disrupted.
pub fn spawn(port: u16) {
    std::thread::Builder::new()
        .name("vernier-figma-bridge".into())
        .spawn(move || run(port))
        .ok();
}

fn run(port: u16) {
    let bind = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("figma bridge: bind {bind}: {e}");
            return;
        }
    };
    log::info!("figma bridge: listening on {bind}");
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                std::thread::Builder::new()
                    .name("vernier-figma-conn".into())
                    .spawn(move || handle(s))
                    .ok();
            }
            Err(e) => log::warn!("figma bridge: accept: {e}"),
        }
    }
}

fn handle(stream: TcpStream) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    let mut ws = match accept(stream) {
        Ok(w) => w,
        Err(e) => {
            log::debug!("figma bridge: handshake from {peer}: {e}");
            return;
        }
    };
    log::info!("figma bridge: plugin connected ({peer})");
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => parse_and_cache(&t),
            Ok(Message::Close(_)) | Err(_) => break,
            // Pings, pongs, binary frames — ignore.
            Ok(_) => {}
        }
    }
    // Drop the cached zoom on disconnect so a stale value doesn't
    // outlast the freshness window if the plugin crashes mid-poll.
    if let Ok(mut g) = FIGMA_ZOOM.write() {
        *g = None;
    }
    log::info!("figma bridge: plugin disconnected ({peer})");
}

/// Resolve the absolute path of `figma-plugin/manifest.json` for the
/// "Install plugin in Figma" button. Returns the first path that
/// exists from these candidates, in order:
///
/// 1. `$VERNIER_FIGMA_PLUGIN_DIR/manifest.json` — escape hatch.
/// 2. `<exe-dir>/figma-plugin/manifest.json` — portable install
///    (the plugin sits next to the binary, e.g. tarball / AppImage).
/// 3. `<exe-dir>/../share/vernier/figma-plugin/manifest.json` —
///    FHS install (`/usr/bin/vernier` → `/usr/share/vernier/...`).
/// 4. `<workspace>/figma-plugin/manifest.json` resolved at compile
///    time via `CARGO_MANIFEST_DIR` — convenience for `cargo run`.
pub fn manifest_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VERNIER_FIGMA_PLUGIN_DIR") {
        let p = PathBuf::from(dir).join("manifest.json");
        if p.exists() {
            return canonicalize(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let portable = parent.join("figma-plugin").join("manifest.json");
            if portable.exists() {
                return canonicalize(portable);
            }
            let fhs = parent.join("../share/vernier/figma-plugin/manifest.json");
            if fhs.exists() {
                return canonicalize(fhs);
            }
        }
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../figma-plugin/manifest.json");
    if dev.exists() {
        return canonicalize(dev);
    }
    None
}

fn canonicalize(p: PathBuf) -> Option<PathBuf> {
    std::fs::canonicalize(&p).ok().or(Some(p))
}

fn parse_and_cache(text: &str) {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            log::debug!("figma bridge: bad json: {e}: {text}");
            return;
        }
    };
    let zoom = match v.get("value").and_then(|v| v.as_f64()) {
        Some(z) if z > 0.0 && z.is_finite() => z,
        _ => return,
    };
    if let Ok(mut g) = FIGMA_ZOOM.write() {
        *g = Some((zoom, Instant::now()));
    }
}
