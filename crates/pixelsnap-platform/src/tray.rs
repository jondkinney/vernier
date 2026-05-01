//! Tray icon backend, shared across all platforms via the `tray-icon` crate.
//!
//! Linux: tray-icon uses libayatana-appindicator + GTK3 internally. GTK
//! requires init + main loop on whatever thread owns the icon, so a
//! dedicated `vernier-tray-gtk` thread runs gtk::main(). Two forwarder
//! threads pump menu activations and tray clicks into the platform event
//! channel.

use std::sync::mpsc::{Receiver, Sender, SyncSender};
use std::time::Duration;

use crate::{
    EventSender, PlatformError, PlatformEvent, Result, TrayHandle, TrayMenu, TrayMenuItem, TrayOps,
};

pub(crate) fn create(menu: TrayMenu, events_tx: EventSender) -> Result<TrayHandle> {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TrayCmd>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);

    // Forwarder: muda's global MenuEvent receiver -> PlatformEvent.
    let events_for_menu = events_tx.clone();
    std::thread::Builder::new()
        .name("vernier-tray-events".into())
        .spawn(move || {
            let receiver = tray_icon::menu::MenuEvent::receiver();
            for event in receiver {
                let id = event.id.0.clone();
                if events_for_menu
                    .send(PlatformEvent::TrayMenuActivated { id })
                    .is_err()
                {
                    break;
                }
            }
        })
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("spawn tray events thread: {e}")))?;

    // Forwarder: tray-icon's left-click events.
    let events_for_clicks = events_tx.clone();
    std::thread::Builder::new()
        .name("vernier-tray-clicks".into())
        .spawn(move || {
            let receiver = tray_icon::TrayIconEvent::receiver();
            for event in receiver {
                if let tray_icon::TrayIconEvent::Click {
                    button,
                    button_state,
                    ..
                } = event
                {
                    if button == tray_icon::MouseButton::Left
                        && button_state == tray_icon::MouseButtonState::Up
                    {
                        if events_for_clicks
                            .send(PlatformEvent::TrayIconLeftClicked)
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("spawn tray clicks thread: {e}")))?;

    // GTK thread: owns the TrayIcon, runs gtk::main() until shutdown.
    let ready_tx_for_thread = ready_tx.clone();
    std::thread::Builder::new()
        .name("vernier-tray-gtk".into())
        .spawn(move || {
            if let Err(e) = run_gtk_tray(menu, cmd_rx, &ready_tx_for_thread) {
                let _ = ready_tx_for_thread.send(Err(e));
            }
        })
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("spawn tray gtk thread: {e}")))?;

    ready_rx
        .recv()
        .map_err(|_| PlatformError::Other(anyhow::anyhow!("tray failed to come up")))??;

    Ok(TrayHandle::from_backend(TrayBackend { cmd_tx }))
}

#[derive(Debug)]
enum TrayCmd {
    UpdateMenu(TrayMenu),
    #[allow(dead_code)] // SetActive is wired up in a later milestone
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

fn run_gtk_tray(
    initial: TrayMenu,
    cmd_rx: Receiver<TrayCmd>,
    ready_tx: &SyncSender<Result<()>>,
) -> Result<()> {
    gtk::init().map_err(|e| PlatformError::Other(anyhow::anyhow!("gtk init: {e}")))?;

    let menu = build_menu(&initial)?;
    let icon = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(&initial.tooltip)
        .with_icon(make_placeholder_icon())
        .build()
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("tray build: {e}")))?;

    let _ = ready_tx.send(Ok(()));

    let mut icon_holder = Some(icon);
    gtk::glib::timeout_add_local(Duration::from_millis(50), move || {
        let cmds: Vec<_> = cmd_rx.try_iter().collect();
        for cmd in cmds {
            match cmd {
                TrayCmd::UpdateMenu(new_menu) => {
                    if let Some(ic) = icon_holder.as_ref() {
                        match build_menu(&new_menu) {
                            Ok(m) => {
                                ic.set_menu(Some(Box::new(m)));
                            }
                            Err(e) => log::error!("rebuild menu: {e}"),
                        }
                    }
                }
                TrayCmd::SetActive(_) => {}
                TrayCmd::Shutdown => {
                    icon_holder = None;
                    gtk::main_quit();
                    return gtk::glib::ControlFlow::Break;
                }
            }
        }
        gtk::glib::ControlFlow::Continue
    });

    gtk::main();
    Ok(())
}

fn build_menu(menu: &TrayMenu) -> Result<tray_icon::menu::Menu> {
    let m = tray_icon::menu::Menu::new();
    for item in &menu.items {
        append_to_menu(&m, item)?;
    }
    Ok(m)
}

fn append_to_menu(parent: &tray_icon::menu::Menu, item: &TrayMenuItem) -> Result<()> {
    use tray_icon::menu::*;
    match item {
        TrayMenuItem::Action {
            id, label, enabled, ..
        } => {
            let mi = MenuItem::with_id(id.clone(), label, *enabled, None);
            parent
                .append(&mi)
                .map_err(|e| PlatformError::Other(anyhow::anyhow!("append menu item: {e}")))?;
        }
        TrayMenuItem::Toggle {
            id,
            label,
            enabled,
            checked,
        } => {
            let mi = CheckMenuItem::with_id(id.clone(), label, *enabled, *checked, None);
            parent
                .append(&mi)
                .map_err(|e| PlatformError::Other(anyhow::anyhow!("append toggle: {e}")))?;
        }
        TrayMenuItem::Separator => {
            parent
                .append(&PredefinedMenuItem::separator())
                .map_err(|e| PlatformError::Other(anyhow::anyhow!("append separator: {e}")))?;
        }
        TrayMenuItem::Submenu { id, label, items } => {
            let sub = Submenu::with_id(id.clone(), label, true);
            for child in items {
                append_to_submenu(&sub, child)?;
            }
            parent
                .append(&sub)
                .map_err(|e| PlatformError::Other(anyhow::anyhow!("append submenu: {e}")))?;
        }
    }
    Ok(())
}

fn append_to_submenu(parent: &tray_icon::menu::Submenu, item: &TrayMenuItem) -> Result<()> {
    use tray_icon::menu::*;
    match item {
        TrayMenuItem::Action {
            id, label, enabled, ..
        } => {
            let mi = MenuItem::with_id(id.clone(), label, *enabled, None);
            parent
                .append(&mi)
                .map_err(|e| PlatformError::Other(anyhow::anyhow!("append submenu action: {e}")))?;
        }
        TrayMenuItem::Toggle {
            id,
            label,
            enabled,
            checked,
        } => {
            let mi = CheckMenuItem::with_id(id.clone(), label, *enabled, *checked, None);
            parent
                .append(&mi)
                .map_err(|e| PlatformError::Other(anyhow::anyhow!("append submenu toggle: {e}")))?;
        }
        TrayMenuItem::Separator => {
            parent
                .append(&PredefinedMenuItem::separator())
                .map_err(|e| {
                    PlatformError::Other(anyhow::anyhow!("append submenu separator: {e}"))
                })?;
        }
        TrayMenuItem::Submenu { id, label, items } => {
            let sub = Submenu::with_id(id.clone(), label, true);
            for child in items {
                append_to_submenu(&sub, child)?;
            }
            parent
                .append(&sub)
                .map_err(|e| PlatformError::Other(anyhow::anyhow!("append nested submenu: {e}")))?;
        }
    }
    Ok(())
}

fn make_placeholder_icon() -> tray_icon::Icon {
    let size: i32 = 32;
    let cx: i32 = size / 2;
    let cy: i32 = size / 2;
    let r_outer: i32 = (size / 2) - 1;
    let r_inner: i32 = (size / 2) - 6;
    let r_outer_sq = r_outer * r_outer;
    let r_inner_sq = r_inner * r_inner;

    let mut rgba: Vec<u8> = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = x - cx;
            let dy = y - cy;
            let r2 = dx * dx + dy * dy;
            let pixel: [u8; 4] = if r2 <= r_outer_sq && r2 > r_inner_sq {
                [0x00, 0x88, 0xFF, 0xFF]
            } else if r2 <= r_inner_sq {
                [0x00, 0x44, 0x88, 0xFF]
            } else {
                [0x00, 0x00, 0x00, 0x00]
            };
            rgba.extend_from_slice(&pixel);
        }
    }
    tray_icon::Icon::from_rgba(rgba, size as u32, size as u32)
        .expect("placeholder icon construction must succeed")
}
