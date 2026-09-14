//! Pure, terminal-agnostic animation math for the scanline reveal.
//!
//! Nothing here touches crossterm: positions and row visibility are computed
//! from a normalized progress value, so the whole logic is unit-tested without
//! a TTY. The scan line starts one row above the logo and ends one row below
//! it, sweeping downward while revealing one logo row at a time.

/// Smooth the scan progression so it eases in and out instead of moving
/// linearly. Returns 0 at `0.0`, 1 at `1.0` and 0.5 at the midpoint.
pub fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

/// Vertical position of the scan line, in terminal rows, for a given reveal
/// progress.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scanline {
    /// Fractional row of the scan line, allowing sub-row smoothing.
    pub y: f32,
}

impl Scanline {
    /// Build the scan position from normalized progress `p` in `0.0..=1.0`.
    ///
    /// The scan starts just above the logo (`-1.0`) and ends just below it
    /// (`height + 1.0`), easing through `smoothstep` so the sweeping motion
    /// feels natural.
    pub fn from_progress(p: f32, height: usize) -> Self {
        let t = p.clamp(0.0, 1.0);
        Self {
            y: -1.0 + (height as f32 + 1.0) * smoothstep(t),
        }
    }
}

/// Fully revealed rows for a scan position, clamped to `0..=height`.
///
/// The scan line sits on row `revealed_rows`: rows above it are already
/// visible, the row it occupies is being revealed and the rest are hidden.
pub fn revealed_rows(scan_y: f32, height: usize) -> usize {
    let raw = scan_y.floor().max(0.0);
    (raw as usize).min(height)
}

/// Left column that centers a `width` block in a `terminal_width` screen.
///
/// When the block is wider than the screen, saturating subtraction yields `0`
/// so the caller never writes past the left edge.
pub fn center_x(width: usize, terminal_width: usize) -> usize {
    terminal_width.saturating_sub(width) / 2
}

/// Top row that centers a `height` block in a `terminal_height` screen.
pub fn center_y(height: usize, terminal_height: usize) -> usize {
    terminal_height.saturating_sub(height) / 2
}

/// Combined centering origin for a `logo_w` x `logo_h` block.
pub fn center_xy(
    logo_width: usize,
    logo_height: usize,
    terminal_width: usize,
    terminal_height: usize,
) -> (usize, usize) {
    (
        center_x(logo_width, terminal_width),
        center_y(logo_height, terminal_height),
    )
}

/// The first `revealed` rows of `logo`, leaving the rest out.
///
/// Used to render the partially revealed art. The renderer passes
/// `revealed + 1` (clamped inside here) so the row the scan line currently
/// crosses is painted too — it is the row "being revealed", not yet a fully
/// revealed one. Rows beyond the scan line stay blank.
pub fn build_rows<'a>(logo: &[&'a str], revealed: usize) -> Vec<&'a str> {
    logo.iter()
        .take(revealed.min(logo.len()))
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::splash::logo::{LOGO, LOGO_HEIGHT};

    #[test]
    fn smoothstep_hands_back_the_endpoints_and_midpoint() {
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert_eq!(smoothstep(0.5), 0.5);
    }

    #[test]
    fn scanline_spans_from_above_to_below_the_logo() {
        let start = Scanline::from_progress(0.0, LOGO_HEIGHT);
        assert_eq!(start.y, -1.0);

        let end = Scanline::from_progress(1.0, LOGO_HEIGHT);
        assert!(end.y >= LOGO_HEIGHT as f32);
    }

    #[test]
    fn revealed_rows_are_clamped() {
        assert_eq!(revealed_rows(-3.0, 5), 0);
        assert_eq!(revealed_rows(0.0, 5), 0);
        assert_eq!(revealed_rows(2.0, 5), 2);
        assert_eq!(revealed_rows(4.9, 5), 4);
        assert_eq!(revealed_rows(10.0, 5), 5);
    }

    #[test]
    fn revealed_rows_are_monotonic() {
        let height = 6;
        let mut previous = 0usize;
        for step in 0..=20 {
            let rows = revealed_rows(step as f32 * 0.5, height);
            assert!(rows >= previous, "reveal went backwards at step {step}");
            previous = rows;
        }
    }

    #[test]
    fn reveal_starts_empty_and_completes() {
        let at_start = Scanline::from_progress(0.0, LOGO_HEIGHT);
        assert_eq!(revealed_rows(at_start.y, LOGO_HEIGHT), 0);

        let at_end = Scanline::from_progress(1.0, LOGO_HEIGHT);
        assert!(revealed_rows(at_end.y, LOGO_HEIGHT) >= LOGO_HEIGHT);
    }

    #[test]
    fn center_x_handles_even_odd_and_small_screens() {
        assert_eq!(center_x(10, 40), 15);
        assert_eq!(center_x(10, 41), 15);
        assert_eq!(center_x(11, 40), 14);
        assert_eq!(center_x(60, 40), 0);
    }

    #[test]
    fn center_y_handles_even_odd_and_small_screens() {
        assert_eq!(center_y(5, 20), 7);
        assert_eq!(center_y(5, 21), 8);
        assert_eq!(center_y(30, 20), 0);
    }

    #[test]
    fn center_xy_combines_both_axes() {
        assert_eq!(center_xy(10, 5, 40, 20), (15, 7));
        assert_eq!(center_xy(60, 30, 40, 20), (0, 0));
    }

    #[test]
    fn build_rows_returns_the_requested_prefix() {
        let prefix = build_rows(LOGO, 3);
        assert_eq!(prefix, vec![LOGO[0], LOGO[1], LOGO[2]]);

        let all = build_rows(LOGO, LOGO_HEIGHT + 10);
        assert_eq!(all.len(), LOGO_HEIGHT);

        let none = build_rows(LOGO, 0);
        assert!(none.is_empty());
    }
}
