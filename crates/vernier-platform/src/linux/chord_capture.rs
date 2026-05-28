//! Chord recording from `/dev/input` for the prefs UI on Linux.
//!
//! egui-winit drops the Super modifier when it translates winit
//! state to `egui::Modifiers` on Linux, so the prefs window — which
//! uses egui — cannot honestly record SUPER-containing chords on
//! its own. This module sidesteps the egui-winit pipeline entirely:
//! it opens evdev keyboards, derives the keysym + modifier mask via
//! xkbcommon, and returns the first non-modifier press as a chord
//! string in the same `CTRL+SHIFT+ALT+SUPER+KEY` form the rest of
//! vernier already understands.
//!
//! The daemon (vernier-app) calls [`record_chord`] from its IPC
//! handler on `capture-chord`; the prefs UI calls [`record_chord_ipc`]
//! to talk to that handler over the existing `vernier.sock`.
//!
//! Requires the user to be in the `input` group so `/dev/input/event*`
//! is readable.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use evdev::{Device, EventSummary, KeyCode};
use xkbcommon::xkb;

/// Errors recording a chord directly via evdev.
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// No keyboard devices were found under `/dev/input`.
    #[error("no keyboard devices found under /dev/input")]
    NoKeyboards,
    /// `/dev/input` devices exist but could not be opened.
    #[error(
        "permission denied reading /dev/input — add your user to the 'input' group (`sudo usermod -aG input $USER`) and log back in"
    )]
    Permission,
    /// xkbcommon failed to compile the system keymap.
    #[error("could not compile the keyboard layout (xkbcommon)")]
    Keymap,
    /// No key was pressed within the timeout.
    #[error("chord-capture timed out")]
    Timeout,
}

/// Open every keyboard under `/dev/input`, wait for the first
/// non-modifier key press, and return the chord string.
///
/// Blocking; runs the evdev reader threads only for the duration of
/// the call. Caller should provide a generous timeout (e.g. 30 s) —
/// the user may take a moment between clicking "Record" and pressing
/// the chord.
///
/// # Errors
///
/// See [`RecordError`].
pub fn record_chord(timeout: Duration) -> Result<String, RecordError> {
    log::info!("chord_capture: compiling keymap");
    let keymap_text = compile_keymap()?;
    log::info!("chord_capture: enumerating keyboards");
    let keyboards = keyboard_devices()?;
    log::info!(
        "chord_capture: opened {} keyboard device(s)",
        keyboards.len()
    );
    let (tx, rx) = mpsc::channel::<String>();
    for (idx, device) in keyboards.into_iter().enumerate() {
        let tx = tx.clone();
        let keymap_text = keymap_text.clone();
        let name = device
            .name()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "<unnamed>".to_string());
        // Detached on purpose — the reader is blocked inside
        // `evdev::Device::fetch_events`, and there's no portable
        // way to interrupt that from another thread. Once the
        // winning reader's `tx.send()` succeeds, this `rx` is
        // dropped; the other readers will exit on their *next*
        // event (`tx.send` returns `Err`), which happens the next
        // time the user touches a key on that device. Short-lived
        // leak in the worst case, no leak in the common one.
        thread::spawn(move || {
            log::info!("chord_capture: device #{idx} reader started ({name})");
            read_until_chord(device, &keymap_text, &tx);
            log::info!("chord_capture: device #{idx} reader exiting ({name})");
        });
    }
    drop(tx);
    match rx.recv_timeout(timeout) {
        Ok(chord) => Ok(chord),
        Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
            Err(RecordError::Timeout)
        }
    }
}

fn compile_keymap() -> Result<String, RecordError> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap =
        xkb::Keymap::new_from_names(&context, "", "", "", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .ok_or(RecordError::Keymap)?;
    Ok(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
}

fn keyboard_devices() -> Result<Vec<Device>, RecordError> {
    let entries = match std::fs::read_dir("/dev/input") {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::PermissionDenied => return Err(RecordError::Permission),
        Err(_) => return Err(RecordError::NoKeyboards),
    };

    let mut keyboards = Vec::new();
    let mut permission_denied = false;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_event_node = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event_node {
            continue;
        }
        log::info!("chord_capture: opening {}", path.display());
        match Device::open(&path) {
            Ok(device) if is_keyboard(&device) => {
                log::info!("chord_capture: keyboard {}", path.display());
                keyboards.push(device);
            }
            Ok(_) => {
                log::info!("chord_capture: non-keyboard {}", path.display());
            }
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                log::warn!("chord_capture: permission denied on {}", path.display());
                permission_denied = true;
            }
            Err(e) => {
                log::warn!("chord_capture: error opening {}: {e}", path.display());
            }
        }
    }
    if !keyboards.is_empty() {
        Ok(keyboards)
    } else if permission_denied {
        Err(RecordError::Permission)
    } else {
        Err(RecordError::NoKeyboards)
    }
}

fn is_keyboard(device: &Device) -> bool {
    device
        .supported_keys()
        .is_some_and(|keys| keys.contains(KeyCode::KEY_A))
}

/// Read one device until the first non-modifier press, then send
/// the chord string back. Exits if the receiver is dropped
/// (chord already captured on another device, or timeout).
fn read_until_chord(mut device: Device, keymap_text: &str, tx: &mpsc::Sender<String>) {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let Some(keymap) = xkb::Keymap::new_from_string(
        &context,
        keymap_text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) else {
        return;
    };
    let mut state = xkb::State::new(&keymap);
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(e) => {
                log::warn!("chord_capture: fetch_events error: {e}");
                return;
            }
        };
        for input in events {
            let EventSummary::Key(_, code, value) = input.destructure() else {
                continue;
            };
            let keycode = xkb::Keycode::new(u32::from(code.0) + 8);
            if value == 1
                && let Some(chord) = chord_from_state(&state, keycode)
            {
                // Either we win the race and send the chord, or
                // another reader already did and `rx` is dropped —
                // either way our work is done.
                let _ = tx.send(chord);
                return;
            }
            if value != 2 {
                let direction = if value == 0 {
                    xkb::KeyDirection::Up
                } else {
                    xkb::KeyDirection::Down
                };
                state.update_key(keycode, direction);
            }
        }
    }
}

/// Build the chord string for a non-modifier key pressed in the
/// current xkb modifier state. Returns `None` if the press is a
/// bare modifier (so the caller keeps reading).
fn chord_from_state(state: &xkb::State, keycode: xkb::Keycode) -> Option<String> {
    let sym = state.key_get_one_sym(keycode).raw();
    if is_modifier_keysym(sym) {
        return None;
    }
    let key_token = chord_key_token(sym)?;
    // Canonical order: CTRL, SHIFT, ALT, SUPER — matches macOS
    // native menus (⌃⇧⌥⌘) and the Electron `CommandOrControl`
    // convention. The chip renderer reads this order when it splits
    // the string for display.
    let active = |m: &str| state.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
    let mut parts: Vec<&str> = Vec::new();
    if active(xkb::MOD_NAME_CTRL) {
        parts.push("CTRL");
    }
    if active(xkb::MOD_NAME_SHIFT) {
        parts.push("SHIFT");
    }
    if active(xkb::MOD_NAME_ALT) {
        parts.push("ALT");
    }
    if active(xkb::MOD_NAME_LOGO) {
        parts.push("SUPER");
    }
    Some(if parts.is_empty() {
        key_token
    } else {
        format!("{}+{key_token}", parts.join("+"))
    })
}

fn chord_key_token(sym: u32) -> Option<String> {
    // Canonical tokens vernier uses for keys whose xkb keysym name
    // would otherwise differ from what the rest of vernier (settings
    // defaults, the egui-side format_accelerator, the chip
    // renderer) already round-trips. Keeps Escape from showing up
    // as "ESCAPE" and the punctuation keys from colliding with the
    // `+` modifier separator.
    let named = match sym {
        0xff1b => Some("ESC"),            // Escape
        0xff0d | 0xff8d => Some("ENTER"), // Return / KP_Enter
        0xff09 => Some("TAB"),            // Tab
        0xff08 => Some("BACKSPACE"),      // BackSpace
        0xffff => Some("DELETE"),         // Delete
        0xff52 => Some("UP"),             // Up
        0xff54 => Some("DOWN"),           // Down
        0xff51 => Some("LEFT"),           // Left
        0xff53 => Some("RIGHT"),          // Right
        0x20 => Some("SPACE"),            // space
        0x2b => Some("PLUS"),             // +
        0x2d => Some("MINUS"),            // -
        0x3d => Some("EQUAL"),            // =
        _ => None,
    };
    if let Some(token) = named {
        return Some(token.to_string());
    }
    if (0x21..=0x7E).contains(&sym) {
        let ch = char::from_u32(sym)?.to_ascii_uppercase();
        return Some(ch.to_string());
    }
    let name = xkb::keysym_get_name(xkb::Keysym::from(sym));
    if name.is_empty() {
        return None;
    }
    Some(name.to_ascii_uppercase())
}

fn is_modifier_keysym(sym: u32) -> bool {
    (0xffe1..=0xffee).contains(&sym)
}

// ===== IPC client (used by vernier-ui) =================================

/// Errors talking to the daemon's chord-capture IPC.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The daemon is not running or hasn't started its listener yet.
    #[error("daemon not running (no vernier socket)")]
    DaemonOffline,
    /// Couldn't connect or talk to the daemon.
    #[error("chord-capture IPC: {0}")]
    Io(String),
    /// The user (or a timeout) cancelled the recording.
    #[error("chord-capture cancelled")]
    Cancelled,
    /// The daemon refused the request or hit an error.
    #[error("daemon error: {0}")]
    Daemon(String),
}

/// Default per-request timeout matching the daemon-side limit.
pub const DEFAULT_RECORD_TIMEOUT: Duration = Duration::from_secs(30);

/// Polling handle returned by [`record_chord_ipc`]. Drive it from
/// the UI thread with [`try_recv`](Self::try_recv); call
/// [`abort`](Self::abort) on Esc or window close.
pub struct ChordRecording {
    rx: mpsc::Receiver<Result<String, ClientError>>,
    abort: UnixStream,
}

impl ChordRecording {
    pub fn try_recv(&self) -> Result<Option<String>, ClientError> {
        match self.rx.try_recv() {
            Ok(result) => result.map(Some),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(ClientError::Cancelled),
        }
    }

    pub fn abort(&self) {
        let _ = self.abort.shutdown(std::net::Shutdown::Both);
    }
}

/// Connect to the daemon's IPC socket and arm a chord recording.
/// Spawns a worker thread to block on the reply; the caller polls
/// the returned [`ChordRecording`] from the UI loop.
///
/// # Errors
///
/// See [`ClientError`].
pub fn record_chord_ipc() -> Result<ChordRecording, ClientError> {
    let path = ipc_socket_path();
    log::info!("chord_capture client: connecting to {}", path.display());
    let mut stream = UnixStream::connect(&path).map_err(|e| {
        if e.kind() == ErrorKind::NotFound || e.kind() == ErrorKind::ConnectionRefused {
            ClientError::DaemonOffline
        } else {
            ClientError::Io(e.to_string())
        }
    })?;
    stream
        .write_all(b"capture-chord\n")
        .map_err(|e| ClientError::Io(e.to_string()))?;
    let abort = stream
        .try_clone()
        .map_err(|e| ClientError::Io(e.to_string()))?;

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let result = match reader.read_line(&mut line) {
            Ok(_) => parse_reply(line.trim()),
            Err(e) => Err(ClientError::Io(e.to_string())),
        };
        let _ = tx.send(result);
    });
    Ok(ChordRecording { rx, abort })
}

fn parse_reply(line: &str) -> Result<String, ClientError> {
    if line.is_empty() || line == "cancel" {
        return Err(ClientError::Cancelled);
    }
    if let Some(rest) = line.strip_prefix("error: ") {
        return Err(ClientError::Daemon(rest.to_string()));
    }
    if let Some(rest) = line.strip_prefix("err ") {
        return Err(ClientError::Daemon(rest.to_string()));
    }
    Ok(line.to_string())
}

fn ipc_socket_path() -> PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    runtime_dir.join("vernier.sock")
}
