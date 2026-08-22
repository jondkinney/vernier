//! Localhost WebSocket bridge for the Vernier Figma plugin.
//!
//! The Figma plugin runs inside the user's Figma tab and pushes the
//! current viewport zoom to localhost. Each verified connection owns
//! its own cached sample. The HUD reads the most recently activated
//! fresh sample without doing WebSocket I/O on the rendering thread.

use std::collections::HashMap;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::{StatusCode, header::ORIGIN};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, accept_hdr_with_config};

type ConnectionId = u64;

const PROTOCOL_VERSION: u64 = 1;
const MIN_ZOOM: f64 = 0.01;
const MAX_ZOOM: f64 = 256.0;
const MAX_CLIENTS: usize = 8;
const MAX_MESSAGE_SIZE: usize = 4 * 1024;
const MAX_FRAME_SIZE: usize = 4 * 1024;
const WRITE_BUFFER_SIZE: usize = 1024;
const MAX_WRITE_BUFFER_SIZE: usize = 8 * 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(6);
const CONNECTION_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
struct ZoomSample {
    value: f64,
    refreshed_at: Instant,
    activation_order: u64,
}

/// All samples belong to the currently configured listener generation.
/// Advancing the generation makes already-accepted sockets from an old
/// port harmless even if their handler thread has not observed shutdown yet.
#[derive(Debug, Default)]
struct BridgeState {
    generation: u64,
    activation_order: u64,
    connections: HashMap<ConnectionId, Option<ZoomSample>>,
}

impl BridgeState {
    fn reset(&mut self, generation: u64) {
        self.generation = generation;
        self.activation_order = 0;
        self.connections.clear();
    }

    fn connect(&mut self, generation: u64, id: ConnectionId) -> bool {
        if generation != self.generation {
            return false;
        }
        self.connections.insert(id, None);
        true
    }

    fn update(
        &mut self,
        generation: u64,
        id: ConnectionId,
        value: f64,
        active: bool,
        received_at: Instant,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(previous) = self.connections.get(&id).copied() else {
            return false;
        };
        let next = if active {
            let activation_order = match previous {
                Some(sample)
                    if received_at
                        .checked_duration_since(sample.refreshed_at)
                        .is_some_and(|age| age <= FRESHNESS) =>
                {
                    sample.activation_order
                }
                _ => {
                    self.activation_order = self.activation_order.saturating_add(1);
                    self.activation_order
                }
            };
            Some(ZoomSample {
                value,
                refreshed_at: received_at,
                activation_order,
            })
        } else {
            None
        };
        self.connections.insert(id, next);
        true
    }

    fn disconnect(&mut self, generation: u64, id: ConnectionId) {
        if generation == self.generation {
            self.connections.remove(&id);
        }
    }

    fn current_at(&self, now: Instant) -> Option<f64> {
        self.connections
            .values()
            .filter_map(|sample| *sample)
            .filter(|sample| {
                now.checked_duration_since(sample.refreshed_at)
                    .is_some_and(|age| age <= FRESHNESS)
            })
            .max_by_key(|sample| sample.activation_order)
            .map(|sample| sample.value)
    }
}

fn bridge_state() -> &'static RwLock<BridgeState> {
    static STATE: OnceLock<RwLock<BridgeState>> = OnceLock::new();
    STATE.get_or_init(|| RwLock::new(BridgeState::default()))
}

/// A heartbeat from the plugin refreshes the sample even when the zoom itself
/// has not changed. Five seconds leaves room for a temporarily busy browser
/// event loop while still dropping an abruptly-dead plugin promptly.
const FRESHNESS: Duration = Duration::from_secs(5);
const MANAGER_TICK: Duration = Duration::from_millis(25);
const BIND_RETRY: Duration = Duration::from_secs(2);

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_CLIENTS: AtomicUsize = AtomicUsize::new(0);

struct ClientSlot<'a> {
    counter: &'a AtomicUsize,
}

impl Drop for ClientSlot<'_> {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire_client_slot(counter: &AtomicUsize, limit: usize) -> Option<ClientSlot<'_>> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < limit).then_some(active + 1)
        })
        .ok()
        .map(|_| ClientSlot { counter })
}

fn acquire_client_slot() -> Option<ClientSlot<'static>> {
    try_acquire_client_slot(&ACTIVE_CLIENTS, MAX_CLIENTS)
}

/// Read the authoritative live Figma zoom factor (for example, 2.0 = 200%
/// zoom). Activation order is stable across heartbeats, so two visible
/// windows cannot alternate merely because their timers fire in a different
/// order. Returns None when no verified active plugin is reporting, all
/// samples are stale, or the state lock is poisoned.
pub fn current_figma_zoom() -> Option<f64> {
    bridge_state().read().ok()?.current_at(Instant::now())
}

/// Configure the process-wide bridge listener. Some(port) starts it or moves
/// it to a new port; None stops accepting connections and invalidates every
/// cached sample. Calls are cheap and idempotent, so settings reloads can
/// always pass their latest value.
pub fn configure(port: Option<u16>) {
    if bridge_control().send(port).is_err() {
        log::warn!("figma bridge: manager thread is unavailable");
    }
}

/// Backwards-compatible startup shorthand.
pub fn spawn(port: u16) {
    configure(Some(port));
}

/// Stop the listener and immediately invalidate its cached zoom samples.
pub fn stop() {
    configure(None);
}

fn bridge_control() -> &'static Sender<Option<u16>> {
    static CONTROL: OnceLock<Sender<Option<u16>>> = OnceLock::new();
    CONTROL.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("vernier-figma-bridge".into())
            .spawn(move || run_manager(rx))
            .expect("spawn vernier Figma bridge manager");
        tx
    })
}

fn run_manager(rx: Receiver<Option<u16>>) {
    let mut desired_port = None;
    let mut bound_port = None;
    let mut listener: Option<TcpListener> = None;
    let mut generation = 0_u64;
    let mut next_bind_attempt = Instant::now();

    loop {
        match rx.recv_timeout(MANAGER_TICK) {
            Ok(mut requested) => {
                // Collapse a burst of saves to the newest setting before doing
                // any socket work.
                while let Ok(newer) = rx.try_recv() {
                    requested = newer;
                }
                if requested != desired_port {
                    desired_port = requested;
                    listener = None;
                    bound_port = None;
                    generation = generation.wrapping_add(1);
                    if let Ok(mut state) = bridge_state().write() {
                        state.reset(generation);
                    }
                    next_bind_attempt = Instant::now();
                    match desired_port {
                        Some(port) => log::info!("figma bridge: configuring port {port}"),
                        None => log::info!("figma bridge: stopped"),
                    }
                } else if listener.is_none() && desired_port.is_some() {
                    // An explicit save of the same port is also a useful retry
                    // after a previous bind failure.
                    next_bind_attempt = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if listener.is_none() && Instant::now() >= next_bind_attempt {
            let Some(port) = desired_port else { continue };
            let bind = format!("127.0.0.1:{port}");
            match TcpListener::bind(&bind) {
                Ok(candidate) => {
                    if let Err(e) = candidate.set_nonblocking(true) {
                        log::warn!("figma bridge: make {bind} nonblocking: {e}");
                        next_bind_attempt = Instant::now() + BIND_RETRY;
                    } else {
                        log::info!("figma bridge: listening on {bind}");
                        listener = Some(candidate);
                        bound_port = Some(port);
                    }
                }
                Err(e) => {
                    log::warn!("figma bridge: bind {bind}: {e}; retrying");
                    next_bind_attempt = Instant::now() + BIND_RETRY;
                }
            }
        }

        let Some(active_listener) = listener.as_ref() else {
            continue;
        };
        debug_assert_eq!(bound_port, desired_port);
        loop {
            match active_listener.accept() {
                Ok((stream, _)) => {
                    let Some(slot) = acquire_client_slot() else {
                        log::debug!(
                            "figma bridge: rejecting connection; {MAX_CLIENTS}-client limit reached"
                        );
                        drop(stream);
                        continue;
                    };
                    let id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = std::thread::Builder::new()
                        .name("vernier-figma-conn".into())
                        .spawn(move || handle(stream, generation, id, slot))
                    {
                        log::warn!("figma bridge: spawn connection handler: {e}");
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    log::warn!("figma bridge: accept: {e}");
                    break;
                }
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum ClientMessage {
    Hello,
    Zoom { value: f64, active: bool },
}

fn valid_editor_type(value: &serde_json::Value) -> bool {
    matches!(
        value.get("editorType").and_then(|editor| editor.as_str()),
        Some("figma" | "dev")
    )
}

fn valid_protocol_envelope(value: &serde_json::Value) -> bool {
    value.get("protocol").and_then(|protocol| protocol.as_u64()) == Some(PROTOCOL_VERSION)
        && value.get("client").and_then(|client| client.as_str()) == Some("figma")
        && valid_editor_type(value)
}

fn parse_client_message(text: &str) -> Option<ClientMessage> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(e) => {
            log::debug!("figma bridge: bad json: {e}: {text}");
            return None;
        }
    };
    match value.get("type").and_then(|kind| kind.as_str()) {
        Some("hello") if valid_protocol_envelope(&value) => Some(ClientMessage::Hello),
        Some("zoom") if valid_protocol_envelope(&value) => {
            let zoom = value.get("value").and_then(|zoom| zoom.as_f64())?;
            let active = value.get("active").and_then(|active| active.as_bool())?;
            (zoom.is_finite() && (MIN_ZOOM..=MAX_ZOOM).contains(&zoom)).then_some(
                ClientMessage::Zoom {
                    value: zoom,
                    active,
                },
            )
        }
        _ => None,
    }
}

/// Figma's sandboxed plugin iframe currently sends the opaque Origin value
/// "null". Exact first-party origins cover hosts that preserve Figma's page
/// origin instead. Missing origins, arbitrary subdomains, HTTP, and every
/// unrelated site fail closed.
fn origin_allowed(origin: Option<&str>) -> bool {
    match origin {
        Some("null") => true,
        Some(origin) => {
            origin.eq_ignore_ascii_case("https://www.figma.com")
                || origin.eq_ignore_ascii_case("https://figma.com")
        }
        None => false,
    }
}

// Tungstenite's Callback contract requires its concrete HTTP ErrorResponse;
// boxing it would no longer satisfy accept_hdr_with_config.
#[allow(clippy::result_large_err)]
fn validate_origin(request: &Request, response: Response) -> Result<Response, ErrorResponse> {
    let origin = request
        .headers()
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok());
    if origin_allowed(origin) {
        return Ok(response);
    }

    let mut rejected = ErrorResponse::new(Some("Forbidden WebSocket origin".into()));
    *rejected.status_mut() = StatusCode::FORBIDDEN;
    Err(rejected)
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        write_buffer_size: WRITE_BUFFER_SIZE,
        max_write_buffer_size: MAX_WRITE_BUFFER_SIZE,
        max_message_size: Some(MAX_MESSAGE_SIZE),
        max_frame_size: Some(MAX_FRAME_SIZE),
        accept_unmasked_frames: false,
        ..WebSocketConfig::default()
    }
}

fn set_handshake_timeouts(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))
}

fn set_connection_timeouts(stream: &TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(CONNECTION_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_WRITE_TIMEOUT))
}

fn hello_response() -> String {
    serde_json::json!({
        "type": "hello",
        "protocol": PROTOCOL_VERSION,
        "server": "vernier",
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
}

fn handle(stream: TcpStream, generation: u64, id: ConnectionId, _slot: ClientSlot<'static>) {
    let peer = stream
        .peer_addr()
        .map(|address| address.to_string())
        .unwrap_or_else(|_| "?".into());
    // BSD-derived systems (macOS) hand accepted sockets the listener's
    // O_NONBLOCK flag; Linux always accepts blocking sockets. The
    // handler relies on blocking reads bounded by the read/write
    // timeouts below, so force blocking mode explicitly.
    if let Err(e) = stream.set_nonblocking(false) {
        log::debug!("figma bridge: make connection blocking for {peer}: {e}");
        return;
    }
    if let Err(e) = set_handshake_timeouts(&stream) {
        log::debug!("figma bridge: configure handshake timeout for {peer}: {e}");
        return;
    }
    let mut ws = match accept_hdr_with_config(stream, validate_origin, Some(websocket_config())) {
        Ok(socket) => socket,
        Err(e) => {
            log::debug!("figma bridge: handshake from {peer}: {e}");
            return;
        }
    };
    let registered = bridge_state()
        .write()
        .map(|mut state| state.connect(generation, id))
        .unwrap_or(false);
    if !registered {
        let _ = ws.close(None);
        return;
    }
    log::info!("figma bridge: socket connected ({peer}, connection {id})");
    let mut verified = false;
    let hello_deadline = Instant::now() + HELLO_TIMEOUT;
    loop {
        if !verified && Instant::now() >= hello_deadline {
            log::debug!("figma bridge: hello timed out ({peer}, connection {id})");
            break;
        }
        match ws.read() {
            Ok(Message::Text(text)) => match parse_client_message(&text) {
                Some(ClientMessage::Hello) => {
                    if ws.send(Message::Text(hello_response())).is_err() {
                        break;
                    }
                    if let Err(e) = set_connection_timeouts(ws.get_ref()) {
                        log::debug!("figma bridge: configure connection timeout for {peer}: {e}");
                        break;
                    }
                    verified = true;
                    log::info!(
                        "figma bridge: plugin verified ({peer}, connection {id}, protocol {PROTOCOL_VERSION})"
                    );
                }
                Some(ClientMessage::Zoom { value, active }) if verified => {
                    let current_generation = bridge_state()
                        .write()
                        .map(|mut state| {
                            state.update(generation, id, value, active, Instant::now())
                        })
                        .unwrap_or(false);
                    if !current_generation {
                        break;
                    }
                }
                Some(ClientMessage::Zoom { .. }) | None => {}
            },
            Ok(Message::Close(_)) | Err(_) => break,
            // Pings, pongs, and binary frames carry no zoom state.
            Ok(_) => {}
        }
    }
    if let Ok(mut state) = bridge_state().write() {
        state.disconnect(generation, id);
    }
    log::info!("figma bridge: plugin disconnected ({peer}, connection {id})");
}

#[cfg(test)]
mod tests {
    use super::*;
    use tungstenite::client::IntoClientRequest;

    fn client_request(port: u16, origin: &'static str) -> Request {
        let mut request = format!("ws://127.0.0.1:{port}")
            .into_client_request()
            .expect("client request");
        request
            .headers_mut()
            .insert(ORIGIN, origin.parse().expect("origin header"));
        request
    }

    fn connected_state(now: Instant) -> BridgeState {
        let mut state = BridgeState::default();
        state.reset(7);
        assert!(state.connect(7, 11));
        assert!(state.update(7, 11, 1.25, true, now));
        state
    }

    #[test]
    fn repeated_unchanged_zoom_refreshes_the_lease() {
        let now = Instant::now();
        let old = now - FRESHNESS - Duration::from_millis(1);
        let mut state = connected_state(old);
        assert_eq!(state.current_at(now), None);

        assert!(state.update(7, 11, 1.25, true, now));
        assert_eq!(state.current_at(now), Some(1.25));
    }

    #[test]
    fn disconnect_only_removes_its_own_connection() {
        let now = Instant::now();
        let mut state = connected_state(now - Duration::from_millis(10));
        assert!(state.connect(7, 12));
        assert!(state.update(7, 12, 2.0, true, now));
        assert_eq!(state.current_at(now), Some(2.0));

        state.disconnect(7, 11);
        assert_eq!(state.current_at(now), Some(2.0));
        state.disconnect(7, 12);
        assert_eq!(state.current_at(now), None);
    }

    #[test]
    fn activation_order_is_stable_across_heartbeats_and_deactivation_falls_back() {
        let now = Instant::now();
        let mut state = connected_state(now);
        assert!(state.connect(7, 12));
        assert!(state.update(7, 12, 2.0, true, now + Duration::from_millis(1),));
        assert_eq!(state.current_at(now + Duration::from_millis(1)), Some(2.0));

        // Client 11 heartbeats later, but it was activated earlier.
        // Freshness and value update without stealing authority.
        assert!(state.update(7, 11, 1.5, true, now + Duration::from_millis(2),));
        assert_eq!(state.current_at(now + Duration::from_millis(2)), Some(2.0));

        // Hiding client 12 removes only its lease; client 11 resumes.
        assert!(state.update(7, 12, 2.0, false, now + Duration::from_millis(3),));
        assert_eq!(state.current_at(now + Duration::from_millis(3)), Some(1.5));

        // A later reactivation deliberately becomes authoritative.
        assert!(state.update(7, 12, 2.25, true, now + Duration::from_millis(4),));
        assert_eq!(state.current_at(now + Duration::from_millis(4)), Some(2.25));
    }

    #[test]
    fn reconfiguration_invalidates_old_connection_handlers() {
        let now = Instant::now();
        let mut state = connected_state(now);
        state.reset(8);

        assert!(!state.update(7, 11, 9.0, true, now));
        assert_eq!(state.current_at(now), None);
        assert!(state.connect(8, 21));
        assert!(state.update(8, 21, 1.5, true, now));
        assert_eq!(state.current_at(now), Some(1.5));
    }

    #[test]
    fn parser_requires_the_complete_protocol_schema() {
        assert_eq!(
            parse_client_message(
                r#"{"type":"hello","protocol":1,"client":"figma","editorType":"dev"}"#
            ),
            Some(ClientMessage::Hello)
        );
        assert_eq!(
            parse_client_message(
                r#"{"type":"zoom","value":1.75,"protocol":1,"client":"figma","editorType":"figma","active":true,"extra":"ignored"}"#
            ),
            Some(ClientMessage::Zoom {
                value: 1.75,
                active: true,
            })
        );
        assert_eq!(
            parse_client_message(
                r#"{"type":"zoom","value":2.0,"protocol":1,"client":"figma","editorType":"dev","active":false}"#
            ),
            Some(ClientMessage::Zoom {
                value: 2.0,
                active: false,
            })
        );

        let rejected = [
            r#"{"type":"hello","protocol":1,"client":"figma"}"#,
            r#"{"type":"hello","protocol":2,"client":"figma","editorType":"dev"}"#,
            r#"{"type":"hello","protocol":1,"client":"other","editorType":"dev"}"#,
            r#"{"type":"zoom","value":1.75,"client":"figma","editorType":"dev","active":true}"#,
            r#"{"type":"zoom","value":1.75,"protocol":2,"client":"figma","editorType":"dev","active":true}"#,
            r#"{"type":"zoom","value":1.75,"protocol":1,"client":"other","editorType":"dev","active":true}"#,
            r#"{"type":"zoom","value":1.75,"protocol":1,"client":"figma","active":true}"#,
            r#"{"type":"zoom","value":1.75,"protocol":1,"client":"figma","editorType":"slides","active":true}"#,
            r#"{"type":"zoom","value":1.75,"protocol":1,"client":"figma","editorType":"dev"}"#,
            r#"{"type":"zoom","value":1.75,"protocol":1,"client":"figma","editorType":"dev","active":"true"}"#,
            r#"{"type":"heartbeat","value":1.75,"protocol":1,"client":"figma","editorType":"dev","active":true}"#,
            r#"{"value":1.75,"protocol":1,"client":"figma","editorType":"dev","active":true}"#,
            "not json",
        ];
        for message in rejected {
            assert!(
                parse_client_message(message).is_none(),
                "unexpectedly accepted {message}"
            );
        }
    }

    #[test]
    fn parser_enforces_plausible_zoom_bounds_inclusively() {
        let message = |zoom: f64| {
            format!(
                r#"{{"type":"zoom","value":{zoom},"protocol":1,"client":"figma","editorType":"dev","active":true}}"#
            )
        };

        assert!(parse_client_message(&message(MIN_ZOOM)).is_some());
        assert!(parse_client_message(&message(MAX_ZOOM)).is_some());
        assert!(parse_client_message(&message(MIN_ZOOM - 0.001)).is_none());
        assert!(parse_client_message(&message(MAX_ZOOM + 0.001)).is_none());
        assert!(parse_client_message(&message(0.0)).is_none());
        assert!(parse_client_message(&message(-1.0)).is_none());
    }

    #[test]
    fn hello_identifies_vernier_and_the_protocol_version() {
        let response: serde_json::Value =
            serde_json::from_str(&hello_response()).expect("valid hello response");
        assert_eq!(response["type"], "hello");
        assert_eq!(response["protocol"], PROTOCOL_VERSION);
        assert_eq!(response["server"], "vernier");
        assert_eq!(response["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn origin_policy_allows_only_opaque_or_exact_figma_origins() {
        for origin in [
            "null",
            "https://www.figma.com",
            "https://figma.com",
            "HTTPS://WWW.FIGMA.COM",
        ] {
            let request = Request::builder()
                .header(ORIGIN, origin)
                .body(())
                .expect("origin request");
            assert!(
                validate_origin(&request, Response::new(())).is_ok(),
                "expected origin to be allowed: {origin}"
            );
        }

        for origin in [
            "https://evil.example",
            "http://www.figma.com",
            "https://www.figma.com.evil.example",
            "https://plugins.figma.com",
            "https://www.figma.com/",
        ] {
            let request = Request::builder()
                .header(ORIGIN, origin)
                .body(())
                .expect("origin request");
            let rejection = validate_origin(&request, Response::new(()))
                .expect_err("origin should be rejected");
            assert_eq!(rejection.status(), StatusCode::FORBIDDEN);
        }

        let missing = Request::builder().body(()).expect("originless request");
        assert!(validate_origin(&missing, Response::new(())).is_err());
    }

    #[test]
    fn websocket_limits_are_small_and_masking_remains_required() {
        let config = websocket_config();
        assert_eq!(config.write_buffer_size, WRITE_BUFFER_SIZE);
        assert_eq!(config.max_write_buffer_size, MAX_WRITE_BUFFER_SIZE);
        assert_eq!(config.max_message_size, Some(MAX_MESSAGE_SIZE));
        assert_eq!(config.max_frame_size, Some(MAX_FRAME_SIZE));
        assert!(!config.accept_unmasked_frames);
    }

    #[test]
    fn socket_timeout_profiles_are_applied() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("timeout test listener");
        let address = listener.local_addr().expect("timeout test address");
        let _client = TcpStream::connect(address).expect("timeout test client");
        let (server, _) = listener.accept().expect("timeout test accept");

        set_handshake_timeouts(&server).expect("handshake timeouts");
        assert_eq!(server.read_timeout().unwrap(), Some(HANDSHAKE_TIMEOUT));
        assert_eq!(server.write_timeout().unwrap(), Some(HANDSHAKE_TIMEOUT));

        set_connection_timeouts(&server).expect("connection timeouts");
        assert_eq!(
            server.read_timeout().unwrap(),
            Some(CONNECTION_READ_TIMEOUT)
        );
        assert_eq!(
            server.write_timeout().unwrap(),
            Some(CONNECTION_WRITE_TIMEOUT)
        );
    }

    #[test]
    fn client_slots_enforce_the_limit_and_release_on_drop() {
        let counter = AtomicUsize::new(0);
        let first = try_acquire_client_slot(&counter, 2).expect("first slot");
        let second = try_acquire_client_slot(&counter, 2).expect("second slot");
        assert!(try_acquire_client_slot(&counter, 2).is_none());
        assert_eq!(counter.load(Ordering::Acquire), 2);

        drop(first);
        let replacement = try_acquire_client_slot(&counter, 2).expect("released slot");
        assert_eq!(counter.load(Ordering::Acquire), 2);
        drop(second);
        drop(replacement);
        assert_eq!(counter.load(Ordering::Acquire), 0);
    }

    #[test]
    fn live_bridge_verifies_client_and_caches_zoom() {
        let reservation = TcpListener::bind("127.0.0.1:0").expect("reserve test port");
        let port = reservation.local_addr().expect("test address").port();
        drop(reservation);
        configure(Some(port));

        let deadline = Instant::now() + Duration::from_secs(2);
        let rejected_stream = loop {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(stream) => break stream,
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("bridge did not bind test port {port}: {e}"),
            }
        };
        rejected_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set client read timeout");
        assert!(
            tungstenite::client(
                client_request(port, "https://evil.example"),
                rejected_stream
            )
            .is_err(),
            "non-Figma origin unexpectedly completed the handshake"
        );

        let stream = TcpStream::connect(("127.0.0.1", port)).expect("allowed client connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set client read timeout");
        let (mut client, _) =
            tungstenite::client(client_request(port, "null"), stream).expect("client handshake");
        client
            .send(Message::Text(
                r#"{"type":"hello","protocol":1,"client":"figma","editorType":"dev"}"#.into(),
            ))
            .expect("send hello");
        let response = client.read().expect("read server hello");
        let Message::Text(response) = response else {
            panic!("expected text server hello");
        };
        let response: serde_json::Value =
            serde_json::from_str(&response).expect("parse server hello");
        assert_eq!(response["server"], "vernier");

        client
            .send(Message::Text(
                r#"{"type":"zoom","value":1.75,"protocol":1,"client":"figma","editorType":"dev","active":true}"#
                    .into(),
            ))
            .expect("send zoom");
        let deadline = Instant::now() + Duration::from_secs(1);
        while current_figma_zoom() != Some(1.75) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(current_figma_zoom(), Some(1.75));

        stop();
        let deadline = Instant::now() + Duration::from_secs(1);
        while current_figma_zoom().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(current_figma_zoom(), None);
    }
}
