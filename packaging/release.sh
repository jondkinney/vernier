#!/usr/bin/env bash
#
# release.sh — cut a new vernier release: GitHub Release, all three
# AUR variants (vernier, vernier-bin, vernier-git), and the crates.io
# crates.
#
# Run from a clean working tree on `main`. The script:
#   1. Bumps Cargo.toml + Cargo.lock, commits, tags.
#   2. Pushes the bump commit + tag to GitHub.
#   3. Creates the GitHub Release for the tag. Publishing it triggers
#      the release-x86_64 / -aarch64 / -macos workflows, which build
#      and upload every binary artifact.
#   4. Waits for the CI-built x86_64 + aarch64 tarballs, then pins
#      their (and the source tarball's) sha256s into the in-repo
#      PKGBUILDs, refreshes the -git pkgver placeholder, commits,
#      pushes.
#   5. Syncs PKGBUILD + .SRCINFO to each of three AUR repos
#      (ssh://aur@aur.archlinux.org/{vernier,vernier-bin,vernier-git}.git).
#   6. Publishes the four crates to crates.io.
#
# The Linux tarballs are built by CI, never here: the release-x86_64
# workflow re-uploads the x86_64 tarball with --clobber, so a tarball
# built locally would just be replaced — leaving the PKGBUILD
# checksum describing a file nobody downloads.
#
# Usage:
#   packaging/release.sh 0.2.0
#   packaging/release.sh 0.2.0 --dry-run
#   packaging/release.sh 0.2.0 --skip-push
#
# Flags:
#   --dry-run     print every step but don't touch anything
#   --skip-push   commit + tag locally, then stop (no GitHub, AUR, or
#                 crates.io push)

set -euo pipefail

#--- arg parsing ---------------------------------------------------------------

DRY_RUN=0
SKIP_PUSH=0
NEW_VER=""

while (($#)); do
    case "$1" in
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
    echo "usage: $0 <new-version> [--dry-run] [--skip-push]" >&2
    exit 2
fi

if ! [[ "$NEW_VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$ ]]; then
    echo "version '$NEW_VER' doesn't look like semver (e.g. 0.2.0 or 0.2.0-rc1)" >&2
    exit 2
fi

#--- helpers + paths -----------------------------------------------------------

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
PKGNAME="vernier"
GH_REPO="jondkinney/$PKGNAME"
TAG="v$NEW_VER"
TARBALL_URL="https://github.com/$GH_REPO/archive/refs/tags/${TAG}.tar.gz"
X86_NAME="$PKGNAME-$NEW_VER-x86_64.tar.gz"
ARM_NAME="$PKGNAME-$NEW_VER-aarch64.tar.gz"

# crates.io crates, in dependency order — each must publish before the
# next so the path/version deps resolve against a live dependency.
CRATES=(vernier-rs-core vernier-rs-platform vernier-rs-ui vernier-rs)

PKGBUILD_SRC="$REPO_ROOT/packaging/aur/PKGBUILD"
PKGBUILD_BIN="$REPO_ROOT/packaging/aur-bin/PKGBUILD"
PKGBUILD_GIT="$REPO_ROOT/packaging/aur-git/PKGBUILD"

say()  { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }
run()  {
    printf '   \033[2m$\033[0m %s\n' "$*"
    if (( ! DRY_RUN )); then "$@"; fi
}

# Poll the Release for a CI-built asset; echo its sha256 once it
# lands. 40 × 30s = up to 20 min — the aarch64 cross-build is the slow
# one and typically lands within ~10 min.
wait_for_asset_sha() {
    local name="$1" url dest attempt
    url="https://github.com/$GH_REPO/releases/download/$TAG/$name"
    dest="/tmp/$name"
    for attempt in $(seq 1 40); do
        if (( DRY_RUN )); then echo "dryrun$(printf '%064x' 0 | head -c 64)"; return 0; fi
        if curl -sfL "$url" -o "$dest" 2>/dev/null; then
            sha256sum "$dest" | awk '{print $1}'
            return 0
        fi
        sleep 30
    done
    return 1
}

cd "$REPO_ROOT"

#--- precondition checks -------------------------------------------------------

say "checking preconditions"

[[ -f "$PKGBUILD_SRC" ]] || die "no PKGBUILD at $PKGBUILD_SRC"
[[ -f "$PKGBUILD_BIN" ]] || die "no PKGBUILD at $PKGBUILD_BIN"
[[ -f "$PKGBUILD_GIT" ]] || die "no PKGBUILD at $PKGBUILD_GIT"
command -v gh    >/dev/null || die "gh CLI not installed"
command -v curl  >/dev/null || die "curl not installed"
command -v cargo >/dev/null || die "cargo not installed"
if (( ! SKIP_PUSH )) && (( ! DRY_RUN )); then
    gh auth status >/dev/null 2>&1 || die "gh not authenticated; run 'gh auth login'"
    cargo_home="${CARGO_HOME:-$HOME/.cargo}"
    [[ -f "$cargo_home/credentials.toml" || -f "$cargo_home/credentials" \
       || -n "${CARGO_REGISTRY_TOKEN:-}" ]] \
        || die "no crates.io token; run 'cargo login' or set CARGO_REGISTRY_TOKEN"
fi

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
    # [workspace.package] version — the first `version = "..."` line.
    sed -i -E "0,/^version = \"[0-9.]+\"$/{s//version = \"$NEW_VER\"/}" Cargo.toml
    # [workspace.dependencies] version pins on the in-workspace path
    # deps (the `vernier-rs-*` lines). A crate published at $NEW_VER
    # must require its siblings at $NEW_VER too — a stale pin lets the
    # published manifest resolve an older sibling that may lack APIs
    # this version depends on.
    sed -i -E "/package = \"vernier-rs-/ s/version = \"[^\"]+\"/version = \"$NEW_VER\"/" Cargo.toml
fi
# Build so Cargo.lock picks up the bumped workspace version and a
# broken release-profile compile is caught before we tag. The shipped
# x86_64 / aarch64 binaries are built by CI, not here — this build is
# only a sanity check.
say "running cargo build to refresh Cargo.lock + sanity-check the build"
run cargo build --quiet --release

run git add Cargo.toml Cargo.lock
run git commit -m "Bump version to $NEW_VER"

#--- tag + push to GitHub ------------------------------------------------------

run git tag -a "$TAG" -m "$TAG"

if (( SKIP_PUSH )); then
    warn "--skip-push set; bump committed locally only. Stopping before any push."
    exit 0
fi

run git push origin main
run git push origin "$TAG"

#--- wait for the GitHub source tarball + compute sha256 -----------------------

say "waiting for GitHub to serve the $TAG source tarball"
SRC_SHA=""
for attempt in 1 2 3 4 5 6 7 8 9 10; do
    if (( DRY_RUN )); then SRC_SHA="dryrun$(printf '%064x' $attempt | head -c 64)"; break; fi
    if curl -sfL "$TARBALL_URL" -o "/tmp/${PKGNAME}-${NEW_VER}.tar.gz"; then
        SRC_SHA="$(sha256sum "/tmp/${PKGNAME}-${NEW_VER}.tar.gz" | awk '{print $1}')"
        break
    fi
    sleep 3
done
[[ -n "$SRC_SHA" ]] || die "couldn't fetch $TARBALL_URL after 10 tries"
say "source sha256   = $SRC_SHA"

#--- create the GitHub Release (CI builds + uploads the binaries) --------------

# Publishing the Release triggers release-x86_64, release-aarch64, and
# release-macos. Each builds its artifact on a GitHub runner and
# uploads it here; we wait for the two Linux tarballs below and pin
# *their* sha256s. The Release is created with no assets so the only
# x86_64 / aarch64 tarballs that ever exist are the CI-built ones.
say "creating GitHub Release $TAG"
if (( ! DRY_RUN )) && gh release view "$TAG" --repo "$GH_REPO" >/dev/null 2>&1; then
    say "Release $TAG already exists — leaving it in place"
else
    run gh release create "$TAG" \
        --repo "$GH_REPO" \
        --title "$TAG" \
        --generate-notes
fi

#--- wait for the CI-built Linux tarballs + compute sha256s --------------------

say "waiting for the release workflows to upload the Linux tarballs"

BIN_SHA="$(wait_for_asset_sha "$X86_NAME")" || {
    warn "x86_64 asset never showed up — check"
    warn "  gh run list --workflow release-x86_64.yml --repo $GH_REPO"
    die "aborting before AUR push so aur-bin doesn't ship a stale x86_64 sha"
}
say "x86_64 sha256   = $BIN_SHA"

ARM_SHA="$(wait_for_asset_sha "$ARM_NAME")" || {
    warn "aarch64 asset never showed up — check"
    warn "  gh run list --workflow release-aarch64.yml --repo $GH_REPO"
    die "aborting before AUR push so aur-bin doesn't ship a stale aarch64 sha"
}
say "aarch64 sha256  = $ARM_SHA"

#--- rewrite all three in-repo PKGBUILDs --------------------------------------

GIT_PLACEHOLDER="$NEW_VER.r0.g$(git rev-parse --short=7 HEAD)"
say "updating PKGBUILDs (src=$NEW_VER, git placeholder=$GIT_PLACEHOLDER)"
if (( ! DRY_RUN )); then
    sed -i -E \
        -e "s/^pkgver=.*/pkgver=$NEW_VER/" \
        -e "s/^pkgrel=.*/pkgrel=1/" \
        -e "s/^sha256sums=\\(.*\\)$/sha256sums=('$SRC_SHA')/" \
        "$PKGBUILD_SRC"

    sed -i -E \
        -e "s/^pkgver=.*/pkgver=$NEW_VER/" \
        -e "s/^pkgrel=.*/pkgrel=1/" \
        -e "s/^sha256sums_x86_64=\\(.*\\)$/sha256sums_x86_64=('$BIN_SHA')/" \
        -e "s/^sha256sums_aarch64=\\(.*\\)$/sha256sums_aarch64=('$ARM_SHA')/" \
        "$PKGBUILD_BIN"

    sed -i -E \
        -e "s/^pkgver=.*/pkgver=$GIT_PLACEHOLDER/" \
        -e "s/^pkgrel=.*/pkgrel=1/" \
        "$PKGBUILD_GIT"
fi
run git add "$PKGBUILD_SRC" "$PKGBUILD_BIN" "$PKGBUILD_GIT"
run git commit -m "PKGBUILDs: pin $TAG (source + prebuilt sha256, -git placeholder)"
run git push origin main

#--- sync each variant into its AUR repo --------------------------------------

push_aur() {
    local aur_pkg="$1" pkgbuild_src="$2"
    local dir
    dir="$(mktemp -d)/aur-$aur_pkg"
    say "syncing $aur_pkg → AUR"
    run git clone "ssh://aur@aur.archlinux.org/$aur_pkg.git" "$dir"
    run cp "$pkgbuild_src" "$dir/PKGBUILD"
    if (( ! DRY_RUN )); then
        ( cd "$dir" && makepkg --printsrcinfo > .SRCINFO )
        if [[ -z "$(git -C "$dir" status --porcelain)" ]]; then
            warn "no changes in $aur_pkg AUR repo — skipping push"
            return
        fi
    fi
    run git -C "$dir" add PKGBUILD .SRCINFO
    run git -C "$dir" commit -m "Upgrade to ${NEW_VER}-1"
    run git -C "$dir" push origin master
}

push_aur "$PKGNAME"      "$PKGBUILD_SRC"
push_aur "$PKGNAME-bin"  "$PKGBUILD_BIN"
push_aur "$PKGNAME-git"  "$PKGBUILD_GIT"

#--- publish the crates to crates.io ------------------------------------------

# Bottom-up so each crate resolves against an already-published
# dependency. `cargo publish` blocks until the crate is queryable on
# the index, so the next one in the list can build against it.
say "publishing crates to crates.io"
for crate in "${CRATES[@]}"; do
    run cargo publish -p "$crate"
done

say "done."
say "  https://aur.archlinux.org/packages/$PKGNAME"
say "  https://aur.archlinux.org/packages/$PKGNAME-bin"
say "  https://aur.archlinux.org/packages/$PKGNAME-git"
say "  https://crates.io/crates/vernier-rs"
