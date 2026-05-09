//! Egui-based preferences window. Launched by `vernier prefs`
//! (called by the tray menu's "Preferences..." entry). Reads
//! settings on open, edits in-memory, persists on Save, and notifies
//! the daemon via the supplied callback so it can reload without
//! restart.

use anyhow::Result;
use eframe::{egui, App, CreationContext, Frame, NativeOptions};
use vernier_core::{
    AppearanceSettings, ColorRgba, CopyFormat, GeneralSettings, IntegrationSettings,
    RoundingMode, ScreenshotSettings, Settings, ShortcutSettings, ToleranceLevel,
    ToleranceSettings, Units,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    General,
    Screenshots,
    Tolerance,
    Appearance,
    Integrations,
    Shortcuts,
    About,
}

impl Section {
    fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Screenshots => "Screenshots",
            Self::Tolerance => "Tolerance",
            Self::Appearance => "Appearance",
            Self::Integrations => "Integrations",
            Self::Shortcuts => "Shortcuts",
            Self::About => "About",
        }
    }
}

const SECTIONS: &[Section] = &[
    Section::General,
    Section::Screenshots,
    Section::Tolerance,
    Section::Appearance,
    Section::Integrations,
    Section::Shortcuts,
    Section::About,
];

struct PrefsApp {
    section: Section,
    /// Edited copy. Save commits to disk + invokes the callback.
    edited: Settings,
    /// Last saved snapshot — drives the "unsaved changes" indicator
    /// and reverts.
    saved: Settings,
    on_saved: Box<dyn FnMut() + Send>,
    /// Invoked when the user clicks "Quit vernier" — the caller
    /// is responsible for telling the running daemon to exit.
    on_quit: Box<dyn FnMut() + Send>,
    last_status: Option<String>,
    logo: Option<egui::TextureHandle>,
}

impl PrefsApp {
    fn new(
        cc: &CreationContext<'_>,
        on_saved: Box<dyn FnMut() + Send>,
        on_quit: Box<dyn FnMut() + Send>,
    ) -> Self {
        apply_style(&cc.egui_ctx);
        let logo = load_logo_texture(&cc.egui_ctx);
        let initial = Settings::load().unwrap_or_default();
        Self {
            section: Section::General,
            edited: initial.clone(),
            saved: initial,
            on_saved,
            on_quit,
            last_status: None,
            logo,
        }
    }

    fn dirty(&self) -> bool {
        self.edited != self.saved
    }

    fn save_now(&mut self) {
        match self.edited.save() {
            Ok(_) => {
                self.saved = self.edited.clone();
                self.last_status = Some("Saved.".to_string());
                (self.on_saved)();
            }
            Err(e) => {
                self.last_status = Some(format!("Save failed: {e:#}"));
            }
        }
    }

    fn revert_now(&mut self) {
        self.edited = self.saved.clone();
        self.last_status = Some("Reverted to last save.".to_string());
    }
}

impl App for PrefsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        egui::SidePanel::left("prefs_sidebar")
            .resizable(false)
            .default_width(200.0)
            .show(ctx, |ui| {
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    if let Some(logo) = &self.logo {
                        ui.add(
                            egui::Image::new(logo)
                                .fit_to_exact_size(egui::vec2(28.0, 28.0)),
                        );
                        ui.add_space(8.0);
                    }
                    ui.heading("macOS");
                });
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(8.0);
                for section in SECTIONS {
                    let selected = self.section == *section;
                    if sidebar_item(ui, selected, section.label()).clicked() {
                        self.section = *section;
                    }
                }
            });

        let mut quit_requested = false;
        egui::TopBottomPanel::bottom("prefs_actions")
            .min_height(54.0)
            .show(ctx, |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    let quit_label = egui::RichText::new("Quit vernier")
                        .color(egui::Color32::from_rgb(220, 90, 90));
                    if ui.add(egui::Button::new(quit_label)).clicked() {
                        quit_requested = true;
                    }
                    ui.add_space(12.0);
                    if let Some(msg) = &self.last_status {
                        ui.label(egui::RichText::new(msg).color(egui::Color32::from_gray(180)));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(4.0);
                        let dirty = self.dirty();
                        if ui.add_enabled(dirty, egui::Button::new("Save")).clicked() {
                            self.save_now();
                        }
                        ui.add_space(4.0);
                        if ui.add_enabled(dirty, egui::Button::new("Revert")).clicked() {
                            self.revert_now();
                        }
                        if dirty {
                            ui.add_space(8.0);
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 160, 50),
                                "● unsaved changes",
                            );
                        }
                    });
                });
                ui.add_space(10.0);
            });
        if quit_requested {
            (self.on_quit)();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::symmetric(20, 18)))
            .show(ctx, |ui| {
                if !matches!(self.section, Section::About) {
                    ui.heading(self.section.label());
                    ui.add_space(14.0);
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.section {
                    Section::General => general_section(ui, &mut self.edited.general),
                    Section::Screenshots => screenshots_section(ui, &mut self.edited.screenshots),
                    Section::Tolerance => tolerance_section(ui, &mut self.edited.tolerance),
                    Section::Appearance => appearance_section(ui, &mut self.edited.appearance),
                    Section::Integrations => integrations_section(ui, &mut self.edited.integrations),
                    Section::Shortcuts => shortcuts_section(ui, &mut self.edited.shortcuts),
                    Section::About => about_section(ui, self.logo.as_ref()),
                });
            });
    }
}

/// Apply the prefs window's font + spacing scale on init. Egui's
/// defaults are quite tight; bumping headings to 21 / body to 14 /
/// captions to 12 with consistent button + input padding lines up
/// with what most native settings panes use.
fn apply_style(ctx: &egui::Context) {
    use egui::FontFamily::Proportional;
    use egui::TextStyle::*;
    ctx.style_mut(|style| {
        style.text_styles = [
            (Heading, egui::FontId::new(21.0, Proportional)),
            (Body, egui::FontId::new(14.0, Proportional)),
            (Monospace, egui::FontId::new(13.0, egui::FontFamily::Monospace)),
            (Button, egui::FontId::new(14.0, Proportional)),
            (Small, egui::FontId::new(12.0, Proportional)),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.indent = 14.0;
        style.spacing.interact_size = egui::vec2(40.0, 28.0);
        style.spacing.icon_width = 18.0;
        style.spacing.icon_spacing = 6.0;
        style.visuals.widgets.inactive.expansion = 0.0;
    });
}

/// Left-aligned, full-width sidebar row. Egui's stock
/// `SelectableLabel` centers its text inside its rect; we want a
/// settings-pane look (`Section name      `), so paint it
/// ourselves with `Align2::LEFT_CENTER` over a clickable rect.
fn sidebar_item(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    let height = 32.0;
    let response = ui.allocate_response(
        egui::vec2(ui.available_width(), height),
        egui::Sense::click(),
    );
    let visuals = ui.style().interact_selectable(&response, selected);
    if selected || response.hovered() {
        ui.painter().rect_filled(
            response.rect.expand(-2.0),
            egui::CornerRadius::same(6),
            visuals.bg_fill,
        );
    }
    let text_pos = response.rect.left_center() + egui::vec2(12.0, 0.0);
    ui.painter().text(
        text_pos,
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(14.0),
        visuals.text_color(),
    );
    response
}

/// Render the procedural app icon at 256×256 and load it as an
/// egui texture so the About screen + sidebar can show it. Returns
/// `None` if `vernier-platform` somehow returns an empty buffer
/// (shouldn't happen).
fn load_logo_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let size = 256;
    let rgba = vernier_platform::render_app_icon_rgba(size);
    if rgba.len() != (size as usize) * (size as usize) * 4 {
        return None;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([size as usize, size as usize], &rgba);
    Some(ctx.load_texture("vernier_logo", image, egui::TextureOptions::LINEAR))
}

fn general_section(ui: &mut egui::Ui, s: &mut GeneralSettings) {
    setting(ui, |ui| {
        ui.checkbox(&mut s.launch_at_login, "Launch at login");
        ui.label(caption(
            "Adds an autostart entry. Uncheck to remove it on save.",
        ));
    });
    setting(ui, |ui| {
        ui.checkbox(&mut s.hide_tray_icon, "Hide tray icon");
        ui.label(caption(
            "The daemon keeps running; drive it with the global hotkey or `vernier toggle`.",
        ));
    });
    setting(ui, |ui| {
        ui.checkbox(&mut s.session_persistence, "Save & restore last session");
        ui.label(caption(
            "Persist held content across Esc-exit; Shift+R restores it.",
        ));
    });
}

fn screenshots_section(ui: &mut egui::Ui, s: &mut ScreenshotSettings) {
    setting(ui, |ui| {
        field_label(ui, "Output directory");
        let mut dir_str = s
            .output_dir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if padded_text_edit(ui, &mut dir_str).changed() {
            s.output_dir = if dir_str.trim().is_empty() {
                None
            } else {
                Some(std::path::PathBuf::from(dir_str.trim()))
            };
        }
        ui.label(caption(
            "Empty = $XDG_PICTURES_DIR (or ~/Pictures). Non-existent paths are created on capture.",
        ));
    });

    setting(ui, |ui| {
        field_label(ui, "Filename template");
        padded_text_edit(ui, &mut s.filename_template);
        ui.label(caption("Tokens: {ts} timestamp, {w} width, {h} height."));
    });

    setting(ui, |ui| {
        ui.horizontal(|ui| {
            field_label(ui, "Padding");
            let mut pad = s.padding_px as i32;
            if ui
                .add(
                    egui::DragValue::new(&mut pad)
                        .range(0..=64)
                        .suffix(" px"),
                )
                .changed()
            {
                s.padding_px = pad.max(0) as u32;
            }
        });
        ui.label(caption(
            "Pixels of transparent space added around the captured region.",
        ));
    });

    setting(ui, |ui| {
        ui.checkbox(&mut s.retina_downscale, "Retina downscale");
        ui.label(caption(
            "Save the captured region at logical (point) pixels rather than the raw HiDPI buffer.",
        ));
    });

    setting(ui, |ui| {
        ui.checkbox(&mut s.copy_to_clipboard, "Copy image to clipboard");
        ui.checkbox(
            &mut s.satty_edit_action,
            "Show \"Edit\" action in notification (opens in Satty)",
        );
        ui.checkbox(&mut s.capture_sound, "Play shutter sound");
    });
}

fn tolerance_section(ui: &mut egui::Ui, s: &mut ToleranceSettings) {
    ui.label(caption(
        "Default tolerance level applied each time the daemon enters measure mode. Live `+`/`-` keys still cycle within a session.",
    ));
    ui.add_space(10.0);
    setting(ui, |ui| {
        for level in [
            ToleranceLevel::Zero,
            ToleranceLevel::Low,
            ToleranceLevel::Medium,
            ToleranceLevel::High,
        ] {
            ui.radio_value(
                &mut s.default_level,
                level,
                format!("{}  ({})", level.label(), level.value()),
            );
        }
    });
}

fn appearance_section(ui: &mut egui::Ui, s: &mut AppearanceSettings) {
    setting(ui, |ui| {
        field_label(ui, "Primary color");
        color_picker(ui, &mut s.primary_color);
        ui.label(caption("Coral by default — matches macOS conventions."));
    });

    setting(ui, |ui| {
        field_label(ui, "Alternative color (toggled with `x`)");
        color_picker(ui, &mut s.alternative_color);
    });

    setting(ui, |ui| {
        field_label(ui, "Guide color");
        color_picker(ui, &mut s.guide_color);
    });

    ui.separator();
    ui.add_space(10.0);

    setting(ui, |ui| {
        field_label(ui, "Units");
        ui.radio_value(&mut s.units, Units::Px, "Pixels (px)");
        ui.radio_value(&mut s.units, Units::Pt, "Points (pt)");
    });

    setting(ui, |ui| {
        field_label(ui, "Coordinate rounding");
        ui.radio_value(
            &mut s.rounding_mode,
            RoundingMode::Points,
            "Points (logical, fractional)",
        );
        ui.radio_value(
            &mut s.rounding_mode,
            RoundingMode::PointsRounded,
            "Points (rounded to integer)",
        );
        ui.radio_value(
            &mut s.rounding_mode,
            RoundingMode::ScreenPixels,
            "Screen pixels (multiplied by display scale)",
        );
    });
}

fn integrations_section(ui: &mut egui::Ui, s: &mut IntegrationSettings) {
    setting(ui, |ui| {
        field_label(
            ui,
            "Copy-dimensions clipboard format (used when you press Enter on a held rect)",
        );
        for fmt in [
            CopyFormat::WidthCommaHeight,
            CopyFormat::HeightCommaWidth,
            CopyFormat::CssWidthFirst,
            CopyFormat::CssHeightFirst,
            CopyFormat::SassWidthFirst,
            CopyFormat::SassHeightFirst,
        ] {
            ui.radio_value(&mut s.copy_dimensions_format, fmt, fmt.label());
        }
    });

    setting(ui, |ui| {
        field_label(ui, "External screenshot tool (run by the right-click menu)");
        padded_text_edit(ui, &mut s.external_screenshot_command);
        ui.label(caption("Spawned via the shell, with no arguments."));
    });
}

fn shortcuts_section(ui: &mut egui::Ui, s: &mut ShortcutSettings) {
    ui.label(caption(
        "Keyboard shortcuts. Restart the daemon (`vernier quit && vernier`) for changes to register globally.",
    ));
    ui.add_space(12.0);
    shortcut_row(ui, "Toggle measure mode", &mut s.toggle);
    shortcut_row(ui, "Background mode", &mut s.background_mode);
    shortcut_row(ui, "Restore session", &mut s.restore_session);
    shortcut_row(ui, "Capture (copy dimensions)", &mut s.capture);
}

fn shortcut_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [200.0, 28.0],
            egui::Label::new(egui::RichText::new(label).size(14.0)),
        );
        ui.add_sized(
            [220.0, 28.0],
            egui::TextEdit::singleline(value).margin(egui::Margin::symmetric(8, 6)),
        );
    });
    ui.add_space(6.0);
}

fn about_section(ui: &mut egui::Ui, logo: Option<&egui::TextureHandle>) {
    ui.vertical_centered(|ui| {
        ui.add_space(28.0);
        if let Some(logo) = logo {
            ui.add(egui::Image::new(logo).fit_to_exact_size(egui::vec2(112.0, 112.0)));
        }
        ui.add_space(18.0);
        ui.label(egui::RichText::new("macOS").size(28.0).strong());
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("Version {}", env!("CARGO_PKG_VERSION")))
                .size(14.0)
                .color(egui::Color32::from_gray(170)),
        );
        ui.add_space(20.0);
        ui.label(
            egui::RichText::new(
                "A cross-platform Rust port of macOS measurement tools targeting Hyprland on Omarchy.",
            )
            .size(14.0),
        );
        ui.add_space(8.0);
        ui.hyperlink_to(
            "github.com/jondkinney/vernier",
            "https://github.com/jondkinney/vernier",
        );
        ui.add_space(20.0);
        ui.label(
            egui::RichText::new(format!(
                "Settings file: {}",
                vernier_core::settings_path().display()
            ))
            .color(egui::Color32::from_gray(150))
            .size(12.0),
        );
        ui.add_space(20.0);
    });
}

/// Wrap a logical settings group in a vertical block followed by
/// breathing room. Lets callers keep the per-setting code flat
/// while consistent spacing comes from one place.
fn setting<R>(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let r = ui.vertical(|ui| content(ui)).inner;
    ui.add_space(14.0);
    r
}

/// Bold-ish label introducing a setting. Slightly larger than the
/// caption text below the input.
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).strong().size(14.0));
}

/// Caption row — the muted explainer text under inputs / checkboxes.
fn caption(text: &str) -> egui::RichText {
    egui::RichText::new(text)
        .color(egui::Color32::from_gray(170))
        .size(12.0)
}

/// Single-line text input with consistent inner padding so the
/// fields don't collapse to ~16 px tall.
fn padded_text_edit(ui: &mut egui::Ui, text: &mut String) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .margin(egui::Margin::symmetric(8, 6))
            .desired_width(f32::INFINITY),
    )
}

fn color_picker(ui: &mut egui::Ui, c: &mut ColorRgba) {
    let mut color = egui::Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a);
    if egui::color_picker::color_edit_button_srgba(
        ui,
        &mut color,
        egui::color_picker::Alpha::OnlyBlend,
    )
    .changed()
    {
        c.r = color.r();
        c.g = color.g();
        c.b = color.b();
        c.a = color.a();
    }
    ui.label(format!(
        "#{:02X}{:02X}{:02X} (a={})",
        c.r, c.g, c.b, c.a
    ));
}

/// Open the prefs window. Returns when the user closes it.
/// `on_saved` runs synchronously after each successful save (the
/// caller plugs in an IPC reload ping). `on_quit` runs when the
/// user clicks the "Quit vernier" button so the caller can send
/// the daemon-shutdown IPC.
pub fn run_prefs(
    on_saved: Box<dyn FnMut() + Send>,
    on_quit: Box<dyn FnMut() + Send>,
) -> Result<()> {
    let options = NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("macOS Preferences")
            .with_inner_size([720.0, 520.0])
            .with_min_inner_size([520.0, 360.0]),
        ..Default::default()
    };
    eframe::run_native(
        "macOS Preferences",
        options,
        Box::new(move |cc| Ok(Box::new(PrefsApp::new(cc, on_saved, on_quit)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
