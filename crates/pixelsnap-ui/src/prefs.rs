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
    last_status: Option<String>,
}

impl PrefsApp {
    fn new(_cc: &CreationContext<'_>, on_saved: Box<dyn FnMut() + Send>) -> Self {
        let initial = Settings::load().unwrap_or_default();
        Self {
            section: Section::General,
            edited: initial.clone(),
            saved: initial,
            on_saved,
            last_status: None,
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
            .default_width(160.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.heading("macOS");
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);
                for section in SECTIONS {
                    let selected = self.section == *section;
                    if ui
                        .selectable_label(selected, section.label())
                        .clicked()
                    {
                        self.section = *section;
                    }
                }
            });

        egui::TopBottomPanel::bottom("prefs_actions").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if let Some(msg) = &self.last_status {
                    ui.label(msg);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let dirty = self.dirty();
                    let save = ui.add_enabled(dirty, egui::Button::new("Save"));
                    if save.clicked() {
                        self.save_now();
                    }
                    let revert = ui.add_enabled(dirty, egui::Button::new("Revert"));
                    if revert.clicked() {
                        self.revert_now();
                    }
                    if dirty {
                        ui.colored_label(
                            egui::Color32::from_rgb(220, 160, 50),
                            "● unsaved changes",
                        );
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(self.section.label());
            ui.add_space(8.0);
            egui::ScrollArea::vertical().show(ui, |ui| match self.section {
                Section::General => general_section(ui, &mut self.edited.general),
                Section::Screenshots => screenshots_section(ui, &mut self.edited.screenshots),
                Section::Tolerance => tolerance_section(ui, &mut self.edited.tolerance),
                Section::Appearance => appearance_section(ui, &mut self.edited.appearance),
                Section::Integrations => integrations_section(ui, &mut self.edited.integrations),
                Section::Shortcuts => shortcuts_section(ui, &mut self.edited.shortcuts),
                Section::About => about_section(ui),
            });
        });
    }
}

fn general_section(ui: &mut egui::Ui, s: &mut GeneralSettings) {
    ui.checkbox(&mut s.launch_at_login, "Launch at login");
    ui.label(small("Adds an autostart entry. Uncheck to remove it on save."));
    ui.add_space(8.0);

    ui.checkbox(&mut s.hide_tray_icon, "Hide tray icon");
    ui.label(small("The daemon keeps running; drive it with the global hotkey or `vernier toggle`."));
    ui.add_space(8.0);

    ui.checkbox(
        &mut s.session_persistence,
        "Save & restore last session",
    );
    ui.label(small("Persist held content across Esc-exit; Shift+R restores it."));
}

fn screenshots_section(ui: &mut egui::Ui, s: &mut ScreenshotSettings) {
    ui.label("Output directory");
    let mut dir_str = s
        .output_dir
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if ui.text_edit_singleline(&mut dir_str).changed() {
        s.output_dir = if dir_str.trim().is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(dir_str.trim()))
        };
    }
    ui.label(small(
        "Empty = $XDG_PICTURES_DIR (or ~/Pictures). Non-existent paths are created on capture.",
    ));
    ui.add_space(8.0);

    ui.label("Filename template");
    ui.text_edit_singleline(&mut s.filename_template);
    ui.label(small("Tokens: {ts} timestamp, {w} width, {h} height."));
    ui.add_space(8.0);

    ui.horizontal(|ui| {
        ui.label("Padding");
        let mut pad = s.padding_px as i32;
        if ui
            .add(egui::DragValue::new(&mut pad).range(0..=64).suffix(" px"))
            .changed()
        {
            s.padding_px = pad.max(0) as u32;
        }
    });
    ui.label(small("Pixels of transparent space added around the captured region."));
    ui.add_space(8.0);

    ui.checkbox(&mut s.retina_downscale, "Retina downscale");
    ui.label(small("Save the captured region at logical (point) pixels rather than the raw HiDPI buffer."));
    ui.add_space(8.0);

    ui.checkbox(&mut s.copy_to_clipboard, "Copy image to clipboard");
    ui.checkbox(&mut s.satty_edit_action, "Show \"Edit\" action in notification (opens in Satty)");
    ui.checkbox(&mut s.capture_sound, "Play shutter sound");
}

fn tolerance_section(ui: &mut egui::Ui, s: &mut ToleranceSettings) {
    ui.label("Default tolerance level applied each time the daemon enters measure mode. Live `+`/`-` keys still cycle within a session.");
    ui.add_space(8.0);
    for level in [
        ToleranceLevel::Zero,
        ToleranceLevel::Low,
        ToleranceLevel::Medium,
        ToleranceLevel::High,
    ] {
        ui.radio_value(
            &mut s.default_level,
            level,
            format!("{} ({})", level.label(), level.value()),
        );
    }
}

fn appearance_section(ui: &mut egui::Ui, s: &mut AppearanceSettings) {
    ui.label("Primary color");
    color_picker(ui, &mut s.primary_color);
    ui.label(small("Coral by default — matches macOS conventions."));
    ui.add_space(8.0);

    ui.label("Alternative color (toggled with `x`)");
    color_picker(ui, &mut s.alternative_color);
    ui.add_space(8.0);

    ui.label("Guide color");
    color_picker(ui, &mut s.guide_color);
    ui.add_space(12.0);

    ui.separator();
    ui.add_space(8.0);
    ui.label("Units");
    ui.radio_value(&mut s.units, Units::Px, "Pixels (px)");
    ui.radio_value(&mut s.units, Units::Pt, "Points (pt)");
    ui.add_space(12.0);

    ui.label("Coordinate rounding");
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
}

fn integrations_section(ui: &mut egui::Ui, s: &mut IntegrationSettings) {
    ui.label("Copy-dimensions clipboard format (used when you press Enter on a held rect)");
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
    ui.add_space(8.0);

    ui.label("External screenshot tool (run by the right-click menu)");
    ui.text_edit_singleline(&mut s.external_screenshot_command);
    ui.label(small("Spawned via the shell, with no arguments."));
}

fn shortcuts_section(ui: &mut egui::Ui, s: &mut ShortcutSettings) {
    ui.label(small(
        "Keyboard shortcuts. Restart the daemon (`vernier quit && vernier`) for changes to register globally.",
    ));
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Toggle measure mode:");
        ui.text_edit_singleline(&mut s.toggle);
    });
    ui.horizontal(|ui| {
        ui.label("Background mode:");
        ui.text_edit_singleline(&mut s.background_mode);
    });
    ui.horizontal(|ui| {
        ui.label("Restore session:");
        ui.text_edit_singleline(&mut s.restore_session);
    });
    ui.horizontal(|ui| {
        ui.label("Capture (copy dimensions):");
        ui.text_edit_singleline(&mut s.capture);
    });
}

fn about_section(ui: &mut egui::Ui) {
    ui.heading("macOS");
    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
    ui.add_space(8.0);
    ui.label("A cross-platform Rust port of macOS measurement tools targeting Hyprland on Omarchy.");
    ui.add_space(8.0);
    ui.hyperlink_to(
        "github.com/jondkinney/vernier",
        "https://github.com/jondkinney/vernier",
    );
    ui.add_space(8.0);
    ui.label(format!(
        "Settings file: {}",
        vernier_core::settings_path().display()
    ));
}

fn small(text: &str) -> egui::RichText {
    egui::RichText::new(text)
        .color(egui::Color32::from_gray(170))
        .size(11.0)
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
/// `on_saved` is invoked synchronously after each successful save —
/// the daemon-facing IPC ping lives in the caller so the UI crate
/// stays platform-agnostic.
pub fn run_prefs(on_saved: Box<dyn FnMut() + Send>) -> Result<()> {
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
        Box::new(move |cc| Ok(Box::new(PrefsApp::new(cc, on_saved)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
