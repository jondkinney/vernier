//! Transparent fullscreen overlay window per monitor.
//!
//! Each [`MonitorId`] gets one borderless NSWindow at the status
//! window level. The content view is a custom subclass that:
//!
//! * Fills its bounds with the current tint colour in `drawRect:`.
//! * Blits the rasterized HUD bitmap (cursor, pills, guides,
//!   stuck measurements) on top of the tint when `set_hud` is
//!   called — the same pixmap renderer the Linux backend uses.
//! * Forwards mouse / keyboard events into the daemon's
//!   [`PlatformEvent`] channel.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSCursor, NSEvent, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::CGImage;
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect};

use crate::{
    Color, Hud, MonitorId, OverlayHandle, OverlayOps, PlatformError, PlatformEvent, Result,
};

use super::keymap::vkey_to_xkb_keysym;

pub(crate) struct OverlayResources {
    pub window: Retained<OverlayWindow>,
    pub view: Retained<OverlayView>,
}

pub(crate) fn create(monitor: MonitorId) -> Result<OverlayHandle> {
    super::app::run_on_main_sync(move || -> Result<OverlayHandle> {
        let mtm = MainThreadMarker::new().expect("create overlay on main");
        let screen = super::monitor::ns_screen_for(monitor)
            .ok_or(PlatformError::MonitorNotFound(monitor))?;
        // Cover the full screen, not `visibleFrame`. We used to clip
        // out the menu-bar/Dock strip so a stuck overlay couldn't
        // occlude the tray icon, but the "freeze screen" feature
        // needs to paint the captured display frame 1:1 — anything
        // smaller squeezes the image vertically and offsets every
        // measurement by the Dock/menu-bar delta. The tray remains
        // reachable: in passthrough (post-measure) mode the overlay
        // is `setIgnoresMouseEvents(true)` so clicks fall through
        // to the menu bar, and during measure mode the Esc / toggle
        // hotkey is the escape hatch (the overlay is at
        // NSStatusWindowLevel = 25, just above NSMainMenuWindowLevel,
        // so a wedged measure mode visually hides the tray but the
        // keyboard out still works).
        let frame = screen.frame();

        let style = NSWindowStyleMask::Borderless;
        // OverlayWindow subclass overrides canBecomeKeyWindow → YES so the
        // window can take key status. Without this, AppKit defers cursor-
        // rect resolution to whatever underlying app is key (Safari, Finder,
        // ...) and our transparent NSCursor never wins — the OS arrow stays
        // visible on top of our drawn crosshair.
        let alloc = mtm.alloc::<OverlayWindow>();
        let window: Retained<OverlayWindow> = unsafe {
            msg_send![
                alloc,
                initWithContentRect: frame,
                styleMask: style,
                backing: NSBackingStoreType::Buffered,
                defer: false,
            ]
        };

        window.setOpaque(false);
        let clear = NSColor::clearColor();
        window.setBackgroundColor(Some(&clear));
        // Above normal/floating app windows, below pop-up menus
        // (101) and the menu bar (24-25) so the user can still
        // reach the tray icon. NSStatusWindowLevel = 25 puts us
        // just above the menu bar background but below the menu
        // bar's text and any popped menus.
        window.setLevel(NS_WINDOW_LEVEL_STATUS);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary,
        );
        window.setIgnoresMouseEvents(true);
        window.setHasShadow(false);
        window.setAcceptsMouseMovedEvents(true);

        let ivars = OverlayIvars {
            monitor,
            tint: RefCell::new(Color::TRANSPARENT),
            hud: RefCell::new(None),
            static_hud_image: RefCell::new(None),
            static_hud_hash: Cell::new(0),
            dynamic_hud_image: RefCell::new(None),
            background_image: RefCell::new(None),
            cursor_hidden: RefCell::new(false),
            pointing_hand: RefCell::new(false),
            last_flags: RefCell::new(0),
            transparent_cursor: RefCell::new(None),
            captures_input: RefCell::new(false),
        };
        let view: Retained<OverlayView> = OverlayView::new(mtm, ivars);
        view.setFrame(NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: frame.size,
        });
        // OverlayView : NSView via define_class!, so deref gives
        // us the &NSView the AppKit API wants.
        window.setContentView(Some(&view));
        window.makeFirstResponder(Some(&view));

        // Tracking area for live `mouseMoved:` even when our
        // borderless window isn't key. `InVisibleRect` keeps the
        // tracking area in sync with the view's bounds so a
        // future window resize doesn't strand it.
        install_tracking_area(&view, frame.size);

        super::with_main_state(|s| {
            s.overlays.insert(
                monitor,
                OverlayResources {
                    window: window.clone(),
                    view: view.clone(),
                },
            );
        });

        Ok(OverlayHandle::from_backend(MacOverlay { monitor }))
    })
}

#[allow(dead_code)]
const NS_WINDOW_LEVEL_SCREEN_SAVER: isize = 1000;
const NS_WINDOW_LEVEL_STATUS: isize = 25;

struct MacOverlay {
    monitor: MonitorId,
}

impl OverlayOps for MacOverlay {
    fn show(&mut self) {
        let monitor = self.monitor;
        super::app::run_on_main_async(move || {
            super::with_main_state(|s| {
                if let Some(o) = s.overlays.get(&monitor) {
                    o.window.orderFrontRegardless();
                }
            });
        });
    }

    fn hide(&mut self) {
        let monitor = self.monitor;
        super::app::run_on_main_async(move || {
            super::with_main_state(|s| {
                if let Some(o) = s.overlays.get(&monitor) {
                    o.window.orderOut(None);
                }
            });
        });
    }

    fn toggle(&mut self) {
        if self.is_visible() {
            self.hide();
        } else {
            self.show();
        }
    }

    fn is_visible(&self) -> bool {
        let monitor = self.monitor;
        super::app::run_on_main_sync(move || {
            super::with_main_state(|s| {
                s.overlays
                    .get(&monitor)
                    .map(|o| o.window.isVisible())
                    .unwrap_or(false)
            })
        })
    }

    fn monitor(&self) -> MonitorId {
        self.monitor
    }

    fn set_tint(&mut self, tint: Color) {
        let monitor = self.monitor;
        super::app::run_on_main_async(move || {
            super::with_main_state(|s| {
                if let Some(o) = s.overlays.get(&monitor) {
                    *o.view.ivars().tint.borrow_mut() = tint;
                    o.view.setNeedsDisplay(true);
                }
            });
        });
    }

    fn set_input_capturing(&mut self, capturing: bool) {
        let monitor = self.monitor;
        super::app::run_on_main_async(move || {
            let cursor_xy = if capturing { current_cursor_xy() } else { None };
            super::with_main_state(|s| {
                if let Some(o) = s.overlays.get(&monitor) {
                    o.window.setIgnoresMouseEvents(!capturing);
                    *o.view.ivars().captures_input.borrow_mut() = capturing;
                    if capturing {
                        // Activate the app and bring our overlay key.
                        // Without this, AppKit's cursor-rect mechanism
                        // defers to the underlying (key) app and our
                        // transparent NSCursor never wins — so the OS
                        // pointer keeps drawing on top of our HUD's
                        // crosshair. canBecomeKeyWindow on OverlayWindow
                        // permits the promotion; activate + makeKey
                        // actually performs it.
                        let mtm =
                            MainThreadMarker::new().expect("set_input_capturing on main thread");
                        let app = NSApplication::sharedApplication(mtm);
                        // `activate()` supersedes `activateIgnoringOtherApps(true)`
                        // as of macOS 14 — the new method always activates
                        // unconditionally (the old `false` variant was the
                        // odd one out and is gone).
                        app.activate();
                        o.window.makeKeyAndOrderFront(None);
                        o.window.makeFirstResponder(Some(&o.view));
                        let was_hidden = *o.view.ivars().cursor_hidden.borrow();
                        if !was_hidden {
                            *o.view.ivars().cursor_hidden.borrow_mut() = true;
                            hide_cursor_on(&o.view);
                            // System-wide hide: NSCursor::hide is
                            // refcounted. Works for .accessory apps
                            // when the calling window is key, which
                            // we just arranged via
                            // `makeKeyAndOrderFront`. This is what
                            // keeps the cursor stable through the
                            // ~50 ms `CGWindowListCreateImage` calls
                            // the live-mode refresh path performs —
                            // those briefly block the main thread,
                            // and without a sticky hide the cursor
                            // would pop visible during the block.
                            NSCursor::hide();
                        }
                    } else {
                        let was_hidden = *o.view.ivars().cursor_hidden.borrow();
                        if was_hidden {
                            *o.view.ivars().cursor_hidden.borrow_mut() = false;
                            show_cursor_on(&o.view);
                            // Pop the refcount balanced with the
                            // `hide()` above. Refcounted, so a missed
                            // `unhide()` would leave the cursor
                            // permanently hidden — guarded by the
                            // `was_hidden` check so we only call this
                            // when there's a hide to balance.
                            NSCursor::unhide();
                        }
                    }
                }
            });
            // Synthesize the first PointerMove so the daemon
            // renders the crosshair at the cursor's actual
            // position immediately — without this the user has
            // to wiggle the mouse 1px before anything shows up,
            // since `mouseMoved:` doesn't fire until the cursor
            // actually moves.
            if capturing {
                if let (Some((sx, sy)), Some(tx)) = (cursor_xy, super::event_tx()) {
                    let (x, y) = screen_to_view_xy(monitor, sx, sy);
                    let _ = tx.send(PlatformEvent::PointerMove { monitor, x, y });
                }
            }
        });
    }

    fn set_hud(&mut self, hud: Option<Hud>) {
        let monitor = self.monitor;
        super::app::run_on_main_async(move || {
            super::with_main_state(|s| {
                if let Some(o) = s.overlays.get(&monitor) {
                    use crate::hud_render::{render_dynamic_into, render_static_into, static_hash};
                    let ivars = o.view.ivars();
                    let tint = match &hud {
                        // Tint is the HUD background; the renderer
                        // composes strokes/pills/guides over it.
                        Some(h) => h.background,
                        None => Color::TRANSPARENT,
                    };
                    *ivars.tint.borrow_mut() = tint;

                    // Static layer: only re-rasterize when the
                    // hash-tracked inputs (held rects, guides, stuck
                    // measurements, colors, measurement format) change.
                    // On a hash hit the cursor-only frame skips the
                    // entire static stroke pass — that's the whole
                    // point of the split.
                    let new_hash = hud.as_ref().map(static_hash).unwrap_or(0);
                    if new_hash != ivars.static_hud_hash.get() {
                        let image = hud
                            .as_ref()
                            .and_then(|h| rasterize_layer_for_view(&o.view, h, render_static_into));
                        *ivars.static_hud_image.borrow_mut() = image;
                        ivars.static_hud_hash.set(new_hash);
                    }
                    // Dynamic layer: always re-rasterize. Smaller
                    // stroke set, no glyphs in the static-layer pill
                    // text path, so this is the path that has to be
                    // fast.
                    let dynamic = hud
                        .as_ref()
                        .and_then(|h| rasterize_layer_for_view(&o.view, h, render_dynamic_into));
                    *ivars.dynamic_hud_image.borrow_mut() = dynamic;

                    *ivars.hud.borrow_mut() = hud;
                    o.view.setNeedsDisplay(true);
                }
            });
        });
    }

    fn set_background_frame(&mut self, frame: Option<crate::Frame>) {
        let monitor = self.monitor;
        super::app::run_on_main_async(move || {
            // Convert the RGBA8 packed bytes into a CGImage once
            // here, on entry; subsequent draws just blit the image.
            // Pixel layout is RGBA non-premultiplied — `Frame` is
            // produced by `native_to_packed_rgba` which already
            // swizzles BGRA→RGBA.
            let image = frame.and_then(|f| cgimage_from_rgba(&f.pixels, f.width, f.height));
            super::with_main_state(|s| {
                if let Some(o) = s.overlays.get(&monitor) {
                    *o.view.ivars().background_image.borrow_mut() = image;
                    // Mark the entire view dirty so `drawRect:`
                    // repaints the new background.
                    o.view.setNeedsDisplay(true);
                }
            });
        });
    }

    fn set_system_pointer_visible(&mut self, visible: bool) {
        let monitor = self.monitor;
        super::app::run_on_main_async(move || {
            super::with_main_state(|s| {
                if let Some(o) = s.overlays.get(&monitor) {
                    let was_hidden = *o.view.ivars().cursor_hidden.borrow();
                    if visible && was_hidden {
                        *o.view.ivars().cursor_hidden.borrow_mut() = false;
                        // Rebalance the sticky `NSCursor::hide()`
                        // that `set_input_capturing(true)` installed.
                        // The cursor-rect machinery alone cannot
                        // override a refcounted hide — even with an
                        // arrow cursor in the rect, a hidden NSCursor
                        // stays invisible. show_cursor_on then sets
                        // the shape (arrow or pointing-hand) that the
                        // newly-visible cursor takes.
                        NSCursor::unhide();
                        show_cursor_on(&o.view);
                    } else if !visible && !was_hidden {
                        *o.view.ivars().cursor_hidden.borrow_mut() = true;
                        // Re-establish the global hide so the cursor
                        // disappears off the held rect / pill again,
                        // even on monitors / windows where AppKit's
                        // cursor-rect arbitration has lost track of
                        // our transparent cursor.
                        NSCursor::hide();
                        hide_cursor_on(&o.view);
                    }
                }
            });
        });
    }

    fn set_pointing_hand_cursor(&mut self, pointing: bool) {
        let monitor = self.monitor;
        super::app::run_on_main_async(move || {
            super::with_main_state(|s| {
                if let Some(o) = s.overlays.get(&monitor) {
                    let prev = *o.view.ivars().pointing_hand.borrow();
                    if prev == pointing {
                        return;
                    }
                    *o.view.ivars().pointing_hand.borrow_mut() = pointing;
                    // Re-apply only when the system pointer is
                    // currently shown — show_cursor_on reads the
                    // pointing_hand flag and picks the right NSCursor.
                    // When the cursor is hidden the latched flag
                    // applies on the next visible→true transition.
                    if !*o.view.ivars().cursor_hidden.borrow() {
                        show_cursor_on(&o.view);
                    }
                }
            });
        });
    }

    fn confine_pointer(&mut self, _x: i32, _y: i32, _w: i32, _h: i32) {
        // No direct macOS equivalent of Wayland pointer
        // constraints. CGAssociateMouseAndMouseCursorPosition
        // + warp could approximate; defer.
    }

    fn release_pointer_confine(&mut self) {
        // See `confine_pointer`.
    }
}

// --- NSView subclass --------------------------------------------------------

pub(crate) struct OverlayIvars {
    monitor: MonitorId,
    tint: RefCell<Color>,
    #[allow(dead_code)]
    hud: RefCell<Option<Hud>>,
    /// Cached "static" HUD layer — held rects, guides, stuck
    /// measurements. Re-rasterized only when `static_hud_hash`
    /// changes, so cursor-only frames skip the expensive stroke
    /// pass entirely. `None` means nothing to draw on this layer.
    static_hud_image: RefCell<Option<CFRetained<CGImage>>>,
    /// Digest of the HUD fields that affect `static_hud_image`. A
    /// `set_hud` call whose digest matches reuses the cached image
    /// and only rebuilds the dynamic layer.
    static_hud_hash: Cell<u64>,
    /// "Dynamic" HUD layer — cursor crosshair, live drag rect, toast,
    /// context menu, corner indicator. Re-rasterized on every
    /// `set_hud` since the cursor moves every frame.
    dynamic_hud_image: RefCell<Option<CFRetained<CGImage>>>,
    /// "Freeze screen" background: the captured display frame at the
    /// moment measure mode opened, converted to a CGImage so
    /// `drawRect:` can paint it opaquely under the HUD. Held as a
    /// retained CGImage rather than the raw pixel Vec so each redraw
    /// doesn't have to re-upload to the GPU. `None` outside measure
    /// mode (overlay stays transparent and shows live content).
    background_image: RefCell<Option<CFRetained<CGImage>>>,
    /// NSCursor::hide / unhide is reference-counted on macOS. Track
    /// whether we're currently in the hidden state so we don't
    /// unbalance the counter (which would either leave the cursor
    /// permanently hidden after measurement mode, or make Alt's
    /// momentary unhide a no-op).
    cursor_hidden: RefCell<bool>,
    /// Which AppKit cursor `show_cursor_on` should set when the
    /// pointer becomes visible — `false` = `arrowCursor`, `true` =
    /// `pointingHandCursor`. Daemon flips this whenever the hover
    /// state crosses a clickable element (the camera-icon pill on
    /// a held rect, today). Latched even while the cursor is
    /// hidden so the next show picks up the right kind.
    pointing_hand: RefCell<bool>,
    /// Last seen NSEvent modifier-flag bits. `flagsChanged:` only
    /// tells us "the modifiers changed"; we diff against this to
    /// emit the right XKB press/release pair through the
    /// PlatformEvent channel.
    last_flags: RefCell<u64>,
    /// Lazy 16x16 transparent NSCursor; pushed into AppKit's
    /// cursor stack while in measure mode so the cursor over our
    /// overlay is invisible.
    transparent_cursor: RefCell<Option<Retained<objc2_app_kit::NSCursor>>>,
    #[allow(dead_code)]
    captures_input: RefCell<bool>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "VernierOverlayView"]
    #[ivars = OverlayIvars]
    pub(crate) struct OverlayView;

    impl OverlayView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let tint = *self.ivars().tint.borrow();
            let color = NSColor::colorWithCalibratedRed_green_blue_alpha(
                tint.r as f64 / 255.0,
                tint.g as f64 / 255.0,
                tint.b as f64 / 255.0,
                tint.a as f64 / 255.0,
            );
            color.setFill();
            objc2_app_kit::NSRectFill(bounds);
            // Paint, bottom to top:
            //   1. tint (filled above)
            //   2. freeze-screen background (if any)
            //   3. cached static HUD layer — held rects, guides, stuck
            //   4. dynamic HUD layer — crosshair, drag rect, toast, menu
            // CGContext::draw_image composites each layer via SrcOver on
            // the GPU; no CPU compositing on the hot path.
            let ivars = self.ivars();
            if let Some(image) = ivars.background_image.borrow().as_ref() {
                draw_hud_image(bounds, image);
            }
            if let Some(image) = ivars.static_hud_image.borrow().as_ref() {
                draw_hud_image(bounds, image);
            }
            if let Some(image) = ivars.dynamic_hud_image.borrow().as_ref() {
                draw_hud_image(bounds, image);
            }
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            forward_pointer_move(self, event);
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            forward_pointer_move(self, event);
        }
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            forward_pointer_button(self, event, BTN_LEFT, true);
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            forward_pointer_button(self, event, BTN_LEFT, false);
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            forward_pointer_button(self, event, BTN_RIGHT, true);
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            forward_pointer_button(self, event, BTN_RIGHT, false);
        }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            forward_key(self, event, true);
        }
        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            forward_key(self, event, false);
        }
        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            forward_flags_changed(self, event);
        }
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            if *self.ivars().cursor_hidden.borrow() {
                let cursor = transparent_cursor(self);
                cursor.set();
            }
        }
        // AppKit polls this whenever the cursor rects need to be
        // rebuilt — on view resize, on key-window change, or when
        // we explicitly call `invalidateCursorRectsForView:`. This
        // is the canonical way to claim the cursor for a view that
        // wraps around its bounds — and unlike `cursorUpdate:` /
        // `NSCursor::set()` it sticks across mouse-button transitions
        // and `setIgnoresMouseEvents` flips.
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            if *self.ivars().cursor_hidden.borrow() {
                let bounds = self.bounds();
                let cursor = transparent_cursor(self);
                self.addCursorRect_cursor(bounds, &cursor);
            }
        }
    }
);

impl OverlayView {
    fn new(mtm: MainThreadMarker, ivars: OverlayIvars) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }
}

// --- NSWindow subclass ------------------------------------------------------
//
// macOS has one global cursor owned by the key window of the active app.
// Borderless NSWindows return NO from canBecomeKeyWindow by default, so a
// stock borderless overlay can never win cursor-rect arbitration against
// the underlying app — the OS arrow keeps drawing on top of our HUD even
// when we push a transparent NSCursor via resetCursorRects. Returning YES
// here lets makeKeyAndOrderFront promote the overlay to key, after which
// AppKit honors our cursor rects.

define_class!(
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "VernierOverlayWindow"]
    pub(crate) struct OverlayWindow;

    impl OverlayWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            true
        }
    }
);

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
#[allow(dead_code)]
const BTN_MIDDLE: u32 = 0x112;

fn forward_pointer_move(view: &OverlayView, event: &NSEvent) {
    let monitor = view.ivars().monitor;
    let (x, y) = surface_local_point(view, event);
    // No per-move cursor toggle: `NSCursor::setHiddenUntilMouseMoves`
    // is literally "hide until the user moves the mouse" — calling
    // it on every mouseMoved made the cursor flash visible on every
    // movement and re-hide between events, the exact opposite of
    // what we want. The system-wide hide is now handled once by
    // `NSCursor::hide()` in `set_input_capturing(true)` (works
    // because we made the overlay key earlier — that workaround for
    // .accessory apps unlocked NSCursor::hide). The transparent
    // NSCursor pushed via `resetCursorRects` stays as a belt-and-
    // suspenders fallback for the rare race where AppKit re-queries
    // cursor rects during a captured event.
    if let Some(tx) = super::event_tx() {
        let _ = tx.send(PlatformEvent::PointerMove { monitor, x, y });
    }
}

fn forward_pointer_button(view: &OverlayView, event: &NSEvent, button: u32, pressed: bool) {
    let monitor = view.ivars().monitor;
    let (x, y) = surface_local_point(view, event);
    if let Some(tx) = super::event_tx() {
        let _ = tx.send(PlatformEvent::PointerButton {
            monitor,
            button,
            pressed,
            x,
            y,
        });
    }
}

// `NSEventModifierFlags` device-independent bits (objc2 exposes them
// as raw u64). These are the same constants AppKit publishes via
// `NSEventModifierFlagsDeviceIndependentFlagsMask`-aware code.
const NS_FLAG_CAPS_LOCK: u64 = 1 << 16;
const NS_FLAG_SHIFT: u64 = 1 << 17;
const NS_FLAG_CONTROL: u64 = 1 << 18;
const NS_FLAG_OPTION: u64 = 1 << 19;
const NS_FLAG_COMMAND: u64 = 1 << 20;

fn forward_flags_changed(view: &OverlayView, event: &NSEvent) {
    let flags: u64 = event.modifierFlags().0 as u64;
    let prev = *view.ivars().last_flags.borrow();
    *view.ivars().last_flags.borrow_mut() = flags;
    let monitor = view.ivars().monitor;
    let Some(tx) = super::event_tx() else {
        return;
    };
    // For each tracked modifier, emit a press if it transitioned
    // 0 → 1 in `flags`, a release if it transitioned 1 → 0. We
    // can't tell left vs right from the device-independent bits
    // (AppKit also exposes per-side flags, but the daemon treats
    // L and R as equivalent for these modifiers) so we always
    // emit the "_L" XKB keysym.
    for (mask, keysym) in [
        (NS_FLAG_SHIFT, 0xFFE1u32),
        (NS_FLAG_CONTROL, 0xFFE3),
        (NS_FLAG_OPTION, 0xFFE9),
        (NS_FLAG_COMMAND, 0xFFEB),
        (NS_FLAG_CAPS_LOCK, 0xFFE5),
    ] {
        let was = (prev & mask) != 0;
        let now = (flags & mask) != 0;
        if was != now {
            let _ = tx.send(PlatformEvent::KeyboardKey {
                monitor,
                keysym,
                pressed: now,
                is_repeat: false,
            });
        }
    }
}

fn forward_key(view: &OverlayView, event: &NSEvent, pressed: bool) {
    let monitor = view.ivars().monitor;
    let vkey = event.keyCode();
    let is_repeat = pressed && event.isARepeat();
    let keysym = vkey_to_xkb_keysym(vkey);
    if keysym == 0 {
        return;
    }
    if let Some(tx) = super::event_tx() {
        let _ = tx.send(PlatformEvent::KeyboardKey {
            monitor,
            keysym,
            pressed,
            is_repeat,
        });
    }
}

/// NSEvent::mouseLocation returns the cursor position in screen
/// coordinates (origin bottom-left, points). Returns None only if
/// the call isn't safe to make (e.g. early shutdown).
fn current_cursor_xy() -> Option<(f64, f64)> {
    let p = objc2_app_kit::NSEvent::mouseLocation();
    Some((p.x, p.y))
}

/// Convert a screen-space point (Cocoa convention: bottom-left
/// origin) into surface-local (top-left) coordinates for the
/// overlay on `monitor`. Mirrors `surface_local_point`.
fn screen_to_view_xy(monitor: MonitorId, sx: f64, sy: f64) -> (f64, f64) {
    let height = super::with_main_state(|s| {
        s.overlays
            .get(&monitor)
            .map(|o| o.window.frame())
            .map(|f| f.size.height)
            .unwrap_or(0.0)
    });
    let frame_origin = super::with_main_state(|s| {
        s.overlays
            .get(&monitor)
            .map(|o| o.window.frame().origin)
            .unwrap_or(NSPoint { x: 0.0, y: 0.0 })
    });
    let local_x = sx - frame_origin.x;
    let local_y_bottom_up = sy - frame_origin.y;
    (local_x, height - local_y_bottom_up)
}

fn install_tracking_area(view: &OverlayView, size: objc2_foundation::NSSize) {
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{NSTrackingArea, NSTrackingAreaOptions};
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size,
    };
    let options = NSTrackingAreaOptions::MouseMoved
        | NSTrackingAreaOptions::MouseEnteredAndExited
        | NSTrackingAreaOptions::CursorUpdate
        | NSTrackingAreaOptions::ActiveAlways
        | NSTrackingAreaOptions::InVisibleRect;
    // owner: &AnyObject; pass the view itself (it has the
    // mouseMoved/Entered/Exited methods we defined).
    let owner: &AnyObject = unsafe { &*((&**view) as *const NSView as *const AnyObject) };
    let area: Retained<NSTrackingArea> = NSTrackingArea::initWithRect_options_owner_userInfo(
        NSTrackingArea::alloc(),
        rect,
        options,
        Some(owner),
        None,
    );
    view.addTrackingArea(&area);
}

fn surface_local_point(view: &OverlayView, event: &NSEvent) -> (f64, f64) {
    let p_window = event.locationInWindow();
    let p_view = view.convertPoint_fromView(p_window, None);
    let height = view.bounds().size.height;
    (p_view.x, height - p_view.y)
}

// --- System cursor hide/show -----------------------------------------------
//
// We tried `NSCursor::hide` (only effective while the app has
// focus — fails for `.accessory` daemons) and `CGDisplayHideCursor`
// (requires the calling app to be "trusted" / foreground, also
// flaky for `.accessory`). The reliable approach for menu-bar
// utilities is to *push a transparent NSCursor* via the view's
// `cursorUpdate:` callback so the visible cursor over our overlay
// is the transparent one — effectively invisible. When the
// overlay stops capturing input, AppKit returns to the system
// arrow on the next mouse event.

/// Trigger AppKit to re-run `resetCursorRects` on the view, which
/// reads `cursor_hidden` and conditionally installs the transparent
/// cursor over the view's bounds. Caller has already flipped
/// `cursor_hidden`. Does NOT re-enter the main-thread state.
fn hide_cursor_on(view: &OverlayView) {
    if let Some(window) = view.window() {
        window.invalidateCursorRectsForView(view);
    }
    // Also nudge the current cursor immediately so the user
    // doesn't wait for the next mouse-move to see the change.
    let cursor = transparent_cursor(view);
    cursor.set();
}

fn show_cursor_on(view: &OverlayView) {
    use objc2_app_kit::NSCursor;
    if let Some(window) = view.window() {
        window.invalidateCursorRectsForView(view);
    }
    let pointing = *view.ivars().pointing_hand.borrow();
    let cursor = if pointing {
        NSCursor::pointingHandCursor()
    } else {
        NSCursor::arrowCursor()
    };
    cursor.set();
}

/// Build (once per view) and `.set()` a 16x16 fully transparent
/// NSCursor. The view's `cursorUpdate:` calls back into this on
/// each cross of the tracking area.
fn transparent_cursor(view: &OverlayView) -> Retained<objc2_app_kit::NSCursor> {
    if let Some(c) = view.ivars().transparent_cursor.borrow().as_ref() {
        return c.clone();
    }
    use objc2_app_kit::{NSCursor, NSImage};
    use objc2_foundation::NSSize;
    // 16x16 NSImage with no drawing → fully transparent.
    let size = NSSize {
        width: 16.0,
        height: 16.0,
    };
    let image = NSImage::initWithSize(NSImage::alloc(), size);
    let hot = NSPoint { x: 0.0, y: 0.0 };
    let cursor = NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, hot);
    *view.ivars().transparent_cursor.borrow_mut() = Some(cursor.clone());
    cursor
}

// --- HUD rasterization + blit ----------------------------------------------

/// Rasterize a single HUD layer through `render_fn` (either
/// `render_static_into` or `render_dynamic_into`) and wrap the
/// resulting RGBA bytes in a CGImage sized to the view's physical
/// pixel grid. Returns `None` if the view has zero area or CGImage
/// allocation fails.
///
/// Both static and dynamic layers share this path so the pixel
/// format, scale handling, and CGImage construction stay in one
/// place — and so the static cache and the per-frame dynamic
/// rasterize never disagree on canvas dimensions.
fn rasterize_layer_for_view(
    view: &OverlayView,
    hud: &Hud,
    render_fn: fn(&mut [u8], u32, u32, u32, &Hud),
) -> Option<CFRetained<CGImage>> {
    let bounds = view.bounds();
    let scale = view
        .window()
        .map(|w| w.backingScaleFactor())
        .unwrap_or(1.0)
        .max(1.0);
    let phys_w = ((bounds.size.width * scale).round() as u32).max(1);
    let phys_h = ((bounds.size.height * scale).round() as u32).max(1);

    let mut canvas = vec![0u8; (phys_w as usize) * (phys_h as usize) * 4];
    render_fn(
        &mut canvas,
        phys_w,
        phys_h,
        scale.round().max(1.0) as u32,
        hud,
    );

    cgimage_from_rgba(&canvas, phys_w, phys_h)
}

fn cgimage_from_rgba(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Option<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>> {
    use objc2_core_foundation::CFData;
    use objc2_core_graphics::{
        CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
        CGImageAlphaInfo,
    };

    let len = rgba.len() as isize;
    let data = unsafe { CFData::new(None, rgba.as_ptr(), len) }?;
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let colorspace = CGColorSpace::new_device_rgb()?;
    // Premultiplied RGBA, big-endian byte order — matches the
    // canvas tiny-skia fills.
    let bitmap_info = CGBitmapInfo(CGImageAlphaInfo::PremultipliedLast.0);
    unsafe {
        CGImage::new(
            width as usize,
            height as usize,
            8,
            32,
            (width as usize) * 4,
            Some(&colorspace),
            bitmap_info,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}

/// Blit `image` over the view's bounds. The CGImage carries its own
/// pixel dimensions (physical px from tiny-skia or `CGDisplayCreateImage`);
/// `CGContext::draw_image` scales it into the destination rect on the
/// GPU, and AppKit's `drawRect:` already gave us a context whose unit
/// space is points — so passing `bounds` (logical px) makes the image
/// appear at 1:1 logical size while preserving every captured pixel on
/// Retina (a 2× source into a 1× rect renders at the display's native
/// pixel grid). Shared by the freeze-screen background, the static
/// HUD layer, and the dynamic HUD layer so the four passes in
/// `drawRect:` all go through the same scale + colorspace path.
fn draw_hud_image(bounds: NSRect, image: &CFRetained<CGImage>) {
    use objc2_app_kit::NSGraphicsContext;
    use objc2_core_foundation::CGRect as CFRect;
    let Some(ctx) = NSGraphicsContext::currentContext() else {
        return;
    };
    let cg_ctx = ctx.CGContext();
    let rect = CFRect {
        origin: objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
        size: objc2_core_foundation::CGSize {
            width: bounds.size.width,
            height: bounds.size.height,
        },
    };
    objc2_core_graphics::CGContext::draw_image(Some(&cg_ctx), rect, Some(image));
}
