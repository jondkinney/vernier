# Vernier

Cross-platform pixel-measurement overlay in Rust. Runs on macOS, Windows, Linux X11 + Wayland.
Linux/Wayland (Hyprland on Omarchy) is the primary development target.

## Status

Pre-alpha. **Milestone 1 (skeleton) complete on Hyprland**: workspace,
transparent layer-shell overlay, tray icon, global hotkey via the
`GlobalShortcuts` portal, and IPC fallback for compositors that need a
direct keybind.

## Building

Requires Rust 1.85+ (stable). System packages on Arch:

```bash
sudo pacman -S --needed \
  rust base-devel pkgconf \
  wayland wayland-protocols libxkbcommon \
  pipewire xdg-desktop-portal xdg-desktop-portal-hyprland \
  libx11 libxcb dbus \
  gtk3 libayatana-appindicator
```

```bash
cargo build --release
./target/release/vernier
```

## Running on Hyprland (Omarchy)

```bash
./target/debug/vernier            # daemon, foreground
./target/debug/vernier toggle     # toggle the running daemon's overlay
./target/debug/vernier quit       # tell the running daemon to quit
```

### Tray icon

The daemon registers a `StatusNotifierItem`. waybar's `tray` module
renders it. The default Omarchy waybar config is already wired up — the
icon appears in the *tray-expander* group on the right side of the bar.
Left-click to open the menu (Preferences, Quit).

If your waybar lacks the tray module, add:

```jsonc
{ "modules-right": ["tray", ...],
  "tray": { "icon-size": 16, "spacing": 8 } }
```

### Hotkey

Two paths depending on your setup:

**Portal (preferred long-term).** The daemon registers a global shortcut
named `hk_1` with default trigger `CTRL+SHIFT+P` via
`xdg-desktop-portal-hyprland`. To map an actual key to it, add to
`~/.config/hypr/hyprland.conf`:

```
bind = SHIFT CTRL, P, global, vernier:hk_1
```

`xdg-desktop-portal-hyprland` 1.3+ is required.

**CLI fallback (simpler, no portal config).** Bind directly to the
`vernier toggle` subcommand:

```
bind = SHIFT CTRL, P, exec, vernier toggle
```

This uses a Unix domain socket at `$XDG_RUNTIME_DIR/vernier.sock` to
talk to the running daemon. Works without the GlobalShortcuts portal.

## Layout

Cargo workspace:

```
crates/
├── vernier-core/      algorithms, geometry, settings
├── vernier-platform/  Platform trait + per-OS impls
├── vernier-ui/        egui screens (prefs, HUD)
└── vernier-app/       binary
```

`vernier-platform` exposes a single `Platform` trait the rest of the
codebase binds against. Linux backend autoselects Wayland when
`$WAYLAND_DISPLAY` is set, otherwise X11. macOS and Windows backends
are stubs pending later milestones.
