//! macOS backend.

use crate::{EventReceiver, Platform, PlatformError, Result};

pub(crate) fn init() -> Result<(Box<dyn Platform>, EventReceiver)> {
    Err(PlatformError::Unsupported {
        what: "macos backend not implemented yet",
    })
}
