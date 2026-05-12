# AUR packaging

Reference `PKGBUILD` for the `vernier` AUR package. The AUR repository itself
lives at `aur@aur.archlinux.org:vernier.git` — this directory mirrors the file
so changes can land here first and be tested before being pushed to the AUR.

## Releasing a new version

1. Bump `workspace.package.version` in the root `Cargo.toml` (and run
   `cargo build` to refresh `Cargo.lock`).
2. Commit and push.
3. Tag and push:
   ```sh
   git tag -a v0.1.0 -m 'v0.1.0'
   git push origin v0.1.0
   ```
   GitHub auto-generates a tarball at
   `https://github.com/jondkinney/vernier/archive/refs/tags/v0.1.0.tar.gz`.
4. Update `pkgver` (and bump `pkgrel` back to `1`) in the PKGBUILD.
5. Regenerate the source hash:
   ```sh
   cd packaging/aur
   updpkgsums            # rewrites sha256sums=
   makepkg --printsrcinfo > .SRCINFO
   makepkg -si           # smoke-test the build + install
   ```
6. Push the PKGBUILD and `.SRCINFO` to the AUR repo (`aur/vernier`).

## First-time setup (AUR repo)

```sh
# Clone the AUR repo (creates an empty one if the package name is free)
git clone ssh://aur@aur.archlinux.org/vernier.git aur-vernier
cd aur-vernier
cp ../vernier/packaging/aur/PKGBUILD .
updpkgsums
makepkg --printsrcinfo > .SRCINFO
git add PKGBUILD .SRCINFO
git commit -m 'Initial import: vernier 0.1.0'
git push origin master
```
