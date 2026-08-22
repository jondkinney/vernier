const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");

const PLUGIN_DIR = path.resolve(__dirname, "..");

function plain(value) {
  return JSON.parse(JSON.stringify(value));
}

function readPluginFile(name) {
  return fs.readFileSync(path.join(PLUGIN_DIR, name), "utf8");
}

test("manifest is publishable, narrowly scoped, and supports Design and Dev Mode", () => {
  const manifest = JSON.parse(readPluginFile("manifest.json"));

  assert.equal(manifest.name, "Vernier Bridge");
  assert.match(
    manifest.id,
    /^(REPLACE_WITH_FIGMA_ASSIGNED_PLUGIN_ID|[0-9]+)$/,
    "the plugin ID must be the explicit placeholder or a Figma-assigned numeric ID",
  );
  assert.notEqual(manifest.id, "vernier-bridge-dev");
  assert.equal(manifest.main, "main.js");
  assert.equal(manifest.ui, "ui.html");
  assert.deepEqual(manifest.editorType, ["figma", "dev"]);
  assert.equal(manifest.documentAccess, "dynamic-page");
  assert.deepEqual(manifest.networkAccess.allowedDomains, [
    "ws://localhost:8765",
  ]);
  assert.ok(manifest.networkAccess.reasoning.length > 0);
  assert.equal(manifest.permissions, undefined);
});

test("sandbox answers UI zoom requests without relying on sandbox timers", () => {
  const messages = [];
  let showUIOptions = null;
  const context = {
    __html__: "<html></html>",
    Number,
    figma: {
      editorType: "figma",
      viewport: { zoom: 1 },
      showUI(_html, options) {
        showUIOptions = options;
      },
      ui: {
        onmessage: null,
        postMessage(message) {
          messages.push(message);
        },
      },
    },
  };

  // setTimeout and setInterval are intentionally absent: the sandbox must not
  // depend on browser timers. The visible iframe owns all polling and leases.
  vm.createContext(context);
  vm.runInContext(readPluginFile("main.js"), context);

  assert.deepEqual(plain(showUIOptions), {
    visible: true,
    width: 360,
    height: 360,
    themeColors: true,
  });
  assert.deepEqual(plain(messages), [
    { type: "context", editorType: "figma" },
    { type: "zoom", value: 1 },
  ]);

  messages.length = 0;
  context.figma.viewport.zoom = 1.25;
  context.figma.ui.onmessage({ type: "request-zoom" });
  assert.deepEqual(plain(messages), [{ type: "zoom", value: 1.25 }]);

  messages.length = 0;
  context.figma.editorType = "dev";
  context.figma.ui.onmessage({ type: "ui-ready" });
  assert.deepEqual(plain(messages), [
    { type: "context", editorType: "dev" },
    { type: "zoom", value: 1.25 },
  ]);

  messages.length = 0;
  context.figma.viewport.zoom = Number.NaN;
  context.figma.ui.onmessage({ type: "request-zoom" });
  assert.deepEqual(messages, []);
});

class FakeElement {
  constructor() {
    this.className = "";
    this.textContent = "";
    this.hidden = false;
    this.listeners = new Map();
  }

  addEventListener(type, listener) {
    this.listeners.set(type, listener);
  }

  emit(type) {
    const listener = this.listeners.get(type);
    if (listener) listener();
  }
}

class FakeScheduler {
  constructor() {
    this.nextId = 1;
    this.timers = new Map();
  }

  setTimeout(callback, delay) {
    return this.add("timeout", callback, delay);
  }

  clearTimeout(id) {
    this.timers.delete(id);
  }

  setInterval(callback, delay) {
    return this.add("interval", callback, delay);
  }

  clearInterval(id) {
    this.timers.delete(id);
  }

  add(kind, callback, delay) {
    const id = this.nextId;
    this.nextId += 1;
    this.timers.set(id, { kind, callback, delay });
    return id;
  }

  run(kind, delay) {
    const entry = [...this.timers.entries()].find(
      ([, timer]) => timer.kind === kind && timer.delay === delay,
    );
    assert.ok(entry, `expected an active ${delay}ms ${kind}`);

    const [id, timer] = entry;
    if (kind === "timeout") this.timers.delete(id);
    timer.callback();
  }
}

class FakeWebSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;

  static instances = [];

  constructor(url) {
    this.url = url;
    this.readyState = FakeWebSocket.CONNECTING;
    this.listeners = new Map();
    this.sent = [];
    FakeWebSocket.instances.push(this);
  }

  addEventListener(type, listener) {
    this.listeners.set(type, listener);
  }

  send(payload) {
    this.sent.push(JSON.parse(payload));
  }

  close() {
    if (this.readyState === FakeWebSocket.CLOSED) return;
    this.readyState = FakeWebSocket.CLOSED;
    this.emit("close", {});
  }

  emit(type, event) {
    const listener = this.listeners.get(type);
    if (listener) listener(event);
  }

  open() {
    this.readyState = FakeWebSocket.OPEN;
    this.emit("open", {});
  }

  receive(message) {
    this.emit("message", { data: JSON.stringify(message) });
  }
}

function loadUi() {
  FakeWebSocket.instances = [];

  const elementIds = [
    "badge",
    "status-dot",
    "status-title",
    "status-detail",
    "zoom",
    "mode",
    "retry",
  ];
  const elements = Object.fromEntries(
    elementIds.map((id) => [id, new FakeElement()]),
  );
  const scheduler = new FakeScheduler();
  const windowListeners = new Map();
  const documentListeners = new Map();
  const parentMessages = [];
  const windowObject = {
    addEventListener(type, listener) {
      windowListeners.set(type, listener);
    },
    setTimeout: scheduler.setTimeout.bind(scheduler),
    clearTimeout: scheduler.clearTimeout.bind(scheduler),
    setInterval: scheduler.setInterval.bind(scheduler),
    clearInterval: scheduler.clearInterval.bind(scheduler),
  };

  const html = readPluginFile("ui.html");
  const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)];
  assert.equal(scripts.length, 1, "ui.html should have one inline script");

  const context = {
    JSON,
    Math,
    Number,
    WebSocket: FakeWebSocket,
    document: {
      visibilityState: "visible",
      addEventListener(type, listener) {
        documentListeners.set(type, listener);
      },
      getElementById(id) {
        return elements[id];
      },
    },
    parent: {
      postMessage(message, origin) {
        parentMessages.push({ message, origin });
      },
    },
    window: windowObject,
  };
  vm.createContext(context);
  vm.runInContext(scripts[0][1], context);

  return {
    elements,
    parentMessages,
    scheduler,
    sockets: FakeWebSocket.instances,
    sendPluginMessage(pluginMessage) {
      windowListeners.get("message")({ data: { pluginMessage } });
    },
    setVisibility(visibilityState) {
      context.document.visibilityState = visibilityState;
      documentListeners.get("visibilitychange")();
    },
  };
}

test("UI polls in the iframe and gates zoom leases on Vernier's hello", () => {
  const ui = loadUi();

  assert.deepEqual(plain(ui.parentMessages), [
    { message: { pluginMessage: { type: "ui-ready" } }, origin: "*" },
  ]);
  assert.equal(ui.sockets.length, 0, "connection waits for editor context");

  ui.scheduler.run("interval", 100);
  assert.deepEqual(plain(ui.parentMessages.at(-1)), {
    message: { pluginMessage: { type: "request-zoom" } },
    origin: "*",
  });

  ui.sendPluginMessage({ type: "context", editorType: "dev" });
  assert.equal(ui.elements.mode.textContent, "Dev Mode");
  assert.equal(ui.sockets.length, 1);
  const socket = ui.sockets[0];
  assert.equal(socket.url, "ws://localhost:8765");

  ui.sendPluginMessage({ type: "zoom", value: 1.275 });
  socket.open();
  assert.equal(ui.elements.badge.textContent, "Verifying");
  assert.deepEqual(socket.sent, [
    { type: "hello", protocol: 1, client: "figma", editorType: "dev" },
  ]);

  socket.receive({ type: "hello", protocol: 1, server: "something-else" });
  assert.equal(ui.elements.badge.textContent, "Verifying");
  assert.equal(socket.sent.length, 1, "zoom is withheld before verification");

  socket.receive({
    type: "hello",
    protocol: 1,
    server: "vernier",
    version: "0.4.6",
  });
  assert.equal(ui.elements.badge.textContent, "Connected");
  assert.match(ui.elements["status-detail"].textContent, /0\.4\.6/);
  assert.equal(ui.elements.zoom.textContent, "127.5%");
  assert.deepEqual(socket.sent[1], {
    type: "zoom",
    value: 1.275,
    protocol: 1,
    client: "figma",
    editorType: "dev",
    active: true,
  });

  ui.scheduler.run("interval", 750);
  assert.deepEqual(socket.sent[2], socket.sent[1], "heartbeat renews unchanged zoom");

  ui.sendPluginMessage({ type: "zoom", value: 2 });
  assert.deepEqual(socket.sent[3], {
    type: "zoom",
    value: 2,
    protocol: 1,
    client: "figma",
    editorType: "dev",
    active: true,
  });
});

test("UI revokes a hidden tab and refreshes zoom before reactivating it", () => {
  const ui = loadUi();
  ui.sendPluginMessage({ type: "context", editorType: "figma" });
  ui.sendPluginMessage({ type: "zoom", value: 0.5 });

  const socket = ui.sockets[0];
  socket.open();
  socket.receive({
    type: "hello",
    protocol: 1,
    server: "vernier",
    version: "0.4.6",
  });
  assert.deepEqual(socket.sent[1], {
    type: "zoom",
    value: 0.5,
    protocol: 1,
    client: "figma",
    editorType: "figma",
    active: true,
  });

  ui.setVisibility("hidden");
  assert.deepEqual(socket.sent[2], {
    type: "zoom",
    value: 0.5,
    protocol: 1,
    client: "figma",
    editorType: "figma",
    active: false,
  });

  const messagesBeforeVisible = ui.parentMessages.length;
  ui.setVisibility("visible");
  assert.equal(
    socket.sent.length,
    3,
    "visibility alone must not reactivate a stale zoom",
  );
  assert.equal(ui.parentMessages.length, messagesBeforeVisible + 1);
  assert.deepEqual(plain(ui.parentMessages.at(-1)), {
    message: { pluginMessage: { type: "request-zoom" } },
    origin: "*",
  });

  ui.scheduler.run("interval", 750);
  assert.equal(
    socket.sent.length,
    3,
    "a heartbeat must not reactivate the cached zoom while refresh is pending",
  );

  ui.sendPluginMessage({ type: "zoom", value: 0.75 });
  assert.deepEqual(socket.sent[3], {
    type: "zoom",
    value: 0.75,
    protocol: 1,
    client: "figma",
    editorType: "figma",
    active: true,
  });

  ui.scheduler.run("interval", 750);
  assert.deepEqual(
    socket.sent[4],
    socket.sent[3],
    "the active lease retains the complete schema on heartbeat",
  );
});

test("UI rejects an unverified listener and reconnects with bounded backoff", () => {
  const ui = loadUi();
  ui.sendPluginMessage({ type: "context", editorType: "figma" });
  const firstSocket = ui.sockets[0];
  firstSocket.open();

  ui.scheduler.run("timeout", 2000);
  assert.equal(firstSocket.readyState, FakeWebSocket.CLOSED);
  assert.equal(ui.elements.badge.textContent, "Unverified");
  assert.match(ui.elements["status-detail"].textContent, /port 8765/);

  ui.scheduler.run("timeout", 1000);
  assert.equal(ui.sockets.length, 2);
  const secondSocket = ui.sockets[1];
  secondSocket.open();
  assert.deepEqual(secondSocket.sent[0], {
    type: "hello",
    protocol: 1,
    client: "figma",
    editorType: "figma",
  });
});
