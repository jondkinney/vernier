//! NSStatusItem-backed menubar icon.
//!
//! AppKit gives each process a single status item by convention.
//! The icon is a template image (auto-tinted for light/dark menu
//! bars) sourced from `crate::icon::render_tray_icon_rgba`. Menu
//! items use a single Objective-C target/action that funnels the
//! activation back through a per-item id stored in the menu
//! item's `representedObject`.

use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem};
use objc2_foundation::{MainThreadMarker, NSSize, NSString};

use crate::{PlatformError, PlatformEvent, Result, TrayHandle, TrayMenu, TrayMenuItem, TrayOps};

pub(crate) struct TrayResources {
    pub status_item: Retained<NSStatusItem>,
    pub target: Retained<TrayTarget>,
}

pub(crate) fn create(menu: TrayMenu) -> Result<TrayHandle> {
    super::app::run_on_main_sync(move || -> Result<TrayHandle> {
        let mtm = MainThreadMarker::new().expect("tray on main");
        log::info!("macos tray: creating NSStatusItem on main");

        if super::with_main_state(|s| s.tray.is_some()) {
            return Err(PlatformError::Other(anyhow::anyhow!(
                "macOS allows only one tray status item per process"
            )));
        }

        let bar = NSStatusBar::systemStatusBar();
        // NSStatusItem.variableLength == -1.0.
        let status_item = bar.statusItemWithLength(-1.0);
        log::info!("macos tray: status item created");

        let button = status_item
            .button(mtm)
            .ok_or_else(|| PlatformError::Other(anyhow::anyhow!("NSStatusItem.button was nil")))?;

        // Prefer Vernier's custom V glyph (the dashed-tick variant of
        // the brand mark, designed to live in the menu bar at
        // ~18 pt). Rasterized from the SVG via tiny-skia, wrapped
        // as a template NSImage so AppKit tints it for light / dark
        // menu bars automatically. Falls through to the SF Symbol
        // "ruler" if anything in the render path fails (missing
        // assets, allocation), and to a plain "V" title as a last
        // resort — NSStatusBar buttons render a zero-width button
        // on Sequoia if both image and title are empty.
        const TRAY_GLYPH_PT: f64 = 18.0;
        if let Some(img) = render_tray_template_image(TRAY_GLYPH_PT) {
            log::info!("macos tray: using custom V glyph");
            img.setTemplate(true);
            button.setImage(Some(&img));
        } else {
            let symbol_name = NSString::from_str("ruler");
            let accessibility = NSString::from_str("Vernier");
            let symbol_image: Option<Retained<NSImage>> =
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &symbol_name,
                    Some(&accessibility),
                );
            match symbol_image {
                Some(img) => {
                    log::info!("macos tray: custom glyph failed, using SF Symbol 'ruler'");
                    img.setTemplate(true);
                    button.setImage(Some(&img));
                }
                None => {
                    log::info!("macos tray: no image available, falling back to 'V' title");
                    button.setTitle(&NSString::from_str("V"));
                }
            }
        }
        button.setToolTip(Some(&NSString::from_str(&menu.tooltip)));

        let target = TrayTarget::new(mtm);
        // The status-item button itself triggers `on_status_click`
        // when the user left-clicks. Menu items run through their
        // own per-item target/action.
        button.setTarget(Some(&*target));
        button.setAction(Some(sel!(onStatusClick:)));

        let ns_menu = build_menu(&menu, &target, mtm);
        status_item.setMenu(Some(&ns_menu));

        // Explicit visibility — defaults to true, but some
        // launch paths on Sequoia have left items invisible.
        // Belt-and-suspenders.
        status_item.setVisible(true);

        super::with_main_state(|s| {
            s.tray = Some(TrayResources {
                status_item: status_item.clone(),
                target: target.clone(),
            });
        });
        log::info!(
            "macos tray: stored TrayResources, visible={}",
            status_item.isVisible()
        );

        Ok(TrayHandle::from_backend(MacTray {}))
    })
}

fn build_menu(menu: &TrayMenu, target: &TrayTarget, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let ns_menu = NSMenu::new(mtm);
    for item in &menu.items {
        append_item(&ns_menu, item, target, mtm);
    }
    ns_menu
}

fn append_item(parent: &NSMenu, item: &TrayMenuItem, target: &TrayTarget, mtm: MainThreadMarker) {
    match item {
        TrayMenuItem::Separator => {
            let sep = NSMenuItem::separatorItem(mtm);
            parent.addItem(&sep);
        }
        TrayMenuItem::Action {
            id, label, enabled, ..
        } => {
            let mi = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel!(onMenuItem:)),
                &NSString::from_str(""),
            );
            mi.setTarget(Some(target));
            mi.setEnabled(*enabled);
            mi.setRepresentedObject(Some(&NSString::from_str(id)));
            parent.addItem(&mi);
        }
        TrayMenuItem::Toggle {
            id,
            label,
            enabled,
            checked,
        } => {
            let mi = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                Some(sel!(onMenuItem:)),
                &NSString::from_str(""),
            );
            mi.setTarget(Some(target));
            mi.setEnabled(*enabled);
            mi.setState(if *checked {
                objc2_app_kit::NSControlStateValueOn
            } else {
                objc2_app_kit::NSControlStateValueOff
            });
            mi.setRepresentedObject(Some(&NSString::from_str(id)));
            parent.addItem(&mi);
        }
        TrayMenuItem::Submenu {
            id: _,
            label,
            items,
        } => {
            let mi = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(label),
                None,
                &NSString::from_str(""),
            );
            let submenu = NSMenu::new(mtm);
            for sub in items {
                append_item(&submenu, sub, target, mtm);
            }
            mi.setSubmenu(Some(&submenu));
            parent.addItem(&mi);
        }
    }
}

struct MacTray {}

impl TrayOps for MacTray {
    fn update_menu(&mut self, menu: TrayMenu) -> Result<()> {
        super::app::run_on_main_sync(move || -> Result<()> {
            let mtm = MainThreadMarker::new().expect("tray update on main");
            super::with_main_state(|s| {
                if let Some(t) = s.tray.as_ref() {
                    let new_menu = build_menu(&menu, &t.target, mtm);
                    t.status_item.setMenu(Some(&new_menu));
                }
            });
            Ok(())
        })
    }

    fn set_active(&mut self, active: bool) {
        super::app::run_on_main_async(move || {
            super::with_main_state(|s| {
                if let Some(t) = s.tray.as_ref() {
                    if let Some(button) =
                        t.status_item.button(MainThreadMarker::new().expect("main"))
                    {
                        button.setAppearsDisabled(!active);
                    }
                }
            });
        });
    }
}

// --- Target/action delegate -------------------------------------------------

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "VernierTrayTarget"]
    pub(crate) struct TrayTarget;

    impl TrayTarget {
        #[unsafe(method(onStatusClick:))]
        fn on_status_click(&self, _sender: Option<&NSObject>) {
            // The default NSStatusItem menu handling pops the menu
            // on left-click already. Emit a `TrayIconLeftClicked`
            // for parity with the Linux SNI event.
            if let Some(tx) = super::event_tx() {
                let _ = tx.send(PlatformEvent::TrayIconLeftClicked { x: 0, y: 0 });
            }
        }

        #[unsafe(method(onMenuItem:))]
        fn on_menu_item(&self, sender: Option<&NSMenuItem>) {
            let Some(item) = sender else { return };
            let Some(id) = item.representedObject() else {
                return;
            };
            let Ok(s) = id.downcast::<NSString>() else {
                return;
            };
            if let Some(tx) = super::event_tx() {
                let _ = tx.send(PlatformEvent::TrayMenuActivated { id: s.to_string() });
            }
        }
    }
);

impl TrayTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![mtm.alloc::<Self>(), init] }
    }
}

/// Build the menu-bar template image at the requested point size.
/// Rasterizes the V-glyph SVG at 2× the requested points so it's
/// crisp on Retina, then wraps the bytes in a CGImage and hands it
/// to NSImage with the *logical* size as its declared dimensions
/// (so AppKit scales the @2x backing automatically). Returns `None`
/// if any step in the render pipeline fails — caller should fall
/// through to a glyph the OS can render itself.
fn render_tray_template_image(size_pt: f64) -> Option<Retained<NSImage>> {
    use objc2_core_foundation::CFData;
    use objc2_core_graphics::{
        CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage,
        CGImageAlphaInfo,
    };

    let pixel_size = (size_pt * 2.0).round().max(1.0) as u32;
    let rgba = crate::icon::render_tray_icon_rgba(pixel_size);
    if rgba.len() != (pixel_size * pixel_size * 4) as usize {
        return None;
    }

    let data = unsafe { CFData::new(None, rgba.as_ptr(), rgba.len() as isize) }?;
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let colorspace = CGColorSpace::new_device_rgb()?;
    // The SVG rasterizer (`rasterize_svg`) demultiplies before
    // returning, so the bytes are straight (un-premultiplied) RGBA.
    // `Last` alpha info matches that layout; using
    // `PremultipliedLast` here would darken the glyph against the
    // template's tinted backing.
    let bitmap_info = CGBitmapInfo(CGImageAlphaInfo::Last.0);
    let cg = unsafe {
        CGImage::new(
            pixel_size as usize,
            pixel_size as usize,
            8,
            32,
            (pixel_size as usize) * 4,
            Some(&colorspace),
            bitmap_info,
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;

    let ns_size = NSSize {
        width: size_pt,
        height: size_pt,
    };
    let image = NSImage::initWithCGImage_size(NSImage::alloc(), &cg, ns_size);
    Some(image)
}
