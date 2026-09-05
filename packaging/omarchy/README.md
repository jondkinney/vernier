# Vernier companion for Omarchy Shell

This directory contains Vernier's optional native Omarchy bar widget. The
Rust daemon still owns capture, measurement, the fullscreen Wayland overlay,
preferences, and persistence. The QML companion is deliberately a small
control and status surface rather than a second implementation of Vernier.
Vernier does not require it: non-Quickshell sessions can keep using the
daemon's standard `StatusNotifierItem` through a tray host such as Waybar or
KDE.

## Prerequisites

- Omarchy 4.0+ with its Quickshell-based `omarchy-shell` and `omarchy plugin`
  commands;
- an active graphical Wayland session managed by UWSM;
- network and Git access to add or update the plugin;
- for first-launch installation, AUR access, an interactive terminal for any
  package-manager prompts, and an x86_64 or aarch64 system supported by
  `vernier-bin`;
- a real, user-owned `$XDG_RUNTIME_DIR` (the normal Omarchy session setup).

Keep `grim` installed for fresh measurement activation and refresh captures,
including the companion's popup-free **Start Measuring** snapshot path. AUR
packages install it automatically. `vernier doctor` reports missing Linux tools.

## Install

The repository root contains the Omarchy `manifest.json`, so the repository
itself is the plugin:

```bash
omarchy plugin add https://github.com/jondkinney/vernier.git --enable
```

The command warns that third-party plugins run as unsandboxed user code, asks
for confirmation, clones and validates the repository, then enables the
widget. It can only install files committed and pushed to the repository; a
local working tree is not part of the GitHub install.

The widget is added to the right side of the bar by default. It supports:

- left-click: open or close the Vernier panel;
- middle-click: toggle measuring while Vernier is running;
- right-click: open Vernier Preferences when Vernier is installed;
- panel actions: install, start, activate/deactivate, clear measurements, open
  Preferences, or quit Vernier;
- live mode plus held-rectangle, guide, and pinned-measurement counts;
- live shortcut hints for measuring and clear-and-exit. Clicking **Clear**
  removes content without leaving measure mode; its displayed **Clear & Exit**
  shortcut also leaves measure mode when invoked from the keyboard.

If the installed Vernier predates the companion API, the widget stays useful
in legacy mode with toggle, Preferences, and quit controls. Live status and
duplicate-tray suppression become available after Vernier is updated.

## First-launch installation

Loading the plugin never installs software or asks for privileges. When no
`vernier` executable is available, the panel shows an explicit **Install
Vernier** button. That action opens Omarchy's visible presentation terminal
and runs `scripts/install-vernier`.

The helper:

1. holds a nonblocking per-session lock;
2. runs the supported `omarchy pkg aur add vernier-bin` flow;
3. verifies both pacman's package record and the installed executable;
4. asks UWSM to start Vernier as a background graphical service;
5. atomically publishes `installing`, `ready`, or `failed` under
   `$XDG_RUNTIME_DIR/vernier-companion/install-state`.

The runtime directory is required to be a real, user-owned directory. State
directories use mode 0700 and the state file uses mode 0600. There is no `/tmp`
fallback, hidden sudo call, automatic install hook, or daemon lifetime tied to
Quickshell.

## Update and removal

Use the plugin id to update only this companion:

```bash
omarchy plugin update com.jondkinney.vernier
```

Remove it with:

```bash
omarchy plugin remove com.jondkinney.vernier
```

`omarchy plugin disable com.jondkinney.vernier` unloads the companion while
keeping its checkout. Removing it deletes the Git-managed checkout, but does
not quit the daemon, uninstall Vernier, or delete Vernier's settings. If the
first-launch helper installed `vernier-bin` and the app should be removed too,
quit it and remove that package separately:

```bash
vernier quit
omarchy pkg drop vernier-bin
```

Use the corresponding uninstall method for a different Vernier package or a
Cargo installation. When the companion stops heartbeating, a running daemon's
portable tray item returns after the 15-second lease expires.

## Daemon contract

The companion polls `vernier status` once per second. Schema version 1 is a
single JSON line with app/build identity, active/background state, interaction
mode, overlay visibility, measurement counts, and canonical configured toggle
and clear-and-exit shortcuts. Controls use idempotent `vernier activate` and
`vernier deactivate` commands.
The additive `clear_supported` capability enables the panel's acknowledged
`vernier clear` action; older schema-v1 daemons simply omit the button.

The manifest declares both `service` and `bar-widget` entry points. Omarchy
loads one shared service for the shell, so additional monitors add only visual
widget instances rather than duplicate status processes or heartbeats.

While the companion is connected it runs `vernier companion attach` every
five seconds. The daemon makes its generic StatusNotifierItem passive for a
15-second renewable lease. The companion intentionally sends no explicit
detach during QML teardown: this prevents an old monitor or pre-reload widget
instance from revoking a newer instance's lease. Missing heartbeats restore
the portable tray item automatically.

## Development checks

From the repository root:

```bash
omarchy plugin validate .
qmllint -I /usr/share/omarchy/shell -I packaging/omarchy \
  packaging/omarchy/BarWidget.qml \
  packaging/omarchy/VernierService.qml \
  packaging/omarchy/VernierIcon.qml
bash -n packaging/omarchy/scripts/install-vernier \
  packaging/omarchy/tests/install-vernier.sh
packaging/omarchy/tests/install-vernier.sh
```

Plugin code runs unsandboxed inside the long-lived Omarchy Shell process. Keep
blocking work in bounded child processes, keep mutations behind explicit user
actions, and never make the companion the daemon supervisor.
