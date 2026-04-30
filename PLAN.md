# macOS Clone — Implementation Plan

A cross-platform macOS clone in Rust for macOS, Windows, and Linux
(X11 + Wayland). Linux/Wayland — specifically **Hyprland on Omarchy** — is the
primary development and testing target.

## Goal

Match the feature set of macOS measurement tools (macOS) on all three platforms:

- **Measurement overlay** with cursor-snapping edge detection, configurable
  tolerance, distance + area tools, crosshair mode, color toggle.
- **Persistent markers**: horizontal/vertical guides and held-distance
  markers.
- **Screenshot tool**: auto-snapped region capture, padding slider with Alt
  toggle, optional Retina→1× downscale, capture sound, clipboard + file
  export with multiple formats (raw, CSS, SASS).
- **System integration**: tray-icon menu, global hotkey, background mode,
  Freeze Screen, per-app disable list, start-at-login.
- **Theming**: light/dark/auto, custom overlay/alternative/guides colors,
  units toggle, arrow-key cursor nudging (1px / 10px).

## Stack

- **Language**: Rust (stable).
- **Core engine**: pure Rust, no GUI deps. Edge detection, state machine,
  geometry, color math, settings serialization.
- **GUI / rendering**: `winit` for windowing, `wgpu` for overlay rendering,
  `egui` for prefs and HUD widgets, `tray-icon` for the menu-bar entry.
- **Per-OS backends** (capture, global hotkey, focused-app id, overlay
  surface):

  | Platform | Capture | Global hotkey | Overlay window |
  |---|---|---|---|
  | Linux Wayland (primary) | `ashpd` → `org.freedesktop.portal.ScreenCast` (PipeWire) | `ashpd` → `org.freedesktop.portal.GlobalShortcuts` | `smithay-client-toolkit` + `wlr-layer-shell` (Hyprland: ✅) |
  | Linux X11 | `x11rb` + MIT-SHM | `x11rb` `GrabKey` | override-redirect transparent window |
  | macOS | `screencapturekit-rs` (10.15+) | Carbon `RegisterEventHotKey` via `objc2` | `NSPanel` at floating level |
  | Windows | DXGI Desktop Duplication (`windows` crate) | `RegisterHotKey` | `WS_EX_LAYERED \| WS_EX_NOACTIVATE` topmost |

- **Sound**: `rodio` for capture-sound playback, with a small bundled
  selection (subtle / pop / classic).
- **Settings persistence**: TOML via `serde` + `directories-next` for the
  config path (`~/.config/vernier/` on Linux, `~/Library/Application
  Support/macOS/` on macOS, `%APPDATA%\macOS\` on Windows).

## Repo layout

Cargo workspace:

```
vernier/
├── Cargo.toml                  # workspace root
├── PLAN.md
├── README.md
├── crates/
│   ├── vernier-core/         # algorithms, state machine, geometry, types
│   ├── vernier-platform/     # traits + per-OS impls behind cfg flags
│   ├── vernier-ui/           # egui screens (prefs, overlay HUD, tray menus)
│   └── vernier-app/          # binary entry point
├── assets/                     # icons, capture sounds
└── packaging/                  # cargo-bundle config, AppImage recipe, MSI script
```

`vernier-platform` exposes a single `Platform` trait the app binds against:

```rust
pub trait Platform {
    fn capture_screen(&self, monitor: MonitorId) -> Result<Frame>;
    fn register_hotkey(&self, accel: Accelerator) -> Result<HotkeyId>;
    fn focused_app(&self) -> Option<AppIdentity>;
    fn create_overlay(&self, monitor: MonitorId) -> Result<OverlayHandle>;
    fn create_tray(&self, menu: TrayMenu) -> Result<TrayHandle>;
}
```

Per-OS modules implement it. The rest of the codebase never imports
platform crates directly.

## Milestone order

1. **Skeleton** — workspace, transparent fullscreen overlay window per
   platform, tray icon, global hotkey end-to-end on Hyprland first, then
   X11, macOS, Windows.
2. **Capture pipeline** — RGBA8 frames + scale factor + multi-monitor;
   PipeWire stream lifecycle on Wayland.
3. **Edge detection** — scanline scan from cursor outward, color-delta vs
   tolerance, snap-point selection, ranked output.
4. **Distance + area tools** — drag, readout HUD, aspect-ratio modes
   (Automatic / Standard 16:9 / Reduced 1.77:1 / Only common values).
5. **Guides + held-distance markers** — Shift+H / Shift+V add guides; H / V
   hold the current horizontal/vertical distance as a movable reference.
6. **Screenshot tool** — auto-snap region, padding (Alt toggle), Retina
   downscale, capture sound, clipboard + file export, all 6 clipboard
   formats (`width,height` / `height,width` / CSS w-first / CSS h-first /
   SASS w-first / SASS h-first).
7. **Crosshair mode (Shift held), color toggle (X), arrow-key 1px/10px
   nudge, copy-dimensions (Enter).**
8. **Preferences UI** — General, Screenshots, Tolerance, Appearance,
   Integrations placeholder, Shortcuts, About.
9. **Background Mode** (overlay stays usable while another app is focused)
   **+ Freeze Screen** (snapshot to measure moving UI).
10. **App-specific disable list** — per-platform focused-app id:
    - Wayland: portal `Window` interface or `wlr-foreign-toplevel`
    - X11: `_NET_ACTIVE_WINDOW` + `WM_CLASS`
    - macOS: `NSWorkspace.frontmostApplication`
    - Windows: `GetForegroundWindow` + `GetWindowModuleFileName`
11. **Polish** — animations, multi-monitor, HiDPI/Retina rounding modes
    (Points / Points rounded / Screen pixels), units toggle, start-at-login,
    dark/light + custom overlay/alternative/guides colors, restore-last-
    session shortcut.

## Target environment notes (Hyprland + Omarchy)

- **Compositor**: Hyprland, wlroots-based. Supports `wlr-layer-shell` →
  overlay can sit above all windows reliably.
- **Portals**: `xdg-desktop-portal-hyprland` provides ScreenCast and
  GlobalShortcuts. ScreenCast prompts once; we'll request a persistent
  token so the user approves the app, not each session.
- **Tray**: KDE-style `StatusNotifierItem` works on Hyprland with `waybar`
  / `eww` / `dunst` consumers. Documented in README.
- **Fallback hotkey path**: if `GlobalShortcuts` portal is unavailable, we
  document the Hyprland `bind = ... , exec, vernier toggle` pattern and
  expose a CLI subcommand for it.

## Risks / known unknowns

- **`GlobalShortcuts` portal** is GNOME 47+ / KDE Plasma 6.x+ / recent
  Hyprland. Older Wayland systems need the CLI fallback above.
- **System tray on Wayland**: KDE works out of the box; GNOME requires
  `AppIndicator` extension; Hyprland depends on the bar in use.
- **Wayland pixel-read constraint**: clients can't sample pixels under the
  cursor at will. Every measurement session goes through the ScreenCast
  portal, with the persistent-token optimisation above.
- **GNOME on Wayland** doesn't support `wlr-layer-shell`; overlay falls
  back to a regular fullscreen window (works, less ideal stacking). Not a
  problem for the Hyprland primary target.
- **`screencapturekit-rs` maturity** on macOS — if it's too thin we drop to
  raw `objc2` bindings against ScreenCaptureKit directly.

## Out of scope (until v2)

- Sketch / Adobe XD / Figma-app / Affinity **app** integrations (zoom-aware
  measurements inside those apps). Figma-on-web measuring works fine via
  the per-app disable list.
- CleanShot integration (we ship our own screenshot path).
- Multi-language UI (English only at first).
- Apple Events / scripting bridge.

## What "v1" ships

A single binary per platform that:

- Launches into the tray with a global hotkey to enter measurement mode.
- Performs accurate edge-snapping measurements on Hyprland, X11, macOS, and
  Windows, with all the keyboard shortcuts and right-click options shown
  in macOS measurement tools.
- Captures auto-snapped screenshots with padding, scaling, sound, clipboard
  and file export.
- Has a full prefs window matching macOS measurement tools's six tabs (Integrations
  shows "coming soon" placeholders for the design-tool plugins).
- Persists settings, last-session state, and the app-disable list across
  restarts.
- Is unsigned. macOS users right-click → Open the first time. Windows
  users dismiss SmartScreen. Linux users `chmod +x` the AppImage.
