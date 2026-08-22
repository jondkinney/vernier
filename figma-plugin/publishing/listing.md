# Vernier Bridge — Community listing copy

Use this copy when publishing the first Community version. Keep the privacy and
native-companion disclosures intact when editing it in Figma Desktop.

## Listing fields

- **Name:** Vernier Bridge
- **Tagline:** Measure Figma designs in exact canvas pixels with Vernier.
- **Category:** Design tools
- **Website:** https://usevernier.com
- **Support:** https://github.com/jondkinney/vernier/issues
- **Privacy:** https://github.com/jondkinney/vernier/blob/main/figma-plugin/PRIVACY.md
- **Icon:** `../../assets/icons/png/vernier-128.png`

## Description

Vernier Bridge makes the open-source Vernier desktop measurement overlay aware
of the current Figma viewport zoom. Keep the plugin window open while measuring
and Vernier converts on-screen distances back to Figma canvas pixels at any
zoom level.

Vernier Bridge requires the free Vernier desktop app from usevernier.com. It
works in Figma Design and Dev Mode. Zoom correction is currently supported in
Figma Desktop on macOS and Figma Web under Hyprland on Linux after the plugin
is installed from Community.

The plugin is intentionally read-only. It reads the numeric viewport zoom and
Design/Dev editor type, and its panel reports whether its tab is active. It does
not read or change layers, selections, text, comments, names, images, variables,
or any other document content.

Figma allows only one plugin to run at a time. Starting another plugin stops
Vernier Bridge, so run it again before your next measuring session.

## Network and data disclosure

Vernier Bridge sends the numeric viewport zoom, editor type, and active-tab
flag to the Vernier process on the same computer using
`ws://localhost:8765`. The connection never leaves the loopback interface. No
account information, document content, identifiers, usage analytics, or
telemetry are collected, stored, or sent off-device.

The localhost connection is required because Figma plugins cannot directly
control or communicate with a native desktop overlay. If the browser requests
Local network access for Figma, the user must allow it for the bridge to
connect.

## First-publication checklist

- Replace the development manifest ID with the numeric ID assigned by Figma.
- Enable two-factor authentication on the publishing Figma account.
- Test Design Mode and Dev Mode in Figma Desktop.
- Test the same build in Figma Web under Hyprland on Linux.
- Run the plugin in two Figma tabs at different zoom levels and verify the
  visible tab controls Vernier as the tabs are switched.
- Verify 50%, 100%, and 200% zoom and an unchanged zoom for 30 seconds.
- Verify reconnect after stopping and restarting Vernier.
- Capture a 1920×960 cover image (Figma's plugin cover frame size) showing the
  connected plugin, a canvas-pixel measurement at a non-100% zoom, and
  Vernier's `F · <zoom>%` indicator; do not include private document content.
- Review the final network access label before submitting.
- Complete the Data security questionnaire using
  [`security-disclosure.md`](security-disclosure.md), checking every answer
  against the current wording in Figma's publishing modal.
