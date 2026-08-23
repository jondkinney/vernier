# Vernier Bridge — official Figma plugin

Vernier Bridge reports the current Figma viewport zoom to the local Vernier
app so on-screen measurements are expressed in canvas pixels rather than
zoomed screen pixels. It supports both Figma Design and Dev Mode.

The plugin is intentionally read-only. It never reads, creates, changes, or
deletes document nodes. It sends the positive numeric viewport zoom, Design or
Dev editor type, and active-tab flag over a localhost WebSocket; none of that
context or any file content leaves the computer.

The manifest's `inspect` capability is Figma's required declaration for
showing a plugin UI in Dev Mode's Plugins/Inspect panel. Vernier does not
inspect selections or document nodes.

## Import the registered plugin for development

The checked-in `manifest.json` contains Vernier Bridge's permanent Figma plugin
ID, `1673041143009172236`. Keep that ID unchanged so development builds and
future Community releases update the existing plugin listing.

1. On a Mac or Windows computer, open **Figma Desktop** and sign in to the
   account that owns the Community listing.
2. Choose **Plugins → Development → Import plugin from manifest…** and select
   this directory's `manifest.json`.
3. Run **Vernier Bridge** from **Plugins → Development** in a test file.

A fork that intentionally creates a separate Community plugin must first
register its own plugin in Figma Desktop and replace the ID with the one Figma
assigns to that new listing.

Figma currently requires its macOS or Windows desktop app for local plugin
development and Community publishing. Vernier itself currently supports this
integration in Figma Desktop on macOS and Figma Web under Hyprland on Linux.
The published plugin can connect from other Linux desktops, but Vernier fails
closed there until it has a trustworthy active-window backend, so ordinary
screen-pixel measurements remain unchanged.

## Test locally

1. Start Vernier and enable its Figma integration. The default local endpoint
   is `ws://localhost:8765`.
2. Open a test file in Figma Desktop. Do not use a production document for
   initial testing even though the plugin is read-only.
3. Run **Vernier Bridge** from **Plugins → Development** in Design Mode.
4. Confirm that the visible panel reports **Connected** and shows the current
   canvas zoom. Change only the viewport zoom and verify the value updates.
5. Repeat in Dev Mode. The panel should identify the active mode and reconnect
   automatically if Vernier is restarted.
6. Leave the zoom unchanged for at least several seconds and confirm Vernier
   continues applying the zoom. The plugin refreshes its zoom lease every
   750 ms even when the value has not changed.

The browser may request permission for Figma to access a device on the local
network. Allow it so the plugin can reach Vernier on loopback. If the panel
shows **Vernier isn't reachable**, start Vernier, enable the integration, check
that permission, and choose **Retry now**.

## Publish

Publishing is done from Figma Desktop using the development plugin's publish
action. Before submitting:

- confirm the manifest still uses the assigned plugin ID
  `1673041143009172236`;
- test both Design Mode and Dev Mode against the release build of Vernier;
- prepare the Community icon, cover art, description, support link, and release
  notes required by Figma;
- use [`PRIVACY.md`](PRIVACY.md) for the Community privacy-policy link;
- enable two-factor authentication on the publisher account (Figma requires
  it for plugin publication); and
- make the privacy disclosure explicit: zoom, editor type, and active-tab state
  are sent only to a local Vernier process over the loopback interface.

The plugin is published at
<https://www.figma.com/community/plugin/1673041143009172236/vernier-bridge>.
Hyprland users install it from that listing and run it in Figma Web.

## Runtime behavior

- `ui.html` owns all timers because Figma's browser-backed UI iframe provides
  reliable browser timers. It requests the current zoom every 100 ms and sends
  an unchanged value every 750 ms as a lease heartbeat.
- `main.js` answers each request with the read-only `figma.viewport.zoom`
  value. It does not access the document tree.
- The UI also owns the visible status panel and WebSocket connection to
  `ws://localhost:8765`. It verifies Vernier with a protocol handshake before
  reporting **Connected**, sends zoom leases with protocol and editor context,
  marks hidden Figma tabs inactive, retries with bounded backoff, and resends
  the latest value immediately after a reconnect. When Figma pauses the
  sandbox (as it does in background tabs and windows), zoom requests go
  unanswered and the UI leases its zoom inactive after two silent
  heartbeats, so a visible tab's live zoom stays authoritative over a
  frozen one. Once verified, every heartbeat sends a message even when the
  UI cannot vouch for its zoom — Vernier reads liveness from message
  arrival, and a silent connection would be dropped and thrash through
  reconnects.
- Vernier accepts only the opaque (`Origin: null`) plugin UI origin or exact
  first-party HTTPS Figma origins, bounds connection and message sizes, and
  expires stale zoom leases. The bridge carries no document data or privileged
  commands.
- The panel must remain open while measuring. Figma allows only one plugin to
  run at a time, so starting another plugin stops Vernier Bridge.

The port is intentionally fixed in the published plugin manifest because Figma
must approve network destinations ahead of time. The endpoint is written as
`ws://localhost:8765` rather than `ws://127.0.0.1:8765` because Figma's
manifest validator rejects IP literals in `allowedDomains`; the name resolves
to the same loopback interface, where Vernier binds `127.0.0.1` only. If Vernier supports a
configurable bridge port, users of this official plugin should keep it at the
default `8765`.

### Local protocol

After the WebSocket opens, the plugin identifies itself before sending zoom:

```json
{"type":"hello","protocol":1,"client":"figma","editorType":"figma"}
```

Vernier confirms its identity and protocol version:

```json
{"type":"hello","protocol":1,"server":"vernier","version":"<installed Vernier version>"}
```

Only after that confirmation does the plugin send its renewable zoom lease:

```json
{"type":"zoom","value":1.5,"protocol":1,"client":"figma","editorType":"figma","active":true}
```

`editorType` is `figma` in Design Mode and `dev` in Dev Mode. `active` becomes
`false` when the plugin UI's Figma tab is hidden, allowing another visible file
to become authoritative. A service that does not return the expected handshake
is never shown as connected and receives no zoom data.

## Automated checks

The plugin tests have no package dependencies and run on Node.js:

```sh
cd figma-plugin
npm test
```

They cover manifest permissions, timer-free sandbox behavior, iframe polling,
hello verification, zoom heartbeats, paused-sandbox lease revocation, and
reconnection.
