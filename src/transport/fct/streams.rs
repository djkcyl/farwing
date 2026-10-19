//! Keep early reliable messages until DCEP identifies and opens their channel.
use std::collections::{HashMap, VecDeque};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Binding {
    Waiting,
    Open,
    Rejected,
}

#[derive(Default)]
pub(super) struct Streams {
    bindings: HashMap<u16, Binding>,
    pending: VecDeque<(u16, Vec<u8>)>,
    bytes: usize,
}

impl Streams {
    pub fn bind(&mut self, id: u16, control: bool) {
        // A replacement channel cannot inherit an old channel's queued input.
        // Only the first DCEP registration may claim pre-registration messages.
        if !control || self.bindings.contains_key(&id) {
            self.discard(id);
        }
        self.bindings.insert(
            id,
            if control {
                Binding::Waiting
            } else {
                Binding::Rejected
            },
        );
    }

    pub fn set_open(&mut self, id: u16, open: bool) {
        if !open {
            self.discard(id);
        }
        self.bindings.insert(
            id,
            if open {
                Binding::Open
            } else {
                Binding::Rejected
            },
        );
    }

    pub fn push(&mut self, id: u16, bytes: Vec<u8>) -> Result<bool, &'static str> {
        if self.bindings.get(&id) == Some(&Binding::Rejected) {
            return Ok(false);
        }
        if self.pending.len() >= 64
            || bytes.len() > super::MAX_CONTROL_MESSAGE.saturating_sub(self.bytes)
        {
            return Err("mixed-KCP pre-open CONTROL queue exceeded its bound");
        }
        self.bytes += bytes.len();
        self.pending.push_back((id, bytes));
        Ok(true)
    }

    pub fn pop_ready(&mut self) -> Option<(u16, Vec<u8>)> {
        let index = self
            .pending
            .iter()
            .position(|(id, _)| self.bindings.get(id) == Some(&Binding::Open))?;
        let item = self.pending.remove(index)?;
        self.bytes -= item.1.len();
        Some(item)
    }

    fn discard(&mut self, id: u16) {
        self.pending.retain(|(stream, bytes)| {
            if *stream == id {
                self.bytes -= bytes.len();
                false
            } else {
                true
            }
        });
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}
