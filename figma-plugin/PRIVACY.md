# Vernier Bridge privacy statement

Effective August 22, 2026.

Vernier Bridge reads the current numeric Figma viewport zoom so the Vernier
desktop app can translate screen distances into Figma canvas pixels. It also
uses the Figma editor type (Design or Dev Mode) and whether the plugin panel's
tab is visible to keep the correct local zoom active.

The plugin API does not read or change document nodes, layers, selections,
text, comments, file names, images, variables, account information, or file
identifiers. It has no analytics, advertising, telemetry, or remote service.

On Hyprland, the native Vernier app transiently checks the operating system's
active-window class and title to apply Figma zoom only while a Figma browser
tab is focused. That title may contain the Figma file name. Vernier retains
only the current title in memory; it does not store or transmit it. On macOS,
Vernier checks the frontmost application's bundle identity instead of its
window title. Focus polling is inactive when the integration is disabled.

The viewport zoom, editor type, and active flag are sent only to the Vernier
process on the same computer over `ws://localhost:8765`. Loopback traffic does
not leave the computer. Vernier keeps the latest value only in memory under a
short expiry lease; neither the plugin nor Vernier stores it on disk.

Vernier Bridge is open source. Its plugin code and native bridge are available
at <https://github.com/jondkinney/vernier>. Privacy questions and reports can
be filed at <https://github.com/jondkinney/vernier/issues>.
