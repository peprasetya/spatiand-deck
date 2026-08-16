//! Laying out a menu panel.
//!
//! Every menu in the shell — settings, the environment picker, the file browser — is the same
//! object: a card with a title, a column of rows, one of them selected, an explanation and a
//! line of button hints. This module works out where each of those goes, in plain numbers, so
//! the arithmetic can be tested without a GPU, a headset or a display.
//!
//! ## Logical pixels
//!
//! Sizes here are *logical panel pixels*, [`WIDTH`] of them across the card, and the caller
//! scales that to whatever the card measures in the world. Two things fall out of that. The
//! proportions are fixed, so the panel looks the same on optics with a different field of view
//! rather than having its type quietly grow; and the caller can rasterise at the device's real
//! resolution by scaling once, so nothing is drawn from a stretched bitmap.
//!
//! ## Why the row count is not a constant
//!
//! The vertical field is about 23°, and roughly two thirds of that is comfortably readable.
//! Chrome — a title, an explanation, hints — buys legibility with the space rows would
//! otherwise have. Rather than pick a row count and hope the chrome fits inside the remainder,
//! [`Layout::new`] is told the height it may use and fits as many rows as will honestly go,
//! then scrolls the rest. A menu that grew one entry never silently pushes its last row off
//! the bottom of the field, which is the failure this is built to make impossible.

/// A rectangle in logical panel pixels, origin at the card's top left, y downwards.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// The card's width in logical pixels. Everything else is proportional to it.
pub const WIDTH: f32 = 1000.0;

/// Padding between the card's edge and its contents.
pub const PAD_X: f32 = 42.0;

/// The width every line of text is set to.
///
/// Public because the explanation is *wrapped* to it before the layout runs — the layout needs
/// its height, and its height is however many lines it took. Two places deriving this
/// separately would put the text at one width and the box for it at another, which shows up as
/// the explanation being subtly stretched.
pub const TEXT_WIDTH: f32 = WIDTH - PAD_X * 2.0 - ROW_INSET * 2.0;
const PAD_TOP: f32 = 30.0;
const PAD_BOTTOM: f32 = 26.0;

/// Em size of the title, and the height of the header line it sits on.
pub const TITLE_EM: f32 = 40.0;
const HEADER_LINE: f32 = TITLE_EM * 1.4;
const HEADER_GAP: f32 = 16.0;

/// How tall one row is, and the gap between rows.
///
/// The row is considerably taller than its text. That space is what the selection is drawn
/// *in*: a highlight tight against the glyphs reads as a printing error, and the same
/// highlight with air around it reads as a selected item.
pub const ROW_HEIGHT: f32 = 60.0;
const ROW_GAP: f32 = 4.0;
/// Em size of a row's label, and the inset from the row's own edge to its text.
///
/// Just under a degree of the wearer's field, which is as small as this gets: the optics
/// resolve about 48 pixels per degree and a row label is read at a glance, not studied.
pub const ROW_EM: f32 = 36.0;
pub const ROW_INSET: f32 = 24.0;
/// Corner radius of the selection behind a row.
pub const ROW_RADIUS: f32 = 14.0;

/// The rule between the rows and the explanation under them.
const SEPARATOR_GAP: f32 = 18.0;
const SEPARATOR_HEIGHT: f32 = 2.0;

/// Em size of the explanation, and of the button hints.
pub const DETAIL_EM: f32 = 25.0;
pub const FOOTER_EM: f32 = 23.0;

/// Corner radius of the card itself.
pub const CARD_RADIUS: f32 = 34.0;
/// How far the rim extends past the card, giving it an edge to end at.
pub const RIM: f32 = 3.0;

/// Width of the scrollbar track, and its inset from the card's right edge.
pub const SCROLL_WIDTH: f32 = 5.0;
const SCROLL_INSET: f32 = 16.0;

/// What has to be laid out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Menu {
    pub rows: usize,
    pub cursor: usize,
    /// Height the wrapped explanation needs, in logical pixels; zero if there is none.
    pub detail_height: f32,
    /// Whether there is a line of button hints.
    pub footer: bool,
    /// The tallest the card may be. Rows are dropped into the scroll region until it fits.
    pub budget_height: f32,
}

/// Where everything goes. All rectangles are in logical pixels within a card of [`WIDTH`] by
/// [`Layout::height`].
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub height: f32,
    pub title: Rect,
    /// Index of the first row drawn.
    pub first: usize,
    /// Rows actually drawn, one rectangle each, starting at [`Layout::first`].
    pub rows: Vec<Rect>,
    pub separator: Option<Rect>,
    pub detail: Option<Rect>,
    /// The band the button hints are set in, at the **right** of the header line, sharing it
    /// with the title. On its own line the hints cost a row of the list, and they are the least
    /// important thing on the card — the one line nobody reads twice.
    pub footer: Option<Rect>,
    /// Present only when the list is longer than the space for it.
    pub scroll_track: Option<Rect>,
    pub scroll_thumb: Option<Rect>,
}

impl Layout {
    /// Lay out `menu`, keeping the cursor visible and preferring not to move the list.
    ///
    /// `first_hint` is where the list was scrolled to last time. Scrolling only when the cursor
    /// would otherwise leave the card is the difference between a list that follows you and one
    /// that slides under you on every press — recentring on the cursor is easier to write and
    /// much worse to use.
    pub fn new(menu: &Menu, first_hint: usize) -> Self {
        let content_width = WIDTH - PAD_X * 2.0;
        // Text lines up with the *row labels*, not with the row's own edge, so the card has one
        // left edge rather than two. The selection is what extends past it on both sides, which
        // is what makes the highlight read as wrapping the row rather than as a stray band.
        let text_x = PAD_X + ROW_INSET;
        let text_width = TEXT_WIDTH;
        let title = Rect {
            x: text_x,
            y: PAD_TOP,
            w: text_width,
            h: HEADER_LINE,
        };
        // The right half of the header line. The title is short in every menu there is, so
        // sharing the line costs nothing and saves a row.
        let footer = menu.footer.then_some(Rect {
            x: text_x + text_width * 0.45,
            w: text_width * 0.55,
            ..title
        });

        // Everything that is not rows, so what is left over can be divided into them.
        let below_rows = if menu.detail_height > 0.0 {
            SEPARATOR_GAP + SEPARATOR_HEIGHT + SEPARATOR_GAP + menu.detail_height
        } else {
            0.0
        };
        let chrome = PAD_TOP + HEADER_LINE + HEADER_GAP + below_rows + PAD_BOTTOM;

        let per_row = ROW_HEIGHT + ROW_GAP;
        let room = (menu.budget_height - chrome).max(0.0);
        // `+ ROW_GAP` because the last row carries no gap after it.
        let fits = ((room + ROW_GAP) / per_row).floor().max(0.0) as usize;
        let visible = fits.min(menu.rows).max(if menu.rows == 0 { 0 } else { 1 });
        let first = keep_in_view(first_hint, menu.cursor, visible, menu.rows);

        let rows_top = PAD_TOP + HEADER_LINE + HEADER_GAP;
        let rows_height = if visible == 0 {
            0.0
        } else {
            visible as f32 * per_row - ROW_GAP
        };
        let rows: Vec<Rect> = (0..visible)
            .map(|i| Rect {
                x: PAD_X,
                y: rows_top + i as f32 * per_row,
                w: content_width,
                h: ROW_HEIGHT,
            })
            .collect();

        let mut y = rows_top + rows_height;
        let (separator, detail) = if menu.detail_height > 0.0 {
            y += SEPARATOR_GAP;
            let sep = Rect {
                x: PAD_X,
                y,
                w: content_width,
                h: SEPARATOR_HEIGHT,
            };
            y += SEPARATOR_HEIGHT + SEPARATOR_GAP;
            let det = Rect {
                x: text_x,
                y,
                w: text_width,
                h: menu.detail_height,
            };
            y += menu.detail_height;
            (Some(sep), Some(det))
        } else {
            (None, None)
        };

        let height = y + PAD_BOTTOM;

        // The scrollbar exists only when something is hidden. A track that is always there,
        // full for most lists, is one more thing to look at that says nothing.
        let (scroll_track, scroll_thumb) = if visible > 0 && menu.rows > visible {
            let track = Rect {
                x: WIDTH - SCROLL_INSET - SCROLL_WIDTH,
                y: rows_top,
                w: SCROLL_WIDTH,
                h: rows_height,
            };
            let fraction = visible as f32 / menu.rows as f32;
            // A thumb shorter than it is wide stops reading as a bar, so it has a floor —
            // which does mean it no longer measures the list exactly once there are very many
            // rows. Being visible matters more than being to scale.
            let thumb_h = (track.h * fraction).max(SCROLL_WIDTH * 4.0).min(track.h);
            let travel = track.h - thumb_h;
            let progress = first as f32 / (menu.rows - visible) as f32;
            (
                Some(track),
                Some(Rect {
                    x: track.x,
                    y: track.y + travel * progress,
                    w: track.w,
                    h: thumb_h,
                }),
            )
        } else {
            (None, None)
        };

        Self {
            height,
            title,
            first,
            rows,
            separator,
            detail,
            footer,
            scroll_track,
            scroll_thumb,
        }
    }

    /// The row rectangle for `index`, if it is currently on the card.
    pub fn row(&self, index: usize) -> Option<Rect> {
        index
            .checked_sub(self.first)
            .and_then(|i| self.rows.get(i))
            .copied()
    }
}

/// Scroll as little as possible while still showing `cursor`.
pub fn keep_in_view(first: usize, cursor: usize, visible: usize, total: usize) -> usize {
    if visible == 0 || total == 0 {
        return 0;
    }
    // A list that shrank — a folder with fewer things in it than the one you left — can strand
    // the offset past the end, which would draw a card of nothing.
    let mut first = first.min(total.saturating_sub(visible));
    if cursor < first {
        first = cursor;
    } else if cursor >= first + visible {
        first = cursor + 1 - visible;
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu(rows: usize, cursor: usize) -> Menu {
        Menu {
            rows,
            cursor,
            detail_height: 70.0,
            footer: true,
            budget_height: 630.0,
        }
    }

    #[test]
    fn the_card_never_grows_past_the_height_it_was_given() {
        // The failure this exists to prevent: a menu gains an entry, the card gets taller, and
        // the bottom row is outside the field of view where it cannot be seen at all — only
        // discovered by someone wearing the glasses.
        for rows in 0..40 {
            let layout = Layout::new(&menu(rows, 0), 0);
            assert!(
                layout.height <= 630.0,
                "{rows} rows produced a card {:.0} tall",
                layout.height
            );
        }
    }

    #[test]
    fn a_short_list_is_drawn_whole_and_has_no_scrollbar() {
        let layout = Layout::new(&menu(3, 0), 0);
        assert_eq!(layout.rows.len(), 3);
        assert_eq!(layout.first, 0);
        assert!(layout.scroll_track.is_none());
    }

    #[test]
    fn a_long_list_scrolls_rather_than_running_off_the_card() {
        let layout = Layout::new(&menu(40, 0), 0);
        assert!(layout.rows.len() < 40, "should not draw all forty");
        assert!(layout.scroll_thumb.is_some());
        let last = layout.rows.last().unwrap();
        assert!(
            last.y + last.h <= layout.height,
            "last row runs past the bottom of the card"
        );
    }

    #[test]
    fn the_cursor_is_always_somewhere_on_the_card() {
        // Walked the whole way down and back up, carrying the offset forward as the real
        // caller does. A cursor that leaves the card is a cursor nobody can see.
        let mut first = 0;
        let total = 30;
        for cursor in (0..total).chain((0..total).rev()) {
            let layout = Layout::new(&menu(total, cursor), first);
            first = layout.first;
            assert!(
                layout.row(cursor).is_some(),
                "row {cursor} was off the card (first {first})"
            );
        }
    }

    #[test]
    fn walking_within_the_card_does_not_move_the_list() {
        // Recentring on the cursor is easier to write and makes every press slide the whole
        // list under you, which is much harder to read than it sounds.
        let layout = Layout::new(&menu(30, 0), 0);
        let visible = layout.rows.len();
        assert!(visible >= 2);
        let inside = Layout::new(&menu(30, visible - 1), 0);
        assert_eq!(inside.first, 0, "the last visible row is already visible");
        let past = Layout::new(&menu(30, visible), 0);
        assert_eq!(past.first, 1, "one row past the edge should scroll by one");
    }

    #[test]
    fn a_list_that_shrank_does_not_leave_the_card_empty() {
        // Walking into a directory with fewer entries than the one before it, while the
        // offset is still deep in the old list.
        let layout = Layout::new(&menu(2, 0), 25);
        assert_eq!(layout.first, 0);
        assert_eq!(layout.rows.len(), 2);
    }

    #[test]
    fn the_thumb_stays_inside_its_track() {
        for cursor in 0..30 {
            let mut first = 0;
            let layout = Layout::new(&menu(30, cursor), first);
            first = layout.first;
            let _ = first;
            let (track, thumb) = (
                layout.scroll_track.unwrap(),
                layout.scroll_thumb.unwrap(),
            );
            assert!(thumb.y >= track.y - 0.01);
            assert!(thumb.y + thumb.h <= track.y + track.h + 0.01);
        }
    }

    #[test]
    fn an_empty_menu_still_lays_out() {
        // The launcher with no applications on it, which is a real state on a fresh machine.
        let layout = Layout::new(
            &Menu {
                rows: 0,
                cursor: 0,
                detail_height: 60.0,
                footer: true,
                budget_height: 630.0,
            },
            0,
        );
        assert!(layout.rows.is_empty());
        assert!(layout.detail.is_some());
        assert!(layout.height > 0.0);
    }

    #[test]
    fn nothing_is_laid_out_past_the_cards_own_edges() {
        let layout = Layout::new(&menu(40, 20), 12);
        let inside = |r: Rect, what: &str| {
            assert!(r.x >= 0.0 && r.x + r.w <= WIDTH, "{what} is outside the card");
            assert!(r.y >= 0.0 && r.y + r.h <= layout.height, "{what} is off the card");
        };
        inside(layout.title, "title");
        for row in &layout.rows {
            inside(*row, "row");
        }
        for (rect, what) in [
            (layout.separator, "separator"),
            (layout.detail, "detail"),
            (layout.footer, "footer"),
            (layout.scroll_track, "scroll track"),
            (layout.scroll_thumb, "scroll thumb"),
        ] {
            if let Some(r) = rect {
                inside(r, what);
            }
        }
    }

    #[test]
    fn a_tighter_field_of_view_costs_rows_rather_than_legibility() {
        // Shrinking the type to fit is the wrong answer: this is read through optics that
        // resolve about 48 pixels per degree, and there is no room to give.
        let roomy = Layout::new(&menu(20, 0), 0);
        let tight = Layout::new(
            &Menu {
                budget_height: 420.0,
                ..menu(20, 0)
            },
            0,
        );
        assert!(tight.rows.len() < roomy.rows.len());
        assert_eq!(tight.rows[0].h, roomy.rows[0].h, "rows must not shrink");
        assert!(tight.height <= 420.0);
    }
}

#[cfg(test)]
mod field_of_view {
    use super::*;

    /// How many rows actually fit in an XREAL Air's field, as the compositor computes it.
    ///
    /// Written down because it is the number that decides whether the settings list scrolls,
    /// and it is otherwise buried in three multiplications. If chrome grows and this drops,
    /// the test says so rather than the wearer discovering it.
    #[test]
    fn a_card_in_the_air_holds_six_rows() {
        // 23.14 degrees vertical, about two thirds of it comfortably readable, and a card
        // 62% of the horizontal field wide.
        let logical_per_degree = WIDTH / (40.0 * 0.62);
        let budget = 23.14 * 0.68 * logical_per_degree;
        let layout = Layout::new(
            &Menu {
                rows: 20,
                cursor: 0,
                detail_height: DETAIL_EM * 1.4,
                footer: true,
                budget_height: budget,
            },
            0,
        );
        assert_eq!(layout.rows.len(), 6, "card was {:.0} logical px", layout.height);
        // And a row is close to a degree tall, which is what makes it readable at all.
        let degrees = ROW_EM / logical_per_degree;
        assert!((0.8..1.0).contains(&degrees), "row text is {degrees:.2} degrees");
    }
}
