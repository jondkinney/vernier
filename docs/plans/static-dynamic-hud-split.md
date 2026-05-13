# Plan: Split HUD rasterization into static + dynamic layers

**Status:** draft, ready for review
**Goal:** Reduce per-frame HUD render cost so measure-mode feels native
on both macOS and Wayland, especially during live-mode capture.

---

## Problem

Every cursor move during measure mode triggers a `set_hud` call from
the daemon to the platform overlay. Both backends respond the same
way:

1. Allocate a fresh RGBA8 buffer the size of the full overlay
   (logical px × backing scale)² × 4 bytes. On a 1536×1728 logical
   Retina display that's a **42 MB** zeroed allocation per frame.
2. Tiny-skia rasterizes **everything** in the `Hud` description
   into that buffer: held rects, guides, stuck-measurement pills,
   the freeze-screen edges that were detected, the live crosshair,
   the live W×H pill, the toast.
3. Wrap the bytes into a `CGImage` (macOS) or attach the SHM
   buffer (Wayland).
4. The compositor / `drawRect:` blits the result.

The vast majority of that work is wasted: between cursor frames,
**only the live crosshair and W×H pill change**. Held rects don't
move. Guides don't move. Stuck pills don't move. The freeze-frame
background doesn't move. All of that gets repainted at 60 Hz anyway.

Per-frame budget breakdown (rough, on Apple Silicon at 2× Retina):

| Step | Cost |
|---|---|
| `vec![0u8; ~42 MB]` (memset zeroing) | 3–6 ms |
| tiny-skia stroke rendering (full HUD) | 5–15 ms |
| `CGImage::new` / SHM buffer wrap | 1–2 ms |
| `CGContext::draw_image` blit | 1–3 ms |
| **Total** | **10–25 ms** |

In **live mode**, the daemon thread additionally runs
`CGWindowListCreateImage` (~30–60 ms) at 10 Hz inline. That pushes
the effective render rate down to ~25–30 Hz with periodic stalls.

After this refactor, target per-frame cost when only the cursor
moves: **~1–3 ms** (small dynamic-overlay rasterize + two GPU
blits). The macOS `CALayer`-backed view will composite the cached
static bitmap and the small dynamic bitmap without any CPU work on
our side.

---

## Architecture

Replace the single rasterize-everything path with two cached
bitmaps that update on different invalidation triggers.

```
┌──────────────────────────────────────────────────────┐
│ Overlay surface (tint background)                    │
│                                                      │
│   ┌────────────────────────────────────────────────┐ │
│   │ Static layer (cached, ~10–15 ms to build)      │ │
│   │  • freeze-screen captured frame                │ │
│   │  • held_rects                                  │ │
│   │  • guides                                      │ │
│   │  • stuck_measurements                          │ │
│   │  • align_mode full-screen lines (when on)      │ │
│   │  → re-rendered only when these inputs change   │ │
│   └────────────────────────────────────────────────┘ │
│                                                      │
│   ┌────────────────────────────────────────────────┐ │
│   │ Dynamic layer (small bbox, ~0.5–1 ms each)     │ │
│   │  • live crosshair (axis lines + tick caps)     │ │
│   │  • live W×H pill                               │ │
│   │  • show_cursor `+` marker                      │ │
│   │  • move/resize cursor glyph                    │ │
│   │  • context menu (when open)                    │ │
│   │  • toast (when active)                         │ │
│   │  → re-rendered every set_hud call              │ │
│   └────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────┘
```

### Invalidation rules

The platform overlay decides whether the static layer needs a
rebuild by comparing a **content hash** of the static fields
against the previous frame's hash. If the hash matches, reuse the
cached bitmap; otherwise re-rasterize.

**Static-affecting fields** (a change in any of these invalidates
the static cache):

- `held_rects: Vec<HeldRect>` — vec contents + ordering
- `guides: Vec<Guide>` — vec contents + ordering
- `stuck_measurements: Vec<StuckMeasurement>` — vec contents
- `align_mode: bool` — full-screen alignment lines toggle
- `guide_color: Color`, `alternative_guide_color: Color`
- `primary_fg: Color`, `alternate_fg: Color`
- `measurement_format: HudMeasurementFormat` — pill text formatting
- The freeze-screen background frame (separately cached, see below)

**Dynamic-only fields** (don't trigger static rebuild):

- `kind: HudKind` (cursor position lives here)
- `show_cursor: bool`
- `toast: Option<HudToast>`
- `move_cursor_at: Option<(f64, f64)>`, `cursor_kind: CursorKind`
- `context_menu: Option<HudContextMenu>`
- `cursor_in_rect: bool`

**Conservative hashing:** start with `std::hash::Hash` on the
relevant fields (the existing types are already `Debug` / `Clone`;
make them `Hash` where they aren't). A non-cryptographic
`DefaultHasher` is fine — a hash collision just means an extra
unnecessary rebuild, never a correctness bug.

### Dynamic-layer bbox

The dynamic layer can be much smaller than the full overlay. The
crosshair stretches across the full surface (axis lines), but the
**tick caps + W×H pill + custom cursor `+`** fit inside ~400×400
logical px around the cursor. Two options:

1. **Full-surface dynamic layer** (simple): same dimensions as the
   static layer. tiny-skia only draws the pixels we touch, so the
   stroke cost is similar to (1b) below, but the allocation cost
   is still per-frame. Best for v1.
2. **Cursor-local dynamic layer** (smaller, more invasive): track
   the cursor bbox + a small margin, allocate / render just that.
   `drawRect:` composes static + offset(dynamic, bbox.origin). Saves
   the per-frame allocation entirely. Defer to v2 unless v1 isn't
   fast enough.

V1 keeps the same buffer size for the dynamic layer but separates
the rasterize calls so the static layer's stroke work is skipped
on the hot path. That alone should drop per-frame cost from ~25 ms
to ~5 ms — enough to feel native.

---

## File-by-file changes

### 1. `crates/vernier-platform/src/hud_render.rs`

Split `render_hud_into` into three functions:

```rust
// Currently does everything.
pub(crate) fn render_hud_into(canvas: &mut [u8], buf_w, buf_h, scale, hud)

// After:
pub(crate) fn render_static_into(canvas: &mut [u8], buf_w, buf_h, scale, hud)
pub(crate) fn render_dynamic_into(canvas: &mut [u8], buf_w, buf_h, scale, hud)
pub(crate) fn static_hash(hud: &Hud) -> u64
```

Internals refactor:

- `render_hud_strokes` already separates "additive" content (held
  rects, stuck, guides) from the "live" content (current kind,
  toast, cursor). Today the function does both passes in one
  borrow of `pixmap`. Lift the held/guide/stuck/align passes into
  `render_static_strokes`; lift the kind/cursor/toast/move-cursor
  passes into `render_dynamic_strokes`.
- The pill-text pass at the bottom of `render_hud_into` (the
  fontdue path) splits the same way — committed-rect pills go to
  static, the live-W×H pill goes to dynamic.
- `placement::compute_pill_layout` runs in both: it's cheap and
  needs the held + stuck inputs to position pills correctly.
  Cleaner long-term to split it the same way, but defer.
- `render_static_into` clears the canvas before rendering (so it
  produces an alpha-zero background where strokes don't reach).
- `render_dynamic_into` clears the canvas the same way — the two
  layers will be composited by the backend, so each layer starts
  transparent.

`static_hash` is a small function that feeds the static-affecting
fields into a hasher and returns the digest. Keep its logic in the
same file as `render_static_into` so the two are visually
co-located — if you add a new static-affecting field, both places
get updated together.

### 2. `crates/vernier-platform/src/types.rs`

`HeldRect`, `Guide`, `StuckMeasurement`, `Color`,
`HudMeasurementFormat`, and the `Hud` enum / struct types need
`Hash` derives. Floats (`f64` coordinates) don't implement `Hash`
in stdlib — wrap with `OrderedFloat` (new workspace dep) or quantize
to bit patterns via `f64::to_bits` before hashing. Bit-pattern
hashing is fine for our use case because the daemon writes the same
bit pattern every frame for the same logical value.

### 3. `crates/vernier-platform/src/lib.rs`

Add a public-to-crate `StaticHudHash` type alias (just `u64`) so
backends share the same hash semantics. No external API change.

### 4. macOS backend — `crates/vernier-platform/src/macos/overlay.rs`

Add two ivars alongside the existing `hud_bitmap`:

```rust
pub(crate) struct OverlayIvars {
    // existing:
    monitor: MonitorId,
    tint: RefCell<Color>,
    hud: RefCell<Option<Hud>>,
    background_image: RefCell<Option<CFRetained<CGImage>>>,
    // new:
    static_hud_image: RefCell<Option<CFRetained<CGImage>>>,
    static_hud_hash: Cell<u64>,
    dynamic_hud_image: RefCell<Option<CFRetained<CGImage>>>,
    // existing trailing fields ...
}
```

`set_hud` becomes:

```rust
fn set_hud(&mut self, hud: Option<Hud>) {
    let new_hash = hud.as_ref().map(hud_render::static_hash).unwrap_or(0);
    let static_dirty = new_hash != ivars.static_hud_hash.get();
    if static_dirty {
        let bitmap = hud.as_ref().and_then(|h| rasterize_static(&view, h));
        *ivars.static_hud_image.borrow_mut() = bitmap;
        ivars.static_hud_hash.set(new_hash);
    }
    let dynamic = hud.as_ref().and_then(|h| rasterize_dynamic(&view, h));
    *ivars.dynamic_hud_image.borrow_mut() = dynamic;
    *ivars.hud.borrow_mut() = hud;
    view.setNeedsDisplay(true);
}
```

`drawRect:` paints in order:

```
tint fill
└─ background_image (freeze-screen)
   └─ static_hud_image (held rects, guides, stuck, align)
      └─ dynamic_hud_image (crosshair, pill, cursor, toast, menu)
```

`rasterize_static` / `rasterize_dynamic` are clones of the existing
`rasterize_hud_for_view` but call the split renderers. They share
the same buffer sizing, color space, and CGImage construction —
factor that into a `rasterize_layer(view, render_fn)` helper to
avoid duplication.

**One subtlety:** `align_mode` draws full-screen guide lines through
the cursor. Today those lines live in the same render pass as the
crosshair. They're conceptually static (the guides themselves don't
move), but they're drawn relative to the cursor position. Pragmatic
split: when `align_mode == true`, render the alignment lines in the
**dynamic** layer. The static layer skips them. The static-hash
treats `align_mode` as a flag that doesn't itself trigger static
rebuild — only its on/off state matters for the dynamic layer.

### 5. Linux backend — `crates/vernier-platform/src/linux/wayland.rs`

Wayland's compositing model differs from macOS. `wl_surface` has a
single buffer; we don't get GPU-side layer composition for free.
Options:

**A. Two `wl_subsurface`s** (clean, ~half-day):

- Create the overlay surface as today.
- Add a child `wl_subsurface` for the dynamic layer, parented to
  the main surface, positioned at (0, 0), same size.
- Static SHM buffer → main surface; dynamic SHM buffer → subsurface.
- Subsurface is sync mode so it commits with the parent.
- `set_hud` writes only the changed buffer(s); the unchanged one
  keeps its existing buffer reference.

This is the closest analog to the macOS CALayer approach and
saves all of the cached static layer's render cost.

**B. CPU composite into one SHM buffer** (simpler, less effective):

- Maintain two cached pixmaps in-process: `static_pixmap`,
  `dynamic_pixmap`.
- On `set_hud`: re-render the dirty layer(s) into their pixmap.
- `draw_overlay` allocates the SHM buffer and `copy_from_slice`s
  the static pixmap, then alpha-blends the dynamic pixmap on top.

The alpha-blend in plain Rust is cheap (~1–2 ms for 12 MP) but
still allocates the full SHM buffer every frame. Cleaner code but
keeps the per-frame allocation cost.

**Recommendation:** start with option B (least invasive to the
existing damage / frame-callback plumbing). If the cursor still
feels sticky on Hyprland under load, follow up with subsurfaces.

### 6. Daemon — `crates/vernier-app/src/main.rs`

No changes expected. The daemon already calls `overlay.set_hud(Some(hud))`
once per frame; the backend handles the static/dynamic split
internally. The daemon stays oblivious to the layering.

The one place to verify: `toggle_measurement`'s "OFF clean" path
calls `overlay.set_hud(None)` — both backends should treat that as
"clear both layers".

### 7. Tests

- Unit-test `static_hash` for stability: clone a `Hud`, mutate
  only a dynamic field (`kind` cursor position, toast), confirm
  hash unchanged. Mutate a static field (push a `HeldRect`),
  confirm hash changes.
- Snapshot-test that `render_static_into + render_dynamic_into`
  composited together produces the same pixels as the old
  `render_hud_into`. tiny-skia is deterministic — a byte-by-byte
  compare on a small fixture canvas catches drift.
- Manual: measure mode entry / exit, drag a rect, place a guide,
  freeze a stuck measurement, open the context menu, scroll a
  long toast. Each interaction should still render correctly.

---

## Rollout

Stage the change so the codebase compiles after every commit and the
old behavior is one revert away:

1. **Add `static_hash` + the split render functions** in
   `hud_render.rs`. The existing `render_hud_into` keeps working;
   the new functions are added alongside. Verify the snapshot test
   passes (rasterizing static then dynamic gives the same pixels as
   `render_hud_into`).
2. **Wire macOS backend to the split.** Add the new ivars, plumb
   `set_hud`, update `drawRect:`. Verify visually that measure
   mode looks identical and feels snappier (cursor movement
   should be ~native speed even in live mode).
3. **Wire Wayland backend** (option B initially). Verify on
   Hyprland with stuck measurements + a few held rects — the
   acceptance criterion is that adding held content doesn't slow
   cursor movement.
4. **Optional: Wayland subsurface upgrade** (option A) if option
   B's per-frame allocation still hurts at high pixel counts.
5. **Delete `render_hud_into`** once both backends are on the
   split path. (Or keep it as a one-shot convenience that calls
   both halves into the same buffer — useful for tests.)

---

## Risk / open questions

- **`Hash` for floats.** `f64::to_bits` is the standard trick. Verify
  the float values flowing into the hash actually round-trip
  identically frame-to-frame for an unmoving held rect (no
  accumulated FP drift from re-derived layout). If they don't, a
  small quantization (`(x * 1000.0).round() as i64`) is a safe
  fallback — sub-pixel drift in a layout calculation will never
  produce a visibly-different render.
- **Layer composition order.** Today `render_hud_strokes` draws
  `held_rects` first, then live drag rect, then stuck, then guides,
  then cursor crosshair. The static layer would be
  held + stuck + guides; the dynamic layer would be live drag rect
  + cursor + toast. The split should preserve the z-order: static
  first, dynamic on top. Verify the snapshot test catches any
  z-fighting between held content and live content.
- **`align_mode` lines.** Today they're treated like part of the
  crosshair render. They draw across the full surface, so they're
  technically expensive — but they're transient (only while Shift
  is held). Putting them in the dynamic layer means a small
  cost-per-frame in alignment mode, but only while the user
  actively holds the modifier. Acceptable.
- **macOS `CALayer` upgrade** (deferred). The current `drawRect:`
  approach already gets the win because `CGContext::draw_image`
  on two cached `CGImage`s is GPU-blittable. If we still see
  contention, switch the view to layer-backed with two
  `CALayer.contents` assignments (no `drawRect:` invocation per
  frame at all). Out of scope for this round.
- **Live-mode capture latency** is a separate issue (the daemon
  thread blocks on `CGWindowListCreateImage`). The static/dynamic
  split helps because the HUD redraws don't stall behind the
  capture, but the capture itself still costs ~30–60 ms every
  100 ms. Moving the capture to a worker thread is a follow-up
  PR.

---

## Acceptance criteria

- Measure mode with 5+ held rects + 10+ guides + the freeze-screen
  background painted feels indistinguishable from measure mode
  with no held content. Cursor moves at display refresh rate.
- Live mode (freeze off) feels smooth between captures; only the
  brief capture-block window shows any stutter.
- Snapshot tests confirm pixel parity with the pre-split renderer.
- No regressions in macOS or Wayland behavior on the existing
  measure-mode flows (drag, snap, guide placement, stuck
  measurement, context menu, toast, freeze toggle).

---

## Out of scope (follow-up tickets)

- Cursor-local dynamic layer (smaller per-frame bitmap).
- Layer-backed `NSView` with `CALayer.contents` (skip `drawRect:`
  entirely on macOS).
- Worker-thread screen capture so the daemon loop doesn't block
  on `CGWindowListCreateImage`.
- `wl_subsurface` upgrade on Wayland if option B isn't enough.
