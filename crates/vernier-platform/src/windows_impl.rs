//! Windows backend.

use crate::{EventReceiver, Platform, PlatformError, Result};

pub(crate) fn init() -> Result<(Box<dyn Platform>, EventReceiver)> {
    Err(PlatformError::Unsupported {
        what: "windows backend not implemented yet",
    })
}
