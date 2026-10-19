// SPDX-License-Identifier: LGPL-2.1-or-later
//! POC output ordering, separate from the lifetime of reference surfaces.
use super::dxva::Picture;
use anyhow::{Result, ensure};
use std::collections::VecDeque;

pub struct Frame {
    pub picture: Picture,
    pub token: i64,
}
#[derive(Default)]
pub struct Output {
    waiting: Vec<Frame>,
    ready: VecDeque<Frame>,
    dropped: VecDeque<i64>,
}
impl Output {
    pub fn push(&mut self, frame: Frame) -> Result<()> {
        let limit = frame.picture.reorder_limit as usize;
        ensure!(limit <= 16, "invalid output reorder bound");
        if let Some(discard) = frame.picture.sequence_start {
            if discard {
                self.dropped.extend(self.waiting.drain(..).map(|f| f.token));
                self.dropped.extend(self.ready.drain(..).map(|f| f.token));
            } else {
                self.drain();
            }
        }
        if !frame.picture.needed_for_output {
            self.dropped.push_back(frame.token);
            return Ok(());
        }
        self.waiting.push(frame);
        while self.waiting.len() > limit {
            self.bump();
        }
        Ok(())
    }
    fn bump(&mut self) {
        if let Some((index, _)) = self
            .waiting
            .iter()
            .enumerate()
            .min_by_key(|(_, f)| f.picture.poc)
        {
            self.ready.push_back(self.waiting.remove(index));
        }
    }
    pub fn poll(&mut self) -> Option<Frame> {
        self.ready.pop_front()
    }
    pub fn discard_token(&mut self, token: i64) {
        self.dropped.push_back(token);
    }
    pub fn poll_dropped(&mut self) -> Option<i64> {
        self.dropped.pop_front()
    }
    pub fn drain(&mut self) {
        while !self.waiting.is_empty() {
            self.bump();
        }
    }
    pub fn reset(&mut self) {
        self.waiting.clear();
        self.ready.clear();
        self.dropped.clear();
    }
}
