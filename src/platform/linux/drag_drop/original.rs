//! The original gesture of a drag that was handed off and may be resumed.
//! Linux does not offer the drag return (`super::RETURN_CAPABLE`), so there
//! is never one to keep.
use anyhow::Result;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Original {}
impl Original {
    pub fn live(&self) -> bool {
        match *self {}
    }
    pub fn cancel(&self) -> Result<()> {
        match *self {}
    }
}
