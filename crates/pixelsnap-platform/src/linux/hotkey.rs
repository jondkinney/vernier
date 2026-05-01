//! Global shortcuts via the freedesktop `GlobalShortcuts` portal.
//!
//! Runs a dedicated tokio current-thread runtime on a `vernier-portals`
//! thread. Sync `register`/`unregister` calls cross into the runtime via an
//! mpsc channel + reply oneshot. Activation events flow back as
//! [`PlatformEvent::HotkeyPressed`].

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender};

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use futures_util::StreamExt;

use crate::{
    Accelerator, EventSender, HotkeyId, Key, Modifiers, PlatformError, PlatformEvent, Result,
};

pub(crate) struct HotkeyService {
    cmd_tx: Sender<HotkeyCmd>,
    next_id: AtomicU64,
    bindings: Mutex<Vec<(HotkeyId, String)>>,
}

impl HotkeyService {
    pub fn register(&self, accel: Accelerator, label: &str) -> Result<HotkeyId> {
        let id = HotkeyId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let name = format!("hk_{}", id.0);
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        self.cmd_tx
            .send(HotkeyCmd::Bind {
                name: name.clone(),
                label: label.to_string(),
                accel,
                reply: reply_tx,
            })
            .map_err(|_| PlatformError::Other(anyhow::anyhow!("portal thread gone")))?;
        reply_rx
            .recv()
            .map_err(|_| PlatformError::Other(anyhow::anyhow!("bind reply lost")))??;
        self.bindings.lock().unwrap().push((id, name));
        Ok(id)
    }

    pub fn unregister(&self, _id: HotkeyId) -> Result<()> {
        // The portal has no unbind-single primitive; full rebind on every
        // change is the documented pattern. Milestone 1 has at most one
        // shortcut for the toggle, so leaving it bound is fine.
        Ok(())
    }
}

#[derive(Debug)]
enum HotkeyCmd {
    Bind {
        name: String,
        label: String,
        accel: Accelerator,
        reply: SyncSender<Result<()>>,
    },
}

pub(crate) fn create(events_tx: EventSender) -> Result<HotkeyService> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<HotkeyCmd>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let ready_tx_for_thread = ready_tx.clone();

    std::thread::Builder::new()
        .name("vernier-portals".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = ready_tx_for_thread.send(Err(PlatformError::Other(
                        anyhow::anyhow!("tokio runtime: {e}"),
                    )));
                    return;
                }
            };
            runtime.block_on(async move {
                if let Err(e) =
                    run_portal_async(cmd_rx, events_tx, ready_tx_for_thread.clone()).await
                {
                    let _ = ready_tx_for_thread.send(Err(e));
                }
            });
        })
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("spawn portal thread: {e}")))?;

    ready_rx
        .recv()
        .map_err(|_| PlatformError::Other(anyhow::anyhow!("portal init failed")))??;

    Ok(HotkeyService {
        cmd_tx,
        next_id: AtomicU64::new(1),
        bindings: Mutex::new(Vec::new()),
    })
}

async fn run_portal_async(
    cmd_rx: Receiver<HotkeyCmd>,
    events_tx: EventSender,
    ready_tx: SyncSender<Result<()>>,
) -> Result<()> {
    let proxy = GlobalShortcuts::new().await.map_err(|e| PlatformError::Portal {
        reason: format!("create proxy: {e}"),
    })?;
    let session = proxy
        .create_session(Default::default())
        .await
        .map_err(|e| PlatformError::Portal {
            reason: format!("create session: {e}"),
        })?;

    let _ = ready_tx.send(Ok(()));

    let mut activated = proxy
        .receive_activated()
        .await
        .map_err(|e| PlatformError::Portal {
            reason: format!("activated stream: {e}"),
        })?;

    // Forward sync std::mpsc into tokio so we can select! on it.
    let (tcmd_tx, mut tcmd_rx) = tokio::sync::mpsc::unbounded_channel::<HotkeyCmd>();
    std::thread::spawn(move || {
        while let Ok(cmd) = cmd_rx.recv() {
            if tcmd_tx.send(cmd).is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            cmd = tcmd_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                match cmd {
                    HotkeyCmd::Bind { name, label, accel, reply } => {
                        let trigger = format_trigger(&accel);
                        log::info!("binding global shortcut '{name}' = '{trigger}' ({label})");
                        let ns = NewShortcut::new(name.as_str(), label.as_str())
                            .preferred_trigger(trigger.as_str());
                        let res = (async {
                            let request = proxy
                                .bind_shortcuts(
                                    &session,
                                    &[ns],
                                    None,
                                    BindShortcutsOptions::default(),
                                )
                                .await
                                .map_err(|e| PlatformError::Portal {
                                    reason: format!("bind {name}: {e}"),
                                })?;
                            request.response().map_err(|e| PlatformError::Portal {
                                reason: format!("bind {name} response: {e}"),
                            })?;
                            Ok::<(), PlatformError>(())
                        })
                        .await;
                        let _ = reply.send(res);
                    }
                }
            }
            ev = activated.next() => {
                let Some(activation) = ev else { break; };
                let shortcut_id = activation.shortcut_id().to_string();
                log::debug!("portal activation: id={shortcut_id}");
                if let Some(id) = parse_hotkey_id(&shortcut_id) {
                    let _ = events_tx.send(PlatformEvent::HotkeyPressed(id));
                }
            }
        }
    }
    Ok(())
}

fn parse_hotkey_id(name: &str) -> Option<HotkeyId> {
    name.strip_prefix("hk_")
        .and_then(|s| s.parse::<u64>().ok())
        .map(HotkeyId)
}

fn format_trigger(accel: &Accelerator) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if accel.modifiers.contains(Modifiers::CTRL) {
        parts.push("CTRL");
    }
    if accel.modifiers.contains(Modifiers::ALT) {
        parts.push("ALT");
    }
    if accel.modifiers.contains(Modifiers::SHIFT) {
        parts.push("SHIFT");
    }
    if accel.modifiers.contains(Modifiers::META) {
        parts.push("LOGO");
    }
    let key = key_to_str(accel.key);
    let mut out = parts.join("+");
    if !out.is_empty() {
        out.push('+');
    }
    out.push_str(&key);
    out
}

fn key_to_str(key: Key) -> String {
    match key {
        Key::Char(c) => c.to_ascii_uppercase().to_string(),
        Key::F(n) => format!("F{n}"),
        Key::Escape => "Escape".into(),
        Key::Enter => "Return".into(),
        Key::Space => "space".into(),
        Key::Tab => "Tab".into(),
        Key::Backspace => "BackSpace".into(),
        Key::Delete => "Delete".into(),
        Key::Up => "Up".into(),
        Key::Down => "Down".into(),
        Key::Left => "Left".into(),
        Key::Right => "Right".into(),
    }
}
