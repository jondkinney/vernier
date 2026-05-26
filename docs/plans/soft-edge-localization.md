# Plan: consistent soft-edge localization across measurement modes

**Status:** done (2026-05-22) — follow-up to the pixel-perfect-
measurement work (PR #23). Both modes now route through the shared
`edge.rs::localize_edge` helper; positions are carried as fractional
edge boundaries end-to-end, so a soft edge localises to its gradient
midpoint and a crisp edge stays byte-identical.

Refined beyond the original sketch: **detection and localization are
fully decoupled.** The user's `Tolerance` governs only phase-1
detection (whether a transition registers as an edge). Localization is
the sub-pixel 50%-brightness crossing between the two plateau colours
— computed with a fixed `PLATEAU_EPS`, independent of tolerance — so
adjusting the tolerance pref never moves a measurement, only changes
which edges snap. A thin-feature (`outside ≈ inside`) fallback uses
the geometric centre.

**Live-test passed.** A 2560×1600 target with two boxes of true size
400×300 — one crisp, one with a 16px anti-aliased ramp — measured
identically across both modes: crosshair and area-rectangle both
reported 400 × 300 on both boxes (versus the old code's ~384/~416
straddle on the soft box).

**Edge-bias control added.** An `EdgeBias` parameter
(`Inner` / `Midpoint` / `Outer`) is threaded through the localizer so
the user can pick where on a soft edge the measurement lands:
`Midpoint` (default) = the 50%-brightness crossing; `Inner` = the
inside-plateau edge of the ramp; `Outer` = the outside-plateau edge.
On a *crisp* edge all three biases collapse to the same half-pixel
boundary — the crisp-invariant property holds. Surfaced as:

- `[edge_bias] default = "Inner|Midpoint|Outer"` in `settings.toml`
  (reload via `vernier reload-settings`).
- An `E` hotkey that cycles bias live during measurement
  (`shortcuts.bias_cycle`), with a HUD toast on each press.
- A `--bias` flag on `vernier detect-edges` for ad-hoc spot-checks.

Live-verified on the same 16px-ramp target: Inner reports 385, Midpoint
400, Outer 415; crisp reports 400 at every bias.

## Problem

vernier's two measurement modes detect an edge differently on
*soft / anti-aliased* edges:

- **Crosshair** — `vernier-core/src/edge.rs::scan` walks **outward**
  from the cursor anchor and stops at the first pixel whose colour
  delta from the anchor exceeds `tolerance`.
- **Area rectangle** — `edge.rs::shrink_to_content_with_bg` walks
  **inward** from the dragged rect and stops at the first row/column
  whose pixels differ from the background corner by more than
  `tolerance`.

On a **crisp** edge the two stop at the identical boundary — verified
pixel-exact (both modes read exactly 400 for a 400 px target at 1×).

On a **soft** edge (a multi-pixel black→grey→white gradient — common
wherever screen content is anti-aliased, upscaled by a viewer, or
fractionally scaled) the outward scan stops at the *near* side of the
gradient and the inward walk stops at the *far* side. The modes then
disagree by roughly the gradient width. Observed on a fractionally-
scaled (and thus resampled) reference: crosshair 249, area-rect 251,
for a true-250 edge — straddling the real value by 1 px each way.

## Goal

Both modes localise a soft edge to the **same** point: the gradient's
perceptual midpoint — the 50% crossing between the stable inside
colour and the stable outside colour.

Hard constraint: **crisp-edge behaviour must stay byte-identical.** On
a hard edge both modes already agree and the existing `edge.rs` unit
tests assert exact integer `distance` / `position`. Those must still
pass.

## Where

`crates/vernier-core/src/edge.rs` — `scan` and
`shrink_to_content_with_bg`. The fix is a shared "localise this edge"
routine both call, so they cannot drift apart.

## Approach (to be refined by an investigation pass)

When a scan detects a colour change, instead of returning the first
over-tolerance pixel, continue until the colour **stabilises** (a
plateau within tolerance of a new reference colour). The edge is the
**midpoint** between the last pixel stable at the inside colour and
the first pixel stable at the outside colour — a possibly *fractional*
position. Both routines use the identical helper, so they agree by
construction. On a hard edge the gradient is zero pixels wide and the
midpoint collapses to the existing integer boundary — no change.

## The crux — reconcile with the fence-post convention

PR #23 established: a measurement is an **inclusive pixel count**,
`last - first + 1`, over integer content-pixel coordinates.

Soft-edge midpoints are **fractional**. The span between two
*midpoints* is the count directly — **no `+1`**. A crisp N-pixel
region's two edge-midpoints are exactly N apart (left edge at
x = L − 0.5, right at x = L + N − 0.5 → span N).

So this task must unify both cases: `HudEdge.pos_phys` (currently
`(i32, i32)`) and the count logic in `format_wh_phys` /
`inclusive_span_phys` must carry and combine **fractional** edge
positions such that a crisp N-px target still yields exactly N and a
soft edge yields its true midpoint-to-midpoint span. This reconciliation
is the heart of the task — get it wrong and the crisp pixel-perfect
result regresses.

## Verification

- Existing `edge.rs` hard-edge unit tests still pass unchanged.
- New tests over a synthetic gradient: a region with a known
  midpoint-to-midpoint width measures that width from both a
  `scan`-style and a `shrink`-style call.
- Live: a genuinely anti-aliased on-screen target — crosshair and
  area-rect agree; a crisp target still reads exactly N in both.

## Process

Same as the pixel-perfect-measurement round: Plan-agent investigation
→ implementation sub-agent → verify in the main thread → live-test
with anti-aliased targets. This is an algorithm change in
`vernier-core`, the most precision-sensitive crate — treat it carefully.
