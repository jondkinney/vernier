#!/usr/bin/env bash
#
# Build a macOS .app bundle and DMG for Vernier.
#
# Outputs (under `target/macos/`):
#   AppIcon.icns                          — generated icon set
#   Vernier.app/                          — app bundle
#   Vernier-<version>-aarch64.dmg         — drag-to-Applications DMG
#
# Assumes `target/release/vernier` already exists. Pass `--build` to
# run `cargo build --release` first, or call this from CI after a
# build step.
#
# Tooling required (all preinstalled on GitHub's macos-latest runner):
#   iconutil       — built into macOS
#   sips           — built into macOS
#   codesign       — built into macOS (ad-hoc signing)
#   hdiutil        — built into macOS (via create-dmg)
#   create-dmg     — `brew install create-dmg`
#   rsvg-convert   — optional, `brew install librsvg`; used to render
#                    the 1024px slot at full quality from the SVG.
#                    Falls back to sips upscaling the 512px PNG if
#                    rsvg isn't available.
#
# Signing: ad-hoc (`codesign --sign -`). Users will see a Gatekeeper
# prompt on first launch and need to right-click → Open. Switch to a
# real Developer ID later by passing CODESIGN_IDENTITY=<id>.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

VERSION="$(awk -F'"' '/^version =/ { print $2; exit }' Cargo.toml)"
APP_NAME="Vernier"
BUNDLE_DIR="target/macos/${APP_NAME}.app"
ICNS_PATH="target/macos/AppIcon.icns"
DMG_PATH="target/macos/${APP_NAME}-${VERSION}-aarch64.dmg"
PLIST_TEMPLATE="packaging/macos/Info.plist.template"
SVG_SOURCE="assets/icons/svg/vernier.svg"
CODESIGN_IDENTITY="${CODESIGN_IDENTITY:--}"   # `-` = ad-hoc

if [[ "${1:-}" == "--build" ]]; then
    echo "==> cargo build --release --bin vernier"
    cargo build --release --bin vernier
fi

if [[ ! -x "target/release/vernier" ]]; then
    echo "error: target/release/vernier not found. Run this with --build, or" >&2
    echo "       run \`cargo build --release\` first." >&2
    exit 1
fi

# ---------------------------------------------------------------- icon
echo "==> Generating AppIcon.icns"
ICONSET_DIR="$(mktemp -d)/${APP_NAME}.iconset"
mkdir -p "$ICONSET_DIR"

# Render or copy each iconset slot. macOS expects 1x and @2x at every
# size — we share the underlying bitmap where the slot sizes coincide
# (e.g. 32 fills both icon_16x16@2x.png and icon_32x32.png).
render_size_to() {
    local size="$1"
    local out="$2"
    local png="assets/icons/png/vernier-${size}.png"
    if [[ -f "$png" ]]; then
        cp "$png" "$out"
    elif command -v rsvg-convert >/dev/null; then
        rsvg-convert -w "$size" -h "$size" "$SVG_SOURCE" -o "$out"
    else
        # Fallback when rsvg isn't installed (e.g. on the
        # `iconutil`-only runners). Upscaling the 512 PNG to 1024 with
        # sips is slightly soft, but only the 1024 slot needs this.
        sips -z "$size" "$size" "assets/icons/png/vernier-512.png" --out "$out" >/dev/null
    fi
}

render_size_to 16   "$ICONSET_DIR/icon_16x16.png"
render_size_to 32   "$ICONSET_DIR/icon_16x16@2x.png"
render_size_to 32   "$ICONSET_DIR/icon_32x32.png"
render_size_to 64   "$ICONSET_DIR/icon_32x32@2x.png"
render_size_to 128  "$ICONSET_DIR/icon_128x128.png"
render_size_to 256  "$ICONSET_DIR/icon_128x128@2x.png"
render_size_to 256  "$ICONSET_DIR/icon_256x256.png"
render_size_to 512  "$ICONSET_DIR/icon_256x256@2x.png"
render_size_to 512  "$ICONSET_DIR/icon_512x512.png"
render_size_to 1024 "$ICONSET_DIR/icon_512x512@2x.png"

mkdir -p "$(dirname "$ICNS_PATH")"
iconutil -c icns "$ICONSET_DIR" -o "$ICNS_PATH"
rm -rf "$(dirname "$ICONSET_DIR")"

# ---------------------------------------------------------------- bundle
echo "==> Building ${APP_NAME}.app"
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_DIR/Contents/MacOS"
mkdir -p "$BUNDLE_DIR/Contents/Resources"

install -m755 target/release/vernier "$BUNDLE_DIR/Contents/MacOS/vernier"
cp "$ICNS_PATH" "$BUNDLE_DIR/Contents/Resources/AppIcon.icns"
sed "s/@VERSION@/${VERSION}/g" "$PLIST_TEMPLATE" > "$BUNDLE_DIR/Contents/Info.plist"

# Touch the bundle so Finder / LaunchServices re-reads the new Info.plist
# instead of serving a cached entry from a prior build.
touch "$BUNDLE_DIR"

# Ad-hoc sign: lets the app run after the user clicks through Gatekeeper
# once. Without ANY signature (not even ad-hoc), macOS will refuse to
# launch the binary from a downloaded DMG with an opaque "cannot be
# opened" error. `--deep` covers the single Mach-O inside MacOS/ since
# we don't have nested frameworks yet.
#
# Skipped when MACOS_CERTIFICATE_P12_BASE64 is set, because
# sign-and-notarize.sh below replaces this with a real Developer ID
# signature anyway.
if [[ -z "${MACOS_CERTIFICATE_P12_BASE64:-}" ]]; then
    echo "==> Codesigning (identity: ${CODESIGN_IDENTITY})"
    codesign --force --deep --sign "$CODESIGN_IDENTITY" "$BUNDLE_DIR"
    codesign --verify --verbose=2 "$BUNDLE_DIR"
fi

# Real Developer ID signing for the bundle. No-ops when the signing
# secrets aren't in the environment, so local builds stay ad-hoc-signed
# without extra config. Notarization happens once, on the DMG below.
"$REPO_ROOT/packaging/macos/sign-and-notarize.sh" --app "$BUNDLE_DIR"

# ---------------------------------------------------------------- DMG
echo "==> Creating ${DMG_PATH}"
rm -f "$DMG_PATH"
# create-dmg's defaults give us the standard drag-to-Applications
# layout: app icon on the left, Applications-folder shortcut on the
# right, user drags from left to right to install. `--no-internet-enable`
# is the deprecated-flag-as-of-Lion equivalent — we just don't want
# the OS to auto-mount the DMG on download (it's a stub deprecation
# Apple kept around for compatibility, create-dmg passes it for us).
create-dmg \
    --volname "${APP_NAME} ${VERSION}" \
    --window-pos 200 120 \
    --window-size 600 400 \
    --icon-size 96 \
    --icon "${APP_NAME}.app" 150 200 \
    --app-drop-link 450 200 \
    --no-internet-enable \
    "$DMG_PATH" \
    "$BUNDLE_DIR"

# Sign, notarize, and staple the DMG. Apple inspects the nested
# (already-signed) Vernier.app as part of this single submission, so
# the whole release is covered in one notary round-trip. No-op
# locally without secrets.
"$REPO_ROOT/packaging/macos/sign-and-notarize.sh" --dmg "$DMG_PATH"

echo
echo "Done."
echo "  Bundle: $BUNDLE_DIR"
echo "  DMG:    $DMG_PATH"
ls -lh "$DMG_PATH"
