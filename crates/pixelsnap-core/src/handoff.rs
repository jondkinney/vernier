//! Discovery + invocation helpers for the post-capture handoff app.
//!
//! The daemon hands every screenshot off to a single user-chosen
//! annotation tool (Satty by default; Swappy, Krita, GIMP, … if the
//! user browses). This module exposes:
//!
//! - [`HandoffApp`] — name, command, arg template, and icon path.
//! - [`detect_default`] — return Satty's metadata when it's installed,
//!   so a fresh install can hand off without any user configuration.
//! - [`lookup_for_binary`] — resolve a binary the user picked from
//!   disk to its display name, args, and icon by parsing its
//!   `.desktop` file (falling back to a positional `{file}` arg when
//!   nothing matches).
//! - [`render_args`] — split an arg template on whitespace and
//!   substitute `{file}`, producing the runtime argv.
//!
//! Lives in `vernier-core` so both the daemon (`vernier-app`) and
//! the prefs UI (`vernier-ui`) can share one canonical resolver.

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

/// Best-effort default. Currently looks up Satty; returns `None`
/// when satty isn't installed.
pub fn detect_default() -> Option<HandoffApp> {
    lookup_for_binary(Path::new("satty"))
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
    let matches = first_base == expected_basename
        || first_path == resolved_bin
        || (first_path.is_absolute() && first_path == resolved_bin);
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
