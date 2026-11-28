//! Dragging a virtual list's rows to reorder them.
//!
//! A list the app marks `reorder: true` lets its rows be dragged. The compositor
//! tracks the whole drag itself (the app hears nothing while it goes on): a press
//! on a row arms it, moving the pointer `START` pixels starts it, a line shows
//! the gap the row would drop into, holding the pointer near the list's top or
//! bottom edge scrolls it, and Escape cancels. Letting go sends the app one
//! `reorder` event, `[from, to]`: the row at `from` should end up at index `to`
//! (as removing it and inserting it at `to` would put it).
//!
//! This module is the geometry, apart from the desktop so it can be tested on
//! its own: which gap the pointer is nearest, where that moves a row, and how
//! fast to scroll at the edges.

use protocol::NodeId;

/// How far the pointer moves, holding a row, before it's a drag rather than a click.
pub const START: f32 = 4.0;
/// How near a list's top or bottom edge the pointer scrolls it.
pub const EDGE: f32 = 32.0;
/// The fastest the list scrolls, in pixels a second, with the pointer at or past its edge.
pub const MAX_SPEED: f32 = 900.0;

/// A row being dragged in list `list`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reorder {
    pub list: NodeId,
    /// The row pressed.
    pub from: usize,
    /// Where the pointer went down, to tell a drag from a click.
    pub start_y: f32,
    /// Moved far enough to be a drag: the drop line shows.
    pub active: bool,
    /// The gap between rows the row would drop into: 0 above the first, `count`
    /// below the last.
    pub gap: usize,
    /// How fast the list is scrolling (pixels a second, negative up), with the
    /// pointer near an edge.
    pub speed: f32,
}

impl Reorder {
    /// A press on row `from` of `list` at height `y`, not yet a drag.
    pub fn new(list: NodeId, from: usize, y: f32) -> Reorder {
        Reorder { list, from, start_y: y, active: false, gap: from, speed: 0.0 }
    }

    /// Whether the pointer, now at height `y`, has gone far enough to start the drag.
    pub fn starts_at(&self, y: f32) -> bool {
        (y - self.start_y).abs() > START
    }

    /// The `[from, to]` to tell the app on dropping here, if the row moves at all.
    pub fn result(&self) -> Option<(usize, usize)> {
        target(self.from, self.gap).map(|to| (self.from, to))
    }
}

/// The row under content height `at` (pixels from the list's first row, scrolling
/// included), among `count` rows of height `row`.
pub fn row_at(at: f32, row: f32, count: usize) -> Option<usize> {
    let i = (at / row.max(1.0)).floor();
    (i >= 0.0 && (i as usize) < count).then_some(i as usize)
}

/// The gap between rows nearest content height `at`, from 0 (above the first row)
/// to `count` (below the last).
pub fn gap_at(at: f32, row: f32, count: usize) -> usize {
    ((at / row.max(1.0)).round().max(0.0) as usize).min(count)
}

/// Where a row taken from `from` and dropped into gap `gap` ends up, or `None`
/// if that leaves it where it was (the gaps just above and below it).
pub fn target(from: usize, gap: usize) -> Option<usize> {
    let to = if gap > from { gap - 1 } else { gap };
    (to != from).then_some(to)
}

/// How fast to scroll a list `h` high whose top is at `top`, with the pointer at
/// height `y`: faster the nearer the edge, flat out at it or past it. Negative
/// scrolls up. The zone near each edge is at most a quarter of the list, so a
/// short list still has a middle that doesn't scroll.
pub fn edge_speed(y: f32, top: f32, h: f32) -> f32 {
    let zone = EDGE.min(h / 4.0);
    if zone <= 0.0 {
        return 0.0;
    }
    let bottom = top + h;
    if y < top + zone {
        -MAX_SPEED * ((top + zone - y) / zone).min(1.0)
    } else if y > bottom - zone {
        MAX_SPEED * ((y - (bottom - zone)) / zone).min(1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_and_gaps_come_from_the_pointers_height() {
        assert_eq!(row_at(0.0, 20.0, 5), Some(0));
        assert_eq!(row_at(39.9, 20.0, 5), Some(1));
        assert_eq!(row_at(100.0, 20.0, 5), None, "below the last row");
        assert_eq!(row_at(-1.0, 20.0, 5), None);
        // The nearest gap: past the middle of a row is below it.
        assert_eq!(gap_at(9.0, 20.0, 5), 0);
        assert_eq!(gap_at(11.0, 20.0, 5), 1);
        assert_eq!(gap_at(-50.0, 20.0, 5), 0);
        assert_eq!(gap_at(500.0, 20.0, 5), 5, "below the last row");
    }

    #[test]
    fn dropping_next_to_the_row_leaves_it_and_elsewhere_moves_it() {
        // Row 2, dropped into the gaps either side of it, stays.
        assert_eq!(target(2, 2), None);
        assert_eq!(target(2, 3), None);
        // Up: it goes where the gap is. Down: one less, as it left a hole above.
        assert_eq!(target(2, 0), Some(0));
        assert_eq!(target(2, 1), Some(1));
        assert_eq!(target(2, 4), Some(3));
        assert_eq!(target(2, 5), Some(4), "the end of five rows");
    }

    #[test]
    fn a_press_becomes_a_drag_once_it_moves() {
        let mut r = Reorder::new(7, 2, 100.0);
        assert!(!r.starts_at(103.0));
        assert!(r.starts_at(95.0));
        assert_eq!(r.result(), None, "not moved yet");
        r.gap = 5;
        assert_eq!(r.result(), Some((2, 4)));
    }

    #[test]
    fn the_list_scrolls_faster_nearer_its_edges() {
        // A 400-high list from y = 100: the zones are 32 high.
        assert_eq!(edge_speed(300.0, 100.0, 400.0), 0.0, "the middle");
        assert_eq!(edge_speed(132.0, 100.0, 400.0), 0.0, "just inside the top zone's edge");
        assert!((edge_speed(116.0, 100.0, 400.0) + MAX_SPEED / 2.0).abs() < 1e-3, "halfway into it, up");
        assert_eq!(edge_speed(100.0, 100.0, 400.0), -MAX_SPEED);
        assert_eq!(edge_speed(20.0, 100.0, 400.0), -MAX_SPEED, "above the list, flat out");
        assert!((edge_speed(484.0, 100.0, 400.0) - MAX_SPEED / 2.0).abs() < 1e-3, "down");
        assert_eq!(edge_speed(900.0, 100.0, 400.0), MAX_SPEED);
        // A short list keeps a middle.
        assert_eq!(edge_speed(120.0, 100.0, 40.0), 0.0);
        assert_eq!(edge_speed(100.0, 100.0, 0.0), 0.0, "no list to speak of");
    }
}
