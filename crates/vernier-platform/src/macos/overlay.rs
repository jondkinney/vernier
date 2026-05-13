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

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, DefinedClass, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSCursor, NSEvent, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::CFRetained;
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
        let clear = unsafe { NSColor::clearColor() };
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
            hud_bitmap: RefCell::new(None),
            background_image: RefCell::new(None),
            cursor_hidden: RefCell::new(false),
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
                        let mtm = MainThreadMarker::new()
                            .expect("set_input_capturing on main thread");
                        let app = NSApplication::sharedApplication(mtm);
                        app.activateIgnoringOtherApps(true);
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
                            unsafe { NSCursor::hide() };
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
                            unsafe { NSCursor::unhide() };
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
                    let tint = match &hud {
                        // Tint is the HUD background; the renderer
                        // composes strokes/pills/guides over it.
                        Some(h) => h.background,
                        None => Color::TRANSPARENT,
                    };
                    *o.view.ivars().tint.borrow_mut() = tint;
                    let bitmap = hud
                        .as_ref()
                        .and_then(|h| rasterize_hud_for_view(&o.view, h));
                    *o.view.ivars().hud_bitmap.borrow_mut() = bitmap;
                    *o.view.ivars().hud.borrow_mut() = hud;
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
                        show_cursor_on(&o.view);
                    } else if !visible && !was_hidden {
                        *o.view.ivars().cursor_hidden.borrow_mut() = true;
                        hide_cursor_on(&o.view);
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
    /// Rasterized HUD layer + the buffer dimensions that produced
    /// it. Drawn on top of the tint in `drawRect:`. Held in a
    /// RefCell because `set_hud` (async from worker) and `drawRect`
    /// (sync on main) both touch it.
    hud_bitmap: RefCell<Option<HudBitmap>>,
    /// "Freeze screen" background: the captured display frame at the
    /// moment measure mode opened, converted to a CGImage so
    /// `drawRect:` can paint it opaquely under the HUD. Held as a
    /// retained CGImage rather than the raw pixel Vec so each redraw
    /// doesn't have to re-upload to the GPU. `None` outside measure
    /// mode (overlay stays transparent and shows live content).
    background_image: RefCell<Option<CFRetained<objc2_core_graphics::CGImage>>>,
    /// NSCursor::hide / unhide is reference-counted on macOS. Track
    /// whether we're currently in the hidden state so we don't
    /// unbalance the counter (which would either leave the cursor
    /// permanently hidden after measurement mode, or make Alt's
    /// momentary unhide a no-op).
    cursor_hidden: RefCell<bool>,
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

/// A rasterized HUD ready to blit. We hang on to the CGImage so
/// `drawRect:` doesn't have to reconstruct it every paint.
pub(crate) struct HudBitmap {
    pub image: objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>,
    /// View bounds (in points, i.e. logical pixels) the bitmap
    /// was sized for. If the view resizes we'll just blit the
    /// bitmap stretched until the next `set_hud` rasterizes
    /// fresh pixels.
    pub view_size: NSSizeF,
}

#[derive(Clone, Copy)]
pub(crate) struct NSSizeF {
    pub width: f64,
    pub height: f64,
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
            unsafe {
                let color = NSColor::colorWithCalibratedRed_green_blue_alpha(
                    tint.r as f64 / 255.0,
                    tint.g as f64 / 255.0,
                    tint.b as f64 / 255.0,
                    tint.a as f64 / 255.0,
                );
                color.setFill();
                objc2_app_kit::NSRectFill(bounds);
            }
            // "Freeze screen" background: paint the captured display
            // frame underneath the HUD strokes. drawn AFTER the tint
            // (which is usually transparent in measure mode) and
            // BEFORE the HUD so cursor crosshairs / pills sit on top
            // of the frozen pixels.
            if let Some(image) = self.ivars().background_image.borrow().as_ref() {
                draw_background_image(bounds, image);
            }
            // Blit the rasterized HUD over the tint + background,
            // if we have one. The image's pixel grid was rendered at
            // the view's backing scale; CGContextDrawImage handles
            // the logical→physical mapping for us.
            if let Some(bitmap) = self.ivars().hud_bitmap.borrow().as_ref() {
                draw_hud_bitmap(bounds, bitmap);
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
                unsafe { cursor.set() };
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
                unsafe { self.addCursorRect_cursor(bounds, &cursor) };
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
    let flags: u64 = unsafe { event.modifierFlags() }.0 as u64;
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
    let vkey = unsafe { event.keyCode() };
    let is_repeat = pressed && unsafe { event.isARepeat() };
    let keysym = vkey_to_xkb_keysym(vkey as u16);
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
    let p = unsafe { objc2_app_kit::NSEvent::mouseLocation() };
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
    use objc2_app_kit::{NSTrackingArea, NSTrackingAreaOptions};
    use objc2::runtime::AnyObject;
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
    let area: Retained<NSTrackingArea> = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            NSTrackingArea::alloc(),
            rect,
            options,
            Some(owner),
            None,
        )
    };
    view.addTrackingArea(&area);
}

fn surface_local_point(view: &OverlayView, event: &NSEvent) -> (f64, f64) {
    let p_window = unsafe { event.locationInWindow() };
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
        unsafe { window.invalidateCursorRectsForView(view) };
    }
    // Also nudge the current cursor immediately so the user
    // doesn't wait for the next mouse-move to see the change.
    let cursor = transparent_cursor(view);
    unsafe { cursor.set() };
}

fn show_cursor_on(view: &OverlayView) {
    use objc2_app_kit::NSCursor;
    if let Some(window) = view.window() {
        unsafe { window.invalidateCursorRectsForView(view) };
    }
    unsafe { NSCursor::arrowCursor().set() };
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
    let image = unsafe { NSImage::initWithSize(NSImage::alloc(), size) };
    let hot = NSPoint { x: 0.0, y: 0.0 };
    let cursor = unsafe {
        NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, hot)
    };
    *view.ivars().transparent_cursor.borrow_mut() = Some(cursor.clone());
    cursor
}

// --- HUD rasterization + blit ----------------------------------------------

fn rasterize_hud_for_view(view: &OverlayView, hud: &Hud) -> Option<HudBitmap> {
    use crate::hud_render::render_hud_into;
    let bounds = view.bounds();
    let scale = view
        .window()
        .map(|w| w.backingScaleFactor())
        .unwrap_or(1.0)
        .max(1.0);
    let logical_w = bounds.size.width;
    let logical_h = bounds.size.height;
    let phys_w = ((logical_w * scale).round() as u32).max(1);
    let phys_h = ((logical_h * scale).round() as u32).max(1);

    let mut canvas = vec![0u8; (phys_w as usize) * (phys_h as usize) * 4];
    render_hud_into(
        &mut canvas,
        phys_w,
        phys_h,
        scale.round().max(1.0) as u32,
        hud,
    );

    let image = cgimage_from_rgba(&canvas, phys_w, phys_h)?;
    Some(HudBitmap {
        image,
        view_size: NSSizeF {
            width: logical_w,
            height: logical_h,
        },
    })
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

/// Paint the captured "freeze" frame into the view's bounds. The
/// CGImage has its own pixel dimensions (physical px from
/// CGDisplayCreateImage); CGContext::draw_image scales it into the
/// destination rect, and AppKit's `drawRect:` already gave us a
/// context whose unit space is points — so passing `bounds`
/// (logical px) makes the frame appear at 1:1 logical size with
/// every captured pixel preserved on Retina (a 2× source scaled
/// into a 1× destination rect renders at the display's native
/// pixel grid via the GPU). No swizzle needed: `Frame.pixels` is
/// already RGBA per `native_to_packed_rgba`.
fn draw_background_image(
    bounds: NSRect,
    image: &CFRetained<objc2_core_graphics::CGImage>,
) {
    use objc2_app_kit::NSGraphicsContext;
    use objc2_core_foundation::CGRect as CFRect;
    let Some(ctx) = (unsafe { NSGraphicsContext::currentContext() }) else {
        return;
    };
    let cg_ctx = unsafe { ctx.CGContext() };
    let rect = CFRect {
        origin: objc2_core_foundation::CGPoint { x: 0.0, y: 0.0 },
        size: objc2_core_foundation::CGSize {
            width: bounds.size.width,
            height: bounds.size.height,
        },
    };
    unsafe {
        objc2_core_graphics::CGContext::draw_image(Some(&cg_ctx), rect, Some(image));
    }
}

fn draw_hud_bitmap(bounds: NSRect, bitmap: &HudBitmap) {
    use objc2::runtime::AnyObject;
    use objc2_app_kit::NSGraphicsContext;
    use objc2_core_foundation::CGRect as CFRect;

    let Some(ctx) = (unsafe { NSGraphicsContext::currentContext() }) else {
        return;
    };
    // `NSGraphicsContext.CGContext` returns a `Retained<CGContext>`
    // in objc2-app-kit 0.3 — we deref to `&CGContext` and call the
    // CG blit, which handles the HiDPI mapping for us.
    let cg_ctx = unsafe { ctx.CGContext() };
    let rect = CFRect {
        origin: objc2_core_foundation::CGPoint {
            x: 0.0,
            y: 0.0,
        },
        size: objc2_core_foundation::CGSize {
            width: bounds.size.width,
            height: bounds.size.height,
        },
    };
    let _ = bitmap.view_size; // suppress unused-field warning until we wire size invalidation
    unsafe {
        objc2_core_graphics::CGContext::draw_image(Some(&cg_ctx), rect, Some(&bitmap.image));
        let _ = std::ptr::null::<AnyObject>();
    }
}
