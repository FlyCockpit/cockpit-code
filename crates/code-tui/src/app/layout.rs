//! Transcript geometry the last frame recorded: where each entry starts,
//! the scroll offset and the rows on screen. Drawing writes it; pointer
//! handling, search and scrolling read it.
use ratatui::layout::Rect;

/// Transcript geometry from the last frame.
#[derive(Default, Clone)]
pub(super) struct Layout {
    pub starts: Vec<usize>,
    pub lens: Vec<usize>,
    pub total: usize,
    pub top: usize,
    pub view_h: usize,
    /// The transcript rows on screen (sticky header excluded, scrollbar
    /// column excluded).
    pub area: Rect,
}

impl Layout {
    /// The entry and its row at absolute transcript row `abs` (None on a gap).
    pub fn locate(&self, abs: usize) -> Option<(usize, usize)> {
        let i = self.starts.partition_point(|&s| s <= abs).checked_sub(1)?;
        (abs < self.starts[i] + self.lens[i]).then(|| (i, abs - self.starts[i]))
    }
    /// The entry at screen row `y`, counting the gap row below an entry as
    /// part of it (None outside the transcript rows).
    pub fn entry_near(&self, x: u16, y: u16) -> Option<usize> {
        let a = self.area;
        if y < a.y || y >= a.bottom() || x < a.x || x > a.right() {
            return None;
        }
        let abs = self.top + (y - a.y) as usize;
        if abs >= self.total {
            return None;
        }
        self.starts.partition_point(|&s| s <= abs).checked_sub(1)
    }
    /// The absolute row at screen row `y` (clamped into the view).
    pub fn abs_at(&self, y: u16) -> usize {
        let rel = (y as isize - self.area.y as isize).clamp(0, self.view_h.max(1) as isize - 1);
        (self.top + rel as usize).min(self.total.saturating_sub(1))
    }
}
