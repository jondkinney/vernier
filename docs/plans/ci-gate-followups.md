# Plan: CI-gate follow-ups (lint debt + deprecations)

**Status:** draft

**Context:** Two changes in quick succession exposed a backlog of
code-quality debt:

1. Adding `ci.yml` — the per-PR clippy + fmt + test gate the project
   never had before. Before this, lint drift accumulated silently.
2. PR #15 (chore/macos-modernize) — modernizing the macOS code
   against current `objc2`, which surfaced a separate pile of macOS
   lints once macOS was in the CI matrix.

Both rounds were closed out with `#[allow]` annotations and one
`#![allow(deprecated)]` to get CI green *now*. This plan tracks the
proper fixes, **sorted by where the work actually lives** — because
despite the "macOS modernization" framing, most of these items are
cross-platform or Linux-only code that merely got *discovered* during
the macOS pass.

Quick map:

| Category | Items | Actually macOS? |
|---|---|---|
| A — macOS-specific | ScreenCaptureKit migration | yes |
| B — Linux/Wayland-specific | `large_enum_variant` on `Cmd` | no |
| C — Cross-platform (vernier-wide) | ChipSeg cfg-gating, `too_many_arguments` ×8, test coverage | no |
| D — CI / tooling | fmt on macOS | n/a |

---

## Category A — Genuinely macOS-specific

### A1. ScreenCaptureKit migration

**File:** `crates/vernier-platform/src/macos/capture.rs`
**Masked by:** module-level `#![allow(deprecated)]`
**Urgency:** medium — works today, breaks on a future macOS major
when Apple removes the symbols.

This is the **one item in this plan that is truly macOS work** and
the only one with an external clock on it.

#### What's deprecated

| Symbol | Role | Apple's replacement |
|---|---|---|
| `CGDisplayCreateImage(display_id)` | Capture one full display | `SCScreenshotManager` / `SCStream` (ScreenCaptureKit) |
| `CGWindowListCreateImage(bounds, opts, window_id, image_opts)` | Capture a region below a window — we use it to exclude our own overlay from the frozen frame | ScreenCaptureKit content filter |
| `CGBitmapInfo::ByteOrder32Little` | Pixel byte-order constant on the response | Pixel-format descriptor field on the new API |

#### Why it's non-trivial

The current caller is **synchronous**:

```rust
let image: CFRetained<CGImage> = match overlay_window_id_for(monitor) {
    Some(wid) => CGWindowListCreateImage(...)?,
    None      => CGDisplayCreateImage(display_id)?,
};
// ... immediately read bytes off the image
```

ScreenCaptureKit is **async + callback-heavy** by design: configure an
`SCStream` with an `SCContentFilter`, set a delegate, frames arrive on
the delegate's main-thread callback. There is no "give me one frame
right now" call.

#### Options

**A. Block-on-async wrapper (cheapest).** First call to
`capture_screen_native` spins up an `SCStream`, requests one frame,
blocks the caller on a `oneshot` channel until the delegate fires.
Run the blocking part on a worker thread so the main thread keeps
pumping AppKit events. Either tear the stream down each capture or
keep it alive behind a `OnceLock`.
- *Pros:* caller stays synchronous; minimal API change.
- *Cons:* SCK stream startup is ~50-100ms on first frame; a
  long-lived stream means tracking display/content changes.

**B. Refactor the caller to be async.** Push async out through
`freeze_axis_measurement`, the measure-mode toggle, etc. Introduces an
executor dependency vernier doesn't currently carry.
- *Pros:* idiomatic for SCK.
- *Cons:* large refactor that buys nothing beyond SCK compliance.

**C. Third-party crate** — [`screencapturekit`](https://crates.io/crates/screencapturekit)
(direct binding) or [`scap`](https://crates.io/crates/scap) (cross-platform).
- *Pros:* someone else maintains the delegate ceremony.
- *Cons:* another dep; macOS-only crate still needs cfg-gating.

#### Recommendation

Option **A** built on the `screencapturekit` crate. Evaluate moving to
a persistent stream only if first-frame latency becomes a real UX
problem in the freeze-frame path.

#### Pre-work

- [ ] Audit callers of `capture_screen_native`; confirm none are on a
      per-cursor-move hot path (should only be the freeze-frame path).
- [ ] Decide persistent vs. one-shot stream lifecycle.
- [ ] Spike: one `SCStream` call returning a frame on a branch.

#### Done-when

- `capture.rs` no longer needs `#![allow(deprecated)]`.
- `cargo clippy -D warnings` on macOS passes with the allow removed.
- Measure mode → freeze frame works on current macOS major.

---

## Category B — Linux / Wayland-specific

### B1. `large_enum_variant` on `enum Cmd`

**File:** `crates/vernier-platform/src/linux/wayland.rs:315`
**Masked by:** `#[allow(clippy::large_enum_variant)]` on the enum.
**Not a macOS issue at all.** `src/linux/` is `#[cfg(target_os =
"linux")]` — this file doesn't compile on macOS. The lint fires on
Linux clippy; it landed in PR #15's allow-list only because the new
CI gate caught it at the same time as the macOS lints.

`Cmd` has ~10 variants. The largest, `CreateOverlay`, carries a
`std::sync::mpsc::SyncSender<Result<WaylandOverlay>>` plus other
fields; every other variant pays that size on the stack/in the
channel.

**Fix:** extract the big variant's fields into a struct and `Box` it
inside the variant. Touches every `match cmd` arm in the file (~5-6
sites).

**Effort:** ~1 hour.
**Priority:** measure first. The `Cmd` channel isn't on a hot path —
if profiling says the size doesn't matter, the `#[allow]` is a fine
permanent answer. Don't refactor on principle alone.

---

## Category C — Cross-platform (vernier-wide quality)

These are shared code. The lints fire (or would fire) on every
platform; they are not macOS bugs. They appear in this doc only
because PR #15's `#[allow]` block bundled them in.

### C1. Linux-only `ChipSeg` variants — cfg-gate properly

**File:** `crates/vernier-ui/src/prefs.rs`
**Masked by:** `#[allow(dead_code)]` on `fn omarchy_font_available`
and `enum ChipSeg`.

The asymmetric one. The **code** is Linux-specific:
`ChipSeg::OmarchyLogo / Shift / Ctrl / Alt` and
`omarchy_font_available()` are only reached in
`#[cfg(not(target_os = "macos"))]` branches of
`shortcut_chip_segments` (macOS builds plain `ChipSeg::Letter`
segments with Unicode glyphs ⇧ ⌃ ⌥ ⌘ instead). But the **lint fires
on macOS**, where the variants are unreachable, and the **fix lives
in a shared file** (`prefs.rs` compiles on both platforms).

**Fix:** replace the blanket `#[allow(dead_code)]` with
`#[cfg(not(target_os = "macos"))]` on the variants, the function, and
the match arms that reference them:

```rust
#[derive(Clone, Debug)]
enum ChipSeg {
    Letter(String),
    #[cfg(not(target_os = "macos"))] OmarchyLogo,
    #[cfg(not(target_os = "macos"))] Shift,
    #[cfg(not(target_os = "macos"))] Ctrl,
    #[cfg(not(target_os = "macos"))] Alt,
    Enter,
    // ...
}
```

Every `match seg` block (~line 2440-2520) needs the same cfg gates on
those arms.

**Effort:** ~30 min — mechanical, spread across the file.
**Priority:** medium. Pays off the day `prefs.rs` grows a real
macOS-specific modifier variant — then the enum is already
cfg-structured to take it.

### C2. `too_many_arguments` × 8 functions

**Masked by:** `#[allow(clippy::too_many_arguments)]` at 8 sites.
All **cross-platform** code — the rasterizer, the geometry crate, the
main event loop. The lint fires identically on every platform.

| Function | File | Args |
|---|---|---|
| `shrink_to_content_with_bg` | `vernier-core/src/edge.rs` | 8 |
| `push_text_in_box` | `vernier-platform/src/hud_render.rs` | 8 |
| `draw_stuck_measurements` | `vernier-platform/src/hud_render.rs` | 10 |
| `draw_guides` | `vernier-platform/src/hud_render.rs` | 11 |
| `composite_glyph` | `vernier-platform/src/hud_render.rs` | 8 |
| `do_take_normal_screenshot` | `vernier-app/src/main.rs` | 18 |
| `toggle_measurement` | `vernier-app/src/main.rs` | 12 |
| `handle_pointer_button` | `vernier-app/src/main.rs` | 13 |

**Fix:** group related args into context structs, e.g.
`NudgeContext`, `PointerContext`, `ScreenshotContext`, a `DrawContext`
for the rasterizer functions.

**Effort:** ~half a day, mostly threading new structs through call
sites.
**Priority:** medium, incremental. Don't do all 8 at once — refactor
each function the next time it needs a new argument anyway. The
struct grows once instead of an 8-cycle cargo-fail-fix loop.

### C3. Real test coverage for `vernier-app` and `vernier-ui`

**Currently:** 0 tests in `vernier-app`, 0 in `vernier-ui`. The 28
tests in `vernier-core` cover algorithms; the 7 in `vernier-platform`
cover small utilities. Overlay rendering, hotkey routing, screen
capture, the prefs UI — no automated coverage.

**Not macOS-specific** and the largest open quality issue in the
project. Deserves its own plan doc; tracked here only for visibility.

Reasonable first target: smoke-test
`vernier-ui/src/prefs.rs::shortcut_chip_segments` (the function with
the most cfg branching) — exercise both the Linux and macOS code
paths via `#[cfg]`-conditional test cases. Doubles as a regression
guard for the C1 cfg-gating work.

---

## Category D — CI / tooling

### D1. Run `fmt` on macOS in CI

**File:** `.github/workflows/ci.yml`

`fmt` currently runs only on `ubuntu-latest`. Rustfmt is
platform-independent *for code both platforms compile* — but a
`#[cfg(target_os = "macos")]`-only block won't be seen by Linux's
`fmt --check` until someone edits an unrelated part of the same crate
on Linux. PR #15 hit exactly this: Linux fmt was clean while macOS
rustfmt wanted a different match-arm style for the macOS-only code.

**Fix:** add `macos-latest` to the `fmt` job (matrix or a second
job). ~30s of CI time.

**Effort:** 5 min.
**Priority:** quick win — removes a class of "clean on Linux, fails
on Mac" surprises. Bundle with the next CI tweak.

---

## Out of scope (tracked elsewhere)

- **mousehop `SendableDelegate` `#[allow(dead_code)]`** — a cosmetic
  PhantomData cleanup, but it lives in the *mousehop* repo, not
  vernier. Note it on the mousehop side when next touching
  `macos/app.rs` there. Not actionable from this repo.

---

## Recommended order

1. **D1 — fmt on macOS** (5 min) — quick win, stops future
   Linux-clean/Mac-dirty surprises.
2. **C1 — ChipSeg cfg-gating** (~30 min) — clears one `#[allow]`,
   mechanical.
3. **A1 — ScreenCaptureKit migration** (multi-session) — the only
   item with an external deadline. Start the spike early even if the
   full migration waits.
4. **B1 — `Cmd` Boxing** (~1 hour) — only after measuring that the
   variant size matters.
5. **C2 — `too_many_arguments`** (~half day) — incremental, do each
   function when it next needs a change.
6. **C3 — test coverage** (multi-session) — spin off into its own
   plan.

Only **A1** is genuinely macOS work. **B1** is Wayland. **C1/C2/C3**
are vernier-wide quality. **D1** is tooling. The "macOS modernization"
that birthed this list was really just the first time a CI gate
looked hard at the whole codebase.
