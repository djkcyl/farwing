//! Bounded replay of account device pushes across the resident/GUI boundary.
use super::DeviceChange;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Cursor {
    epoch: String,
    sequence: u64,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Batch {
    pub cursor: Cursor,
    pub resync: bool,
    pub changes: Vec<DeviceChange>,
}

pub(crate) struct Relay {
    cursor: Cursor,
    entries: VecDeque<(u64, usize, DeviceChange)>,
    bytes: usize,
}

impl Relay {
    pub fn new() -> Self {
        Self {
            cursor: Cursor {
                epoch: uuid::Uuid::new_v4().to_string(),
                sequence: 0,
            },
            entries: VecDeque::new(),
            bytes: 0,
        }
    }

    /// A new account or reconnected upstream cannot replay the old stream.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn push(&mut self, change: DeviceChange) {
        // Keep this well below the shared IPC frame limit. A large change or
        // a reader that misses retained history recovers from canonical HTTP.
        const MAX_BYTES: usize = 512 * 1024;
        let Ok(encoded) = serde_json::to_vec(&change) else {
            self.reset();
            return;
        };
        let size = encoded.len();
        if size > MAX_BYTES {
            self.reset();
            return;
        }
        if self.cursor.sequence == u64::MAX {
            self.reset();
        }
        self.cursor.sequence += 1;
        self.bytes += size;
        self.entries.push_back((self.cursor.sequence, size, change));
        while self.entries.len() > 128 || self.bytes > MAX_BYTES {
            let (_, size, _) = self.entries.pop_front().unwrap();
            self.bytes -= size;
        }
    }

    /// Reads do not drain the relay: GUI, installer and restarted readers
    /// must never consume each other's notifications.
    pub fn read(&self, after: Option<&Cursor>) -> Batch {
        let retained = after.filter(|after| {
            after.epoch == self.cursor.epoch
                && after.sequence <= self.cursor.sequence
                && (after.sequence == self.cursor.sequence
                    || self
                        .entries
                        .front()
                        .is_some_and(|(first, _, _)| *first <= after.sequence.saturating_add(1)))
        });
        Batch {
            cursor: self.cursor.clone(),
            resync: retained.is_none(),
            changes: retained.map_or_else(Vec::new, |after| {
                self.entries
                    .iter()
                    .filter(|(sequence, _, _)| *sequence > after.sequence)
                    .map(|(_, _, change)| change.clone())
                    .collect()
            }),
        }
    }
}
