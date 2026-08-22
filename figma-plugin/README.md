# Vernier Bridge — official Figma plugin

Vernier Bridge reports the current Figma viewport zoom to the local Vernier
app so on-screen measurements are expressed in canvas pixels rather than
zoomed screen pixels. It supports both Figma Design and Dev Mode.

The plugin is intentionally read-only. It never reads, creates, changes, or
deletes document nodes. It sends the positive numeric viewport zoom, Design or
Dev editor type, and active-tab flag over a localhost WebSocket; none of that
context or any file content leaves the computer.

## Register the plugin before importing it

The checked-in `manifest.json` deliberately contains this nonfunctional ID:

```json
"id": "REPLACE_WITH_FIGMA_ASSIGNED_PLUGIN_ID"
```

Do not publish or ship that placeholder. Figma assigns the real plugin ID when
the plugin is created from Figma Desktop:

1. On a Mac or Windows computer, open **Figma Desktop** and sign in to the
   account that will own the Community listing.
2. Create a new development plugin from Figma's **Plugins → Development**
   menu. Choose a plugin with a custom UI and name it **Vernier Bridge**.
   Figma's menu wording may be **New plugin…** or **Create new plugin…**.
3. In the temporary plugin Figma creates, copy the assigned numeric `id` from
   its `manifest.json`.
4. Replace `REPLACE_WITH_FIGMA_ASSIGNED_PLUGIN_ID` in this directory's
   `manifest.json` with that exact ID. Keep the ID in version control so every
   future release updates the same Community plugin.
5. In Figma Desktop, choose **Plugins → Development → Import plugin from
   manifest…** and select this directory's `manifest.json`.

Figma currently requires its macOS or Windows desktop app for local plugin
development and Community publishing. Vernier itself currently supports this
integration in Figma Desktop on macOS and Figma Web under Hyprland on Linux.
The published plugin can connect from other Linux desktops, but Vernier fails
closed there until it has a trustworthy active-window backend, so ordinary
screen-pixel measurements remain unchanged.

## Test locally

1. Start Vernier and enable its Figma integration. The default local endpoint
   is `ws://127.0.0.1:8765`.
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

- confirm the assigned Figma plugin ID has replaced the placeholder;
- test both Design Mode and Dev Mode against the release build of Vernier;
- prepare the Community icon, cover art, description, support link, and release
  notes required by Figma;
- use [`PRIVACY.md`](PRIVACY.md) for the Community privacy-policy link;
- enable two-factor authentication on the publisher account (Figma requires
  it for plugin publication); and
- make the privacy disclosure explicit: zoom, editor type, and active-tab state
  are sent only to a local Vernier process over `127.0.0.1`.

After Figma approves the Community listing, Hyprland users install it from that
listing and run it in Figma Web. Update Vernier's install UI and documentation
with the final listing URL after publication.

## Runtime behavior

- `ui.html` owns all timers because Figma's browser-backed UI iframe provides
  reliable browser timers. It requests the current zoom every 100 ms and sends
  an unchanged value every 750 ms as a lease heartbeat.
- `main.js` answers each request with the read-only `figma.viewport.zoom`
  value. It does not access the document tree.
- The UI also owns the visible status panel and WebSocket connection to
  `ws://127.0.0.1:8765`. It verifies Vernier with a protocol handshake before
  reporting **Connected**, sends zoom leases with protocol and editor context,
  marks hidden Figma tabs inactive, retries with bounded backoff, and resends
  the latest value immediately after a reconnect.
- Vernier accepts only the opaque (`Origin: null`) plugin UI origin or exact
  first-party HTTPS Figma origins, bounds connection and message sizes, and
  expires stale zoom leases. The bridge carries no document data or privileged
  commands.
- The panel must remain open while measuring. Figma allows only one plugin to
  run at a time, so starting another plugin stops Vernier Bridge.

The port is intentionally fixed in the published plugin manifest because Figma
must approve network destinations ahead of time. If Vernier supports a
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
hello verification, zoom heartbeats, and reconnection.
