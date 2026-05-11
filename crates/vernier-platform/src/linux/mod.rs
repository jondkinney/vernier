//! Linux backend selection. Picks Wayland or X11 at runtime.

pub(crate) mod hotkey;
pub(crate) mod screencast;
mod wayland;
mod x11;

use crate::{EventReceiver, Platform, PlatformError, Result};

pub(crate) fn init() -> Result<(Box<dyn Platform>, EventReceiver)> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        log::info!("linux: selecting Wayland backend");
        wayland::init()
    } else if std::env::var_os("DISPLAY").is_some() {
        log::info!("linux: selecting X11 backend");
        x11::init()
    } else {
        Err(PlatformError::Unsupported {
            what: "no Wayland or X11 display detected",
        })
    }
}
