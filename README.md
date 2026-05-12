<img src="assets/icons/png/vernier-512.png" align="right" width="160" alt="Vernier icon">

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

The default toggle is `CTRL+SHIFT+ALT+SUPER+F` (configurable in the
prefs window's Shortcuts pane).

**On Hyprland: zero setup.** On startup the daemon runs `hyprctl
keyword bind = …, exec, vernier toggle` itself and re-applies the
bind on `configreloaded`, so the shortcut Just Works — no edits to
`~/.config/hypr/hyprland.conf` required. The runtime bind is cleared
when the daemon exits.

**On other wlroots compositors / portal-only setups.** The daemon
also registers a `GlobalShortcuts` portal entry named `hk_1` via
`xdg-desktop-portal-hyprland` (1.3+). To map an actual key to it,
add to `~/.config/hypr/hyprland.conf`:

```
bind = SHIFT CTRL ALT SUPER, F, global, vernier:hk_1
```

**Manual CLI bind.** If you'd rather keep the bind explicit in your
own config and skip the auto-install, bind directly to the toggle
subcommand:

```
bind = SHIFT CTRL ALT SUPER, F, exec, vernier toggle
```

This talks to the running daemon over a Unix domain socket at
`$XDG_RUNTIME_DIR/vernier.sock` — no portal required.

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
