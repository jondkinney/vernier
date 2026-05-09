# Milestone 4f — pending work plan

Picks up from commit `9ffdd0a` (Milestone 4e). Self-contained brief so a
fresh session can clear context and pick up the deferred work without
re-reading the whole transcript.

---

## State at 4e

Cross-platform Rust clone of macOS measurement tools targeting Hyprland on Omarchy.
What works today:

- `SUPER+CTRL+SHIFT+F` toggles measure mode (`vernier toggle` IPC
  fallback).
- Live edge-detection crosshair (axis lines + tick caps + cross marker)
  with snap to detected pixel boundaries.
- Discrete tolerance levels Zero/Low/Medium/High cycled with `+`/`-`.
  Zero halts at the first different pixel (scans through uniform
  regions).
- Drag-to-area draws a coral measurement rect with snap-shrink-to-content
  on release.
- Multiple held rects accumulate. Click pill = screenshot, click
  interior = remove, drag edge/corner = resize (8 handles, axis-matched
  resize cursor on hover).
- Guides: `Shift+H` or `Shift+B` = horizontal, `Shift+V` = vertical.
  Pending placement uses an axis-matched resize cursor; snap to detected
  edges; hold `Super` to free-place. X badge at line midpoint or
  double-click line within 400 ms removes; drag line moves it. Inter-guide
  distance pills: top of screen for vertical pairs, left for horizontal
  pairs.
- Stuck axis measurements: lowercase `h`/`b` = horizontal,
  `v` = vertical. Hover the pill swaps value for an `×` (1.5× glyph),
  click to remove.
- Color toggle (`x`): coral red ↔ near-black foreground.
- Crosshair alignment mode (hold Shift): full-screen axis lines, every
  other affordance still interactive.
- Held-rect screenshot → PNG + `wl-copy` + `notify-send` w/ Edit-in-Satty.
  Toast confirmation, then auto-exit measure mode.
- Persistence after toggle-off: held content stays visible via
  passthrough layer-shell surface (empty input region +
  `KeyboardInteractivity::None`). Toggle on to interact.
- Esc requires double-press within 700 ms to exit. First press shows
  hint toast. On exit, session saves to
  `~/.local/share/vernier/last-session.txt`.
- `Capital R` restores the saved session. Lowercase `r` re-captures the
  frame.
- System pointer cursor via `wp_cursor_shape_v1` over X badges / pills /
  rect interiors. Custom cursors for crosshair / move / resize.
- Procedural tray icon (rounded square, teal→violet gradient, cross +
  T-caps + dashes pill).
- `~/.local/bin/screenshot-cursor`, bound to `CTRL+PrintScreen`, captures
  the desktop with cursor included after a 3-2-1 countdown.

---

## Architecture cheat sheet

### Crates

- `crates/vernier-core` — pure types + algorithms: edge detection
  (`detect_edges`, `shrink_to_content`), measurement math, aspect
  ratios, frame views.
- `crates/vernier-platform` — OS integration. Linux uses
  `smithay-client-toolkit` + raw Wayland (wlr-layer-shell, xdg portals,
  PipeWire screencast, GTK tray via `tray-icon`). HUD rendering via
  `tiny-skia` + `fontdue`.
- `crates/vernier-app` — daemon binary; owns the main event loop +
  all interaction state.
- `crates/vernier-ui` — minimal, not on hot path.

### Hot files

- `crates/vernier-app/src/main.rs` — main loop, all interaction
  state, keyboard / pointer handlers, `refresh_hud`, session save/load,
  helpers (`cursor_over_pill`, `cursor_over_rect_handle`,
  `apply_resize`, `want_system_pointer`, `compose_guides`,
  `snap_to_nearest_x_edge`, `snap_to_nearest_y_edge`).
- `crates/vernier-platform/src/linux/wayland.rs` — Wayland event
  loop, layer-shell surfaces, HUD rendering (`render_hud_strokes` +
  helpers like `draw_pill_bg`, `push_text_in_box`,
  `pill_dims_at`, `draw_camera_icon`, `draw_move_cursor`,
  `draw_resize_cursor`, `draw_guides`, `draw_stuck_measurements`,
  `draw_area_rect`), `wp_cursor_shape_v1` plumbing, screencast capture.
- `crates/vernier-platform/src/types.rs` — public `Hud`, `HudKind`,
  `Guide`, `HeldRect`, `StuckMeasurement`, `CursorKind`, `HudToast`.

### HUD data flow

1. Main loop receives `PlatformEvent::PointerMove` /
   `PlatformEvent::PointerButton` / `PlatformEvent::KeyboardKey`.
2. Main recomputes desired `Hud` (held rects, guides, stuck
   measurements, cursor flags, toast, `align_mode`,
   `move_cursor_at` + `cursor_kind`).
3. `overlay.set_hud(Some(hud))` → `Cmd::OverlaySetHud` →
   `WaylandState::draw_overlay` → `render_hud_strokes` rasterizes into
   a `wl_shm` Abgr8888 buffer at HiDPI scale.
4. Pills are pushed as `PillLayout` entries (`text_x`, `baseline_y`,
   `px_size`); rasterized via `fontdue` after the tiny-skia pixmap
   reference drops.

### Constants worth knowing

- `TEXT_LOGICAL_PX = 12.5` — measurement / aspect pill text size.
- `TEXT_STUCK_LOGICAL_PX = 10.0` — stuck pills (smaller, subordinate).
- `TOAST_TEXT_LOGICAL_PX = 18.0` — toast text size.
- Strokes: axis lines / rect borders = 2 physical px; tick caps =
  2 physical px.
- Cross marker: 2 px black core + 4 px white halo.
- Pill padding: `0.8 * text_size * scale_f` x, `0.4 * text_size * scale_f` y.
- Esc double-tap window: 700 ms.
- Guide double-click window: 400 ms.
- Guide / stuck snap threshold: 8 logical px.

---

## Pending work

### A. Snap-to-guide for box edges *(do first)*

**Goal.** When drawing a new rect (Drawing mode) or resizing an existing
held rect, snap each moving edge to nearby guides so the user can build
rects aligned to guides.

**Behavior:**
- Threshold: 8 logical px (matches the existing edge-snap).
- `Super` held disables the snap → free placement.
- Drawing mode: only the cursor-end of the rect snaps (start was
  committed on press). Snap `cursor.x` to nearest vertical guide and
  `cursor.y` to nearest horizontal guide, independently.
- Resize: only the moving edges snap. Corner handles → both axes;
  side handles → only the moving axis.

**Implementation steps:**

1. **Add snap helpers** in `main.rs`:
   ```rust
   fn snap_x_to_guides(x: f64, guides: &[Guide]) -> f64 {
       const SNAP_PX: f64 = 8.0;
       let mut best = x;
       let mut best_d = SNAP_PX;
       for g in guides.iter().filter(|g| g.axis == GuideAxis::Vertical) {
           let d = (x - g.position as f64).abs();
           if d < best_d { best_d = d; best = g.position as f64; }
       }
       best
   }
   fn snap_y_to_guides(y: f64, guides: &[Guide]) -> f64 { /* mirror */ }
   ```

2. **Drawing mode** — in `refresh_hud`'s `InteractionMode::Drawing`
   branch, around the `HudKind::Drawing { start, cursor }` build,
   pre-compute the snapped cursor:
   ```rust
   let (cx, cy) = if super_held {
       (x, y)
   } else {
       (snap_x_to_guides(x, guides), snap_y_to_guides(y, guides))
   };
   hud.kind = HudKind::Drawing { start: start_pos, cursor: (cx, cy) };
   ```

   Also snap the FINAL committed rect on release. In
   `handle_pointer_button`'s release path (before the
   `snap_shrink_logical_rect` call), snap `raw_end` (start was set on
   press so leave it alone — or snap both for symmetry, optional).

3. **Resize mode** — modify `apply_resize` (currently no awareness of
   guides) to take `guides: &[Guide]` and `super_held: bool`. After
   computing new bounds:
   ```rust
   if !super_held {
       use ResizeHandle::*;
       match op.handle {
           Top | TopLeft | TopRight =>
               lo_y = snap_y_to_guides(lo_y, guides),
           Bottom | BottomLeft | BottomRight =>
               hi_y = snap_y_to_guides(hi_y, guides),
           _ => {}
       }
       match op.handle {
           Left | TopLeft | BottomLeft =>
               lo_x = snap_x_to_guides(lo_x, guides),
           Right | TopRight | BottomRight =>
               hi_x = snap_x_to_guides(hi_x, guides),
           _ => {}
       }
   }
   ```

4. **Update call site** in the `MainEvent::Platform(PointerMove)`
   handler — pass `&guides` and `super_held` to `apply_resize`.

**Files touched:** `crates/vernier-app/src/main.rs` only.

**Test plan:**
- Place horizontal guide at y=200, vertical at x=400. Draw a rect
  starting near (100,100). Drag toward (398,202) → rect end snaps to
  (400,200).
- Resize bottom-right corner near a guide intersection → corner snaps.
  Hold Super while dragging → no snap.
- Multiple guides: snap to nearest within 8 px.

---

### B. Right-click context menu

**Goal.** Right-click on the overlay opens a small floating menu with
the actions the user wants quick access to.

**Menu items** (from the user's reference screenshot):

| Item | Shortcut | Action |
|---|---|---|
| Add Horizontal Guide | ⇧H | `pending_guide = Some(Horizontal)` |
| Add Vertical Guide | ⇧V | `pending_guide = Some(Vertical)` |
| ─ divider ─ | | |
| Hold Horizontal Distance | H | reuse lowercase `h` code path |
| Hold Vertical Distance | V | reuse lowercase `v` code path |
| ─ divider ─ | | |
| Open Screenshot Tool | ⌘S | `Command::spawn("omarchy-cmd-screenshot")` |
| Enter Background Mode | ⌃⇧⌘F | toggle off (passthrough kicks in if content) |
| Restore Last Session | ⇧R | reuse Capital R code path |
| ─ divider ─ | | |
| Clear All | | clear `guides` / `stuck_measurements` / `held_rects` |
| Close macOS | | full daemon exit (same as `vernier quit`) |

**Approach.** In-overlay rendering — fits the existing HUD pipeline.
No new Wayland surfaces, no GTK popup, no extra deps.

**Main-loop state:**

```rust
let mut context_menu: Option<ContextMenuState> = None;

#[derive(Clone)]
struct ContextMenuState {
    origin: (f64, f64),     // logical px where right-click happened
    hovered: Option<usize>, // currently hovered row
}
```

The item list itself is static — define a `const` table or a function.

```rust
struct MenuItem {
    label: &'static str,
    shortcut: Option<&'static str>,
    icon: MenuIcon,
    action: MenuAction,
    divider_after: bool,
}

enum MenuIcon { GuideH, GuideV, StuckH, StuckV, Camera, Background, Restore, Clear, Close }

enum MenuAction { /* one variant per row */ }
```

**Hud changes (`types.rs`):**

```rust
pub struct Hud {
    // ...existing...
    pub context_menu: Option<HudContextMenu>,
}

pub struct HudContextMenu {
    pub origin: (f64, f64),
    pub items: Vec<HudContextMenuItem>,
    pub hovered: Option<usize>,
}

pub struct HudContextMenuItem {
    pub label: String,
    pub shortcut: Option<String>,
    pub icon: HudContextMenuIcon,
    pub divider_after: bool,
}

pub enum HudContextMenuIcon { GuideH, GuideV, StuckH, StuckV, Camera, Background, Restore, Clear, Close }
```

Update `Hud::hover()` default to `context_menu: None`.

**Renderer (`wayland.rs`):**

Add `draw_context_menu` near the toast helpers. Sizes (logical px):
- Item row height: 28
- Bg corner radius: 10
- Bg padding: 8 vertical, 8 horizontal
- Icon column width: 28 (icon centered, ~16 logical px square)
- Label text: `TEXT_LOGICAL_PX` (12.5)
- Shortcut text: right-aligned, `TEXT_STUCK_LOGICAL_PX` (10), muted
  color (e.g. `rgba(200,200,200,255)`)
- Divider: 1 px line, 6 logical px vertical padding above/below
- Hover row bg: ~10 % lighter than menu bg
- Menu bg: `rgba(20, 20, 20, 235)`

Width: ~280 logical px. Compute exactly from the longest label +
shortcut.

Position: anchor menu's top-left at `origin`; clamp to keep on-screen
(if `origin.x + menu_w > buf_w`, shift left).

Render order: AFTER the toast block (top-most).

Each `HudContextMenuIcon` rendered with its own small tiny-skia path —
hand-draw, ~16 logical px square.

**Pointer routing in `main.rs`:**

1. **PointerButton press, BTN_RIGHT** (button == `0x111`):
   - If `context_menu.is_some()`, close it (right-click toggles).
   - Else open at `(x, y)`. Refresh HUD.

2. **PointerButton press, BTN_LEFT, while menu open:**
   - Compute hit row from cursor + `origin`.
   - If a row is hit: dispatch action, close menu.
   - Else: close menu (no action).

3. **PointerMove while menu open:** recompute `hovered` row and update
   HUD.

4. **Esc while menu open:** close menu (don't trigger the existing
   exit-confirm flow). Both Esc presses are absorbed by menu close.

**Action dispatch:**

```rust
fn dispatch_menu(action: MenuAction, /* &mut state */) {
    use MenuAction::*;
    match action {
        AddHorizontalGuide => { pending_guide = Some(GuideAxis::Horizontal); }
        AddVerticalGuide   => { pending_guide = Some(GuideAxis::Vertical); }
        HoldHorizontalDistance => { /* mirror lowercase 'h' branch */ }
        HoldVerticalDistance   => { /* mirror lowercase 'v' branch */ }
        OpenScreenshotTool => {
            std::process::Command::new("omarchy-cmd-screenshot")
                .spawn().ok();
            // optionally exit measure mode first
        }
        EnterBackgroundMode => {
            // Toggle off; passthrough kicks in if content exists.
            toggle_measurement(/* with current state */);
        }
        RestoreLastSession => { /* mirror Capital R branch */ }
        ClearAll => {
            guides.clear();
            stuck_measurements.clear();
            held_rects.clear();
        }
        ClosemacOS => {
            // Full daemon exit — break out of the main loop, same as
            // MainEvent::Ipc(IpcCmd::Quit).
        }
    }
}
```

**In-flight conflict rules:**
- `pending_guide` set when right-click fires: cancel `pending_guide`,
  then open menu.
- `dragging_guide` / `resizing` active: ignore right-click until drag
  ends.

**Files touched:**
- `crates/vernier-platform/src/types.rs` — new types + `Hud`
  field.
- `crates/vernier-platform/src/linux/wayland.rs` — `draw_context_menu`
  + per-icon paths. Call last in `render_hud_strokes` (above toast).
- `crates/vernier-app/src/main.rs` — `ContextMenuState`, menu items
  table, `dispatch_menu`, hit-test helper, BTN_RIGHT handling, hover
  update on PointerMove, Esc absorption.

**Test plan:**
- Right-click → menu appears at cursor.
- Move pointer over rows → row highlights.
- Click "Add Horizontal Guide" → menu closes, pending guide tracks
  cursor.
- Click "Restore Last Session" → menu closes, content comes back.
- Click outside menu → closes, no action.
- Right-click again with menu open → closes.
- Esc while menu open → closes (no exit-confirm).

**Caveats:**
- HiDPI: every dimension and icon multiplied by `scale_f`.
- System pointer (`wp_cursor_shape_v1`) should be visible while the
  menu is open. In `want_system_pointer`, treat menu-open as wanting
  the system arrow.
- Menu rendering happens in active overlay (input-grabbing mode).
  Passthrough mode (after toggle-off) doesn't show menu since there's
  no input grab to receive right-clicks anyway.

---

## Sequencing

1. **A first** (snap-to-guide). Smaller, contained, exercises existing
   helpers, easy to test. One file touched.
2. **B second** (context menu). New render path + state; touches three
   files.

Both designed to need no new external dependencies.

---

## Build / run

```bash
cargo build --release
./target/release/vernier quit
RUST_LOG=info ./target/release/vernier &
```

Hotkey after rebuild: `SUPER+CTRL+SHIFT+F` (Hyprland binding lives in
`~/.config/hypr/bindings.conf`). IPC fallback: `./target/release/vernier
toggle`.
