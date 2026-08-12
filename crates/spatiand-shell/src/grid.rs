//! D-pad navigation over a grid of items.
//!
//! Small enough to look trivial, and worth its own module anyway: every off-by-one here is
//! something the wearer feels rather than sees. The cursor jumping two cells, or wrapping when
//! it should stop, or landing on a different row than the one you were tracking with your eyes
//! — none of that is visible in a screenshot, and all of it is testable here.
//!
//! Two decisions, both deliberate:
//!
//! * **Edges clamp, they do not wrap.** A grid that wraps means pressing right on the last
//!   icon throws the cursor to the far side of the view. On a screen you would catch that; in
//!   a headset you have to turn your head to find out where it went.
//! * **Vertical movement keeps the column.** Moving down from a short last row lands on the
//!   nearest existing item in that column rather than refusing to move, so the cursor never
//!   gets stuck against a ragged edge.

/// A cursor over `count` items laid out in rows of `columns`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    columns: usize,
    count: usize,
    cursor: usize,
}

/// Which way the D-pad was pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Grid {
    pub fn new(columns: usize, count: usize) -> Self {
        Self {
            columns: columns.max(1),
            count,
            cursor: 0,
        }
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn columns(&self) -> usize {
        self.columns
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn row(&self) -> usize {
        self.cursor / self.columns
    }

    pub fn column(&self) -> usize {
        self.cursor % self.columns
    }

    pub fn rows(&self) -> usize {
        self.count.div_ceil(self.columns)
    }

    /// Point the cursor at a specific item, if it exists.
    pub fn set_cursor(&mut self, index: usize) {
        if index < self.count {
            self.cursor = index;
        }
    }

    /// Change the number of items, keeping the cursor somewhere valid.
    ///
    /// Called whenever the app list is rescanned. Without the clamp, a list that shrank would
    /// leave the cursor pointing past the end and the next activation would do nothing.
    pub fn resize(&mut self, count: usize) {
        self.count = count;
        if self.cursor >= count {
            self.cursor = count.saturating_sub(1);
        }
    }

    /// Move the cursor. Returns `true` if it actually moved.
    pub fn step(&mut self, direction: Direction) -> bool {
        if self.count == 0 {
            return false;
        }
        let before = self.cursor;
        match direction {
            Direction::Left => {
                // Stop at the start of the row, not the start of the grid: running off the
                // left edge onto the end of the row above is disorienting when the rows are
                // spread across your field of view.
                if self.column() > 0 {
                    self.cursor -= 1;
                }
            }
            Direction::Right => {
                if self.column() + 1 < self.columns && self.cursor + 1 < self.count {
                    self.cursor += 1;
                }
            }
            Direction::Up => {
                if self.cursor >= self.columns {
                    self.cursor -= self.columns;
                }
            }
            Direction::Down => {
                let below = self.cursor + self.columns;
                if below < self.count {
                    self.cursor = below;
                } else if self.row() + 1 < self.rows() {
                    // There is a row below, but it is short and has nothing in this column.
                    // Land on its last item rather than refusing to move.
                    self.cursor = self.count - 1;
                }
            }
        }
        self.cursor != before
    }

    /// Where item `index` sits, as `(column, row)`.
    pub fn position(&self, index: usize) -> (usize, usize) {
        (index % self.columns, index / self.columns)
    }

    /// Horizontal offset of an item from the middle of its row, in cell widths.
    ///
    /// The launcher centres each row, so the last row of a ragged grid sits centred under the
    /// full ones rather than bunched to the left. Returning this as a float keeps the caller
    /// free of the half-cell arithmetic that makes an even-width row look off by half a slot.
    pub fn centred_offset(&self, index: usize) -> f32 {
        let (column, row) = self.position(index);
        let in_this_row = if row + 1 < self.rows() {
            self.columns
        } else {
            // The last row may be short.
            self.count - row * self.columns
        };
        column as f32 - (in_this_row as f32 - 1.0) * 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Direction::*;

    #[test]
    fn an_empty_grid_absorbs_input_without_panicking() {
        let mut g = Grid::new(4, 0);
        assert!(g.is_empty());
        for d in [Up, Down, Left, Right] {
            assert!(!g.step(d));
        }
        assert_eq!(g.cursor(), 0);
    }

    #[test]
    fn moving_right_advances_one_cell_at_a_time() {
        let mut g = Grid::new(4, 8);
        assert!(g.step(Right));
        assert_eq!(g.cursor(), 1);
        assert!(g.step(Right));
        assert_eq!(g.cursor(), 2);
    }

    #[test]
    fn the_edges_clamp_rather_than_wrap() {
        // Wrapping throws the cursor across the wearer's whole field of view, and unlike on a
        // monitor you cannot see where it went without turning your head.
        let mut g = Grid::new(3, 9);
        g.set_cursor(2); // end of the first row
        assert!(!g.step(Right), "should not wrap to the next row");
        assert_eq!(g.cursor(), 2);
        g.set_cursor(3); // start of the second row
        assert!(!g.step(Left), "should not wrap back to the first row");
        assert_eq!(g.cursor(), 3);
    }

    #[test]
    fn vertical_movement_keeps_the_column() {
        let mut g = Grid::new(4, 12);
        g.set_cursor(1);
        g.step(Down);
        assert_eq!(g.cursor(), 5, "column should be preserved");
        assert_eq!(g.column(), 1);
        g.step(Down);
        assert_eq!(g.cursor(), 9);
        g.step(Up);
        assert_eq!(g.cursor(), 5);
    }

    #[test]
    fn moving_up_from_the_top_row_stays_put() {
        let mut g = Grid::new(4, 12);
        g.set_cursor(2);
        assert!(!g.step(Up));
        assert_eq!(g.cursor(), 2);
    }

    #[test]
    fn a_ragged_last_row_is_still_reachable() {
        // 6 items in rows of 4: the second row holds only two. Pressing down from the third
        // column has nothing directly below it, and getting stuck there makes the last item
        // unreachable by D-pad — which on a controller-only device means unreachable at all.
        let mut g = Grid::new(4, 6);
        g.set_cursor(3);
        assert!(g.step(Down), "should find something on the short row");
        assert_eq!(g.cursor(), 5, "should land on the last item");
    }

    #[test]
    fn moving_down_from_the_bottom_row_stays_put() {
        let mut g = Grid::new(4, 6);
        g.set_cursor(5);
        assert!(!g.step(Down));
        assert_eq!(g.cursor(), 5);
    }

    #[test]
    fn a_shrinking_list_never_leaves_the_cursor_past_the_end() {
        // The app list is rescanned while the launcher is open. A stale cursor here means the
        // next press of A launches nothing, or the wrong thing.
        let mut g = Grid::new(4, 12);
        g.set_cursor(11);
        g.resize(5);
        assert_eq!(g.cursor(), 4);
        g.resize(0);
        assert_eq!(g.cursor(), 0);
        assert!(g.is_empty());
    }

    #[test]
    fn rows_are_counted_including_a_partial_one() {
        assert_eq!(Grid::new(4, 8).rows(), 2);
        assert_eq!(Grid::new(4, 9).rows(), 3);
        assert_eq!(Grid::new(4, 0).rows(), 0);
    }

    #[test]
    fn a_full_row_is_centred_on_zero() {
        let g = Grid::new(4, 8);
        let offsets: Vec<f32> = (0..4).map(|i| g.centred_offset(i)).collect();
        assert_eq!(offsets, vec![-1.5, -0.5, 0.5, 1.5]);
        assert!(offsets.iter().sum::<f32>().abs() < 1e-6, "should balance");
    }

    #[test]
    fn a_short_last_row_is_centred_too() {
        // Otherwise the final row bunches to the left and the grid looks broken rather than
        // merely uneven.
        let g = Grid::new(4, 6);
        assert_eq!(g.centred_offset(4), -0.5);
        assert_eq!(g.centred_offset(5), 0.5);
    }

    #[test]
    fn a_single_item_sits_dead_centre() {
        let g = Grid::new(4, 1);
        assert_eq!(g.centred_offset(0), 0.0);
    }

    #[test]
    fn zero_columns_is_treated_as_one_rather_than_dividing_by_zero() {
        let g = Grid::new(0, 3);
        assert_eq!(g.columns(), 1);
        assert_eq!(g.rows(), 3);
    }
}
