#!/usr/bin/env bash
#
# Sign + notarize + staple a macOS .app bundle or .dmg for Vernier.
#
# Designed to run on a GitHub Actions macos-latest runner. A throwaway
# keychain holds the Developer ID Application cert for the duration of
# the job and is torn down on exit (success or failure).
#
# Usage:
#   sign-and-notarize.sh --app target/macos/Vernier.app
#   sign-and-notarize.sh --dmg target/macos/Vernier-0.1.6-aarch64.dmg
#
# Required env vars (all set as GitHub Actions secrets):
#   MACOS_CERTIFICATE_P12_BASE64   base64 of the exported Developer ID
#                                  Application .p12 file
#   MACOS_CERTIFICATE_PASSWORD     password used when exporting the .p12
#   MACOS_NOTARY_APPLE_ID          Apple ID email (jonkinney@gmail.com)
#   MACOS_NOTARY_TEAM_ID           10-char team ID (e.g. ABCD123456)
#   VERNIER_SIGNING_PASSWORD       app-specific password from
#                                  appleid.apple.com, used by notarytool
#
# Local development: leave MACOS_CERTIFICATE_P12_BASE64 unset and this
# script no-ops with a notice. package.sh's ad-hoc signature stays in
# place, so the resulting bundle still launches after the usual
# right-click → Open dance.

set -euo pipefail

# --- args ----------------------------------------------------------
TARGET=""
KIND=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --app) TARGET="$2"; KIND="app"; shift 2 ;;
        --dmg) TARGET="$2"; KIND="dmg"; shift 2 ;;
        -h|--help)
            sed -n '2,28p' "$0"
            exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$TARGET" || -z "$KIND" ]]; then
    echo "usage: $0 --app <path> | --dmg <path>" >&2
    exit 2
fi

if [[ ! -e "$TARGET" ]]; then
    echo "error: $TARGET not found" >&2
    exit 1
fi

# --- credential / no-op gate --------------------------------------
if [[ -z "${MACOS_CERTIFICATE_P12_BASE64:-}" ]]; then
    echo "==> MACOS_CERTIFICATE_P12_BASE64 unset; skipping notarized signing for $TARGET."
    echo "    (Local builds keep their ad-hoc signature; CI must set the secret.)"
    exit 0
fi

: "${MACOS_CERTIFICATE_PASSWORD:?required when MACOS_CERTIFICATE_P12_BASE64 is set}"
: "${MACOS_NOTARY_APPLE_ID:?required for notarytool submission}"
: "${MACOS_NOTARY_TEAM_ID:?required for notarytool submission}"
: "${VERNIER_SIGNING_PASSWORD:?app-specific password required for notarytool}"

# RUNNER_TEMP is set on GitHub Actions; fall back to a mktemp dir so
# the script also runs cleanly if invoked outside CI for testing.
WORK_DIR="${RUNNER_TEMP:-$(mktemp -d)}"
KEYCHAIN_PATH="$WORK_DIR/vernier-signing.keychain-db"
P12_PATH="$WORK_DIR/vernier-signing.p12"
KEYCHAIN_PASSWORD="$(openssl rand -hex 32)"

# Snapshot the existing keychain search list so we can restore it on
# exit. Without restoring, subsequent steps in the same job inherit
# our temporary keychain even after we delete the file.
ORIGINAL_KEYCHAINS="$(security list-keychains -d user | tr -d '"' | xargs)"

cleanup() {
    if [[ -f "$KEYCHAIN_PATH" ]]; then
        # shellcheck disable=SC2086
        security list-keychains -d user -s $ORIGINAL_KEYCHAINS 2>/dev/null || true
        security delete-keychain "$KEYCHAIN_PATH" 2>/dev/null || true
    fi
    rm -f "$P12_PATH"
}
trap cleanup EXIT

# --- temp keychain + cert import ----------------------------------
echo "==> Setting up temporary signing keychain"
security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN_PATH"
# Auto-lock after 6h is plenty for one job; we delete it long before then.
security set-keychain-settings -lut 21600 "$KEYCHAIN_PATH"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN_PATH"

# Prepend the temp keychain so codesign finds our identity first, but
# keep the previous list so other tooling on the runner still works.
# shellcheck disable=SC2086
security list-keychains -d user -s "$KEYCHAIN_PATH" $ORIGINAL_KEYCHAINS

echo "==> Importing Developer ID Application cert"
echo "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$P12_PATH"
security import "$P12_PATH" \
    -k "$KEYCHAIN_PATH" \
    -P "$MACOS_CERTIFICATE_PASSWORD" \
    -T /usr/bin/codesign \
    -T /usr/bin/security
# Without this, codesign hangs on a UI prompt asking to allow access
# to the private key. apple-tool: + apple: cover codesign / security.
security set-key-partition-list \
    -S apple-tool:,apple: \
    -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN_PATH" >/dev/null

IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN_PATH" \
    | awk -F'"' '/Developer ID Application/ { print $2; exit }')"
if [[ -z "$IDENTITY" ]]; then
    echo "error: no Developer ID Application identity found after import" >&2
    security find-identity -v -p codesigning "$KEYCHAIN_PATH" >&2
    exit 1
fi
echo "==> Signing identity: $IDENTITY"

# Optional entitlements file. Vernier doesn't need any today
# (ScreenCaptureKit / CGWindowList go through runtime TCC prompts,
# not entitlements), but leaving the hook means we can drop a file
# in later without editing this script.
ENTITLEMENTS_ARGS=()
if [[ -f packaging/macos/Vernier.entitlements ]]; then
    ENTITLEMENTS_ARGS=(--entitlements packaging/macos/Vernier.entitlements)
fi

# --- sign ----------------------------------------------------------
if [[ "$KIND" == "app" ]]; then
    # Inside-out: nested executables/frameworks/dylibs first, then the
    # outer wrapper. Vernier ships a single Mach-O today, so the loop
    # is usually empty — keeping it means we don't relearn this when
    # we add a helper.
    if [[ -d "$TARGET/Contents/Frameworks" || -d "$TARGET/Contents/PlugIns" || -d "$TARGET/Contents/Helpers" ]]; then
        while IFS= read -r -d '' nested; do
            echo "==> Signing nested: $nested"
            codesign --force --options runtime --timestamp \
                --keychain "$KEYCHAIN_PATH" \
                --sign "$IDENTITY" \
                "$nested"
        done < <(find \
            "$TARGET/Contents/Frameworks" \
            "$TARGET/Contents/PlugIns" \
            "$TARGET/Contents/Helpers" \
            -type f \( -perm -u+x -o -name "*.dylib" \) -print0 2>/dev/null || true)
    fi

    # Sign the main binary explicitly before the wrapper so a bad
    # entitlements file fails fast instead of after we've sealed the
    # bundle.
    echo "==> Signing main executable"
    codesign --force --options runtime --timestamp \
        --keychain "$KEYCHAIN_PATH" \
        --sign "$IDENTITY" \
        "${ENTITLEMENTS_ARGS[@]}" \
        "$TARGET/Contents/MacOS/vernier"

    echo "==> Signing app bundle"
    codesign --force --options runtime --timestamp \
        --keychain "$KEYCHAIN_PATH" \
        --sign "$IDENTITY" \
        "${ENTITLEMENTS_ARGS[@]}" \
        "$TARGET"

    codesign --verify --deep --strict --verbose=2 "$TARGET"
else
    # DMGs aren't executable code, so no hardened runtime — but
    # --timestamp is still required for notarization to accept it.
    echo "==> Signing DMG"
    codesign --force --timestamp \
        --keychain "$KEYCHAIN_PATH" \
        --sign "$IDENTITY" \
        "$TARGET"

    codesign --verify --verbose=2 "$TARGET"
fi

# --- notarize ------------------------------------------------------
# notarytool will not accept a raw .app — it has to be wrapped in a
# zip, dmg, or pkg. DMGs go in as-is.
SUBMIT_PATH="$TARGET"
if [[ "$KIND" == "app" ]]; then
    SUBMIT_PATH="$WORK_DIR/$(basename "$TARGET").zip"
    rm -f "$SUBMIT_PATH"
    ditto -c -k --keepParent "$TARGET" "$SUBMIT_PATH"
fi

echo "==> Submitting to Apple notary service: $SUBMIT_PATH"
# --wait blocks (typically 1–5 minutes for a small app). On reject
# the failure log lives behind the submission UUID; surface enough
# of it that the build log alone tells us what to fix.
if ! xcrun notarytool submit "$SUBMIT_PATH" \
        --apple-id "$MACOS_NOTARY_APPLE_ID" \
        --team-id "$MACOS_NOTARY_TEAM_ID" \
        --password "$VERNIER_SIGNING_PASSWORD" \
        --wait \
        --output-format json \
        > "$WORK_DIR/notary-result.json"; then
    echo "error: notarytool submit failed" >&2
    cat "$WORK_DIR/notary-result.json" >&2 || true
    exit 1
fi
cat "$WORK_DIR/notary-result.json"

NOTARY_STATUS="$(grep -o '"status":[^,}]*' "$WORK_DIR/notary-result.json" | head -1 | awk -F'"' '{print $4}')"
NOTARY_ID="$(grep -o '"id":[^,}]*' "$WORK_DIR/notary-result.json" | head -1 | awk -F'"' '{print $4}')"
if [[ "$NOTARY_STATUS" != "Accepted" ]]; then
    echo "error: notarization status: $NOTARY_STATUS" >&2
    if [[ -n "$NOTARY_ID" ]]; then
        echo "==> Fetching notary log for $NOTARY_ID" >&2
        xcrun notarytool log "$NOTARY_ID" \
            --apple-id "$MACOS_NOTARY_APPLE_ID" \
            --team-id "$MACOS_NOTARY_TEAM_ID" \
            --password "$VERNIER_SIGNING_PASSWORD" >&2 || true
    fi
    exit 1
fi

# --- staple --------------------------------------------------------
echo "==> Stapling notarization ticket"
xcrun stapler staple "$TARGET"
xcrun stapler validate "$TARGET"

if [[ "$KIND" == "app" ]]; then
    spctl -a -vvv -t exec "$TARGET"
fi

echo "Done: $TARGET signed, notarized, and stapled."
