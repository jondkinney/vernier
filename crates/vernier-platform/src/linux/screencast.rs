//! ScreenCast portal — opens a session, returns the PipeWire remote FD and
//! per-output PipeWire node IDs, then drives the PipeWire stream loop on a
//! dedicated thread to keep a `latest_frame` map up to date.
//!
//! Persists the portal restore token to
//! `$XDG_CONFIG_HOME/vernier/screencast.token` so subsequent runs don't
//! prompt the user again.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};

use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions, Stream as PortalStream,
};
use ashpd::enumflags2::BitFlags;

use pipewire as pw;
use pw::spa::param::ParamType;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Pod, Value};
use pw::spa::utils::{Fraction, Rectangle, SpaTypes};
use pw::stream::StreamFlags;

use crate::{PlatformError, Result};

#[derive(Debug, Clone)]
pub(crate) struct StreamInfo {
    pub node_id: u32,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    pub stream_id: Option<String>,
}

pub(crate) struct SessionState {
    pub streams: Vec<StreamInfo>,
    pub pipewire_fd: OwnedFd,
    #[allow(dead_code)] // consumed when we save the token
    pub restore_token: Option<String>,
}

/// One captured frame as the PipeWire stream most recently produced it.
/// Pixel layout depends on `format` (typically BGRx/BGRA on Hyprland).
#[derive(Debug, Clone)]
pub(crate) struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: VideoFormat,
    pub pixels: Vec<u8>,
}

/// Live capture service: owns the PipeWire main loop on a background thread
/// and exposes the latest frame per portal stream node id.
pub(crate) struct CaptureService {
    streams: Vec<StreamInfo>,
    frames: Arc<Mutex<HashMap<u32, CapturedFrame>>>,
}

impl CaptureService {
    pub fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    pub fn latest_frame(&self, node_id: u32) -> Option<CapturedFrame> {
        self.frames.lock().ok()?.get(&node_id).cloned()
    }
}

/// Spawn the PipeWire thread, connect with `state.pipewire_fd`, and start
/// streaming each output. Returns once the thread has reported back via the
/// ready channel — either with the live service or a fatal init error.
pub(crate) fn start_capture(state: SessionState) -> Result<CaptureService> {
    let frames: Arc<Mutex<HashMap<u32, CapturedFrame>>> = Arc::new(Mutex::new(HashMap::new()));
    let frames_for_thread = frames.clone();
    let streams = state.streams.clone();
    let streams_for_thread = state.streams.clone();
    let fd = state.pipewire_fd;

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let ready_tx_for_thread = ready_tx.clone();

    std::thread::Builder::new()
        .name("vernier-pipewire".into())
        .spawn(move || {
            if let Err(e) = run_pipewire(
                fd,
                streams_for_thread,
                frames_for_thread,
                ready_tx_for_thread.clone(),
            ) {
                let _ = ready_tx_for_thread.send(Err(e));
            }
        })
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("spawn pipewire thread: {e}")))?;

    ready_rx
        .recv()
        .map_err(|_| PlatformError::Other(anyhow::anyhow!("pipewire init failed")))??;

    Ok(CaptureService { streams, frames })
}

struct StreamUserData {
    node_id: u32,
    format: VideoInfoRaw,
    frames: Arc<Mutex<HashMap<u32, CapturedFrame>>>,
    frame_count: u64,
    process_calls: u64,
}

fn run_pipewire(
    fd: OwnedFd,
    streams_meta: Vec<StreamInfo>,
    frames: Arc<Mutex<HashMap<u32, CapturedFrame>>>,
    ready_tx: SyncSender<Result<()>>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("pw mainloop: {e}")))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("pw context: {e}")))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("pw connect_fd: {e}")))?;

    // Keep streams + listeners alive for the lifetime of the loop. Drop = unsubscribe.
    let mut keep_alive: Vec<(
        pw::stream::StreamRc,
        pw::stream::StreamListener<StreamUserData>,
    )> = Vec::new();
    for meta in &streams_meta {
        let (stream, listener) = create_stream(core.clone(), meta.node_id, frames.clone())?;
        keep_alive.push((stream, listener));
    }

    let _ = ready_tx.send(Ok(()));
    log::info!(
        "pipewire: main loop running with {} stream(s)",
        streams_meta.len()
    );

    mainloop.run();
    Ok(())
}

fn create_stream(
    core: pw::core::CoreRc,
    node_id: u32,
    frames: Arc<Mutex<HashMap<u32, CapturedFrame>>>,
) -> Result<(
    pw::stream::StreamRc,
    pw::stream::StreamListener<StreamUserData>,
)> {
    use pw::properties::properties;

    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };
    let stream = pw::stream::StreamRc::new(core, "vernier-capture", props)
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("pw stream new: {e}")))?;

    let user_data = StreamUserData {
        node_id,
        format: VideoInfoRaw::new(),
        frames,
        frame_count: 0,
        process_calls: 0,
    };

    let listener = stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(move |_, _, old, new| {
            log::debug!("pw stream {node_id}: {old:?} -> {new:?}");
        })
        .param_changed(|_, ud, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let (mt, ms) = match format_utils::parse_format(param) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!("pw {} parse_format: {e:?}", ud.node_id);
                    return;
                }
            };
            if mt != MediaType::Video || ms != MediaSubtype::Raw {
                log::warn!("pw {} unsupported media: {mt:?}/{ms:?}", ud.node_id);
                return;
            }
            if let Err(e) = ud.format.parse(param) {
                log::warn!("pw {} VideoInfoRaw parse: {e:?}", ud.node_id);
                return;
            }
            let size = ud.format.size();
            let fr = ud.format.framerate();
            log::info!(
                "pw {}: format negotiated: {:?} {}x{} @ {}/{} fps",
                ud.node_id, ud.format.format(), size.width, size.height, fr.num, fr.denom,
            );
        })
        .process(|stream, ud| {
            ud.process_calls += 1;
            let Some(mut buffer) = stream.dequeue_buffer() else {
                if ud.process_calls <= 3 {
                    log::debug!("pw {}: dequeue_buffer returned None", ud.node_id);
                }
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.iter_mut().next() else {
                if ud.process_calls <= 3 {
                    log::debug!("pw {}: no data slot", ud.node_id);
                }
                return;
            };
            let (size, stride, chunk_offset) = {
                let chunk = data.chunk();
                (
                    chunk.size() as usize,
                    chunk.stride() as u32,
                    chunk.offset() as usize,
                )
            };
            let dtype = data.type_();
            let dflags = data.flags();
            let dfd = data.fd();
            let dmaxsize = data.as_raw().maxsize as usize;
            let Some(slice) = data.data() else {
                if ud.process_calls <= 3 {
                    log::debug!(
                        "pw {}: data.data() None (likely DMA-BUF; size={} stride={})",
                        ud.node_id, size, stride
                    );
                }
                return;
            };
            if ud.process_calls == 0 {
                log::debug!(
                    "pw {}: first process: type={:?} flags={:?} fd={} size={} offset={} stride={} maxsize={}",
                    ud.node_id, dtype, dflags, dfd, size, chunk_offset, stride, dmaxsize
                );
            }
            // PipeWire's MAP_BUFFERS flag should mmap memfd buffers, but on
            // Hyprland we still get FLAGS=MAPPABLE without READABLE — meaning
            // we have to mmap the fd ourselves. Do that on every process call
            // for now; we'll cache the mapping per-buffer in a later milestone.
            let pixels = if dfd >= 0 && dmaxsize > 0 {
                let page = 4096;
                let mmap_len = (dmaxsize + page - 1) & !(page - 1);
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        mmap_len,
                        libc::PROT_READ,
                        libc::MAP_PRIVATE,
                        dfd,
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    let err = std::io::Error::last_os_error();
                    if ud.process_calls < 3 {
                        log::warn!("pw {}: mmap failed: {err}", ud.node_id);
                    }
                    return;
                }
                let end = chunk_offset.saturating_add(size).min(dmaxsize);
                let start = chunk_offset.min(end);
                let bytes =
                    unsafe { std::slice::from_raw_parts((ptr as *const u8).add(start), end - start) };
                let v = bytes.to_vec();
                unsafe {
                    libc::munmap(ptr, mmap_len);
                }
                v
            } else if dflags.contains(pw::spa::buffer::DataFlags::READABLE) && size > 0 {
                let end = chunk_offset.saturating_add(size).min(slice.len());
                let start = chunk_offset.min(end);
                slice[start..end].to_vec()
            } else {
                if ud.process_calls == 0 {
                    log::warn!(
                        "pw {}: cannot read frame (no fd, not readable)",
                        ud.node_id
                    );
                }
                return;
            };
            let frame = CapturedFrame {
                width: ud.format.size().width,
                height: ud.format.size().height,
                stride,
                format: ud.format.format(),
                pixels,
            };
            if ud.frame_count == 0 {
                log::info!(
                    "pw {}: first frame {}x{} {} bytes, format {:?}",
                    ud.node_id, frame.width, frame.height, size, frame.format
                );
            }
            ud.frame_count += 1;
            if let Ok(mut g) = ud.frames.lock() {
                g.insert(ud.node_id, frame);
            }
        })
        .register()
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("register listener: {e}")))?;

    let format_bytes = build_format_pod()?;
    let buffers_bytes = build_buffers_pod()?;
    let format_pod = Pod::from_bytes(&format_bytes)
        .ok_or_else(|| PlatformError::Other(anyhow::anyhow!("format Pod::from_bytes")))?;
    let buffers_pod = Pod::from_bytes(&buffers_bytes)
        .ok_or_else(|| PlatformError::Other(anyhow::anyhow!("buffers Pod::from_bytes")))?;
    let mut params = [format_pod, buffers_pod];

    stream
        .connect(
            pw::spa::utils::Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("pw stream connect: {e}")))?;

    Ok((stream, listener))
}

/// Restrict the stream to CPU-mappable buffers (memfd or anonymous memory).
/// Without this, Hyprland negotiates DMA-BUF, which our process callback
/// can't safely read via `data.data()`.
fn build_buffers_pod() -> Result<Vec<u8>> {
    use pw::spa::pod::{Object, Property, PropertyFlags};
    let mem_types = (1 << pw::spa::sys::SPA_DATA_MemPtr) | (1 << pw::spa::sys::SPA_DATA_MemFd);
    let prop = Property {
        key: pw::spa::sys::SPA_PARAM_BUFFERS_dataType,
        flags: PropertyFlags::empty(),
        value: Value::Int(mem_types),
    };
    let obj = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![prop],
    };
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("serialize buffers pod: {e}")))?
        .0
        .into_inner();
    Ok(bytes)
}

fn build_format_pod() -> Result<Vec<u8>> {
    use pw::spa::pod::{object, property};
    let obj = object! {
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(
            FormatProperties::VideoFormat,
            Choice, Enum, Id,
            VideoFormat::BGRA,
            VideoFormat::BGRA, VideoFormat::RGBA, VideoFormat::BGRx, VideoFormat::RGBx,
            VideoFormat::xRGB, VideoFormat::xBGR,
        ),
        property!(
            FormatProperties::VideoSize,
            Choice, Range, Rectangle,
            Rectangle { width: 1920, height: 1080 },
            Rectangle { width: 1, height: 1 },
            Rectangle { width: 8192, height: 8192 }
        ),
        property!(
            FormatProperties::VideoFramerate,
            Choice, Range, Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction { num: 240, denom: 1 }
        ),
    };
    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(obj))
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("serialize format pod: {e}")))?
        .0
        .into_inner();
    Ok(bytes)
}

/// Run the portal handshake on a fresh tokio runtime. Blocks the calling
/// thread until the user consents (or denies). On success, persists the
/// restore token so the next session is silent.
pub(crate) fn open_session_blocking() -> Result<SessionState> {
    let prev_token = load_token();
    log::info!(
        "screencast: opening portal session (restore_token: {})",
        if prev_token.is_some() { "yes" } else { "no" }
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("tokio runtime: {e}")))?;
    let result = runtime.block_on(open_session_async(prev_token))?;

    // Guard against empty strings so we don't blow away a previously
    // good token if xdph or ashpd hands back `Some("")` on a degenerate
    // path. `load_token()` already treats empty as None.
    if let Some(token) = result.restore_token.as_deref().filter(|t| !t.is_empty()) {
        if let Err(e) = save_token(token) {
            log::warn!("screencast: could not persist restore token: {e}");
        } else {
            log::info!(
                "screencast: persisted restore token at {}",
                token_path_display()
            );
        }
    }
    Ok(result)
}

async fn open_session_async(prev_token: Option<String>) -> Result<SessionState> {
    let proxy = Screencast::new().await.map_err(|e| PlatformError::Portal {
        reason: format!("create screencast proxy: {e}"),
    })?;

    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| PlatformError::Portal {
            reason: format!("create session: {e}"),
        })?;

    let mut select_opts = SelectSourcesOptions::default()
        .set_sources(BitFlags::from_flag(SourceType::Monitor))
        .set_multiple(true)
        // `Hidden` keeps the OS cursor out of the captured frame so edge
        // detection doesn't see the cursor as an edge. The compositor
        // still renders a cursor on top of our overlay surface.
        .set_cursor_mode(CursorMode::Hidden)
        .set_persist_mode(PersistMode::ExplicitlyRevoked);
    if let Some(t) = prev_token.as_deref() {
        select_opts = select_opts.set_restore_token(t);
    }
    proxy
        .select_sources(&session, select_opts)
        .await
        .map_err(|e| PlatformError::Portal {
            reason: format!("select_sources: {e}"),
        })?
        .response()
        .map_err(|e| PlatformError::Portal {
            reason: format!("select_sources response: {e}"),
        })?;

    let started = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| PlatformError::Portal {
            reason: format!("start: {e}"),
        })?
        .response()
        .map_err(|e| PlatformError::Portal {
            reason: format!("start response: {e}"),
        })?;

    let pipewire_fd = proxy
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(|e| PlatformError::Portal {
            reason: format!("open_pipe_wire_remote: {e}"),
        })?;

    let streams: Vec<StreamInfo> = started
        .streams()
        .iter()
        .map(|s: &PortalStream| StreamInfo {
            node_id: s.pipe_wire_node_id(),
            position: s.position(),
            size: s.size(),
            stream_id: s.id().map(String::from),
        })
        .collect();

    Ok(SessionState {
        streams,
        pipewire_fd,
        restore_token: started.restore_token().map(String::from),
    })
}

fn token_path() -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config_home.join("vernier").join("screencast.token"))
}

fn token_path_display() -> String {
    token_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<no $HOME>".into())
}

fn load_token() -> Option<String> {
    let path = token_path()?;
    let s = fs::read_to_string(&path).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn save_token(token: &str) -> io::Result<()> {
    let Some(path) = token_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, token)
}
