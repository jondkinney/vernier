#!/usr/bin/env bash
#
# release.sh — cut a new vernier release and publish it to the AUR.
#
# Run from a clean working tree on `main`. Bumps Cargo.toml, commits,
# pushes the tag to GitHub, refreshes the PKGBUILD's pkgver + sha256
# against the new tarball, regenerates .SRCINFO, and pushes the
# updated package to ssh://aur@aur.archlinux.org/vernier.git.
#
# Usage:
#   packaging/aur/release.sh 0.2.0
#   packaging/aur/release.sh 0.2.0 --aur-dir /tmp/aur-vernier
#
# Flags:
#   --aur-dir PATH   local clone of the AUR repo to push from
#                    (default: $TMPDIR/aur-vernier, cloned fresh)
#   --dry-run        print every step but don't touch anything
#   --skip-push      do the bump + tag locally but don't push to
#                    GitHub or the AUR

set -euo pipefail

#--- arg parsing ---------------------------------------------------------------

DRY_RUN=0
SKIP_PUSH=0
AUR_DIR=""
NEW_VER=""

while (($#)); do
    case "$1" in
        --aur-dir)   AUR_DIR="$2"; shift 2 ;;
        --dry-run)   DRY_RUN=1; shift ;;
        --skip-push) SKIP_PUSH=1; shift ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0 ;;
        --) shift; break ;;
        -*) echo "unknown flag: $1" >&2; exit 2 ;;
        *)  if [[ -z "$NEW_VER" ]]; then NEW_VER="$1"; shift
            else echo "extra arg: $1" >&2; exit 2; fi ;;
    esac
done

if [[ -z "$NEW_VER" ]]; then
    echo "usage: $0 <new-version> [--aur-dir PATH] [--dry-run] [--skip-push]" >&2
    exit 2
fi

if ! [[ "$NEW_VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$ ]]; then
    echo "version '$NEW_VER' doesn't look like semver (e.g. 0.2.0 or 0.2.0-rc1)" >&2
    exit 2
fi

#--- locate repo + helpers -----------------------------------------------------

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
PKGBUILD_SRC="$REPO_ROOT/packaging/aur/PKGBUILD"
PKGNAME="vernier"
TAG="v$NEW_VER"
AUR_REMOTE="ssh://aur@aur.archlinux.org/${PKGNAME}.git"
TARBALL_URL="https://github.com/jondkinney/vernier/archive/refs/tags/${TAG}.tar.gz"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }
run()  {
    printf '   \033[2m$\033[0m %s\n' "$*"
    if (( ! DRY_RUN )); then "$@"; fi
}

cd "$REPO_ROOT"

#--- precondition checks -------------------------------------------------------

say "checking preconditions"

[[ -f "$PKGBUILD_SRC" ]] || die "no PKGBUILD at $PKGBUILD_SRC"

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[[ "$BRANCH" == "main" ]] || warn "current branch is '$BRANCH', not 'main' — proceeding anyway"

if [[ -n "$(git status --porcelain)" ]] && (( ! DRY_RUN )); then
    git status --short
    die "working tree has uncommitted changes — clean it first"
fi

if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    die "tag $TAG already exists locally"
fi

CUR_VER="$(awk -F\" '/^version = "/{print $2; exit}' Cargo.toml)"
say "Cargo.toml currently at $CUR_VER → bumping to $NEW_VER"

#--- bump Cargo.toml + refresh Cargo.lock --------------------------------------

if (( ! DRY_RUN )); then
    sed -i -E "0,/^version = \"[0-9.]+\"$/{s//version = \"$NEW_VER\"/}" Cargo.toml
fi
say "running cargo build to refresh Cargo.lock"
run cargo build --quiet --release

run git add Cargo.toml Cargo.lock
run git commit -m "Bump version to $NEW_VER"

#--- tag + push to GitHub ------------------------------------------------------

run git tag -a "$TAG" -m "$TAG"

if (( SKIP_PUSH )); then
    warn "--skip-push set; bump committed locally only. Stopping before GitHub push."
    exit 0
fi

run git push origin main
run git push origin "$TAG"

#--- wait for the GitHub tarball + compute sha256 ------------------------------

say "waiting for GitHub to serve the $TAG tarball"
SHA=""
for attempt in 1 2 3 4 5 6 7 8 9 10; do
    if (( DRY_RUN )); then SHA="dryrun$(printf '%064x' $attempt | head -c 64)"; break; fi
    if curl -sfL "$TARBALL_URL" -o "/tmp/${PKGNAME}-${NEW_VER}.tar.gz"; then
        SHA="$(sha256sum "/tmp/${PKGNAME}-${NEW_VER}.tar.gz" | awk '{print $1}')"
        break
    fi
    sleep 3
done
[[ -n "$SHA" ]] || die "couldn't fetch $TARBALL_URL after 10 tries"
say "sha256 = $SHA"

#--- rewrite the in-repo PKGBUILD ----------------------------------------------

say "updating $PKGBUILD_SRC"
if (( ! DRY_RUN )); then
    sed -i -E \
        -e "s/^pkgver=.*/pkgver=$NEW_VER/" \
        -e "s/^pkgrel=.*/pkgrel=1/" \
        -e "s/^sha256sums=\\(.*\\)$/sha256sums=('$SHA')/" \
        "$PKGBUILD_SRC"
fi
run git add "$PKGBUILD_SRC"
run git commit -m "PKGBUILD: pin the $TAG source tarball sha256"
run git push origin main

#--- sync into a local AUR clone + push ----------------------------------------

if [[ -z "$AUR_DIR" ]]; then
    AUR_DIR="${TMPDIR:-/tmp}/aur-${PKGNAME}-$$"
    say "cloning AUR repo to $AUR_DIR"
    run git clone "$AUR_REMOTE" "$AUR_DIR"
else
    say "using existing AUR clone at $AUR_DIR"
    [[ -d "$AUR_DIR/.git" ]] || die "$AUR_DIR isn't a git checkout"
    run git -C "$AUR_DIR" fetch --quiet origin
    run git -C "$AUR_DIR" checkout master
    run git -C "$AUR_DIR" reset --hard origin/master
fi

run cp "$PKGBUILD_SRC" "$AUR_DIR/PKGBUILD"
if (( ! DRY_RUN )); then
    ( cd "$AUR_DIR" && makepkg --printsrcinfo > .SRCINFO )
fi
say "wrote .SRCINFO"

if (( ! DRY_RUN )); then
    cd "$AUR_DIR"
    if [[ -z "$(git status --porcelain)" ]]; then
        warn "no changes in AUR repo — nothing to push"
        exit 0
    fi
fi
run git -C "$AUR_DIR" add PKGBUILD .SRCINFO
run git -C "$AUR_DIR" commit -m "Upgrade to ${NEW_VER}-1"
run git -C "$AUR_DIR" push origin master

say "done. https://aur.archlinux.org/packages/${PKGNAME} should refresh within a few minutes."
