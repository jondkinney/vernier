# Security disclosure notes

Use these facts when completing Figma's optional Data security questionnaire.
Answer the exact wording shown by the current publishing form; do not imply
that loopback WebSocket traffic uses TLS.

- **Document access:** none. The plugin reads `figma.viewport.zoom` and
  `figma.editorType`; it does not enumerate or access document nodes.
- **Native focus check:** on Hyprland, Vernier transiently reads the active OS
  window class and title, which may include a Figma file name; this value stays
  in memory and is never stored or transmitted. On macOS it checks only the
  frontmost app identity.
- **Data collected or stored:** none. Viewport zoom, editor type, and panel
  visibility are held briefly in memory and expire when heartbeats stop.
- **Data shared with third parties:** none.
- **Network destination:** only `ws://127.0.0.1:8765`, the Vernier process on
  the same computer. The manifest blocks every other destination.
- **Transport:** unencrypted WebSocket over the operating system's loopback
  interface. Traffic cannot route off the computer.
- **Authentication and validation:** Vernier restricts the WebSocket origin,
  requires a versioned two-way handshake, validates the complete message
  schema and zoom range, limits clients and frame sizes, and expires stale
  clients. The endpoint exposes no read API or privileged command.
- **Analytics, advertising, and telemetry:** none.
- **User accounts:** none.
- **Third-party SDKs or subprocessors:** none.
- **Source:** <https://github.com/jondkinney/vernier/tree/main/figma-plugin>
- **Privacy statement:**
  <https://github.com/jondkinney/vernier/blob/main/figma-plugin/PRIVACY.md>
