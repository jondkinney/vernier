// Vernier Bridge's Figma sandbox runtime.
//
// The sandbox can read Figma's viewport zoom but cannot open network sockets,
// so it answers read-only zoom requests from the plugin UI. The browser-backed
// UI owns all timers and the localhost WebSocket connection to Vernier. This
// plugin never reads or modifies nodes in the user's document.

figma.showUI(__html__, {
  visible: true,
  width: 360,
  height: 360,
  themeColors: true,
});

function currentZoom() {
  const zoom = Number(figma.viewport.zoom);
  return Number.isFinite(zoom) && zoom > 0 ? zoom : null;
}

function reportZoom() {
  const zoom = currentZoom();
  if (zoom == null) return;
  figma.ui.postMessage({ type: "zoom", value: zoom });
}

function reportContext() {
  figma.ui.postMessage({
    type: "context",
    editorType: figma.editorType,
  });
}

figma.ui.onmessage = (message) => {
  if (!message || typeof message !== "object") return;

  // The UI may finish loading after the sandbox's first postMessage. Its ready
  // message guarantees that it receives an initial zoom without waiting for the
  // next heartbeat.
  if (message.type === "ui-ready" || message.type === "request-state") {
    reportContext();
    reportZoom();
    return;
  }

  if (message.type === "request-zoom") {
    reportZoom();
    return;
  }

  // The panel must stay open while measuring (Figma stops a plugin when
  // its window closes), so the UI offers a compact mode instead. Bounds
  // keep a confused UI from producing an unusable window.
  if (message.type === "resize") {
    const width = Math.min(Math.max(Number(message.width) || 0, 200), 400);
    const height = Math.min(Math.max(Number(message.height) || 0, 44), 600);
    figma.ui.resize(width, height);
  }
};

reportContext();
reportZoom();
