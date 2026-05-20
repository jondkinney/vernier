//! Discovery + invocation helpers for the post-capture handoff app.
//!
//! The daemon hands every screenshot off to a single user-chosen
//! annotation tool (Satty, Swappy, Flameshot, …). Selection is
//! explicit — the prefs UI surfaces a dropdown of installed common
//! apps and lets the user Browse to a custom binary; nothing is
//! auto-selected. This module exposes:
//!
//! - [`HandoffApp`] — name, command, arg template, and icon path.
//! - [`KNOWN_HANDOFF_APPS`] — curated list of binary names the
//!   prefs dropdown looks for.
//! - [`find_installed_apps`] — return [`HandoffApp`] metadata for
//!   every entry in [`KNOWN_HANDOFF_APPS`] that's actually on
//!   `$PATH`.
//! - [`lookup_for_binary`] — resolve a binary the user picked from
//!   disk to its display name, args, and icon by parsing its
//!   `.desktop` file (falling back to a positional `{file}` arg
//!   when nothing matches).
//! - [`render_args`] — split an arg template on whitespace and
//!   substitute `{file}`, producing the runtime argv.
//!
//! Lives in `vernier-core` so both the daemon (`vernier-app`)
//! and the prefs UI (`vernier-ui`) can share one canonical
//! resolver.

use std::path::{Path, PathBuf};

/// Resolved metadata for a handoff target. All fields are owned
/// strings so the struct is trivially serializable / clonable
/// without lifetimes leaking into [`crate::ScreenshotSettings`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HandoffApp {
    /// Display name for the prefs card (`Satty`, `Swappy`, …).
    pub name: String,
    /// Binary to spawn — either an absolute path or a name resolved
    /// against `$PATH`.
    pub command: String,
    /// Whitespace-tokenized arg template. `{file}` is substituted
    /// with the captured PNG path at spawn time.
    pub args: String,
    /// Optional icon file (SVG or PNG) on disk. Empty when the
    /// `.desktop`'s `Icon=` couldn't be resolved.
    pub icon_path: String,
}

/// Curated list of binary names the prefs UI scans `$PATH` for to
/// populate its handoff-app dropdown. Annotation-first tools come
/// before heavier editors; order here drives the dropdown order.
///
/// Heavy image editors (GIMP, Krita) are intentionally omitted —
/// users who want them can still pick them via Browse… The list
/// stays focused on tools that take a single PNG path on the
/// command line and open straight into an annotate-and-save view.
pub const KNOWN_HANDOFF_APPS: &[&str] = &[
    "tensaku",   // Wayland-native annotate-and-save (Satty fork)
    "satty",     // Wayland-native, modern annotate-and-save
    "swappy",    // Sway/wlroots annotation companion
    "flameshot", // Cross-platform, popular X11 tool
    "ksnip",     // Cross-platform annotation
    "shutter",   // Long-standing Perl/Gtk tool
    "pinta",     // Light Paint.NET-style raster editor
    "drawing",   // GNOME annotation app
];

/// Curated list of macOS `.app` bundle filenames (without the `.app`
/// suffix) the prefs UI scans for to populate its handoff dropdown.
/// Order drives dropdown order.
///
/// Same curation principle as `KNOWN_HANDOFF_APPS`: annotation-first
/// screenshot tools that accept a single image path on launch (via
/// `open -a "<Name>" file.png`). Heavy raster editors (Pixelmator,
/// Affinity, Photoshop) are omitted — Browse… is the escape hatch.
///
/// Snagit ships under year-stamped bundle names — list a couple
/// recent versions plus the bare name (older releases) so we catch
/// installs without needing to scan generically.
#[cfg(target_os = "macos")]
pub const KNOWN_HANDOFF_APPS_MACOS: &[&str] = &[
    "CleanShot X",
    "Shottr",
    "Xnapper",
    "Monosnap",
    "Annotate",
    "Skitch",
    "Snagit 2025",
    "Snagit 2024",
    "Snagit",
    "Lightshot Screenshot",
    "Droplr",
    "CloudApp",
    "Preview", // built-in, basic markup
];

/// Return [`HandoffApp`] metadata for every installed annotation app
/// the prefs UI knows about. Order matches the platform-specific
/// `KNOWN_HANDOFF_APPS*` list. The prefs UI uses this to drive the
/// picker dropdown.
///
/// On Linux, scans `$PATH` against [`KNOWN_HANDOFF_APPS`] and resolves
/// each match against XDG `.desktop` files for display name + icon.
/// On macOS, scans the standard application directories (including
/// `/Applications/Setapp` for Setapp users) for `.app` bundles named
/// in [`KNOWN_HANDOFF_APPS_MACOS`].
pub fn find_installed_apps() -> Vec<HandoffApp> {
    #[cfg(target_os = "macos")]
    {
        find_installed_apps_macos()
    }
    #[cfg(not(target_os = "macos"))]
    {
        KNOWN_HANDOFF_APPS
            .iter()
            .filter_map(|name| lookup_for_binary(Path::new(name)))
            .collect()
    }
}

/// macOS counterpart to the PATH-scan version. Walks the standard
/// application directories looking for `.app` bundles whose folder
/// name (minus the `.app` suffix) appears in [`KNOWN_HANDOFF_APPS_MACOS`].
/// Each hit becomes a [`HandoffApp`] that invokes `open -a` with the
/// absolute bundle path — the absolute path is unambiguous even when
/// two installed bundles share the display name (e.g. Setapp's
/// CleanShot X alongside a manually-installed copy).
#[cfg(target_os = "macos")]
fn find_installed_apps_macos() -> Vec<HandoffApp> {
    let dirs = macos_application_dirs();
    let mut found: Vec<HandoffApp> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for known in KNOWN_HANDOFF_APPS_MACOS {
        // Order: KNOWN_HANDOFF_APPS_MACOS drives dropdown order; for
        // each name check directories in `macos_application_dirs`
        // priority and stop at the first hit so duplicates don't
        // multiply the list when the same app exists in /Applications
        // and /Applications/Setapp.
        for dir in &dirs {
            let bundle = dir.join(format!("{known}.app"));
            if bundle.is_dir() && seen.insert((*known).to_string()) {
                found.push(handoff_for_macos_bundle(&bundle, known));
                break;
            }
        }
    }
    found
}

/// Standard locations a macOS app might live in. `/Applications` and
/// `~/Applications` are the user-facing canonical roots; Setapp puts
/// its catalog under `/Applications/Setapp`; `/System/Applications`
/// holds Apple's bundled apps (Preview lives there on modern macOS).
#[cfg(target_os = "macos")]
fn macos_application_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/Applications/Setapp"),
        PathBuf::from("/System/Applications"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Applications"));
    }
    dirs
}

/// Build a [`HandoffApp`] that launches `bundle_path` with the
/// captured PNG via `/usr/bin/open -a "<bundle_path>" {file}`.
/// Passing the absolute bundle path (rather than just the display
/// name) makes the spawn unambiguous regardless of LaunchServices
/// state. `icon_path` is set to the bundle path itself; the prefs
/// UI detects the `.app` suffix and routes to
/// `vernier_platform::extract_macos_app_icon_rgba`, which uses
/// `NSWorkspace.iconForFile` to render the icon (handles `.icns`,
/// asset catalogs, and custom icons uniformly).
#[cfg(target_os = "macos")]
fn handoff_for_macos_bundle(bundle_path: &Path, display_name: &str) -> HandoffApp {
    let bundle_str = bundle_path.to_string_lossy().into_owned();
    HandoffApp {
        name: display_name.to_string(),
        command: "/usr/bin/open".to_string(),
        // shell_quote_for_template wraps the bundle path in
        // double-quotes if it contains whitespace — `render_args`
        // tokenizes the args template the same way `.desktop` Exec
        // lines are parsed, so the quoting round-trips correctly.
        args: format!("-a {} {{file}}", shell_quote_for_template(&bundle_str)),
        icon_path: bundle_str,
    }
}

/// Quote `s` so `render_args`'s `.desktop`-style tokenizer keeps it
/// as a single argv slot. Only quotes when whitespace is present;
/// internal double-quotes are escaped the way the tokenizer's
/// `\\` + `next` pair expects.
#[cfg(target_os = "macos")]
fn shell_quote_for_template(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace()) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Resolve `bin` (absolute path or PATH-relative name) to a
/// [`HandoffApp`]. Returns `None` if the binary isn't installed.
///
/// Strategy:
/// 1. Verify the binary is reachable (absolute path exists, or
///    basename resolves on `$PATH`). This avoids returning a stub
///    `HandoffApp` for something that won't spawn.
/// 2. Look for a `.desktop` file whose `Exec=` line names this
///    binary — first the obvious `<basename>.desktop` lookup, then
///    a directory scan as fallback.
/// 3. If found, use the desktop entry's `Name=`, `Icon=`, and
///    `Exec=` (with `%f`/`%F`/`%u`/`%U` rewritten to `{file}`).
/// 4. Otherwise, return a minimal entry that just runs `<bin>
///    {file}` with no icon.
pub fn lookup_for_binary(bin: &Path) -> Option<HandoffApp> {
    // macOS Browse… commonly returns a `.app` bundle path (Finder
    // treats bundles as files even though they're directories on disk).
    // Detour through the bundle handoff so the user gets a working
    // `open -a` invocation instead of hitting the `resolve_binary`
    // fast-path against a directory and failing the `exists` check
    // for the wrong reason.
    #[cfg(target_os = "macos")]
    {
        if bin.extension().and_then(|e| e.to_str()) == Some("app") && bin.is_dir() {
            let display = bin
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "App".to_string());
            return Some(handoff_for_macos_bundle(bin, &display));
        }
    }
    let resolved = resolve_binary(bin)?;
    let basename = bin
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| resolved.to_string_lossy().into_owned());
    if let Some(app) = find_desktop_for_binary(&basename, &resolved) {
        return Some(app);
    }
    Some(HandoffApp {
        name: basename,
        command: resolved.to_string_lossy().into_owned(),
        args: "{file}".to_string(),
        icon_path: String::new(),
    })
}

/// Tokenize `template` on whitespace (respecting double-quotes the
/// way `.desktop` Exec lines do) and substitute `{file}` with
/// `file_path`. Used by the daemon when spawning the handoff app.
pub fn render_args(template: &str, file_path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' if in_quotes => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ' ' | '\t' if !in_quotes => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out.into_iter()
        .map(|t| t.replace("{file}", file_path))
        .collect()
}

/// Search the standard icon roots for `name_or_path`. Accepts an
/// absolute path (returned as-is when it exists) or a theme-style
/// name like `satty` (resolved to the first matching SVG/PNG under
/// hicolor or pixmaps). Returns the absolute path as a String, or
/// empty when nothing matches.
pub fn resolve_icon(name_or_path: &str) -> String {
    if name_or_path.is_empty() {
        return String::new();
    }
    let p = Path::new(name_or_path);
    if p.is_absolute() {
        return if p.exists() {
            name_or_path.to_string()
        } else {
            String::new()
        };
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(h) = home.as_ref() {
        roots.push(h.join(".local/share/icons"));
        roots.push(h.join(".icons"));
    }
    if let Some(extra) = std::env::var_os("XDG_DATA_DIRS") {
        for entry in std::env::split_paths(&extra) {
            roots.push(entry.join("icons"));
        }
    } else {
        roots.push(PathBuf::from("/usr/local/share/icons"));
        roots.push(PathBuf::from("/usr/share/icons"));
    }
    roots.push(PathBuf::from("/usr/share/pixmaps"));
    // Prefer SVG (vector → sharp at any HiDPI) then large PNGs.
    let sizes = [
        "scalable", "512x512", "256x256", "192x192", "128x128", "96x96", "64x64", "48x48",
    ];
    let exts = ["svg", "png"];
    for root in &roots {
        for size in &sizes {
            for ext in &exts {
                let p = root
                    .join("hicolor")
                    .join(size)
                    .join("apps")
                    .join(format!("{name_or_path}.{ext}"));
                if p.exists() {
                    return p.to_string_lossy().into_owned();
                }
            }
        }
        for ext in &exts {
            let p = root.join(format!("{name_or_path}.{ext}"));
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    String::new()
}

fn resolve_binary(bin: &Path) -> Option<PathBuf> {
    if bin.is_absolute() {
        return bin.exists().then(|| bin.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(bin);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

fn xdg_application_dirs() -> Vec<PathBuf> {
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
    roots.into_iter().map(|r| r.join("applications")).collect()
}

fn find_desktop_for_binary(basename: &str, resolved: &Path) -> Option<HandoffApp> {
    // Fast path: matches the common packaging convention.
    for dir in xdg_application_dirs() {
        let direct = dir.join(format!("{basename}.desktop"));
        if let Some(app) = parse_desktop(&direct, basename, resolved) {
            return Some(app);
        }
    }
    // Slow path: some .desktop files don't share their Exec basename.
    for dir in xdg_application_dirs() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(app) = parse_desktop(&p, basename, resolved) {
                return Some(app);
            }
        }
    }
    None
}

fn parse_desktop(path: &Path, expected_basename: &str, resolved_bin: &Path) -> Option<HandoffApp> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut name: Option<String> = None;
    let mut exec: Option<String> = None;
    let mut icon: Option<String> = None;
    let mut hidden = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line.eq_ignore_ascii_case("[Desktop Entry]");
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Name=") {
            if name.is_none() {
                name = Some(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("Exec=") {
            exec = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("Icon=") {
            icon = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("Hidden=") {
            hidden = matches!(rest.trim().to_ascii_lowercase().as_str(), "true" | "1");
        }
    }
    if hidden {
        return None;
    }
    let exec = exec?;
    // Verify the Exec= first token names this binary (by basename or
    // absolute path). split_whitespace + trim_matches('"') is enough
    // for the vast majority of .desktop files in the wild.
    //
    // We use the *resolved* binary path (not the .desktop's Exec
    // token) as the spawn command so a user pointing at their own
    // build of `satty` runs *that* binary instead of the system one
    // PATH would resolve `satty` to.
    let first = exec.split_whitespace().next()?.trim_matches('"');
    let first_path = Path::new(first);
    let first_base = first_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| first.to_string());
    let matches = first_base == expected_basename || first_path == resolved_bin;
    if !matches {
        return None;
    }
    // Build the args template from Exec= tail. Convert the freedesktop
    // field codes the user actually cares about; drop the metadata
    // codes the spec says we don't need.
    let mut argv: Vec<String> = Vec::new();
    for tok in exec.split_whitespace().skip(1) {
        match tok {
            "%f" | "%F" | "%u" | "%U" => argv.push("{file}".to_string()),
            "%i" | "%c" | "%k" => {}
            _ => argv.push(tok.to_string()),
        }
    }
    if !argv.iter().any(|a| a == "{file}") {
        argv.push("{file}".to_string());
    }
    let args = argv.join(" ");
    let icon_path = icon.map(|n| resolve_icon(&n)).unwrap_or_default();
    Some(HandoffApp {
        name: name.unwrap_or_else(|| expected_basename.to_string()),
        command: resolved_bin.to_string_lossy().into_owned(),
        args,
        icon_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_args_substitutes_file_token() {
        let argv = render_args("--filename {file} --output {file}", "/tmp/x.png");
        assert_eq!(
            argv,
            vec!["--filename", "/tmp/x.png", "--output", "/tmp/x.png"]
        );
    }

    #[test]
    fn render_args_handles_quoted_tokens() {
        let argv = render_args("\"--with space\" {file}", "/tmp/x.png");
        assert_eq!(argv, vec!["--with space", "/tmp/x.png"]);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    /// Sanity check: every macOS install ships /System/Applications/Preview.app
    /// (and "Preview" is in KNOWN_HANDOFF_APPS_MACOS), so detection should
    /// always return at least one entry. CI runners on macOS satisfy this too.
    #[test]
    fn lists_installed_macos_apps_includes_preview() {
        let apps = find_installed_apps();
        assert!(
            apps.iter().any(|a| a.name == "Preview"),
            "Preview should be detected on macOS; found: {:?}",
            apps.iter().map(|a| &a.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn browse_to_app_bundle_yields_open_invocation() {
        let preview = Path::new("/System/Applications/Preview.app");
        let app = lookup_for_binary(preview).expect("Preview bundle should resolve");
        assert_eq!(app.name, "Preview");
        assert_eq!(app.command, "/usr/bin/open");
        assert!(
            app.args.contains("Preview.app") && app.args.contains("{file}"),
            "args should reference the bundle and include {{file}}: {}",
            app.args
        );
    }

    #[test]
    fn shell_quote_wraps_paths_with_spaces() {
        assert_eq!(
            shell_quote_for_template("/Applications/Shottr.app"),
            "/Applications/Shottr.app"
        );
        assert_eq!(
            shell_quote_for_template("/Applications/Setapp/CleanShot X.app"),
            "\"/Applications/Setapp/CleanShot X.app\""
        );
    }
}
