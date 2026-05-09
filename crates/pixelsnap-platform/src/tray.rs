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
struct macOSTray {
    title: String,
    items: Vec<TrayMenuItem>,
    active: bool,
    events_tx: EventSender,
}

impl ksni::Tray for macOSTray {
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
        "vernier".to_string()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let rgba = render_app_icon_rgba(64);
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
        let _ = self.events_tx.send(PlatformEvent::TrayIconLeftClicked { x, y });
    }
    fn secondary_activate(&mut self, x: i32, y: i32) {
        log::info!("tray SecondaryActivate at ({x}, {y})");
        let _ = self.events_tx.send(PlatformEvent::TrayIconLeftClicked { x, y });
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        self.items
            .iter()
            .filter_map(|item| build_menu_item(item))
            .collect()
    }
}

fn build_menu_item(item: &TrayMenuItem) -> Option<ksni::MenuItem<macOSTray>> {
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
                    activate: Box::new(move |this: &mut macOSTray| {
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
                    activate: Box::new(move |this: &mut macOSTray| {
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
        TrayMenuItem::Submenu { id: _, label, items } => {
            let children: Vec<ksni::MenuItem<macOSTray>> =
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
    let tray = macOSTray {
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
                handle.update(|tray: &mut macOSTray| {
                    tray.title = new_menu.tooltip.clone();
                    tray.items = new_menu.items.clone();
                });
            }
            TrayCmd::SetActive(active) => {
                handle.update(|tray: &mut macOSTray| {
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
            GradientStop::new(0.0, Color::from_rgba8(0x4C, 0xC9, 0xF0, 0xFF)),
            GradientStop::new(1.0, Color::from_rgba8(0x7B, 0x2C, 0xBF, 0xFF)),
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

    if let Some(r) = Rect::from_xywh(
        center - arm_thick * 0.5,
        cross_pad,
        arm_thick,
        s - 2.0 * cross_pad,
    ) {
        pixmap.fill_rect(r, &ink, Transform::identity(), None);
    }
    if let Some(r) = Rect::from_xywh(
        cross_pad,
        center - arm_thick * 0.5,
        s - 2.0 * cross_pad,
        arm_thick,
    ) {
        pixmap.fill_rect(r, &ink, Transform::identity(), None);
    }
    if let Some(r) = Rect::from_xywh(center - cap_extent * 0.5, cross_pad, cap_extent, cap_thick) {
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
    if let Some(r) = Rect::from_xywh(cross_pad, center - cap_extent * 0.5, cap_thick, cap_extent) {
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

    // tiny-skia hands us premultiplied RGBA; un-premultiply so the
    // PNG-on-disk path looks correct (image::RgbaImage assumes
    // straight alpha). The ksni adapter re-premultiplies into
    // ARGB before publishing.
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
