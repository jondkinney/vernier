#!/usr/bin/env bash
#
# install-local.sh — install the local dev build of vernier into the
# same system paths the AUR package uses, so testing the dev binary
# is identical to testing the AUR package.
#
# Mirrors the package() step of packaging/aur/PKGBUILD: drops the
# binary at /usr/bin/vernier, the .desktop at
# /usr/share/applications/, the hicolor apps/ tree (no status/ — the
# tray draws its own pixmap) at /usr/share/icons/hicolor/, and the
# two LICENSE files at /usr/share/licenses/vernier/. Refreshes the
# GTK icon cache afterwards.
#
# Refuses to run if the AUR `vernier` package is currently installed
# — you'd be fighting pacman over /usr/bin/vernier. `sudo pacman -R
# vernier` first.
#
# Usage:
#   packaging/install-local.sh                 # cargo build + install
#   packaging/install-local.sh --no-build      # use existing target/release/vernier
#   packaging/install-local.sh --uninstall     # remove the dev install
#   packaging/install-local.sh --restart       # also restart the daemon at the end

set -euo pipefail

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
cd "$REPO_ROOT"

MODE="install"
DO_BUILD=1
DO_RESTART=0

while (($#)); do
    case "$1" in
        --uninstall) MODE="uninstall"; shift ;;
        --no-build)  DO_BUILD=0; shift ;;
        --restart)   DO_RESTART=1; shift ;;
        -h|--help)
            # Print the leading comment block.
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0 ;;
        *) echo "unknown flag: $1" >&2; exit 2 ;;
    esac
done

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }

# --- conflict check ----------------------------------------------------------

if pacman -Qq vernier >/dev/null 2>&1; then
    cat >&2 <<EOF
The AUR \`vernier\` package is currently installed via pacman. This
script overwrites /usr/bin/vernier directly, which would leave
pacman's database out of sync with the file. Remove the pacman
package first:

    sudo pacman -R vernier

Then re-run: $0 ${*:-}
EOF
    exit 1
fi

stop_daemon() {
    if pgrep -af '(^|/)vernier( |$)' | grep -qv "^$$"; then
        say "stopping running daemon"
        if command -v vernier >/dev/null 2>&1; then
            vernier quit 2>/dev/null || true
        fi
        # Final fallback for stragglers (don't trust the IPC quit for
        # a stuck daemon).
        pkill -x vernier 2>/dev/null || true
        sleep 1
    fi
}

# --- main --------------------------------------------------------------------

# Trigger the sudo prompt up front so the install/uninstall steps run
# without re-prompting halfway through.
say "requesting sudo (cached for the rest of the script)"
sudo -v

case "$MODE" in
    install)
        if (( DO_BUILD )); then
            say "cargo build --release --bin vernier"
            cargo build --release --bin vernier
        else
            [[ -x target/release/vernier ]] || die "no target/release/vernier (drop --no-build or run cargo build first)"
        fi
        stop_daemon
        say "installing into /usr/bin, /usr/share/..."
        sudo install -Dm755 target/release/vernier             /usr/bin/vernier
        sudo install -Dm644 packaging/vernier.desktop          /usr/share/applications/vernier.desktop
        sudo install -d /usr/share/icons/hicolor
        sudo cp -r assets/icons/hicolor/.                      /usr/share/icons/hicolor/
        # Match PKGBUILD: status/ icons stay out, tray pixmap covers it.
        sudo rm -rf /usr/share/icons/hicolor/*/status
        sudo install -Dm644 LICENSE-MIT                        /usr/share/licenses/vernier/LICENSE-MIT
        sudo install -Dm644 LICENSE-APACHE                     /usr/share/licenses/vernier/LICENSE-APACHE
        # Best-effort icon cache refresh (gtk-update-icon-cache is
        # part of gtk-update-icon-cache or gtk3/gtk4; if missing,
        # menu lookups still work via the fallback search).
        if command -v gtk-update-icon-cache >/dev/null 2>&1; then
            sudo gtk-update-icon-cache --quiet --force /usr/share/icons/hicolor 2>/dev/null || true
        fi
        VER="$(/usr/bin/vernier --version 2>/dev/null || echo unknown)"
        say "installed: /usr/bin/vernier  ($VER)"
        if (( DO_RESTART )); then
            say "starting fresh daemon"
            ( setsid /usr/bin/vernier >/dev/null 2>&1 < /dev/null & )
        else
            say "run \`vernier\` (or pick Vernier from the app menu) to launch"
        fi
        ;;

    uninstall)
        stop_daemon
        say "removing files installed by this script"
        sudo rm -f /usr/bin/vernier
        sudo rm -f /usr/share/applications/vernier.desktop
        sudo rm -rf /usr/share/licenses/vernier
        for size in 16x16 22x22 24x24 32x32 48x48 64x64 128x128 256x256 512x512 scalable; do
            sudo rm -f "/usr/share/icons/hicolor/$size/apps/vernier".{png,svg} 2>/dev/null || true
        done
        if command -v gtk-update-icon-cache >/dev/null 2>&1; then
            sudo gtk-update-icon-cache --quiet --force /usr/share/icons/hicolor 2>/dev/null || true
        fi
        say "uninstalled. (~/.config/vernier/ and ~/.config/autostart/vernier.desktop left intact)"
        ;;
esac
