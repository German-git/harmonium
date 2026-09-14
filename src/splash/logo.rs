//! Static ASCII art of "harmonium" and its derived metrics.
//!
//! The logo is a constant so it is never recomputed at runtime. The module
//! only exposes string slices and helpers; it does not know about crossterm or
//! ratatui, which keeps it trivially testable without a terminal.

/// ASCII rendering of "harmonium", one string per row.
pub const LOGO: &[&str] = &[
    "  _                            _            ",
    " | |_  __ _ _ _ _ __  ___ _ _ (_)_  _ _ __  ",
    " | ' \\/ _` | '_| '  \\/ _ \\ ' \\| | || | '  \\ ",
    " |_||_\\__,_|_| |_|_|_\\___/_||_|_|\\_,_|_|_|_|",
];

/// Number of rows in [`LOGO`]. The reveal sweeps exactly this many lines.
pub const LOGO_HEIGHT: usize = 4;

/// Widest row in [`LOGO`], used to center the art horizontally.
pub fn logo_width() -> usize {
    LOGO.iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_width_is_positive() {
        assert!(logo_width() > 0);
    }

    #[test]
    fn every_row_fits_inside_logo_width() {
        for row in LOGO {
            assert!(row.chars().count() <= logo_width());
        }
    }

    #[test]
    fn logo_has_expected_height() {
        assert_eq!(LOGO.len(), LOGO_HEIGHT);
    }

    #[test]
    fn logo_is_ascii_only() {
        for row in LOGO {
            for ch in row.chars() {
                assert!(ch.is_ascii(), "non-ASCII glyph {ch:?} in logo row");
            }
        }
    }
}
