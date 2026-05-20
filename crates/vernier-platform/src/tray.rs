//! Tray icon backend (Linux). Uses the `ksni` crate to publish a
//! StatusNotifierItem directly so that left-click (Activate) and
//! right-click (ContextMenu) actually round-trip back to the
//! daemon — `tray-icon`'s libayatana-appindicator backend doesn't
//! expose Activate at all, which left waybar dropping every click
//! on the tray.

use std::sync::mpsc::{Receiver, Sender, SyncSender};

use crate::{
    EventSender, PlatformError, PlatformEvent, Result, TrayHandle, TrayMenu, TrayMenuItem, TrayOps,
};

pub(crate) fn create(menu: TrayMenu, events_tx: EventSender) -> Result<TrayHandle> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TrayCmd>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
    let initial = menu;

    std::thread::Builder::new()
        .name("vernier-tray-ksni".into())
        .spawn(move || {
            if let Err(e) = run_ksni_tray(initial, events_tx, cmd_rx, &ready_tx) {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("spawn tray thread: {e}")))?;

    ready_rx
        .recv()
        .map_err(|_| PlatformError::Other(anyhow::anyhow!("tray failed to come up")))??;

    Ok(TrayHandle::from_backend(TrayBackend { cmd_tx }))
}

enum TrayCmd {
    UpdateMenu(TrayMenu),
    SetActive(bool),
    Shutdown,
}

struct TrayBackend {
    cmd_tx: Sender<TrayCmd>,
}

impl TrayOps for TrayBackend {
    fn update_menu(&mut self, menu: TrayMenu) -> Result<()> {
        self.cmd_tx
            .send(TrayCmd::UpdateMenu(menu))
            .map_err(|_| PlatformError::Other(anyhow::anyhow!("tray gone")))
    }
    fn set_active(&mut self, active: bool) {
        let _ = self.cmd_tx.send(TrayCmd::SetActive(active));
    }
}

impl Drop for TrayBackend {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(TrayCmd::Shutdown);
    }
}

/// Snapshot of the menu items + tray identity that the ksni Tray
/// trait reads on every property/menu refresh.
struct VernierTray {
    title: String,
    items: Vec<TrayMenuItem>,
    active: bool,
    events_tx: EventSender,
}

impl ksni::Tray for VernierTray {
    fn id(&self) -> String {
        "vernier".to_string()
    }
    fn title(&self) -> String {
        self.title.clone()
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }
    fn status(&self) -> ksni::Status {
        if self.active {
            ksni::Status::Active
        } else {
            ksni::Status::Passive
        }
    }
    fn icon_name(&self) -> String {
        // Empty so SNI clients skip the icon-theme lookup and use
        // `icon_pixmap` below. Themed lookup is inconsistent across
        // clients (waybar with icon-size=12 picks the 16×16 PNG,
        // which librsvg pre-rasterized in a single color) — the
        // pixmap path is the only one that always renders the
        // symbolic SVG with the runtime-substituted foreground.
        String::new()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let rgba = crate::icon::render_tray_icon_rgba(64);
        vec![ksni::Icon {
            width: 64,
            height: 64,
            data: rgba_to_argb_premul(rgba),
        }]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title.clone(),
            description: String::new(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }
    fn activate(&mut self, x: i32, y: i32) {
        log::info!("tray Activate at ({x}, {y})");
        let _ = self
            .events_tx
            .send(PlatformEvent::TrayIconLeftClicked { x, y });
    }
    fn secondary_activate(&mut self, x: i32, y: i32) {
        log::info!("tray SecondaryActivate at ({x}, {y})");
        let _ = self
            .events_tx
            .send(PlatformEvent::TrayIconLeftClicked { x, y });
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        self.items.iter().filter_map(build_menu_item).collect()
    }
}

fn build_menu_item(item: &TrayMenuItem) -> Option<ksni::MenuItem<VernierTray>> {
    use ksni::menu::*;
    match item {
        TrayMenuItem::Action {
            id, label, enabled, ..
        } => {
            let id = id.clone();
            Some(
                StandardItem {
                    label: label.clone(),
                    enabled: *enabled,
                    activate: Box::new(move |this: &mut VernierTray| {
                        let _ = this
                            .events_tx
                            .send(PlatformEvent::TrayMenuActivated { id: id.clone() });
                    }),
                    ..Default::default()
                }
                .into(),
            )
        }
        TrayMenuItem::Toggle {
            id,
            label,
            enabled,
            checked,
        } => {
            let id = id.clone();
            Some(
                CheckmarkItem {
                    label: label.clone(),
                    enabled: *enabled,
                    checked: *checked,
                    activate: Box::new(move |this: &mut VernierTray| {
                        let _ = this
                            .events_tx
                            .send(PlatformEvent::TrayMenuActivated { id: id.clone() });
                    }),
                    ..Default::default()
                }
                .into(),
            )
        }
        TrayMenuItem::Separator => Some(MenuItem::Separator),
        TrayMenuItem::Submenu {
            id: _,
            label,
            items,
        } => {
            let children: Vec<ksni::MenuItem<VernierTray>> =
                items.iter().filter_map(build_menu_item).collect();
            Some(
                SubMenu {
                    label: label.clone(),
                    submenu: children,
                    ..Default::default()
                }
                .into(),
            )
        }
    }
}

/// Convert a non-premultiplied RGBA buffer into the ARGB32
/// premultiplied byte order ksni hands to dbus. Each pixel is
/// `[A, R, G, B]` with R/G/B premultiplied by alpha.
fn rgba_to_argb_premul(rgba: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for chunk in rgba.chunks_exact(4) {
        let r = chunk[0] as u32;
        let g = chunk[1] as u32;
        let b = chunk[2] as u32;
        let a = chunk[3] as u32;
        out.push(a as u8);
        out.push(((r * a + 127) / 255) as u8);
        out.push(((g * a + 127) / 255) as u8);
        out.push(((b * a + 127) / 255) as u8);
    }
    out
}

fn run_ksni_tray(
    initial: TrayMenu,
    events_tx: EventSender,
    cmd_rx: Receiver<TrayCmd>,
    ready_tx: &SyncSender<Result<()>>,
) -> Result<()> {
    use ksni::blocking::TrayMethods;
    let tray = VernierTray {
        title: initial.tooltip.clone(),
        items: initial.items.clone(),
        active: true,
        events_tx,
    };
    let handle = tray
        .spawn()
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("ksni spawn: {e}")))?;
    let _ = ready_tx.send(Ok(()));

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            TrayCmd::UpdateMenu(new_menu) => {
                handle.update(|tray: &mut VernierTray| {
                    tray.title = new_menu.tooltip.clone();
                    tray.items = new_menu.items.clone();
                });
            }
            TrayCmd::SetActive(active) => {
                handle.update(|tray: &mut VernierTray| {
                    tray.active = active;
                });
            }
            TrayCmd::Shutdown => {
                handle.shutdown();
                break;
            }
        }
    }
    Ok(())
}

// Icon rasterizers moved to `crate::icon` so they remain available
// on non-Linux platforms (this whole file is cfg-gated to Linux).
// SVG sources for the app icon (full color) and the tray icon
// (monochrome, `currentColor`-based, recolored to white at render
// time) live there as APP_ICON_SVG / TRAY_ICON_SVG, embedded at
// compile time so the binary stays self-contained.
