//! Egui-based preferences window. Launched by `vernier prefs`
//! (called by the tray menu's "Preferences..." entry). Reads
//! settings on open, edits in-memory, persists on Save, and notifies
//! the daemon via the supplied callback so it can reload without
//! restart.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortcutId {
    Toggle,
    Background,
    Restore,
    Capture,
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
    /// Invoked when the user clicks "Restart vernier" on the
    /// Shortcuts pane. Caller stops the daemon and respawns a
    /// fresh one so re-registered hotkey bindings take effect.
    on_restart: Box<dyn FnMut() + Send>,
    last_status: Option<String>,
    logo: Option<egui::TextureHandle>,
    /// Receives the path the user picked from the folder dialog.
    /// `Some(rx)` while the dialog is open; cleared once the user
    /// either picks a folder or cancels.
    folder_pick: Option<Receiver<Option<PathBuf>>>,
    /// While `Some(id)`, the prefs window is in capture mode for
    /// the matching Shortcuts row — the next key press (with
    /// modifiers) is recorded as that shortcut's accelerator.
    capturing_shortcut: Option<ShortcutId>,
    /// Path of a config file that has a static `bind = …, exec,
    /// vernier toggle` line shadowing the prefs-managed
    /// shortcut. Surfaced as a banner on the Shortcuts pane so
    /// the user can clean it up.
    static_bind_warning: Option<PathBuf>,
}

impl PrefsApp {
    fn new(
        cc: &CreationContext<'_>,
        on_saved: Box<dyn FnMut() + Send>,
        on_quit: Box<dyn FnMut() + Send>,
        on_restart: Box<dyn FnMut() + Send>,
        static_bind_warning: Option<PathBuf>,
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
            on_restart,
            last_status: None,
            logo,
            folder_pick: None,
            capturing_shortcut: None,
            static_bind_warning,
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
        // While in shortcut-capture mode, drain key events from
        // egui's input queue (so other widgets don't act on them)
        // and apply the first non-modifier key as the new
        // shortcut. Esc cancels capture without changing the
        // value.
        if let Some(target) = self.capturing_shortcut {
            let captured = ctx.input_mut(|i| {
                let result = i.events.iter().find_map(|ev| match ev {
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => Some((*key, *modifiers)),
                    _ => None,
                });
                i.events.retain(|ev| !matches!(ev, egui::Event::Key { .. }));
                result
            });
            if let Some((key, modifiers)) = captured {
                let plain_modifiers = !(modifiers.shift
                    || modifiers.ctrl
                    || modifiers.alt
                    || modifiers.command
                    || modifiers.mac_cmd);
                if key == egui::Key::Escape && plain_modifiers {
                    self.capturing_shortcut = None;
                } else {
                    let s = format_accelerator(key, modifiers);
                    match target {
                        ShortcutId::Toggle => self.edited.shortcuts.toggle = s,
                        ShortcutId::Background => self.edited.shortcuts.background_mode = s,
                        ShortcutId::Restore => self.edited.shortcuts.restore_session = s,
                        ShortcutId::Capture => self.edited.shortcuts.capture = s,
                    }
                    self.capturing_shortcut = None;
                }
            }
        }

        // Pick up a folder-dialog result if one came in since last
        // frame. `try_recv` returns Ok(_) once exactly — either the
        // chosen path or `None` for "user canceled" — and we drop
        // the receiver in either case so the next click reopens
        // the dialog cleanly.
        if let Some(rx) = self.folder_pick.as_ref() {
            match rx.try_recv() {
                Ok(Some(path)) => {
                    self.edited.screenshots.output_dir = Some(path);
                    self.folder_pick = None;
                }
                Ok(None) => {
                    self.folder_pick = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.folder_pick = None;
                }
            }
        }

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
                let restart_clicked = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.section {
                        Section::General => {
                            general_section(ui, &mut self.edited.general);
                            false
                        }
                        Section::Screenshots => {
                            screenshots_section(
                                ui,
                                &mut self.edited.screenshots,
                                &mut self.folder_pick,
                            );
                            false
                        }
                        Section::Tolerance => {
                            tolerance_section(ui, &mut self.edited.tolerance);
                            false
                        }
                        Section::Appearance => {
                            appearance_section(ui, &mut self.edited.appearance);
                            false
                        }
                        Section::Integrations => {
                            integrations_section(ui, &mut self.edited.integrations);
                            false
                        }
                        Section::Shortcuts => shortcuts_section(
                            ui,
                            &mut self.edited.shortcuts,
                            &mut self.capturing_shortcut,
                            self.static_bind_warning.as_deref(),
                            self.on_restart.as_mut(),
                        ),
                        Section::About => {
                            about_section(ui, self.logo.as_ref());
                            false
                        }
                    })
                    .inner;
                if restart_clicked {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
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

fn screenshots_section(
    ui: &mut egui::Ui,
    s: &mut ScreenshotSettings,
    folder_pick: &mut Option<Receiver<Option<PathBuf>>>,
) {
    setting(ui, |ui| {
        field_label(ui, "Output directory");
        ui.horizontal(|ui| {
            let mut dir_str = s
                .output_dir
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut dir_str)
                    .margin(egui::Margin::symmetric(8, 6))
                    .desired_width(ui.available_width() - 96.0),
            );
            if resp.changed() {
                s.output_dir = if dir_str.trim().is_empty() {
                    None
                } else {
                    Some(PathBuf::from(dir_str.trim()))
                };
            }
            // Disabled while a previous picker is still open — rfd
            // doesn't gate concurrent invocations and the user will
            // get two stacked portal dialogs.
            let browse_enabled = folder_pick.is_none();
            if ui
                .add_enabled(browse_enabled, egui::Button::new("Browse…"))
                .clicked()
            {
                let starting = s.output_dir.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let mut dialog = rfd::FileDialog::new().set_title("Output directory");
                    if let Some(d) = starting.as_ref().filter(|p| p.exists()) {
                        dialog = dialog.set_directory(d);
                    }
                    let _ = tx.send(dialog.pick_folder());
                });
                *folder_pick = Some(rx);
            }
        });
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

/// Returns `true` if the Restart button was clicked (so the
/// caller can close the prefs window — the running daemon is on
/// its way out).
fn shortcuts_section(
    ui: &mut egui::Ui,
    s: &mut ShortcutSettings,
    capturing: &mut Option<ShortcutId>,
    static_bind_warning: Option<&std::path::Path>,
    on_restart: &mut dyn FnMut(),
) -> bool {
    if let Some(path) = static_bind_warning {
        egui::Frame::none()
            .fill(egui::Color32::from_rgb(60, 48, 16))
            .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(180, 140, 50)))
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new("⚠ Static bind detected")
                        .color(egui::Color32::from_rgb(255, 200, 90))
                        .size(13.5)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(format!(
                        "A line in {} runs `vernier toggle`. It fires regardless of \
                         what's set here — remove that line if you want only the \
                         shortcut configured below.",
                        path.display()
                    ))
                    .color(egui::Color32::from_gray(220))
                    .size(12.5),
                );
                ui.add_space(8.0);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("Open in editor").size(12.5),
                    ))
                    .clicked()
                {
                    open_in_editor(path);
                }
            });
        ui.add_space(12.0);
    }
    ui.label(caption(
        "Keyboard shortcuts. Restart the daemon for changes to take effect.",
    ));
    ui.add_space(16.0);
    shortcut_row(
        ui,
        "Toggle measure mode",
        &mut s.toggle,
        ShortcutId::Toggle,
        capturing,
    );
    shortcut_row(
        ui,
        "Background mode",
        &mut s.background_mode,
        ShortcutId::Background,
        capturing,
    );
    shortcut_row(
        ui,
        "Restore session",
        &mut s.restore_session,
        ShortcutId::Restore,
        capturing,
    );
    shortcut_row(
        ui,
        "Capture (copy dimensions)",
        &mut s.capture,
        ShortcutId::Capture,
        capturing,
    );
    ui.add_space(8.0);
    let clicked = ui
        .add(
            egui::Button::new(
                egui::RichText::new("Restart vernier")
                    .color(egui::Color32::from_rgb(120, 180, 255)),
            ),
        )
        .clicked();
    if clicked {
        on_restart();
    }
    clicked
}

fn shortcut_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    id: ShortcutId,
    capturing: &mut Option<ShortcutId>,
) {
    ui.horizontal(|ui| {
        // Manual paint for left-aligned label — `ui.add_sized` with
        // `Label` ends up right-justified inside the allocated rect.
        let label_w = 200.0;
        let resp = ui.allocate_response(egui::vec2(label_w, 28.0), egui::Sense::hover());
        ui.painter().text(
            resp.rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        );

        let is_capturing = *capturing == Some(id);
        let display = if is_capturing {
            "Press a shortcut…".to_string()
        } else if value.is_empty() {
            "Click to set".to_string()
        } else {
            value.clone()
        };
        let mut button = egui::Button::new(
            egui::RichText::new(display).monospace().size(13.0),
        );
        if is_capturing {
            button = button.fill(egui::Color32::from_rgb(50, 90, 140));
        } else if value.is_empty() {
            button = button.fill(egui::Color32::from_rgb(40, 40, 40));
        }
        if ui.add_sized([200.0, 28.0], button).clicked() {
            *capturing = Some(id);
        }

        ui.add_space(2.0);
        let clear_btn = egui::Button::new(
            egui::RichText::new("×")
                .size(16.0)
                .color(egui::Color32::from_gray(200)),
        );
        if ui.add_sized([28.0, 28.0], clear_btn).clicked() {
            value.clear();
            *capturing = Some(id);
        }
    });
    ui.add_space(12.0);
}

/// Open `path` in the user's default editor / file handler.
/// Tries `$VISUAL` / `$EDITOR` if set (spawned in a terminal we
/// can find), else falls back to `xdg-open` which delegates to
/// whatever GUI handler the desktop has registered for `.conf`.
fn open_in_editor(path: &std::path::Path) {
    use std::process::{Command, Stdio};
    let path_str = path.to_string_lossy().into_owned();
    // Prefer xdg-open — most Linux desktops have a sensible
    // default for plain-text/.conf files (typically a GUI editor).
    if Command::new("xdg-open")
        .arg(&path_str)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
    {
        log::info!("opened {} via xdg-open", path_str);
        return;
    }
    // Last-ditch: spit it to stderr so the user can grab it.
    log::warn!(
        "couldn't open editor for {}; xdg-open failed",
        path_str
    );
}

/// Render an egui key + modifier combo into the same
/// `SHIFT+CTRL+ALT+SUPER+KEY` text the platform Accelerator parser
/// already understands.
fn format_accelerator(key: egui::Key, modifiers: egui::Modifiers) -> String {
    let mut parts: Vec<&'static str> = Vec::new();
    if modifiers.shift {
        parts.push("SHIFT");
    }
    if modifiers.ctrl {
        parts.push("CTRL");
    }
    if modifiers.alt {
        parts.push("ALT");
    }
    if modifiers.command || modifiers.mac_cmd {
        parts.push("SUPER");
    }
    let key_str = match key {
        egui::Key::Space => "SPACE",
        egui::Key::Enter => "ENTER",
        egui::Key::Escape => "ESC",
        egui::Key::Tab => "TAB",
        egui::Key::Backspace => "BACKSPACE",
        egui::Key::Delete => "DELETE",
        egui::Key::ArrowUp => "UP",
        egui::Key::ArrowDown => "DOWN",
        egui::Key::ArrowLeft => "LEFT",
        egui::Key::ArrowRight => "RIGHT",
        _ => return finalize_with_key(parts, &key.name().to_uppercase()),
    };
    finalize_with_key(parts, key_str)
}

fn finalize_with_key(mut parts: Vec<&'static str>, key: &str) -> String {
    let owned: Vec<String> = parts.drain(..).map(|s| s.to_string()).collect();
    let mut out = owned.join("+");
    if !out.is_empty() {
        out.push('+');
    }
    out.push_str(key);
    out
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
    ui.add_space(22.0);
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

/// Optional initial size + screen position. Wayland doesn't let
/// the client position itself, so the window briefly appears at
/// the compositor's chosen spot before we dispatch
/// `hyprctl movewindowpixel` to it. Set by the caller from the
/// previous prefs window's geometry so the post-Restart prefs
/// reopens in the same place.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrefsGeometry {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub w: Option<u32>,
    pub h: Option<u32>,
}

/// Open the prefs window. Returns when the user closes it.
/// `on_saved` runs synchronously after each successful save (the
/// caller plugs in an IPC reload ping). `on_quit` runs when the
/// user clicks the "Quit vernier" button so the caller can send
/// the daemon-shutdown IPC. `on_restart` runs from the Shortcuts
/// pane's "Restart vernier" button so the caller can stop the
/// daemon and respawn it (so re-registered hotkey bindings take
/// effect).
pub fn run_prefs(
    on_saved: Box<dyn FnMut() + Send>,
    on_quit: Box<dyn FnMut() + Send>,
    on_restart: Box<dyn FnMut() + Send>,
    geometry: PrefsGeometry,
    static_bind_warning: Option<PathBuf>,
) -> Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("macOS Preferences")
        .with_app_id("vernier-prefs")
        .with_min_inner_size([520.0, 360.0]);
    let initial_w = geometry.w.unwrap_or(720) as f32;
    let initial_h = geometry.h.unwrap_or(520) as f32;
    viewport = viewport.with_inner_size([initial_w, initial_h]);
    let options = NativeOptions {
        viewport,
        ..Default::default()
    };
    if geometry.x.is_some() || geometry.y.is_some() {
        // Wayland clients can't set their own position, so once
        // the window's app_id is registered with Hyprland we ask
        // the compositor to slide it into place. Tiny delay so
        // the window is mapped first.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            if let (Some(x), Some(y)) = (geometry.x, geometry.y) {
                let _ = std::process::Command::new("hyprctl")
                    .args([
                        "dispatch",
                        "movewindowpixel",
                        &format!("exact {x} {y}, class:vernier-prefs"),
                    ])
                    .output();
            }
            if let (Some(w), Some(h)) = (geometry.w, geometry.h) {
                let _ = std::process::Command::new("hyprctl")
                    .args([
                        "dispatch",
                        "resizewindowpixel",
                        &format!("exact {w} {h}, class:vernier-prefs"),
                    ])
                    .output();
            }
        });
    }
    eframe::run_native(
        "macOS Preferences",
        options,
        Box::new(move |cc| {
            Ok(Box::new(PrefsApp::new(
                cc,
                on_saved,
                on_quit,
                on_restart,
                static_bind_warning,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
