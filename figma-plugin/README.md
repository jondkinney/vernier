# Vernier Bridge — Figma plugin

Reports the current viewport zoom to the local vernier daemon so on-screen
measurements come back in canvas pixels rather than zoomed screen pixels.

## Install (development)

1. Open Figma in your browser.
2. Click the menu (≡) → **Plugins** → **Development** → **Import plugin from manifest…**
3. Select `figma-plugin/manifest.json` from this repository.
4. The plugin is now available under **Plugins → Development → Vernier Bridge**.

## Use

1. Open any Figma file.
2. **Plugins → Development → Vernier Bridge → Run** once per file.
3. The hidden UI iframe stays alive until you close the file. As long as it's
   running and the vernier daemon is up, your measurements will reflect
   canvas pixels at any zoom level.

## How it works

- `main.js` polls `figma.viewport.zoom` every 100 ms and posts the value
  to a hidden 1×1 UI iframe (the only place plugins can open network
  connections).
- `ui.html` opens a WebSocket to `ws://127.0.0.1:8765` and forwards each
  zoom update as `{type: "zoom", value: 1.5}` JSON.
- The vernier daemon's bridge caches the value; when the focused window
  is a browser tab whose title indicates Figma, the daemon divides
  on-screen pixel distances by the cached zoom before rendering.

The WebSocket auto-reconnects every 2 s if the daemon isn't running yet.
