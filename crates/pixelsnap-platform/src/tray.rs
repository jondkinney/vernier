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
        .with_tooltip(&initial.tooltip)
        .with_icon(make_app_icon())
        .build()
        .map_err(|e| PlatformError::Other(anyhow::anyhow!("tray build: {e}")))?;
    // Set the menu AFTER the icon is built — on the
    // libayatana-appindicator backend `with_menu` doesn't always
    // round-trip through the dbusmenu, so SNI hosts (waybar) see an
    // empty menu on right-click. Pushing it via set_menu on the
    // built icon makes the menu show up.
    icon.set_menu(Some(Box::new(menu)));

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

/// Render the app/tray icon procedurally. Inspired by macOS on
/// macOS — rounded square, gradient background, black cross with
/// T-shaped tick caps, small pill in the lower-right with measurement
/// dashes — but with a Linux-flavored teal-to-violet palette so it
/// reads as the Wayland port rather than a copy.
fn make_app_icon() -> tray_icon::Icon {
    let rgba = render_app_icon_rgba(64);
    tray_icon::Icon::from_rgba(rgba, 64, 64).expect("app icon construction must succeed")
}

/// Render the procedural app icon to a non-premultiplied RGBA8
/// buffer at `size × size` pixels. Same purple-gradient + cross +
/// T-caps + ticks the tray uses. Exposed so the daemon can also
/// drop a PNG on disk for the desktop / launcher entry to
/// reference.
pub fn render_app_icon_rgba(size: u32) -> Vec<u8> {
    use tiny_skia::*;
    let s = size as f32;
    let scale = s / 64.0;
    let mut pixmap = Pixmap::new(size, size).expect("alloc app icon pixmap");

    // --- Rounded-square background with a teal → violet gradient.
    let inset = 2.0 * scale;
    let radius = 12.0 * scale;
    let bg_path = {
        let mut pb = PathBuilder::new();
        let x0 = inset;
        let y0 = inset;
        let x1 = s - inset;
        let y1 = s - inset;
        pb.move_to(x0 + radius, y0);
        pb.line_to(x1 - radius, y0);
        pb.quad_to(x1, y0, x1, y0 + radius);
        pb.line_to(x1, y1 - radius);
        pb.quad_to(x1, y1, x1 - radius, y1);
        pb.line_to(x0 + radius, y1);
        pb.quad_to(x0, y1, x0, y1 - radius);
        pb.line_to(x0, y0 + radius);
        pb.quad_to(x0, y0, x0 + radius, y0);
        pb.close();
        pb.finish().expect("bg path")
    };
    let bg_shader = LinearGradient::new(
        Point::from_xy(inset, inset),
        Point::from_xy(s - inset, s - inset),
        vec![
            GradientStop::new(0.0, Color::from_rgba8(0x4C, 0xC9, 0xF0, 0xFF)), // teal
            GradientStop::new(1.0, Color::from_rgba8(0x7B, 0x2C, 0xBF, 0xFF)), // violet
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .expect("bg gradient");
    let mut bg_paint = Paint {
        shader: bg_shader,
        anti_alias: true,
        ..Default::default()
    };
    bg_paint.anti_alias = true;
    pixmap.fill_path(
        &bg_path,
        &bg_paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // --- Cross with T-shaped end caps.
    let mut ink = Paint::default();
    ink.set_color_rgba8(0x10, 0x10, 0x10, 0xEE);
    ink.anti_alias = true;
    let cross_pad = 11.0 * scale;
    let arm_thick = 5.0 * scale;
    let cap_thick = 4.0 * scale;
    let cap_extent = 16.0 * scale;
    let center = s * 0.5;

    // Vertical and horizontal arms.
    if let Some(r) =
        Rect::from_xywh(center - arm_thick * 0.5, cross_pad, arm_thick, s - 2.0 * cross_pad)
    {
        pixmap.fill_rect(r, &ink, Transform::identity(), None);
    }
    if let Some(r) =
        Rect::from_xywh(cross_pad, center - arm_thick * 0.5, s - 2.0 * cross_pad, arm_thick)
    {
        pixmap.fill_rect(r, &ink, Transform::identity(), None);
    }
    // Four T caps.
    if let Some(r) =
        Rect::from_xywh(center - cap_extent * 0.5, cross_pad, cap_extent, cap_thick)
    {
        pixmap.fill_rect(r, &ink, Transform::identity(), None);
    }
    if let Some(r) = Rect::from_xywh(
        center - cap_extent * 0.5,
        s - cross_pad - cap_thick,
        cap_extent,
        cap_thick,
    ) {
        pixmap.fill_rect(r, &ink, Transform::identity(), None);
    }
    if let Some(r) =
        Rect::from_xywh(cross_pad, center - cap_extent * 0.5, cap_thick, cap_extent)
    {
        pixmap.fill_rect(r, &ink, Transform::identity(), None);
    }
    if let Some(r) = Rect::from_xywh(
        s - cross_pad - cap_thick,
        center - cap_extent * 0.5,
        cap_thick,
        cap_extent,
    ) {
        pixmap.fill_rect(r, &ink, Transform::identity(), None);
    }

    // --- Pill in the lower-right with dashes.
    let pill_w = 18.0 * scale;
    let pill_h = 8.0 * scale;
    let pill_x = s - cross_pad - pill_w - 1.0 * scale;
    let pill_y = center + 6.0 * scale;
    let pill_path = {
        let mut pb = PathBuilder::new();
        let r = pill_h * 0.5;
        pb.move_to(pill_x + r, pill_y);
        pb.line_to(pill_x + pill_w - r, pill_y);
        pb.quad_to(pill_x + pill_w, pill_y, pill_x + pill_w, pill_y + r);
        pb.quad_to(
            pill_x + pill_w,
            pill_y + pill_h,
            pill_x + pill_w - r,
            pill_y + pill_h,
        );
        pb.line_to(pill_x + r, pill_y + pill_h);
        pb.quad_to(pill_x, pill_y + pill_h, pill_x, pill_y + r);
        pb.quad_to(pill_x, pill_y, pill_x + r, pill_y);
        pb.close();
        pb.finish().expect("pill path")
    };
    let mut pill_paint = Paint::default();
    pill_paint.set_color_rgba8(0x18, 0x18, 0x18, 0xFF);
    pill_paint.anti_alias = true;
    pixmap.fill_path(
        &pill_path,
        &pill_paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut tick = Paint::default();
    tick.set_color_rgba8(0xF0, 0xF0, 0xF0, 0xF0);
    let dash_w = 2.4 * scale;
    let dash_h = 2.4 * scale;
    let dash_y = pill_y + pill_h * 0.5 - dash_h * 0.5;
    let gap = 3.6 * scale;
    let span = 4.0 * dash_w + 3.0 * gap;
    let mut dash_x = pill_x + (pill_w - span) * 0.5;
    for _ in 0..4 {
        if let Some(r) = Rect::from_xywh(dash_x, dash_y, dash_w, dash_h) {
            pixmap.fill_rect(r, &tick, Transform::identity(), None);
        }
        dash_x += dash_w + gap;
    }

    // --- Convert tiny-skia premultiplied RGBA to non-premultiplied
    // RGBA. Most of the icon is fully opaque so this only matters
    // along the rounded corners' anti-aliased edges, but doing it
    // properly avoids a dark fringe.
    let mut rgba = pixmap.data().to_vec();
    for chunk in rgba.chunks_exact_mut(4) {
        let a = chunk[3];
        if a > 0 && a < 255 {
            let a32 = a as u32;
            chunk[0] = ((chunk[0] as u32 * 255 + a32 / 2) / a32).min(255) as u8;
            chunk[1] = ((chunk[1] as u32 * 255 + a32 / 2) / a32).min(255) as u8;
            chunk[2] = ((chunk[2] as u32 * 255 + a32 / 2) / a32).min(255) as u8;
        }
    }
    rgba
}
