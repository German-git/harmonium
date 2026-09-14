//! Responsive layout computation from the terminal size.
//!
//! Pure functions only so every rule can be unit tested without a terminal.

use ratatui::layout::{Constraint, Layout, Rect};

/// Heights below this value collapse the now playing band.
///
/// With now playing at 5 rows: now_playing(5) + footer(1) = 6, so at height 21
/// the main area gets 15 rows. The band is a compact strip holding the title,
/// progress bar with time overlay, and status in three lines; artwork no
/// longer lives here.
pub const COMPACT_HEIGHT_THRESHOLD: u16 = 21;
const MAIN_MIN_HEIGHT: u16 = 2;
/// Five rows give the band three inner rows, enough for the three text
/// lines, while freeing vertical space for the browser and playlist panels.
const NOW_PLAYING_HEIGHT: u16 = 5;
const FOOTER_HEIGHT: u16 = 1;

/// Typed rectangles produced by the responsive layout pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutAreas {
    /// Browser column of the main split.
    pub main_browser: Rect,
    /// Playlist column of the main split.
    pub main_playlist: Rect,
    /// Now playing band, hidden on short terminals.
    pub now_playing: Option<Rect>,
    /// One row hint footer.
    pub footer: Rect,
}

/// One-cell gap between painted blocks so every panel is separated by the
/// same uniform margin, matching the one-cell padding inside each block.
pub const PANEL_GAP: u16 = 1;

/// Split the terminal into the standard vertical bands and main columns.
///
/// The now playing band disappears when the terminal is shorter than
/// [`COMPACT_HEIGHT_THRESHOLD`] rows so panels keep usable space. Every
/// painted block is separated by a uniform [`PANEL_GAP`] margin.
pub fn compute_layout(area: Rect) -> LayoutAreas {
    let show_now_playing = area.height >= COMPACT_HEIGHT_THRESHOLD;
    let now_playing_height = if show_now_playing {
        NOW_PLAYING_HEIGHT
    } else {
        0
    };

    // The now playing band sits directly against the main panels and the
    // footer: their own borders already paint the one-cell separator, so an
    // extra gap row would make the top/bottom margin look too tall.
    let [main, now_playing_band, footer] = Layout::vertical([
        Constraint::Min(MAIN_MIN_HEIGHT),
        Constraint::Length(now_playing_height),
        Constraint::Length(FOOTER_HEIGHT),
    ])
    .areas(area);

    let [browser, _gap, playlist] = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Length(PANEL_GAP),
        Constraint::Ratio(2, 3),
    ])
    .areas(main);

    LayoutAreas {
        main_browser: browser,
        main_playlist: playlist,
        now_playing: show_now_playing.then_some(now_playing_band),
        footer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(width: u16, height: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width,
            height,
        }
    }

    #[test]
    fn tall_terminal_shows_now_playing_band() {
        let areas = compute_layout(rect(90, 30));
        let now_playing = areas.now_playing.expect("now playing must be visible");

        assert_eq!(now_playing.height, NOW_PLAYING_HEIGHT);
        // Main panels end at 24 and the band sits directly below (no gap row).
        assert_eq!(areas.main_browser.height, 24);
        assert_eq!(now_playing.y, 24);
        assert_eq!(areas.footer.y, 29);
        assert_eq!(areas.footer.height, FOOTER_HEIGHT);
    }

    #[test]
    fn short_terminal_hides_now_playing_band() {
        let areas = compute_layout(rect(90, COMPACT_HEIGHT_THRESHOLD - 1));

        assert!(areas.now_playing.is_none());
        assert_eq!(areas.footer.y, 19);
    }

    #[test]
    fn boundary_height_keeps_the_band_visible() {
        let areas = compute_layout(rect(90, COMPACT_HEIGHT_THRESHOLD));

        assert!(areas.now_playing.is_some());
        // Threshold 21: main(15) + np(5) + footer(1). No vertical gap rows.
        assert_eq!(areas.main_browser.height, 15);
    }

    #[test]
    fn main_columns_follow_one_third_two_thirds_ratio() {
        let areas = compute_layout(rect(60, 30));

        // Width 60 minus a one-cell gap between the columns: 59 split 1/3-2/3.
        assert_eq!(areas.main_browser.width, 19);
        assert_eq!(areas.main_playlist.width, 40);
    }

    #[test]
    fn columns_fill_the_full_main_height() {
        let areas = compute_layout(rect(60, 30));

        assert_eq!(areas.main_browser.height, 24);
        assert_eq!(areas.main_playlist.height, 24);
    }

    #[test]
    fn band_sits_against_the_main_panels_with_no_extra_gap() {
        let areas = compute_layout(rect(90, 30));
        let main_bottom = areas.main_browser.y + areas.main_browser.height;
        let band = areas.now_playing.expect("band visible");

        assert_eq!(
            band.y, main_bottom,
            "the now playing band must sit directly below the main panels"
        );
    }

    #[test]
    fn columns_are_separated_by_one_cell() {
        let areas = compute_layout(rect(90, 30));
        let browser_right = areas.main_browser.x + areas.main_browser.width;

        assert_eq!(
            areas.main_playlist.x,
            browser_right + PANEL_GAP,
            "the playlist column must be one cell right of the browser column"
        );
    }
}
