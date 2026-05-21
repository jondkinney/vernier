# Plan: macOS modernization follow-ups

**Status:** draft, queued behind PR #15

**Context:** PR #15 (chore/macos-modernize) cleaned up the macOS code
against current `objc2` / `objc2-core-graphics` (~75 unsafe blocks
removed, CGImage free-fn → associated-fn migration, NSApplication
activation API swap). It left behind a small number of deliberate
`#[allow]` annotations representing real work we punted on. This
plan covers what's left, with a recommended order to tackle them.

---

## 1. ScreenCaptureKit migration (the big one)

**Files:** `crates/vernier-platform/src/macos/capture.rs`
**Currently masked by:** module-level `#![allow(deprecated)]`
**Severity:** medium urgency — code works today but will break on
a future macOS release when Apple actually removes the symbols.

### What's deprecated

| Symbol | What it does | Apple's replacement |
|---|---|---|
| `CGDisplayCreateImage(display_id)` | Capture one full display | `SCScreenshotManager` / `SCStream` (ScreenCaptureKit) |
| `CGWindowListCreateImage(bounds, options, window_id, image_options)` | Capture a region below a window (we use this to exclude our own overlay) | Same — ScreenCaptureKit's content filter |
| `CGBitmapInfo::ByteOrder32Little` | Pixel byte-order constant in the response | A bit field on the new API's pixel format descriptor |

### Why this is non-trivial

The current caller is **synchronous**:

```rust
let image: CFRetained<CGImage> = match overlay_window_id_for(monitor) {
    Some(wid) => CGWindowListCreateImage(...)?,
    None => CGDisplayCreateImage(display_id)?,
};
// ... immediately read bytes off the image
```

ScreenCaptureKit is **async + callback-heavy** by design. You configure
an `SCStream` with an `SCContentFilter`, set a delegate, and frames
arrive on the delegate's main-thread callback. There is no "give me
one frame right now" call.

### Options

**A. Block-on-async wrapper (cheapest).** Spin up the SCStream the
first time `capture_screen_native` is called, request one frame, block
the caller on a `oneshot::channel` until the delegate fires. Tear
down the stream after each capture, or keep it alive between captures
behind a `OnceLock`. The blocking part runs on a worker thread so the
main thread keeps pumping AppKit events.

Pros: minimal API surface change. Caller stays sync.
Cons: ScreenCaptureKit isn't really designed for one-shot use — the
stream startup cost is noticeable (~50-100ms on first frame). Keeping
a long-lived stream means tracking display/content changes.

**B. Refactor the caller to be async.** Push the async-ness all the
way out: `freeze_axis_measurement` becomes async, the measure-mode
toggle becomes async, etc. Tokio is already in the deps tree (via
`mousehop-ipc` … wait, that's mousehop. Vernier doesn't have it).
This is the bigger swing.

Pros: idiomatic; matches the SCK design.
Cons: large refactor; introduces an executor dep; doesn't actually
buy us anything beyond ScreenCaptureKit compliance.

**C. Use [`scap`](https://crates.io/crates/scap) or
[`screencapturekit`](https://crates.io/crates/screencapturekit) crates.**
Third-party wrappers exist. `scap` is cross-platform; `screencapturekit`
is a more direct Rust binding.

Pros: someone else maintains the SCK plumbing.
Cons: another dep; we'd inherit their API design choices; macOS-only
crate means it needs cfg-gating like the current direct CG calls.

### Recommendation

Start with **option A** + the `screencapturekit` crate (which already
abstracts the delegate ceremony). If startup latency becomes a real
UX problem, evaluate moving to a persistent stream.

### Pre-work

- [ ] Audit who calls `capture_screen_native` and what their latency
      budget looks like. If any caller is "fire on every cursor move"
      (it shouldn't be — that's the freeze-frame path only), block-on
      will hurt.
- [ ] Decide on persistent vs. one-shot stream lifecycle.
- [ ] Spike: get a single SCStream call returning a frame inside a
      branch.

### Done-when

- `capture.rs` no longer needs `#![allow(deprecated)]`.
- `cargo clippy -D warnings` on macOS still passes after the allow is
  removed.
- Manual capture (measure mode → freeze frame) works on Sequoia 15.x
  and whatever the current macOS major is at the time.

---

## 2. `SendableDelegate` PhantomData cleanup

**File:** `crates/vernier-platform/src/macos/app.rs` (wait — this is
actually mousehop's app.rs, not vernier's. Move this plan item to a
mousehop-side TODO when we get to it.)

**Currently masked by:** `#[allow(dead_code)]` on the tuple field.

The struct exists purely to keep a `Retained<VernierAppDelegate>`
alive for the process lifetime. The field is never read. We could
make the intent explicit with `PhantomData` + a documented `_keep`
field, or use a different lifetime-extension pattern (a `Box::leak`
on the Retained, etc.). Pure cosmetic; no behavior change.

**Effort:** 5 minutes.
**Priority:** lowest. Do this when touching the file for another reason.

---

## 3. Linux-only ChipSeg variants — cfg-gate properly

**File:** `crates/vernier-ui/src/prefs.rs`
**Currently masked by:** `#[allow(dead_code)]` on the function and
the `enum ChipSeg`.

The lint is real — on macOS, `ChipSeg::OmarchyLogo / Shift / Ctrl /
Alt` and `fn omarchy_font_available()` are genuinely unreachable. The
cleaner fix is to `#[cfg(not(target_os = "macos"))]` the items and
the match arms that produce them. The match arms downstream are
unconditional (line ~2440-2520), so this requires sprinkling cfg
attrs on each affected arm — invasive but not difficult.

### Sketch

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

Then every `match seg` block needs the same cfg gates on those arms,
or a single catch-all `#[cfg(target_os = "macos")] _ => unreachable!()`.

**Effort:** ~30 minutes — mechanical but spread across the file.
**Priority:** medium. Pays off if `prefs.rs` ever grows real macOS
modifier handling, since we'd then `#[cfg]` in a Mac-specific variant.

---

## 4. `clippy::large_enum_variant` on `enum Cmd`

**File:** `crates/vernier-platform/src/linux/wayland.rs:315`

This is a **Linux-side** issue, not macOS. Listed here only because
PR #15's allow-list bundled it. Tracked separately when we get to a
Wayland-side cleanup pass.

**Sketch of the fix:** the `CreateOverlay` variant carries an
`std::sync::mpsc::SyncSender<Result<WaylandOverlay>>` plus other
small fields. The whole enum's size is forced to match the largest
variant on every value. `Box<CreateOverlayArgs>` would shrink it.

Touches every `match cmd` arm in the same file.

**Effort:** ~1 hour.
**Priority:** measure first. The Cmd channel isn't on a hot path; if
profiling shows it doesn't matter, leave the allow.

---

## 5. `#[allow(clippy::too_many_arguments)]` × 8

**Files:** `crates/vernier-core/src/edge.rs`,
`crates/vernier-platform/src/hud_render.rs`, `crates/vernier-app/src/main.rs`

Functions at 8–18 args. Largely rendering pipelines and event
handlers that take many small primitives. The honest fix is to group
related args into structs:

| Function | Args | Likely struct |
|---|---|---|
| `apply_nudge_step` | 18 | `NudgeContext { mode, overlay, frozen_frame, ... }` |
| `handle_pointer_button` | 13 | `PointerContext { ... }` |
| `do_take_normal_screenshot` | 18 | `ScreenshotContext` |
| `draw_guides` / `draw_stuck_measurements` / `composite_glyph` | 8-11 | `DrawContext { canvas, scale, transforms, ... }` |

This is **vernier-cross-platform** work, not macOS-specific. Listed
here because it was bundled in the macOS-modernize allow-list.

**Effort:** ~half a day, mostly threading the new structs through
call sites.
**Priority:** medium. Pays off when one of these functions next needs
a new argument — the struct grows once, not 8 cargo-check-fail-fix
cycles.

---

## 6. CI: fmt on macOS

**File:** `.github/workflows/ci.yml`

Currently `fmt` runs only on `ubuntu-latest`. Rustfmt is
platform-independent, so this is *mostly* fine — but a macOS-only
code path (anything under `#[cfg(target_os = "macos")]`) won't get
seen by Linux's `fmt --check`. PR #15 hit this in real life: my fmt
checks on Linux were clean but Mac's rustfmt wanted a different style
for the macOS-only code I'd just added.

### Two options

**A. Add `runs-on: macos-latest` to the existing fmt job matrix.**
~30s of CI time. Cheapest.

**B. Run fmt only on Linux, but require devs to run `cargo fmt`
before pushing macOS-specific code.** Already the case — Linux fmt
DOES eventually flag macOS-cfg formatting drift the next time someone
touches an unrelated file in the same crate. The gap is "macOS
fmt drift visible only when editing on a Mac for a Mac-only file."

### Recommendation

Do A. The cost is trivial and it removes a class of "looks clean on
Linux, fails on Mac" surprises.

**Effort:** 5 minutes.
**Priority:** quick win. Bundle with the next CI tweak.

---

## 7. Real test coverage for `vernier-app` and `vernier-ui`

**Currently:** 0 tests in `vernier-app`, 0 tests in `vernier-ui`.
The 28 tests in `vernier-core` cover algorithms (edge detection,
geometry). The 7 in `vernier-platform` cover small utilities. The
big behavior — overlay rendering, hotkey routing, screen capture, the
prefs UI — has no automated coverage.

This is **not macOS-specific** and is the largest open quality issue
in the project. Out of scope for this plan; tracked here for visibility.

A reasonable starting point: smoke-test the prefs UI's keyboard
shortcut parsing in `vernier-ui/src/prefs.rs::shortcut_chip_segments`
(the function with the most cfg branching) — exercise both
Linux and macOS code paths in one test using `#[cfg]`-conditional
test cases.

---

## Recommended order

1. **CI fmt on macOS** (5 min, quick win, prevents future regressions)
2. **Linux-only ChipSeg cfg-gate** (~30 min, clears one allow)
3. **ScreenCaptureKit migration** (multi-session, the actually
   urgent piece — Apple's clock is ticking)
4. **`large_enum_variant` Boxing** (~1 hour, after measuring whether
   it matters)
5. **`too_many_arguments` refactor** (~half day, do incrementally
   when each function next needs a change)
6. **Tests for vernier-app/vernier-ui** (multi-session, separate plan)

Items 1, 2, 4, 6 are all cross-platform and aren't really "macOS"
work despite being captured here. Item 3 is the macOS-specific
piece that matters.
