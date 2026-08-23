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
   - `release.yml` — builds the Linux x86_64 tarball, the Linux
     aarch64 tarball, and the signed/notarized aarch64 macOS DMG.
     Each platform is a separate job; one failure doesn't block the
     others.
   - `release-flatpak.yml` — builds the Flatpak bundle.
   - `release-update-site.yml` — bumps the version on `usevernier.com`
     (the `vernier-site` repo).
   - `aur-publish.yml` — pushes all three AUR variants
     (`vernier`, `vernier-bin`, `vernier-git`). `vernier-bin` waits
     up to ~30 min for the Linux tarballs to land on the Release
     before pinning per-arch sha256sums.

The native release has no local steps and no `release.sh`. Figma plugin changes
also follow the manual publication lane below.

## Publishing Figma plugin changes

Figma plugin publication is a separate manual lane because Figma only permits
plugin registration and publication from its macOS or Windows desktop app.
End-to-end Vernier integration testing must use macOS because Vernier does not
ship a Windows runtime. When a release changes `figma-plugin/`:

1. Check out the exact commit being released on a Mac with the matching Vernier
   release build installed.
2. Import or locate `figma-plugin/manifest.json` in Figma Desktop and run the
   plugin in both Design and Dev Mode.
3. Verify the connection stays live at unchanged zoom for at least 30 seconds,
   reconnects after Vernier restarts, and reports correctly at 50%, 100%, and
   200% zoom. Repeat at Retina 2× display scale.
4. In **Plugins → Manage plugins**, publish a new version and include the
   relevant Vernier release notes.
5. Verify the [Community
   listing](https://www.figma.com/community/plugin/1673041143009172236/vernier-bridge)
   from Figma Web under Hyprland after publication, including switching
   between two files at different zooms and testing a fractional Wayland
   display scale where available.

The first publication also requires Figma account two-factor authentication,
listing artwork and security disclosures, and Community review. The disclosure
must state that Vernier Bridge sends viewport zoom, editor type, and active-tab
state only to the loopback Vernier process at `localhost:8765`; it does not read
or modify the Figma document.

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
gh workflow run release.yml -f tag=vX.Y.Z
gh workflow run release-flatpak.yml -f tag=vX.Y.Z
```
