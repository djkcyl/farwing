//! A list of rows of different heights where only the visible rows are laid
//! out (`docs/protocol/chat-design.md` §12.1): heights measured when a row
//! was last drawn, an estimate for the others.
use std::collections::HashMap;

pub(crate) struct Heights {
    known: HashMap<i64, f32>,
    estimate: f32,
}

/// What to draw: rows `first..end`, the first at `top`; the whole list is
/// `total` high.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Window {
    pub first: usize,
    pub end: usize,
    pub top: f32,
    pub total: f32,
}

impl Heights {
    pub(crate) fn new(estimate: f32) -> Self {
        Self {
            known: HashMap::new(),
            estimate,
        }
    }

    pub(crate) fn height(&self, id: i64) -> f32 {
        self.known.get(&id).copied().unwrap_or(self.estimate)
    }

    /// A row was drawn this high; whether that changed what was known.
    pub(crate) fn set(&mut self, id: i64, height: f32) -> bool {
        // Sub-pixel changes are kept out: they would only move rows about.
        if self.known.get(&id).is_some_and(|b| (b - height).abs() <= 0.5) {
            return false;
        }
        self.known.insert(id, height);
        true
    }

    /// Forget every height (the width changed: rows wrap differently).
    pub(crate) fn clear(&mut self) {
        self.known.clear();
    }

    pub(crate) fn total(&self, ids: &[i64]) -> f32 {
        ids.iter().map(|id| self.height(*id)).sum()
    }

    /// The top of row `index`.
    pub(crate) fn offset_of(&self, ids: &[i64], index: usize) -> f32 {
        ids[..index.min(ids.len())].iter().map(|id| self.height(*id)).sum()
    }

    /// The rows that meet `top..bottom` (and `overscan` around it).
    pub(crate) fn window(&self, ids: &[i64], top: f32, bottom: f32, overscan: f32) -> Window {
        let (top, bottom) = ((top - overscan).max(0.0), bottom + overscan);
        let mut y = 0.0;
        let mut first = ids.len();
        let mut first_top = 0.0;
        let mut end = ids.len();
        for (i, id) in ids.iter().enumerate() {
            let h = self.height(*id);
            if first == ids.len() && y + h > top {
                first = i;
                first_top = y;
            }
            if y >= bottom {
                end = i;
                break;
            }
            y += h;
        }
        let total = if end == ids.len() { y } else { y + self.total(&ids[end..]) };
        if first == ids.len() {
            // Scrolled past the end: the last rows.
            first = ids.len().saturating_sub(1);
            first_top = self.offset_of(ids, first);
        }
        Window {
            first,
            end: end.max(first),
            top: first_top,
            total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_rows_in_view_are_drawn() {
        let mut h = Heights::new(20.0);
        let ids: Vec<i64> = (0..100).collect();
        let w = h.window(&ids, 0.0, 100.0, 0.0);
        assert_eq!((w.first, w.end, w.top, w.total), (0, 5, 0.0, 2000.0));
        let w = h.window(&ids, 1000.0, 1100.0, 0.0);
        assert_eq!((w.first, w.end, w.top), (50, 55, 1000.0));
        // Measured rows replace the estimate.
        assert!(h.set(0, 100.0));
        assert!(!h.set(0, 100.2));
        let w = h.window(&ids, 0.0, 100.0, 0.0);
        assert_eq!((w.first, w.end, w.total), (0, 1, 2080.0));
        let w = h.window(&ids, 110.0, 150.0, 0.0);
        assert_eq!((w.first, w.end, w.top), (1, 4, 100.0));
    }

    #[test]
    fn overscan_and_the_end() {
        let h = Heights::new(10.0);
        let ids: Vec<i64> = (0..10).collect();
        let w = h.window(&ids, 50.0, 60.0, 20.0);
        assert_eq!((w.first, w.end), (3, 8));
        let w = h.window(&ids, 500.0, 600.0, 0.0);
        assert_eq!((w.first, w.end, w.top), (9, 10, 90.0));
        let none: Vec<i64> = Vec::new();
        let w = h.window(&none, 0.0, 100.0, 0.0);
        assert_eq!((w.first, w.end, w.total), (0, 0, 0.0));
    }

    #[test]
    fn offsets_follow_heights() {
        let mut h = Heights::new(10.0);
        let ids = [5, 6, 7];
        h.set(6, 30.0);
        assert_eq!(h.offset_of(&ids, 2), 40.0);
        assert_eq!(h.total(&ids), 50.0);
        h.clear();
        assert_eq!(h.total(&ids), 30.0);
    }
}
