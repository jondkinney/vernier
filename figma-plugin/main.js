// Plugin sandbox runtime. Polls the current viewport zoom and posts
// it to the hidden UI iframe, which forwards to the vernier daemon
// over a localhost WebSocket. The sandbox itself can't open sockets.
figma.showUI(__html__, { visible: false, width: 1, height: 1 });

const POLL_MS = 100;
const EPS = 0.0001;
let lastSent = -1;

function tick() {
  const z = figma.viewport.zoom;
  if (Math.abs(z - lastSent) > EPS) {
    lastSent = z;
    figma.ui.postMessage({ type: "zoom", value: z });
  }
}

setInterval(tick, POLL_MS);
tick();
