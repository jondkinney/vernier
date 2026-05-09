//! Small floating window used to surface the tray-icon's menu items
//! in compositors where waybar's xdg_popup-based menu doesn't render
//! reliably (Hyprland + waybar 0.15 hits libdbusmenu-gtk3 +
//! gtk_layer_shell quirks). The daemon spawns `vernier tray-menu`
//! on tray-icon left-click; clicking a row writes the matching IPC
//! command back through the supplied callback and the window
//! closes.

use anyhow::Result;
use eframe::{egui, App, CreationContext, Frame, NativeOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMenuChoice {
    ToggleOverlay,
    OpenPrefs,
    Quit,
}

struct TrayMenuApp {
    on_choice: Box<dyn FnMut(TrayMenuChoice) + Send>,
    armed_close: bool,
    /// Skip focus-loss auto-close until the window has held focus
    /// at least once — otherwise the menu dismisses itself on the
    /// first frame because focus may not have arrived yet.
    saw_focus: bool,
}

impl App for TrayMenuApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        // Esc closes immediately.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.armed_close = true;
        }
        // Focus-out closes (matches the "click outside dismisses"
        // convention every desktop tray menu uses).
        let focused = ctx.input(|i| i.viewport().focused == Some(true));
        if focused {
            self.saw_focus = true;
        } else if self.saw_focus {
            self.armed_close = true;
        }

        let mut clicked: Option<TrayMenuChoice> = None;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(egui::Color32::from_rgba_unmultiplied(22, 22, 22, 248))
                    .rounding(egui::Rounding::same(12))
                    .inner_margin(egui::Margin::symmetric(8, 8)),
            )
            .show(ctx, |ui| {
                ui.style_mut().spacing.item_spacing = egui::vec2(0.0, 2.0);
                ui.style_mut().visuals.widgets.active.bg_fill =
                    egui::Color32::from_rgb(48, 48, 48);
                ui.style_mut().visuals.widgets.hovered.bg_fill =
                    egui::Color32::from_rgb(48, 48, 48);
                ui.style_mut().visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
                ui.style_mut().visuals.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
                if menu_row(ui, "Toggle overlay", "Super+Ctrl+Shift+F").clicked() {
                    clicked = Some(TrayMenuChoice::ToggleOverlay);
                }
                if menu_row(ui, "Preferences…", "").clicked() {
                    clicked = Some(TrayMenuChoice::OpenPrefs);
                }
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                if menu_row(ui, "Quit vernier", "").clicked() {
                    clicked = Some(TrayMenuChoice::Quit);
                }
            });

        if let Some(choice) = clicked {
            (self.on_choice)(choice);
            self.armed_close = true;
        }

        if self.armed_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn menu_row(ui: &mut egui::Ui, label: &str, shortcut: &str) -> egui::Response {
    let row_h = 32.0;
    let resp = ui.allocate_response(
        egui::vec2(ui.available_width(), row_h),
        egui::Sense::click(),
    );
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(
            resp.rect,
            egui::Rounding::same(6),
            egui::Color32::from_rgb(48, 48, 48),
        );
    }
    let label_pos = resp.rect.left_center() + egui::vec2(12.0, 0.0);
    painter.text(
        label_pos,
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(13.0),
        egui::Color32::from_gray(230),
    );
    if !shortcut.is_empty() {
        let shortcut_pos = resp.rect.right_center() - egui::vec2(12.0, 0.0);
        painter.text(
            shortcut_pos,
            egui::Align2::RIGHT_CENTER,
            shortcut,
            egui::FontId::proportional(11.0),
            egui::Color32::from_gray(170),
        );
    }
    resp
}

/// Open the tray-menu popup. `on_choice` is invoked synchronously
/// on the click that selects a row, before the window closes. The
/// caller positions the resulting window via `hyprctl dispatch
/// movewindowpixel class:vernier-tray-menu`; the app_id we set
/// here is what Hyprland matches on.
pub fn run_tray_menu(on_choice: Box<dyn FnMut(TrayMenuChoice) + Send>) -> Result<()> {
    let options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("macOS")
            .with_app_id("vernier-tray-menu")
            .with_inner_size([260.0, 130.0])
            .with_min_inner_size([240.0, 120.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(false)
            .with_always_on_top(),
        ..Default::default()
    };
    eframe::run_native(
        "macOS Tray Menu",
        options,
        Box::new(move |_cc: &CreationContext<'_>| {
            Ok(Box::new(TrayMenuApp {
                on_choice,
                armed_close: false,
                saw_focus: false,
            }))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
