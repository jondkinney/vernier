//! Egui-based preferences window. Launched by `vernier prefs`
//! (called by the tray menu's "Preferences..." entry). Reads
//! settings on open, edits in-memory, persists on Save, and notifies
//! the daemon via the supplied callback so it can reload without
//! restart.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

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
    ClearAndHide,
    Restore,
    Capture,
    Crosshair,
    GuideHorizontal,
    GuideVertical,
    ColorToggle,
    StuckHorizontal,
    StuckVertical,
    RefreshCapture,
    ToleranceUp,
    ToleranceDown,
    NudgeLeft,
    NudgeRight,
    NudgeUp,
    NudgeDown,
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
    /// Cached Satty app-icon texture used by the Screenshots pane's
    /// integration card. `None` if `/usr/share/icons/.../satty.svg`
    /// isn't installed or fails to rasterize.
    satty_icon: Option<egui::TextureHandle>,
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
    /// Probe state for the running daemon. Polled every ~750ms;
    /// the modal-dead overlay is drawn when this goes false (and
    /// after a 1s grace period from `prefs_started_at` so the
    /// auto-spawn from `run_prefs_window` has a chance to bind
    /// the IPC socket).
    daemon_alive: bool,
    last_daemon_probe: Instant,
    prefs_started_at: Instant,
}

impl PrefsApp {
    fn new(
        cc: &CreationContext<'_>,
        on_saved: Box<dyn FnMut() + Send>,
        on_quit: Box<dyn FnMut() + Send>,
        static_bind_warning: Option<PathBuf>,
    ) -> Self {
        apply_style(&cc.egui_ctx);
        let logo = load_logo_texture(&cc.egui_ctx);
        let satty_icon = load_satty_texture(&cc.egui_ctx);
        let initial = Settings::load().unwrap_or_default();
        let now = Instant::now();
        Self {
            section: Section::General,
            edited: initial.clone(),
            saved: initial,
            on_saved,
            on_quit,
            last_status: None,
            logo,
            satty_icon,
            folder_pick: None,
            capturing_shortcut: None,
            static_bind_warning,
            // Assume alive on startup — `run_prefs_window` either
            // confirmed the daemon was responsive or auto-spawned
            // one. The first probe (after the 1s grace) corrects
            // this if the spawn failed.
            daemon_alive: true,
            last_daemon_probe: now,
            prefs_started_at: now,
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
        // Daemon health probe — re-runs every 750ms so the modal
        // overlay below can react if the daemon dies (Quit by the
        // user, OOM, crash, etc.). Skipped during the first 1s of
        // prefs lifetime to avoid flashing the modal while the
        // auto-spawned daemon is still binding its IPC socket.
        let probe_grace = Duration::from_secs(1);
        let probe_interval = Duration::from_millis(750);
        if self.prefs_started_at.elapsed() > probe_grace
            && self.last_daemon_probe.elapsed() > probe_interval
        {
            self.daemon_alive = is_daemon_responsive();
            self.last_daemon_probe = Instant::now();
        }
        ctx.request_repaint_after(probe_interval);

        // While in shortcut-capture mode, drain key events from
        // egui's input queue (so other widgets don't act on them)
        // and apply the first non-modifier key as the new
        // shortcut. Esc cancels capture without changing the
        // value.
        if let Some(target) = self.capturing_shortcut {
            let outcome = ctx.input_mut(|i| capture_outcome(i, target));
            if let Some(outcome) = outcome {
                match outcome {
                    CaptureOutcome::Cancel => self.capturing_shortcut = None,
                    CaptureOutcome::Commit(s) => {
                        match target {
                            ShortcutId::Toggle => self.edited.shortcuts.toggle = s,
                            ShortcutId::ClearAndHide => {
                                self.edited.shortcuts.clear_and_hide = s
                            }
                            ShortcutId::Restore => self.edited.shortcuts.restore_session = s,
                            ShortcutId::Capture => self.edited.shortcuts.capture = s,
                            ShortcutId::Crosshair => {
                                self.edited.shortcuts.crosshair_mode = s
                            }
                            ShortcutId::GuideHorizontal => {
                                self.edited.shortcuts.guide_horizontal = s
                            }
                            ShortcutId::GuideVertical => {
                                self.edited.shortcuts.guide_vertical = s
                            }
                            ShortcutId::ColorToggle => {
                                self.edited.shortcuts.color_toggle = s
                            }
                            ShortcutId::StuckHorizontal => {
                                self.edited.shortcuts.stuck_horizontal = s
                            }
                            ShortcutId::StuckVertical => {
                                self.edited.shortcuts.stuck_vertical = s
                            }
                            ShortcutId::RefreshCapture => {
                                self.edited.shortcuts.refresh_capture = s
                            }
                            ShortcutId::ToleranceUp => {
                                self.edited.shortcuts.tolerance_up = s
                            }
                            ShortcutId::ToleranceDown => {
                                self.edited.shortcuts.tolerance_down = s
                            }
                            ShortcutId::NudgeLeft => self.edited.shortcuts.nudge_left = s,
                            ShortcutId::NudgeRight => self.edited.shortcuts.nudge_right = s,
                            ShortcutId::NudgeUp => self.edited.shortcuts.nudge_up = s,
                            ShortcutId::NudgeDown => self.edited.shortcuts.nudge_down = s,
                        }
                        self.capturing_shortcut = None;
                    }
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
                            // Painter-drawn dot — egui's bundled
                            // proportional font doesn't carry the
                            // U+25CF black circle character, so we
                            // were rendering tofu. A small filled
                            // circle from the painter avoids the
                            // font dependency entirely.
                            let dot_size = egui::vec2(8.0, 8.0);
                            let (rect, _) =
                                ui.allocate_exact_size(dot_size, egui::Sense::hover());
                            ui.painter().circle_filled(
                                rect.center(),
                                4.0,
                                egui::Color32::from_rgb(220, 160, 50),
                            );
                            ui.add_space(2.0);
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 160, 50),
                                "unsaved changes",
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
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.section {
                        Section::General => general_section(ui, &mut self.edited),
                        Section::Screenshots => screenshots_section(
                            ui,
                            &mut self.edited.screenshots,
                            &mut self.folder_pick,
                            self.satty_icon.as_ref(),
                        ),
                        Section::Tolerance => tolerance_section(ui, &mut self.edited.tolerance),
                        Section::Appearance => {
                            appearance_section(ui, &mut self.edited.appearance)
                        }
                        Section::Integrations => {
                            integrations_section(ui, &mut self.edited.integrations)
                        }
                        Section::Shortcuts => shortcuts_section(
                            ui,
                            &mut self.edited.shortcuts,
                            &mut self.capturing_shortcut,
                            self.static_bind_warning.as_deref(),
                        ),
                        Section::About => about_section(ui, self.logo.as_ref()),
                    });
            });

        if !self.daemon_alive && self.prefs_started_at.elapsed() > probe_grace {
            paint_daemon_dead_modal(ctx, &mut self.last_daemon_probe);
        }
    }
}

/// Probe the running daemon's IPC socket. A successful Unix-socket
/// connect proves a daemon is listening; a connection refusal (or
/// missing path) means it's not. Mirrors the daemon's own
/// `existing_daemon_responsive` so prefs and daemon agree.
fn is_daemon_responsive() -> bool {
    let path = daemon_socket_path();
    if !path.exists() {
        return false;
    }
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

fn daemon_socket_path() -> PathBuf {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    runtime_dir.join("vernier.sock")
}

/// Full-window dim + centered card shown when the prefs window
/// detects that the daemon has stopped responding. Click "Relaunch"
/// to spawn a fresh daemon (uses `current_exe` so we relaunch the
/// same binary we're currently running). Resets the probe timer
/// immediately so the modal dismisses quickly once the daemon
/// binds its socket.
fn paint_daemon_dead_modal(ctx: &egui::Context, last_probe: &mut Instant) {
    // Dim layer at `Middle` (above the central panel which lives
    // at `Background`) so it dims the prefs UI but stays UNDER
    // the modal card at `Foreground`. Putting both at the same
    // Order made egui's z-ordering nondeterministic and the dim
    // sometimes painted on top of the card, washing out the
    // text — explicit ordering avoids that.
    let dim = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Middle,
        egui::Id::new("vernier_daemon_dim"),
    ));
    dim.rect_filled(
        ctx.screen_rect(),
        egui::CornerRadius::ZERO,
        egui::Color32::from_black_alpha(160),
    );

    let mut relaunch_clicked = false;
    egui::Area::new(egui::Id::new("vernier_daemon_modal"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .interactable(true)
        .show(ctx, |ui| {
            egui::Frame::group(ui.style())
                .fill(egui::Color32::from_gray(34))
                .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(70)))
                .corner_radius(egui::CornerRadius::same(10))
                .inner_margin(egui::Margin::symmetric(24, 22))
                .show(ui, |ui| {
                    ui.set_max_width(360.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("macOS daemon stopped")
                                .size(16.0)
                                .strong(),
                        );
                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new(
                                "The background daemon isn't responding. \
                                 Shortcuts, the tray icon, and the toggle \
                                 hotkey will stay inactive until it's \
                                 running again.",
                            )
                            .color(egui::Color32::from_gray(200)),
                        );
                        ui.add_space(16.0);
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Relaunch daemon")
                                        .size(14.0)
                                        .color(egui::Color32::from_rgb(140, 200, 255)),
                                )
                                .min_size(egui::vec2(160.0, 32.0)),
                            )
                            .clicked()
                        {
                            relaunch_clicked = true;
                        }
                    });
                });
        });

    if relaunch_clicked {
        if let Ok(exe) = std::env::current_exe() {
            match std::process::Command::new(&exe).spawn() {
                Ok(c) => log::info!(
                    "daemon relaunched from prefs modal (pid {})",
                    c.id()
                ),
                Err(e) => log::warn!("relaunch from prefs modal failed: {e:#}"),
            }
        }
        // Force the next probe immediately so the modal dismisses
        // as soon as the new daemon binds the socket (~150-300ms).
        *last_probe = Instant::now() - Duration::from_secs(60);
    }
}

/// Apply the prefs window's font + spacing scale on init. Egui's
/// defaults are quite tight; bumping headings to 21 / body to 14 /
/// captions to 12 with consistent button + input padding lines up
/// with what most native settings panes use.
fn apply_style(ctx: &egui::Context) {
    use egui::FontFamily::Proportional;
    use egui::TextStyle::*;
    install_glyph_fonts(ctx);
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

static OMARCHY_FONT_AVAILABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn omarchy_font_available() -> bool {
    OMARCHY_FONT_AVAILABLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Custom font family (`"shortcut"`) used by the shortcut chips.
/// Bold JetBrains Mono Nerd Font for letters / digits / arrows
/// (covers ⇧ ⌥ ↵ ← → ↑ ↓ at thick weight), with the omarchy.ttf
/// font appended as fallback so SUPER renders as the U+E900 logo
/// the way waybar does.
fn install_glyph_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let mut shortcut_chain: Vec<String> = Vec::new();

    // Bold sans-serif for letters/digits in chips. Liberation Sans
    // is ~414KB; the JetBrains Mono Nerd Font we used to load is
    // 2.4MB and made egui's font init slow enough to trigger the
    // compositor's "Application Not Responding" ping. We don't need
    // the Nerd Font icons since every symbol on the chip is now
    // painter-drawn.
    let letter_paths = [
        "/usr/share/fonts/liberation/LiberationSans-Bold.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Bold.ttf",
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Bold.ttf",
    ];
    for path in letter_paths {
        match std::fs::read(path) {
            Ok(bytes) => {
                fonts.font_data.insert(
                    "shortcut_letters".into(),
                    std::sync::Arc::new(egui::FontData::from_owned(bytes)),
                );
                shortcut_chain.push("shortcut_letters".to_string());
                break;
            }
            Err(_) => continue,
        }
    }

    // Omarchy launcher glyph at U+E900.
    let omarchy_path = std::env::var_os("HOME").map(|h| {
        std::path::PathBuf::from(h).join(".local/share/fonts/omarchy.ttf")
    });
    if let Some(path) = omarchy_path {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let mut data = egui::FontData::from_owned(bytes);
                // Scale tweak: the omarchy glyph fills its full em
                // square; at scale=1 it'd tower over adjacent
                // letters. 0.85 lands the logo at roughly the same
                // visual height as a letter cap + the painted
                // shift outline. y_offset positive = nudge DOWN so
                // the logo sits on the same baseline as the F.
                data.tweak = egui::FontTweak {
                    scale: 0.85,
                    y_offset_factor: 0.10,
                    ..Default::default()
                };
                fonts
                    .font_data
                    .insert("omarchy".into(), std::sync::Arc::new(data));
                shortcut_chain.push("omarchy".to_string());
                OMARCHY_FONT_AVAILABLE
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                log::debug!("omarchy font not loaded ({}): {e}", path.display());
            }
        }
    }

    if !shortcut_chain.is_empty() {
        // Append the egui defaults at the end so nothing in the
        // shortcut chip falls back to tofu — the Nerd Font is
        // first, omarchy second, then whatever else the renderer
        // can find.
        if let Some(default_prop) = fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
        {
            shortcut_chain.extend(default_prop);
        }
        fonts
            .families
            .insert(egui::FontFamily::Name("shortcut".into()), shortcut_chain);
    }

    ctx.set_fonts(fonts);
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

/// Best-effort load of Satty's app icon from the standard
/// hicolor theme location. Renders the SVG at 128px square so
/// it stays sharp when the prefs window is HiDPI.
fn load_satty_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let path = "/usr/share/icons/hicolor/scalable/apps/satty.svg";
    let bytes = std::fs::read(path).ok()?;
    let size = 128u32;
    let rgba = vernier_platform::rasterize_svg(&bytes, size)?;
    if rgba.len() != (size as usize) * (size as usize) * 4 {
        return None;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([size as usize, size as usize], &rgba);
    Some(ctx.load_texture("satty_icon", image, egui::TextureOptions::LINEAR))
}

fn general_section(ui: &mut egui::Ui, settings: &mut Settings) {
    let s = &mut settings.general;
    setting(ui, |ui| {
        ui.checkbox(&mut s.launch_at_login, "Launch at login");
        ui.label(caption(
            "Adds an autostart entry. Uncheck to remove it on save.",
        ));
    });
    setting(ui, |ui| {
        let mut show_tray = !s.hide_tray_icon;
        if ui.checkbox(&mut show_tray, "Show tray icon").changed() {
            s.hide_tray_icon = !show_tray;
        }
        ui.label(caption(
            "Off keeps the daemon running but hides the tray menu. Drive it via the global hotkey or `vernier toggle`.",
        ));
    });
    ui.separator();
    ui.add_space(10.0);

    setting(ui, |ui| {
        ui.checkbox(&mut s.display_units, "Display units (px / pt)");
        ui.label(caption(
            "Append the unit suffix configured under Appearance to dimension pills. Off shows bare numbers.",
        ));
    });
    setting(ui, |ui| {
        ui.checkbox(&mut s.display_wh_indicators, "Display W/H indicators");
        ui.label(caption(
            "Prefix area pills with `W:` and `H:` labels (e.g. `W: 1024 \u{00D7} H: 768`).",
        ));
    });

    setting(ui, |ui| {
        field_label(ui, "Aspect ratio");
        ui.radio_value(
            &mut s.aspect_mode,
            vernier_core::AspectMode::Automatic,
            "Automatic (common ratio when close, otherwise reduced)",
        );
        ui.radio_value(
            &mut s.aspect_mode,
            vernier_core::AspectMode::CommonOnly,
            "Only common values (hide when nothing matches)",
        );
        ui.radio_value(
            &mut s.aspect_mode,
            vernier_core::AspectMode::Standard,
            "Always pick the closest common value",
        );
        ui.radio_value(
            &mut s.aspect_mode,
            vernier_core::AspectMode::Reduced,
            "Always show the reduced fraction",
        );
        ui.checkbox(&mut s.aspect_in_area_tool, "Enable in area tool");
    });

    setting(ui, |ui| {
        field_label(ui, "Distance tool");
        ui.checkbox(&mut s.snap_to_guides, "Snap to guides");
        ui.label(caption(
            "Edges and drag endpoints magnetize to the nearest reference guide within 8 logical pixels.",
        ));
    });

    setting(ui, |ui| {
        ui.checkbox(&mut s.freeze_screen, "Freeze screen");
        ui.label(caption(
            "On (default): the captured frame is locked when measure mode opens; press R to refresh manually. \
             Off: edge detection follows live screen content as the cursor moves.",
        ));
    });

    setting(ui, |ui| {
        ui.checkbox(&mut s.show_cursor, "Show cursor");
        ui.label(caption(
            "Show the white-outlined `+` marker over the cursor while measuring. \
             Off: the marker is hidden — the measurement guides themselves (axis lines, tick caps, W×H pill) keep rendering, and so do the move/resize cursors for guides and held-rect handles.",
        ));
    });
}

fn screenshots_section(
    ui: &mut egui::Ui,
    s: &mut ScreenshotSettings,
    folder_pick: &mut Option<Receiver<Option<PathBuf>>>,
    satty_icon: Option<&egui::TextureHandle>,
) {
    paint_satty_card(ui, &mut s.satty_integration, satty_icon);
    ui.add_space(18.0);

    // Always-active settings — these affect the image bytes (or the
    // local feedback) regardless of who handles the post-capture
    // workflow, so they apply both to vernier-managed saves and
    // to the satty-integration handoff.
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
        ui.checkbox(&mut s.capture_sound, "Play shutter sound");
        ui.label(caption(
            "Plays the system screen-capture sound when a screenshot fires.",
        ));
    });

    ui.separator();
    ui.add_space(8.0);

    // Vernier-managed post-capture flow: file save location,
    // template, clipboard copy, edit notification. Greyed out when
    // satty integration is on — satty owns these.
    let detail_enabled = !s.satty_integration;
    ui.add_enabled_ui(detail_enabled, |ui| {
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
        ui.checkbox(&mut s.copy_to_clipboard, "Copy image to clipboard");
        ui.checkbox(
            &mut s.satty_edit_action,
            "Show \"Edit\" action in notification (opens in Satty)",
        );
    });
    });
}

/// Top card on the Screenshots pane: Satty icon, heading,
/// description, Enable checkbox. When checked, the daemon hands
/// every screenshot off to satty (which handles save / clipboard
/// / share workflows itself), and the rest of the pane greys out.
fn paint_satty_card(
    ui: &mut egui::Ui,
    enabled: &mut bool,
    icon: Option<&egui::TextureHandle>,
) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_gray(34))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(60)))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(18, 16))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if let Some(tex) = icon {
                    ui.add(
                        egui::Image::new(tex).fit_to_exact_size(egui::vec2(72.0, 72.0)),
                    );
                    ui.add_space(14.0);
                } else {
                    // Placeholder square so the layout stays consistent
                    // even when satty isn't installed.
                    let (rect, _) = ui.allocate_exact_size(
                        egui::vec2(72.0, 72.0),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(
                        rect,
                        egui::CornerRadius::same(14),
                        egui::Color32::from_gray(50),
                    );
                    ui.add_space(14.0);
                }
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("Satty integration")
                            .size(16.0)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Hand the captured region straight to Satty for \
                             annotation, save, and share. While enabled, the \
                             options below are managed by Satty.",
                        )
                        .color(egui::Color32::from_gray(190))
                        .size(12.5),
                    );
                    ui.add_space(8.0);
                    ui.checkbox(enabled, "Enable");
                });
            });
        });
}

/// Custom slider tailored for the tolerance pane: rail with
/// purely-decorative tick marks (visual reference, no snap), a
/// circular knob that moves freely along the full range. Built
/// from `Painter` primitives + `allocate_exact_size` so the ticks
/// genuinely live on the rail (egui's `Slider` exposes no tick
/// API).
fn tick_slider(
    ui: &mut egui::Ui,
    value: &mut u32,
    range: std::ops::RangeInclusive<u32>,
    ticks: u32,
    width: f32,
) -> egui::Response {
    let height = 16.0;
    let (rect, mut response) = ui.allocate_exact_size(
        egui::vec2(width, height),
        egui::Sense::click_and_drag(),
    );

    let knob_radius = 5.5;
    let track_left = rect.left() + knob_radius;
    let track_right = rect.right() - knob_radius;
    let track_y = rect.center().y;
    let track_span = track_right - track_left;

    let range_min = *range.start() as f32;
    let range_max = *range.end() as f32;
    let range_span = (range_max - range_min).max(1.0);

    // Free-moving: position maps continuously to value, no snap.
    if response.dragged() || response.clicked() {
        if let Some(pos) = response.interact_pointer_pos() {
            let t = ((pos.x - track_left) / track_span).clamp(0.0, 1.0);
            let raw = range_min + t * range_span;
            let new_val = raw.round().clamp(range_min, range_max) as u32;
            if *value != new_val {
                *value = new_val;
                response.mark_changed();
            }
        }
    }

    let painter = ui.painter();
    let visuals = ui.visuals();

    let normalized = ((*value as f32 - range_min) / range_span).clamp(0.0, 1.0);
    let knob_x = track_left + normalized * track_span;

    // Rail: a thin filled bar with a darker fill to the left of
    // the knob so progress reads at a glance.
    let rail_color = egui::Color32::from_gray(60);
    let rail_thickness = 3.0;
    let rail_rect = egui::Rect::from_min_max(
        egui::pos2(track_left, track_y - rail_thickness * 0.5),
        egui::pos2(track_right, track_y + rail_thickness * 0.5),
    );
    painter.rect_filled(
        rail_rect,
        egui::CornerRadius::same(2),
        rail_color,
    );

    // Tick notches drawn ON the rail (vertical lines slightly
    // taller than the rail so they read as a visual ruler — no
    // snap, just reference).
    let notch_color = egui::Color32::from_gray(115);
    let half_notch = 4.0;
    let notch_stroke = egui::Stroke::new(1.0, notch_color);
    for i in 0..ticks {
        let t = if ticks > 1 {
            i as f32 / (ticks - 1) as f32
        } else {
            0.5
        };
        // +0.5 so 1px-wide strokes hit pixel centers cleanly.
        let x = (track_left + t * track_span).round() + 0.5;
        painter.line_segment(
            [
                egui::pos2(x, track_y - half_notch),
                egui::pos2(x, track_y + half_notch),
            ],
            notch_stroke,
        );
    }

    // Knob — uses the inactive/hovered widget visuals so it
    // matches the rest of the prefs UI's theming.
    let knob_visuals = if response.dragged() {
        visuals.widgets.active
    } else if response.hovered() {
        visuals.widgets.hovered
    } else {
        visuals.widgets.inactive
    };
    painter.circle(
        egui::pos2(knob_x, track_y),
        knob_radius,
        knob_visuals.bg_fill,
        knob_visuals.bg_stroke,
    );

    response
}

fn tolerance_section(ui: &mut egui::Ui, s: &mut ToleranceSettings) {
    ui.label(caption(
        "Numeric value (sum-of-channel difference, 0–255) for each tolerance level. \
         Live + / − cycles between levels in a session; the dropdown picks which one \
         is active each time measure mode opens.",
    ));
    ui.add_space(14.0);

    // Tick marks are decorative — 16 evenly-spaced stops along
    // 0..=255 give the slider a familiar "ruler" feel without
    // restricting where the knob can actually land.
    const TICK_COUNT: u32 = 16;
    let row = |ui: &mut egui::Ui, label: &str, value: &mut u32| {
        ui.horizontal(|ui| {
            // Fixed-width label column so all four sliders line up.
            let label_w = 90.0;
            let resp = ui.allocate_response(egui::vec2(label_w, 22.0), egui::Sense::hover());
            ui.painter().text(
                resp.rect.right_center(),
                egui::Align2::RIGHT_CENTER,
                label,
                egui::FontId::proportional(14.0),
                ui.visuals().text_color(),
            );
            ui.add_space(12.0);
            tick_slider(ui, value, 0..=255, TICK_COUNT, 320.0);
            ui.add_space(10.0);
            ui.label(
                egui::RichText::new(format!("{value}"))
                    .monospace()
                    .color(ui.visuals().weak_text_color()),
            );
        });
        ui.add_space(8.0);
    };

    row(ui, "Zero", &mut s.zero_value);
    row(ui, "Low", &mut s.low_value);
    row(ui, "Medium", &mut s.medium_value);
    row(ui, "High", &mut s.high_value);

    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label("Default tolerance:");
        egui::ComboBox::from_id_salt("default_tolerance_combo")
            .selected_text(s.default_level.label())
            .show_ui(ui, |ui| {
                for level in [
                    ToleranceLevel::Zero,
                    ToleranceLevel::Low,
                    ToleranceLevel::Medium,
                    ToleranceLevel::High,
                ] {
                    ui.selectable_value(&mut s.default_level, level, level.label());
                }
            });
        // Push the Restore Defaults button to the far right so it
        // doesn't crowd the dropdown.
        ui.with_layout(
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                if ui.button("Restore Defaults").clicked() {
                    *s = ToleranceSettings::default();
                }
            },
        );
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
    paint_figma_card(ui, s);
    ui.add_space(18.0);

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

/// Top card on the Integrations pane: heading, description, live
/// connection status, Enable toggle, and an "Install plugin in
/// Figma" button that copies the manifest path to the clipboard
/// and opens Figma in the browser. Figma has no deep link to its
/// "Import plugin from manifest" dialog, so the user still has to
/// click through `Plugins → Development → Import plugin from
/// manifest…` — the inline blurb spells out that path.
fn paint_figma_card(ui: &mut egui::Ui, s: &mut IntegrationSettings) {
    egui::Frame::group(ui.style())
        .fill(egui::Color32::from_gray(34))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(60)))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(18, 16))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(72.0, 72.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    rect,
                    egui::CornerRadius::same(14),
                    egui::Color32::from_gray(50),
                );
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "F",
                    egui::FontId::proportional(36.0),
                    egui::Color32::from_gray(170),
                );
                ui.add_space(14.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("Figma integration")
                            .size(16.0)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Reports the active Figma file's viewport zoom \
                             over a localhost WebSocket so on-screen \
                             measurements come back in canvas pixels rather \
                             than zoomed screen pixels. Requires a one-time \
                             plugin install per machine.",
                        )
                        .color(egui::Color32::from_gray(190))
                        .size(12.5),
                    );
                    ui.add_space(8.0);
                    ui.checkbox(&mut s.figma_zoom_correction, "Enable")
                        .on_hover_text(
                            "When off, the daemon ignores the plugin and \
                             measurements always reflect raw screen pixels.",
                        );
                    ui.add_space(8.0);

                    let connected = vernier_platform::figma_bridge::current_figma_zoom()
                        .is_some();
                    ui.horizontal(|ui| {
                        let (dot_rect, _) = ui.allocate_exact_size(
                            egui::vec2(10.0, 10.0),
                            egui::Sense::hover(),
                        );
                        let (color, label) = if connected {
                            (
                                egui::Color32::from_rgb(80, 200, 120),
                                "Plugin connected",
                            )
                        } else {
                            (
                                egui::Color32::from_gray(120),
                                "Plugin not connected",
                            )
                        };
                        ui.painter().circle_filled(dot_rect.center(), 5.0, color);
                        ui.label(
                            egui::RichText::new(label)
                                .color(egui::Color32::from_gray(200))
                                .size(12.5),
                        );
                    });
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "The button below copies the plugin manifest \
                             path to your clipboard and opens Figma. In \
                             Figma, open the main menu, then Plugins > \
                             Development > Import plugin from manifest..., \
                             and paste the path.",
                        )
                        .color(egui::Color32::from_gray(170))
                        .size(12.0),
                    );
                    ui.add_space(8.0);
                    let manifest = vernier_platform::figma_bridge::manifest_path();
                    ui.add_enabled_ui(manifest.is_some(), |ui| {
                        if ui.button("Install plugin in Figma…").clicked() {
                            if let Some(path) = manifest.as_ref() {
                                ui.ctx().copy_text(path.display().to_string());
                                open_figma_in_browser();
                                log::info!(
                                    "figma plugin: copied manifest path {} \
                                     and launched browser",
                                    path.display()
                                );
                            }
                        }
                    });
                    if manifest.is_none() {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Plugin files not found next to the binary. \
                                 Set $VERNIER_FIGMA_PLUGIN_DIR to the \
                                 directory containing manifest.json.",
                            )
                            .color(egui::Color32::from_rgb(220, 160, 90))
                            .size(11.5),
                        );
                    }
                });
            });
        });
}

/// Open the Figma web app in the user's default browser. We can't
/// deep-link into the "Import plugin from manifest" dialog (Figma
/// exposes no such URL), so we land the user on the recent-files
/// page and rely on the inline instructions in the card to take
/// them the rest of the way.
fn open_figma_in_browser() {
    use std::process::{Command, Stdio};
    let _ = Command::new("xdg-open")
        .arg("https://www.figma.com/files/recent")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn shortcuts_section(
    ui: &mut egui::Ui,
    s: &mut ShortcutSettings,
    capturing: &mut Option<ShortcutId>,
    static_bind_warning: Option<&std::path::Path>,
) {
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
    shortcut_row(
        ui,
        "Toggle measure mode",
        "Show or hide the measurement overlay. Global hotkey — fires \
         even when vernier doesn't have focus.",
        &mut s.toggle,
        ShortcutId::Toggle,
        capturing,
    );
    shortcut_row(
        ui,
        "Clear measurements & hide",
        "Wipe every held rect, guide, and stuck measurement and exit \
         measure mode. To just hide the overlay while keeping everything \
         intact, use the toggle measure mode hotkey instead — that \
         round-trips your session unchanged.",
        &mut s.clear_and_hide,
        ShortcutId::ClearAndHide,
        capturing,
    );
    // Double-press option + window — indented so it visually
    // attaches to the row above. 212px gutter matches the chip
    // column's left edge.
    ui.horizontal(|ui| {
        ui.add_space(212.0);
        ui.checkbox(
            &mut s.clear_and_hide_double_press,
            "Require double press",
        )
        .on_hover_text(
            "When checked, the first press shows a confirmation toast \
             and the action only fires on a second press within the \
             window below. Useful if your physical key for this shortcut \
             overlaps a modifier (e.g. Caps mapped to Ctrl + Esc) and \
             you'd otherwise wipe your session by accident.",
        );
    });
    ui.horizontal(|ui| {
        ui.add_space(212.0);
        ui.add_enabled_ui(s.clear_and_hide_double_press, |ui| {
            ui.label("Press within:");
            ui.add(
                egui::DragValue::new(&mut s.clear_and_hide_double_press_window_ms)
                    .speed(20.0)
                    .range(100..=3000)
                    .suffix(" ms"),
            )
            .on_hover_text(
                "Maximum gap (in milliseconds) between the first and \
                 second press. Clamped to 100–3000 ms.",
            );
        });
    });
    ui.add_space(8.0);
    shortcut_row(
        ui,
        "Restore session",
        "Reload the held rects, guides, and stuck measurements from \
         the last saved session.",
        &mut s.restore_session,
        ShortcutId::Restore,
        capturing,
    );
    shortcut_row(
        ui,
        "Capture (copy dimensions)",
        "Copy the W×H of the hovered held rect (or the only one if \
         there's just one) to the clipboard, formatted per the \
         Integrations pane.",
        &mut s.capture,
        ShortcutId::Capture,
        capturing,
    );
    shortcut_row(
        ui,
        "Crosshair mode",
        "While this modifier is held, the overlay extends the axis \
         lines to the screen edges and suppresses measurement \
         readouts — gives you a clean alignment crosshair to line \
         elements up against.",
        &mut s.crosshair_mode,
        ShortcutId::Crosshair,
        capturing,
    );
    shortcut_row(
        ui,
        "Place horizontal guide",
        "Arm a horizontal guide line — the next click commits it at \
         the cursor's y. Useful as a measurement anchor.",
        &mut s.guide_horizontal,
        ShortcutId::GuideHorizontal,
        capturing,
    );
    shortcut_row(
        ui,
        "Place vertical guide",
        "Arm a vertical guide line — the next click commits it at \
         the cursor's x.",
        &mut s.guide_vertical,
        ShortcutId::GuideVertical,
        capturing,
    );
    shortcut_row(
        ui,
        "Toggle HUD color",
        "Swap the overlay foreground between the primary color (coral \
         red by default) and the alternate (black). Helps when the UI \
         underneath clashes with one of the two.",
        &mut s.color_toggle,
        ShortcutId::ColorToggle,
        capturing,
    );
    shortcut_row(
        ui,
        "Stuck horizontal measurement",
        "Freeze the live crosshair's horizontal extent in place with \
         the pixel distance pinned to it. Stays visible until cleared.",
        &mut s.stuck_horizontal,
        ShortcutId::StuckHorizontal,
        capturing,
    );
    shortcut_row(
        ui,
        "Stuck vertical measurement",
        "Freeze the live crosshair's vertical extent in place with \
         the pixel distance pinned to it.",
        &mut s.stuck_vertical,
        ShortcutId::StuckVertical,
        capturing,
    );
    shortcut_row(
        ui,
        "Refresh capture",
        "Recapture the screen so subsequent edge detection uses the \
         current content (e.g. after the underlying app updates).",
        &mut s.refresh_capture,
        ShortcutId::RefreshCapture,
        capturing,
    );
    shortcut_row(
        ui,
        "Tolerance up",
        "Bump the edge-detection tolerance one level higher. More \
         tolerant snaps merge across small color differences.",
        &mut s.tolerance_up,
        ShortcutId::ToleranceUp,
        capturing,
    );
    shortcut_row(
        ui,
        "Tolerance down",
        "Bump the edge-detection tolerance one level lower. Stricter \
         snaps stop at smaller color differences.",
        &mut s.tolerance_down,
        ShortcutId::ToleranceDown,
        capturing,
    );
    shortcut_row(
        ui,
        "Nudge held rect left",
        "Move the hovered held rect 1 px left. Hold Shift for 10 px.",
        &mut s.nudge_left,
        ShortcutId::NudgeLeft,
        capturing,
    );
    shortcut_row(
        ui,
        "Nudge held rect right",
        "Move the hovered held rect 1 px right. Hold Shift for 10 px.",
        &mut s.nudge_right,
        ShortcutId::NudgeRight,
        capturing,
    );
    shortcut_row(
        ui,
        "Nudge held rect up",
        "Move the hovered held rect 1 px up. Hold Shift for 10 px.",
        &mut s.nudge_up,
        ShortcutId::NudgeUp,
        capturing,
    );
    shortcut_row(
        ui,
        "Nudge held rect down",
        "Move the hovered held rect 1 px down. Hold Shift for 10 px.",
        &mut s.nudge_down,
        ShortcutId::NudgeDown,
        capturing,
    );
    ui.add_space(4.0);
    ui.label(caption(
        "Nudge shortcuts: hold Shift to step 10 px instead of 1 px (built-in modifier).",
    ));
    ui.add_space(12.0);
    ui.with_layout(
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            if ui.button("Restore Defaults").clicked() {
                *s = ShortcutSettings::default();
                *capturing = None;
            }
        },
    );
}

enum CaptureOutcome {
    Cancel,
    Commit(String),
}

/// Drain the input state for a single shortcut-capture frame and
/// return what we got: a fresh accelerator string, an Esc-cancel,
/// or `None` (still waiting). For [`ShortcutId::Crosshair`] this
/// captures the first held modifier (it's a press-and-hold mode,
/// not a keypress); other shortcuts capture a normal Key event.
fn capture_outcome(i: &mut egui::InputState, target: ShortcutId) -> Option<CaptureOutcome> {
    // Esc with no modifiers normally cancels the capture, except
    // for the Clear-and-hide row whose default IS Esc — there we
    // need to treat Esc as the binding itself, otherwise the user
    // can never restore the default after clearing the field.
    // Cancel that row's capture by clicking elsewhere or another
    // shortcut button instead.
    let esc_is_cancel = !matches!(target, ShortcutId::ClearAndHide);
    if esc_is_cancel {
        let escaped = i.events.iter().any(|ev| {
            matches!(
                ev,
                egui::Event::Key {
                    key: egui::Key::Escape,
                    pressed: true,
                    modifiers,
                    ..
                } if !modifiers.shift && !modifiers.ctrl && !modifiers.alt
                    && !modifiers.command && !modifiers.mac_cmd
            )
        });
        if escaped {
            i.events.retain(|ev| !matches!(ev, egui::Event::Key { .. }));
            return Some(CaptureOutcome::Cancel);
        }
    }
    if matches!(target, ShortcutId::Crosshair) {
        // egui doesn't fire `Event::Key` for bare-modifier presses,
        // so we read the live `modifiers` snapshot. Any modifier
        // currently held becomes the binding.
        let m = i.modifiers;
        let token = if m.shift {
            Some("SHIFT")
        } else if m.ctrl {
            Some("CTRL")
        } else if m.alt {
            Some("ALT")
        } else if m.command || m.mac_cmd {
            Some("SUPER")
        } else {
            None
        };
        if let Some(t) = token {
            // Drain pending key events so they don't fire elsewhere.
            i.events.retain(|ev| !matches!(ev, egui::Event::Key { .. }));
            return Some(CaptureOutcome::Commit(t.to_string()));
        }
        return None;
    }
    // Normal shortcuts: take the first non-Esc Key press.
    let result = i.events.iter().find_map(|ev| match ev {
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => Some(format_accelerator(*key, *modifiers)),
        _ => None,
    });
    i.events.retain(|ev| !matches!(ev, egui::Event::Key { .. }));
    result.map(CaptureOutcome::Commit)
}

/// One segment of a rendered shortcut chip. Letters/digits and
/// the omarchy SUPER glyph go through font rendering; everything
/// else is drawn manually so we can guarantee uniform stroke
/// weight, baseline alignment, and the solid stubby Shift / chevron
/// Ctrl / matching arrows the user wants.
#[derive(Clone, Debug)]
enum ChipSeg {
    Letter(String),
    OmarchyLogo,
    Shift,
    Ctrl,
    Alt,
    Enter,
    Arrow(ArrowDir),
    Plus,
    Minus,
    Equal,
    Underscore,
}

#[derive(Clone, Copy, Debug)]
enum ArrowDir {
    Up,
    Down,
    Left,
    Right,
}

fn shortcut_chip_segments(stored: &str) -> Vec<ChipSeg> {
    let omarchy = omarchy_font_available();
    stored
        .split('+')
        .filter(|t| !t.is_empty())
        .map(|tok| match tok {
            "SHIFT" => ChipSeg::Shift,
            "CTRL" => ChipSeg::Ctrl,
            "ALT" => ChipSeg::Alt,
            "SUPER" => {
                if omarchy {
                    ChipSeg::OmarchyLogo
                } else {
                    ChipSeg::Letter("SUPER".to_string())
                }
            }
            "ENTER" | "RETURN" => ChipSeg::Enter,
            "LEFT" => ChipSeg::Arrow(ArrowDir::Left),
            "RIGHT" => ChipSeg::Arrow(ArrowDir::Right),
            "UP" => ChipSeg::Arrow(ArrowDir::Up),
            "DOWN" => ChipSeg::Arrow(ArrowDir::Down),
            "PLUS" => ChipSeg::Plus,
            "MINUS" => ChipSeg::Minus,
            "EQUAL" => ChipSeg::Equal,
            "UNDERSCORE" => ChipSeg::Underscore,
            other => ChipSeg::Letter(other.to_string()),
        })
        .collect()
}

const CHIP_GLYPH_SIZE: f32 = 14.0; // square box (px) each painter glyph fits in
const CHIP_LETTER_PT: f32 = 15.0;  // letters / SUPER font size — sized to match omarchy cap height
const CHIP_GAP: f32 = 6.0;         // gap between segments

fn segment_advance(seg: &ChipSeg, ctx: &egui::Context) -> f32 {
    match seg {
        ChipSeg::Letter(s) => measure_chip_text(ctx, s, CHIP_LETTER_PT),
        ChipSeg::OmarchyLogo => measure_chip_text(ctx, "\u{e900}", CHIP_LETTER_PT),
        // Painter glyphs: most fit in a square, plus/equal/minus/underscore
        // get a slightly wider box so the bars look proportional.
        ChipSeg::Shift
        | ChipSeg::Ctrl
        | ChipSeg::Alt
        | ChipSeg::Enter
        | ChipSeg::Arrow(_) => CHIP_GLYPH_SIZE,
        ChipSeg::Plus | ChipSeg::Minus | ChipSeg::Equal | ChipSeg::Underscore => {
            CHIP_GLYPH_SIZE
        }
    }
}

fn measure_chip_text(ctx: &egui::Context, text: &str, size: f32) -> f32 {
    let family = egui::FontFamily::Name("shortcut".into());
    ctx.fonts(|f| {
        let font_id = egui::FontId::new(size, family);
        text.chars()
            .map(|c| f.glyph_width(&font_id, c))
            .sum::<f32>()
    })
}

/// Paint a single shortcut chip into `chip_rect`. Background is
/// drawn first, then segments are laid out horizontally and
/// rendered glyph-by-glyph: letters via the bold "shortcut" font,
/// SUPER via omarchy.ttf, everything else via stroke + fill paths
/// so weight and baseline match across all symbols.
fn paint_shortcut_chip(
    ui: &mut egui::Ui,
    chip_rect: egui::Rect,
    bg: egui::Color32,
    fg: egui::Color32,
    segments: &[ChipSeg],
) {
    let painter = ui.painter().with_clip_rect(chip_rect);
    painter.rect_filled(chip_rect, egui::CornerRadius::same(4), bg);

    if segments.is_empty() {
        return;
    }

    let ctx = ui.ctx().clone();
    let widths: Vec<f32> = segments.iter().map(|s| segment_advance(s, &ctx)).collect();
    let total: f32 = widths.iter().sum::<f32>() + CHIP_GAP * (segments.len() as f32 - 1.0);
    let mut cursor_x = chip_rect.center().x - total / 2.0;
    let cy = chip_rect.center().y;

    let letter_font = egui::FontId::new(CHIP_LETTER_PT, egui::FontFamily::Name("shortcut".into()));

    for (seg, w) in segments.iter().zip(widths.iter()) {
        let glyph_rect = egui::Rect::from_center_size(
            egui::pos2(cursor_x + w / 2.0, cy),
            egui::vec2(*w, CHIP_GLYPH_SIZE),
        );
        match seg {
            ChipSeg::Letter(s) => {
                painter.text(
                    glyph_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    s,
                    letter_font.clone(),
                    fg,
                );
            }
            ChipSeg::OmarchyLogo => {
                painter.text(
                    glyph_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "\u{e900}",
                    letter_font.clone(),
                    fg,
                );
            }
            ChipSeg::Shift => paint_shift(&painter, glyph_rect, fg),
            ChipSeg::Ctrl => paint_caret(&painter, glyph_rect, fg),
            ChipSeg::Alt => paint_alt(&painter, glyph_rect, fg),
            ChipSeg::Enter => paint_enter(&painter, glyph_rect, fg),
            ChipSeg::Arrow(dir) => paint_arrow(&painter, glyph_rect, fg, *dir),
            ChipSeg::Plus => paint_plus(&painter, glyph_rect, fg),
            ChipSeg::Minus => paint_minus(&painter, glyph_rect, fg),
            ChipSeg::Equal => paint_equal(&painter, glyph_rect, fg),
            ChipSeg::Underscore => paint_underscore(&painter, glyph_rect, fg),
        }
        cursor_x += w + CHIP_GAP;
    }
}

/// Hollow stubby Shift glyph: two
/// strokes form a closed pentagon — triangular cap on top, narrower
/// rectangular stem below. Narrower than letter width so it doesn't
/// look "fat" next to F/V/etc.
fn paint_shift(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let w = rect.width();
    let h = rect.height();
    let cx = rect.center().x;
    let top = rect.top() + h * 0.15;
    let mid = rect.top() + h * 0.55;
    let bot = rect.top() + h * 0.85;
    let stem_half = w * 0.18;
    let cap_half = w * 0.36;
    let pts = vec![
        egui::pos2(cx, top),
        egui::pos2(cx + cap_half, mid),
        egui::pos2(cx + stem_half, mid),
        egui::pos2(cx + stem_half, bot),
        egui::pos2(cx - stem_half, bot),
        egui::pos2(cx - stem_half, mid),
        egui::pos2(cx - cap_half, mid),
    ];
    let shape = egui::epaint::PathShape::closed_line(pts, egui::Stroke::new(1.8, color));
    painter.add(egui::Shape::Path(shape));
}

/// Bold chevron — the macOS Ctrl/control symbol. Apex sits at
/// roughly the letter cap-top so it reads as a superscript caret
/// without floating off the top of the chip.
fn paint_caret(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let w = rect.width();
    let h = rect.height();
    let cx = rect.center().x;
    let apex_y = rect.top() + h * 0.22;
    let foot_y = rect.top() + h * 0.55;
    let stroke = egui::Stroke::new(2.4, color);
    painter.line_segment(
        [
            egui::pos2(cx - w * 0.40, foot_y),
            egui::pos2(cx, apex_y),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(cx, apex_y),
            egui::pos2(cx + w * 0.40, foot_y),
        ],
        stroke,
    );
}

/// Approximation of the macOS Option (⌥) glyph: a top horizontal
/// stroke on the right with a step down, plus a separate bottom
/// horizontal on the left.
fn paint_alt(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let w = rect.width();
    let h = rect.height();
    let stroke = egui::Stroke::new(2.0, color);
    // Proportions derived from rasterizing the canonical Mac ⌥ glyph
    // and measuring pixel positions in the 60×52 trimmed glyph:
    // top-LEFT bar cols 0-22 (x=0%–37%), detached upper-RIGHT cols
    // 34-59 (x=57%–99%), bottom-RIGHT bar cols 33-59 (x=55%–99%).
    // Visual corners (where bars meet the diagonal): top at 37%,
    // bottom at 55%. Vertical extent matches paint_shift (15%–85%).
    let top_y = rect.top() + h * 0.15;
    let bot_y = rect.top() + h * 0.85;
    let left = rect.left();
    // The glyph is intrinsically right-heavy (bottom-right bar +
    // detached tick + diagonal-end-on-right vs. only top-left bar
    // on the left), so a uniform 5% inset would put its visual
    // mass center near 54% — reading as "more space on the left"
    // next to the centered Ctrl/Shift chips. Shifting all anchors
    // ~3pp left aligns the mass center with the chip center.
    let corner_top_x = left + w * 0.35;
    let corner_bot_x = left + w * 0.52;
    // Top-LEFT horizontal.
    painter.line_segment(
        [egui::pos2(left + w * 0.02, top_y), egui::pos2(corner_top_x, top_y)],
        stroke,
    );
    // Diagonal connector: bar-corner top to bar-corner bottom.
    painter.line_segment(
        [egui::pos2(corner_top_x, top_y), egui::pos2(corner_bot_x, bot_y)],
        stroke,
    );
    // Bottom-RIGHT horizontal — starts where the diagonal lands.
    painter.line_segment(
        [egui::pos2(corner_bot_x, bot_y), egui::pos2(left + w * 0.92, bot_y)],
        stroke,
    );
    // Detached upper-right segment.
    painter.line_segment(
        [egui::pos2(left + w * 0.64, top_y), egui::pos2(left + w * 0.92, top_y)],
        stroke,
    );
}

/// Enter / Return arrow: a horizontal stroke at the top with a
/// down-then-left hook ending in an arrowhead.
fn paint_enter(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let w = rect.width();
    let h = rect.height();
    let stroke = egui::Stroke::new(2.0, color);
    let top_y = rect.top() + h * 0.25;
    let mid_y = rect.top() + h * 0.65;
    let left_x = rect.left() + w * 0.12;
    let right_x = rect.right() - w * 0.10;
    // Top horizontal + drop down to mid_y on the right.
    painter.line_segment(
        [egui::pos2(right_x, top_y), egui::pos2(right_x, mid_y)],
        stroke,
    );
    // Tail going left.
    painter.line_segment(
        [egui::pos2(right_x, mid_y), egui::pos2(left_x + 2.0, mid_y)],
        stroke,
    );
    // Filled arrowhead pointing left.
    let head = vec![
        egui::pos2(left_x, mid_y),
        egui::pos2(left_x + 4.0, mid_y - 3.0),
        egui::pos2(left_x + 4.0, mid_y + 3.0),
    ];
    painter.add(egui::Shape::Path(
        egui::epaint::PathShape::convex_polygon(head, color, egui::Stroke::NONE),
    ));
}

/// Identical arrow shape rotated for each direction so the four
/// nudge keys read as a matched set: shaft + filled triangular head,
/// head occupies 35% of the glyph length.
fn paint_arrow(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32, dir: ArrowDir) {
    let w = rect.width();
    let h = rect.height();
    let stroke = egui::Stroke::new(2.0, color);
    let half_head = (w.min(h)) * 0.30;
    match dir {
        ArrowDir::Up => {
            let cx = rect.center().x;
            let tail_y = rect.bottom() - h * 0.10;
            let head_base_y = rect.top() + h * 0.40;
            let head_tip_y = rect.top() + h * 0.05;
            painter.line_segment(
                [egui::pos2(cx, tail_y), egui::pos2(cx, head_base_y)],
                stroke,
            );
            let head = vec![
                egui::pos2(cx, head_tip_y),
                egui::pos2(cx - half_head, head_base_y),
                egui::pos2(cx + half_head, head_base_y),
            ];
            painter.add(egui::Shape::Path(
                egui::epaint::PathShape::convex_polygon(head, color, egui::Stroke::NONE),
            ));
        }
        ArrowDir::Down => {
            let cx = rect.center().x;
            let tail_y = rect.top() + h * 0.10;
            let head_base_y = rect.bottom() - h * 0.40;
            let head_tip_y = rect.bottom() - h * 0.05;
            painter.line_segment(
                [egui::pos2(cx, tail_y), egui::pos2(cx, head_base_y)],
                stroke,
            );
            let head = vec![
                egui::pos2(cx, head_tip_y),
                egui::pos2(cx - half_head, head_base_y),
                egui::pos2(cx + half_head, head_base_y),
            ];
            painter.add(egui::Shape::Path(
                egui::epaint::PathShape::convex_polygon(head, color, egui::Stroke::NONE),
            ));
        }
        ArrowDir::Right => {
            let cy = rect.center().y;
            let tail_x = rect.left() + w * 0.10;
            let head_base_x = rect.right() - w * 0.40;
            let head_tip_x = rect.right() - w * 0.05;
            painter.line_segment(
                [egui::pos2(tail_x, cy), egui::pos2(head_base_x, cy)],
                stroke,
            );
            let head = vec![
                egui::pos2(head_tip_x, cy),
                egui::pos2(head_base_x, cy - half_head),
                egui::pos2(head_base_x, cy + half_head),
            ];
            painter.add(egui::Shape::Path(
                egui::epaint::PathShape::convex_polygon(head, color, egui::Stroke::NONE),
            ));
        }
        ArrowDir::Left => {
            let cy = rect.center().y;
            let tail_x = rect.right() - w * 0.10;
            let head_base_x = rect.left() + w * 0.40;
            let head_tip_x = rect.left() + w * 0.05;
            painter.line_segment(
                [egui::pos2(tail_x, cy), egui::pos2(head_base_x, cy)],
                stroke,
            );
            let head = vec![
                egui::pos2(head_tip_x, cy),
                egui::pos2(head_base_x, cy - half_head),
                egui::pos2(head_base_x, cy + half_head),
            ];
            painter.add(egui::Shape::Path(
                egui::epaint::PathShape::convex_polygon(head, color, egui::Stroke::NONE),
            ));
        }
    }
}

// Bar thickness for +/-/=/_ — sized to match the painted Shift /
// arrow stroke weight (~1.8px) so all painted glyphs read at the
// same line weight as the ExtraBold letter strokes.
const CHIP_BAR_THICKNESS: f32 = 1.8;

fn paint_minus(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let w = rect.width();
    let bar = egui::Rect::from_center_size(
        rect.center(),
        egui::vec2(w * 0.80, CHIP_BAR_THICKNESS),
    );
    painter.rect_filled(bar, egui::CornerRadius::same(1), color);
}

fn paint_plus(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let w = rect.width();
    let h = rect.height();
    let horiz = egui::Rect::from_center_size(
        rect.center(),
        egui::vec2(w * 0.80, CHIP_BAR_THICKNESS),
    );
    let vert = egui::Rect::from_center_size(
        rect.center(),
        egui::vec2(CHIP_BAR_THICKNESS, h * 0.80),
    );
    painter.rect_filled(horiz, egui::CornerRadius::same(1), color);
    painter.rect_filled(vert, egui::CornerRadius::same(1), color);
}

fn paint_equal(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let w = rect.width();
    let h = rect.height();
    let cx = rect.center().x;
    let top = egui::Rect::from_center_size(
        egui::pos2(cx, rect.center().y - h * 0.16),
        egui::vec2(w * 0.80, CHIP_BAR_THICKNESS),
    );
    let bot = egui::Rect::from_center_size(
        egui::pos2(cx, rect.center().y + h * 0.16),
        egui::vec2(w * 0.80, CHIP_BAR_THICKNESS),
    );
    painter.rect_filled(top, egui::CornerRadius::same(1), color);
    painter.rect_filled(bot, egui::CornerRadius::same(1), color);
}

fn paint_underscore(painter: &egui::Painter, rect: egui::Rect, color: egui::Color32) {
    let w = rect.width();
    let h = rect.height();
    let bar = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.bottom() - h * 0.10),
        egui::vec2(w * 0.80, CHIP_BAR_THICKNESS),
    );
    painter.rect_filled(bar, egui::CornerRadius::same(1), color);
}

fn shortcut_row(
    ui: &mut egui::Ui,
    label: &str,
    tooltip: &str,
    value: &mut String,
    id: ShortcutId,
    capturing: &mut Option<ShortcutId>,
) {
    ui.horizontal(|ui| {
        // Manual paint for left-aligned label — `ui.add_sized` with
        // `Label` ends up right-justified inside the allocated rect.
        let label_w = 200.0;
        let resp = ui
            .allocate_response(egui::vec2(label_w, 28.0), egui::Sense::hover())
            .on_hover_text(tooltip);
        ui.painter().text(
            resp.rect.left_center(),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.0),
            ui.visuals().text_color(),
        );

        let is_capturing = *capturing == Some(id);
        // Manually allocate the chip rect so we can paint glyphs
        // ourselves — egui::Button only renders text/widgets, not
        // arbitrary shape primitives.
        let chip_size = egui::vec2(200.0, 28.0);
        let chip_resp = ui.allocate_response(chip_size, egui::Sense::click());
        let chip_rect = chip_resp.rect;
        let bg = if is_capturing {
            egui::Color32::from_rgb(50, 90, 140)
        } else if chip_resp.hovered() {
            egui::Color32::from_gray(74)
        } else if value.is_empty() {
            egui::Color32::from_gray(40)
        } else {
            egui::Color32::from_gray(64)
        };
        // Pure white for chip glyphs/letters: maximizes contrast
        // against the dark gray chip background and reads as
        // crisper than the visuals' off-white text color.
        let fg = egui::Color32::WHITE;
        if is_capturing {
            paint_shortcut_chip(
                ui,
                chip_rect,
                bg,
                fg,
                &[ChipSeg::Letter("Press a shortcut…".into())],
            );
        } else if value.is_empty() {
            paint_shortcut_chip(
                ui,
                chip_rect,
                bg,
                fg,
                &[ChipSeg::Letter("Click to set".into())],
            );
        } else {
            let segments = shortcut_chip_segments(value);
            paint_shortcut_chip(ui, chip_rect, bg, fg, &segments);
        }
        if chip_resp.clicked() {
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

/// Open `path` in whichever app the user's desktop has registered
/// as the default handler for `text/plain`. `xdg-open` would fall
/// back to the `.conf` handler, which often isn't a text editor.
/// Resolves the MIME default via `xdg-mime`, parses the matching
/// `.desktop` file, and either:
///   - launches via the user's preferred terminal (when `Terminal=true`,
///     so terminal editors like nvim/vim/helix work), or
///   - launches via `gtk-launch` for GUI editors.
/// Falls back to `xdg-open` if any step fails.
fn open_in_editor(path: &std::path::Path) {
    use std::process::{Command, Stdio};
    let path_str = path.to_string_lossy().into_owned();

    let desktop_id = Command::new("xdg-mime")
        .args(["query", "default", "text/plain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(desktop_id) = desktop_id {
        if let Some((exec, terminal)) = read_desktop_exec(&desktop_id) {
            let argv = parse_exec_argv(&exec, &path_str);
            if terminal {
                if launch_in_terminal(&argv) {
                    log::info!("opened {} via terminal handler {}", path_str, desktop_id);
                    return;
                }
            } else if !argv.is_empty() {
                if Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .is_ok()
                {
                    log::info!("opened {} via {} ({})", path_str, argv[0], desktop_id);
                    return;
                }
            }
        }
        // Last attempt before xdg-open: gtk-launch the .desktop id.
        let app_name = desktop_id.strip_suffix(".desktop").unwrap_or(&desktop_id);
        if Command::new("gtk-launch")
            .args([app_name, &path_str])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            log::info!("opened {} via gtk-launch {}", path_str, app_name);
            return;
        }
    }

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

    log::warn!("couldn't open editor for {}", path_str);
}

/// Walk the standard XDG application dirs looking for `id` and
/// return its `(Exec, Terminal)` lines. Returns `None` if the
/// file isn't found or doesn't have an `Exec` line.
fn read_desktop_exec(id: &str) -> Option<(String, bool)> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(p) = xdg_data_home {
        roots.push(p);
    }
    if let Some(extra) = std::env::var_os("XDG_DATA_DIRS") {
        for entry in std::env::split_paths(&extra) {
            roots.push(entry);
        }
    } else {
        roots.push(PathBuf::from("/usr/local/share"));
        roots.push(PathBuf::from("/usr/share"));
    }
    for root in roots {
        let candidate = root.join("applications").join(id);
        if let Ok(text) = std::fs::read_to_string(&candidate) {
            let mut exec: Option<String> = None;
            let mut terminal = false;
            let mut in_entry = false;
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_entry = line.eq_ignore_ascii_case("[Desktop Entry]");
                    continue;
                }
                if !in_entry {
                    continue;
                }
                if let Some(rest) = line.strip_prefix("Exec=") {
                    exec = Some(rest.to_string());
                } else if let Some(rest) = line.strip_prefix("Terminal=") {
                    terminal = matches!(rest.trim().to_ascii_lowercase().as_str(), "true" | "1");
                }
            }
            if let Some(e) = exec {
                return Some((e, terminal));
            }
        }
    }
    None
}

/// Translate a `.desktop` `Exec=` line into a runnable argv,
/// substituting `%f` / `%F` / `%u` / `%U` with `file_path` and
/// dropping the field codes the spec says we don't need
/// (`%i %c %k`). Quoting is handled with a tiny shell-style
/// splitter — desktop files don't allow shell substitution so
/// nothing fancier is needed.
fn parse_exec_argv(exec: &str, file_path: &str) -> Vec<String> {
    let mut argv: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' if in_quotes => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' if !in_quotes => {
                if !current.is_empty() {
                    argv.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        argv.push(current);
    }
    let mut out = Vec::with_capacity(argv.len());
    for tok in argv {
        match tok.as_str() {
            "%f" | "%F" | "%u" | "%U" => out.push(file_path.to_string()),
            "%i" | "%c" | "%k" => {} // drop spec metadata
            _ => out.push(tok),
        }
    }
    out
}

/// Run a parsed argv inside the user's preferred terminal. Tries
/// `$TERMINAL`, then `xdg-terminal-exec`, then a few well-known
/// emulators. Returns `true` on first successful spawn.
fn launch_in_terminal(argv: &[String]) -> bool {
    use std::process::{Command, Stdio};
    if argv.is_empty() {
        return false;
    }
    let try_terminal = |bin: &str, args_pre: &[&str]| -> bool {
        let mut cmd = Command::new(bin);
        for a in args_pre {
            cmd.arg(a);
        }
        for a in argv {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    };
    if let Some(t) = std::env::var_os("TERMINAL") {
        let bin = t.to_string_lossy().into_owned();
        if try_terminal(&bin, &[]) {
            return true;
        }
    }
    // xdg-terminal-exec (Omarchy default) — picks the user's
    // chosen terminal and runs argv inside it.
    if try_terminal("xdg-terminal-exec", &[]) {
        return true;
    }
    // Common terminal emulators with a `-e` style "run this".
    for (bin, prefix) in [
        ("ghostty", &["-e"][..]),
        ("alacritty", &["-e"]),
        ("foot", &["-e"]),
        ("kitty", &[][..]), // kitty takes the command directly
        ("gnome-terminal", &["--", ]),
        ("konsole", &["-e"]),
        ("xterm", &["-e"]),
    ] {
        if try_terminal(bin, prefix) {
            return true;
        }
    }
    false
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
        // Punctuation that the daemon spells out so the saved
        // string doesn't collide with the `+` modifier separator.
        egui::Key::Plus => "PLUS",
        egui::Key::Minus => "MINUS",
        egui::Key::Equals => "EQUAL",
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

/// Open the prefs window. Returns when the user closes it.
/// `on_saved` runs synchronously after each successful save (the
/// caller plugs in an IPC reload ping). `on_quit` runs when the
/// user clicks the "Quit vernier" button so the caller can send
/// the daemon-shutdown IPC.
pub fn run_prefs(
    on_saved: Box<dyn FnMut() + Send>,
    on_quit: Box<dyn FnMut() + Send>,
    static_bind_warning: Option<PathBuf>,
) -> Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_title("macOS Preferences")
        .with_app_id("vernier-prefs")
        .with_min_inner_size([520.0, 360.0])
        .with_inner_size([720.0, 520.0]);
    let options = NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "macOS Preferences",
        options,
        Box::new(move |cc| {
            Ok(Box::new(PrefsApp::new(
                cc,
                on_saved,
                on_quit,
                static_bind_warning,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
