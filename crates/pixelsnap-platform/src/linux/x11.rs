//! X11 backend.

use crate::{EventReceiver, Platform, PlatformError, Result};

pub(crate) fn init() -> Result<(Box<dyn Platform>, EventReceiver)> {
    Err(PlatformError::Unsupported {
        what: "x11 backend not implemented yet",
    })
}
