# AUR packaging

Reference PKGBUILDs for the three AUR packages we publish:

| AUR pkg        | This dir              | Source                                  | Build cost     | Who picks this                                    |
|----------------|-----------------------|-----------------------------------------|----------------|---------------------------------------------------|
| `vernier`      | `packaging/aur/`      | Tagged tarball from GitHub              | Full Rust build| Default. Trust-on-the-builder, reproducible.      |
| `vernier-bin`  | `packaging/aur-bin/`  | Prebuilt asset on the GitHub Release    | Just install   | Anyone who doesn't want to wait through cargo.    |
| `vernier-git`  | `packaging/aur-git/`  | `git+https://github.com/jondkinney/vernier.git` | Full Rust build | Power users who want the latest `main`.   |

All three set `provides=vernier` and `conflicts` against the other two, so
exactly one can be installed at a time.

This directory mirrors the PKGBUILDs that live in three separate AUR
repos (`ssh://aur@aur.archlinux.org/{vernier,vernier-bin,vernier-git}.git`)
so changes can land here first, be tested, and only then be pushed.

## Releasing a new version

One command does all of it — bump, build, tag, push, GitHub Release with
the prebuilt asset, AUR sync for all three variants:

```sh
packaging/aur/release.sh 0.2.0
```

Flags:

- `--dry-run` — print every step, change nothing.
- `--skip-push` — bump + commit + tag locally, then stop.

The script requires `gh` to be authenticated (`gh auth login`) and your
SSH key to be the one registered with `aur@aur.archlinux.org`.

If you'd rather do it by hand: bump `Cargo.toml`, `cargo build --release`,
commit + tag + push, then for each PKGBUILD update `pkgver`/`pkgrel`/`sha256sums`,
regenerate `.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO`, and push
to the corresponding AUR repo.

## First-time setup (AUR repos)

Each AUR repo gets created the first time you push to its ssh URL — the
AUR auto-allocates the repo if the package name isn't already taken.

```sh
for pkg in vernier vernier-bin vernier-git; do
    dir=$(mktemp -d)/aur-$pkg
    case $pkg in
        vernier)     src=packaging/aur/PKGBUILD ;;
        vernier-bin) src=packaging/aur-bin/PKGBUILD ;;
        vernier-git) src=packaging/aur-git/PKGBUILD ;;
    esac
    git clone "ssh://aur@aur.archlinux.org/$pkg.git" "$dir"
    cp "$src" "$dir/PKGBUILD"
    ( cd "$dir" && makepkg --printsrcinfo > .SRCINFO )
    git -C "$dir" add PKGBUILD .SRCINFO
    git -C "$dir" commit -m "Initial import: $pkg"
    git -C "$dir" push origin master
done
```

After the first push, `release.sh` handles the rest on every subsequent
version bump.
