# Vernier icon set

## Contents

- `svg/` — vector sources (vernier, vernier-symbolic for menubar, vernier-v alternate)
- `png/` — rasterized app icon at 16, 22, 24, 32, 48, 64, 128, 256, 512
- `hicolor/` — Freedesktop hicolor theme structure, ready to drop into
  `/usr/share/icons/hicolor/` or `~/.local/share/icons/hicolor/`

## Install (system-wide)

```sh
sudo cp -r hicolor/* /usr/share/icons/hicolor/
sudo gtk-update-icon-cache /usr/share/icons/hicolor
```

## Install (user)

```sh
mkdir -p ~/.local/share/icons/hicolor
cp -r hicolor/* ~/.local/share/icons/hicolor/
gtk-update-icon-cache ~/.local/share/icons/hicolor
```

## Reference in `.desktop` file

```ini
[Desktop Entry]
Name=Vernier
Exec=vernier
Icon=vernier
Type=Application
Categories=Utility;Graphics;
```

## Reference symbolic icon (waybar, etc.)

The symbolic variant uses `currentColor` so it inherits theme color.
Reference as `vernier-symbolic` or point waybar directly at the SVG path.
