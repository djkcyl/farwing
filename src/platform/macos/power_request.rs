//! Power requests, viewer-only: macOS keeps the display awake through the
//! window server while a connection renders, so there is nothing to request.
use anyhow::Result;

pub(crate) struct Request;
impl Request {
    pub(crate) fn capture() -> Result<Self> {
        Ok(Self)
    }
    pub(crate) fn viewing() -> Result<Self> {
        Ok(Self)
    }
}
