# Releasing vernier

Releases are automated by [release-plz](https://release-plz.dev) — there
is no release script to run.

## Cutting a release

1. Land changes on `main` using [Conventional Commits](https://www.conventionalcommits.org)
   (`feat:`, `fix:`, `feat!:` …) — release-plz reads them to pick the
   next version.
2. release-plz keeps a **release PR** open that bumps the workspace
   version and updates `CHANGELOG.md`. Review it.
3. **Merge the release PR.** release-plz then:
   - publishes `vernier-rs`, `vernier-rs-core`, `vernier-rs-platform`,
     `vernier-rs-ui` to crates.io in dependency order;
   - creates the `vX.Y.Z` tag and the GitHub Release.
4. The GitHub Release fires the packaging workflows automatically:
   - `release-x86_64.yml` — Linux x86_64 tarball.
   - `release-aarch64.yml` — Linux aarch64 tarball.
   - `release-macos.yml` — signed/notarized aarch64 macOS DMG.
   - `release-update-site.yml` — bumps the version on `usevernier.com`
     (the `vernier-site` repo).
   - `aur-publish.yml` — pushes all three AUR variants
     (`vernier`, `vernier-bin`, `vernier-git`). `vernier-bin` waits
     up to ~30 min for the Linux tarballs to land on the Release
     before pinning per-arch sha256sums.

No local steps, no `release.sh`.

## One-time setup

Repository secrets (Settings → Secrets and variables → Actions):

| Secret | Purpose |
| --- | --- |
| `RELEASE_PLZ_TOKEN` | Fine-grained PAT (`contents: write`, `pull-requests: write`). Required so release-plz's tag/Release events trigger the packaging workflows — the default `GITHUB_TOKEN` cannot. |
| `CARGO_REGISTRY_TOKEN` | crates.io token scoped to publish the `vernier-rs*` crates. |
| `AUR_SSH_KEY` | Private SSH key registered with the AUR account that maintains the `vernier*` packages. |
| `MACOS_CERTIFICATE_P12_BASE64`, `MACOS_CERTIFICATE_PASSWORD`, `MACOS_NOTARY_APPLE_ID`, `MACOS_NOTARY_TEAM_ID`, `VERNIER_SIGNING_PASSWORD` | macOS signing + notarization. Without them the macOS build falls back to ad-hoc signing. |

## The AUR packages

`packaging/aur/PKGBUILD`, `packaging/aur-bin/PKGBUILD`, and
`packaging/aur-git/PKGBUILD` are the source of truth.
`aur-publish.yml` copies each one, pins per-release fields, regenerates
`.SRCINFO`, and pushes to the matching AUR repo
(`ssh://aur@aur.archlinux.org/vernier{,-bin,-git}.git`). Edit
`depends`, `package()`, etc. in `packaging/aur*/PKGBUILD`; never edit
the AUR repos directly.

## Re-running a step

Every packaging workflow has a `workflow_dispatch` trigger with a `tag`
input so a failed build or AUR push can be re-run by hand against an
existing release tag:

```sh
gh workflow run aur-publish.yml -f tag=vX.Y.Z
gh workflow run release-x86_64.yml -f tag=vX.Y.Z
gh workflow run release-aarch64.yml -f tag=vX.Y.Z
gh workflow run release-macos.yml -f tag=vX.Y.Z
```
