//! Startup splash rendered directly with crossterm, before Ratatui attaches.
//!
//! The caller is expected to have already entered raw mode and the alternate
//! screen (via `TerminalGuard`) once. The splash only draws and never manages
//! the terminal lifecycle, so disabling it with [`ENABLED`] or removing the
//! module leaves the app untouched. A horizontal scan line sweeps downward and
//! progressively reveals the static ASCII logo held in [`crate::splash::logo`].
//!
//! Cancellation: Esc or a bare `q` ends the loop early. Other key events are
//! forwarded to the shared event bus, while worker events remain queued because
//! the splash does not drain the bus. The loop is clocked by real time
//! (`Instant`) and throttled by `poll`, never by `sleep`.

mod animation;
mod logo;

use std::io::{self, Stdout, Write};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{Event, KeyCode, KeyModifiers, poll, read};
use crossterm::execute;
use crossterm::queue;
use crossterm::style::{Color as CrosstermColor, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{Clear, ClearType, size};

use harmonium::event::{AppEvent, EventSender};
use harmonium::ui::theme::Theme;
use ratatui::style::Color;

use animation::{Scanline, build_rows, center_xy, revealed_rows};
use logo::{LOGO, LOGO_HEIGHT, logo_width};

/// Master switch: set to false to skip the splash without touching the app.
pub const ENABLED: bool = true;

/// Poll interval: 16ms caps the loop at ~60fps while keeping input latency low
/// and staying well above the 30fps floor.
const FRAME_POLL: Duration = Duration::from_millis(16);
const INPUT_FORWARD_TIMEOUT: Duration = Duration::from_millis(100);

/// Extra columns the scan line extends beyond each edge of the logo, so it
/// frames the art subtly without touching it. Two columns per side.
const SCAN_OVERHANG: usize = 2;

/// How many rows above the exact vertical center the logo block starts.
const VERTICAL_OFFSET: usize = 4;

/// Result of the splash: whether the user cancelled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// True when the user cancelled with Esc or `q` before the timeline ended.
    pub cancelled: bool,
}

/// Duration budget for each phase of the animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplashConfig {
    /// Idle time before the scan starts.
    pub prep: Duration,
    /// Time the scan line takes to sweep the full logo height.
    pub reveal: Duration,
    /// Time the fully revealed logo stays on screen.
    pub hold: Duration,
    /// Nominal tail budget before the splash yields control.
    pub transition: Duration,
}

impl Default for SplashConfig {
    fn default() -> Self {
        Self {
            prep: Duration::from_millis(40),
            reveal: Duration::from_millis(420),
            hold: Duration::from_millis(140),
            transition: Duration::ZERO,
        }
    }
}

impl SplashConfig {
    /// Nominal total budget, materially below the former ~1.4s timeline.
    pub fn total(&self) -> Duration {
        self.prep + self.reveal + self.hold + self.transition
    }
}

/// Timeline phases of the animation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Before the scan begins.
    Prep,
    /// The scan line is sweeping across the logo.
    Scanning,
    /// The logo is fully revealed.
    Hold,
    /// The timeline has completed.
    Done,
}

/// Stateful splash driving the timeline and the per-frame render.
pub struct Splash {
    config: SplashConfig,
    elapsed: Duration,
    phase: Phase,
    scan: Scanline,
    width: usize,
    height: usize,
}

impl Splash {
    /// Build a splash with an empty timeline and an unknown screen size.
    pub fn new(config: SplashConfig) -> Self {
        Self {
            config,
            elapsed: Duration::ZERO,
            phase: Phase::Prep,
            scan: Scanline::from_progress(0.0, LOGO_HEIGHT),
            width: 0,
            height: 0,
        }
    }

    /// Update the cached terminal size (also called on `Resize` events).
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
    }

    /// Advance the timeline by `delta`, moving the phase and the scan position.
    pub fn update(&mut self, delta: Duration) {
        self.elapsed += delta;
        let total = self.config.total();
        let e = self.elapsed.min(total);

        let prep_end = self.config.prep;
        let reveal_end = prep_end + self.config.reveal;
        let hold_end = reveal_end + self.config.hold;

        self.phase = if e < prep_end {
            Phase::Prep
        } else if e < reveal_end {
            Phase::Scanning
        } else if e < hold_end {
            Phase::Hold
        } else {
            Phase::Done
        };

        let reveal_progress = match self.phase {
            Phase::Prep => 0.0,
            Phase::Scanning => {
                let into = e.saturating_sub(prep_end);
                (into.as_secs_f32() / self.config.reveal.as_secs_f32()).clamp(0.0, 1.0)
            }
            Phase::Hold | Phase::Done => 1.0,
        };

        self.scan = Scanline::from_progress(reveal_progress, LOGO_HEIGHT);
    }

    /// True once the timeline has fully completed.
    pub fn is_finished(&self) -> bool {
        self.phase == Phase::Done
    }

    /// Render the current frame to `writer` on the shared alternate screen.
    pub fn render<W: Write>(&self, writer: &mut W, theme: &Theme) -> io::Result<()> {
        let tw = self.width;
        let th = self.height;

        // Reposition from the origin and clear the prior frame state cheaply:
        // rows are painted with absolute MoveTo, the hidden cursor stays off.
        queue!(writer, Hide, MoveTo(0, 0))?;

        let (start_x, start_y) = center_xy(logo_width(), LOGO_HEIGHT, tw, th);
        // Nudge the whole block (logo + scanline) a few rows above the exact
        // vertical center; saturating keeps it from leaving the top edge.
        let start_y = start_y.saturating_sub(VERTICAL_OFFSET);
        let avail = tw.saturating_sub(start_x);
        let revealed = revealed_rows(self.scan.y, LOGO_HEIGHT);

        // Center the scan line on the art and let it overhang a couple of
        // columns on each side so it frames the logo without touching it. Both
        // are clamped so the line never leaves the terminal.
        let art_center = start_x + logo_width() / 2;
        let scan_width = (logo_width() + 2 * SCAN_OVERHANG).min(tw);
        let scan_x = art_center
            .saturating_sub(scan_width / 2)
            .min(tw.saturating_sub(scan_width));
        let scan_right = scan_x + scan_width;

        // Paint one row further: `revealed` counts the rows fully above the
        // scan line, so the row the line sits on is still "being revealed"
        // and must be visible under the line. Otherwise the scan line would
        // sit one row ahead of the art it is supposed to be revealing.
        for (i, row) in build_rows(LOGO, revealed + 1).into_iter().enumerate() {
            if avail == 0 {
                continue;
            }
            let y = start_y + i;
            if y >= th {
                continue;
            }
            // Never write past the right edge: truncate to the available width.
            let visible: String = row.chars().take(avail).collect();
            // Coordinates fit u16 by construction: they derive from the cached
            // terminal size and the `y < th` guard keeps them in range.
            let x = start_x as u16;
            let row_y = y as u16;
            queue!(
                writer,
                MoveTo(x, row_y),
                SetForegroundColor(crossterm_color(theme.border_focused)),
                Print(&visible)
            )?;
            // Erase any residue the wider scan line left on this row: a run
            // after the art and a run before it, because the line is centered
            // and therefore overhangs both edges.
            let visible_len = visible.chars().count();
            let trailing = scan_right.saturating_sub(start_x + visible_len);
            if trailing > 0 {
                queue!(writer, Print(" ".repeat(trailing)))?;
            }
            let leading = start_x.saturating_sub(scan_x);
            if leading > 0 {
                queue!(
                    writer,
                    MoveTo(scan_x as u16, row_y),
                    Print(" ".repeat(leading))
                )?;
            }
        }

        if revealed < LOGO_HEIGHT && scan_width > 0 {
            let scan_y = start_y + revealed;
            if scan_y < th {
                queue!(
                    writer,
                    MoveTo(scan_x as u16, scan_y as u16),
                    SetForegroundColor(crossterm_color(theme.border_focused)),
                    Print("─".repeat(scan_width))
                )?;
            }
        }

        queue!(writer, ResetColor)?;
        Ok(())
    }
}

/// Run the splash on the already-configured alternate screen.
///
/// Returns `Ok(outcome)` after the timeline completes or the user cancels.
/// I/O failures are returned for the caller to log and continue: the splash
/// must never prevent the app from booting.
pub fn run(theme: &Theme, events: &EventSender) -> io::Result<Outcome> {
    let mut stdout = io::stdout();
    let mut splash = Splash::new(SplashConfig::default());

    let (tw, th) = size()?;
    splash.resize(tw as usize, th as usize);

    // Prepare a clean, cursor-free screen once; render() owns per-frame output.
    queue!(stdout, Hide, Clear(ClearType::All), MoveTo(0, 0))?;
    stdout.flush()?;

    let cancelled = splash_loop(&mut stdout, &mut splash, theme, events);

    // Always restore a blank, default-colored screen with the cursor back on,
    // so a mid-splash error never leaves a hidden cursor or dirty screen.
    let restore = execute!(
        stdout,
        Show,
        ResetColor,
        Clear(ClearType::All),
        MoveTo(0, 0)
    );
    match cancelled {
        Ok(cancelled) => {
            restore?;
            Ok(Outcome { cancelled })
        }
        Err(loop_error) => {
            // Terminal restoration (raw mode + alternate screen) is owned by
            // TerminalGuard::drop; here we only reset the screen we dirtied.
            let _ = restore;
            Err(loop_error)
        }
    }
}

/// Drive the animation loop, returning whether the user cancelled.
///
/// Clocked by real time (`Instant`) and throttled by `poll`, never by `sleep`.
/// Returns a cancelled flag on a clean exit and an I/O error otherwise.
fn splash_loop(
    stdout: &mut Stdout,
    splash: &mut Splash,
    theme: &Theme,
    events: &EventSender,
) -> io::Result<bool> {
    let mut last = Instant::now();
    let mut cancelled = false;

    loop {
        splash.update(last.elapsed());
        last = Instant::now();

        if poll(FRAME_POLL)? {
            match read()? {
                Event::Key(event)
                    if event.code == KeyCode::Esc
                        || (event.code == KeyCode::Char('q')
                            && !event.modifiers.contains(KeyModifiers::SHIFT)) =>
                {
                    cancelled = true;
                    break;
                }
                Event::Resize(width, height) => {
                    splash.resize(width as usize, height as usize);
                }
                Event::Key(event) => {
                    let _ =
                        events.send_critical_timeout(AppEvent::Key(event), INPUT_FORWARD_TIMEOUT);
                }
                _ => {}
            }
        }

        if splash.is_finished() {
            splash.render(stdout, theme)?;
            stdout.flush()?;
            break;
        }

        splash.render(stdout, theme)?;
        stdout.flush()?;
    }

    Ok(cancelled)
}

/// Map a ratatui theme color into the crossterm palette.
///
/// This is the single place where colors cross the crate boundary. Bright
/// variants collapse onto their base color (crossterm has no distinct bright
/// set) and indexed/rgb values pass through unchanged.
fn crossterm_color(color: Color) -> CrosstermColor {
    match color {
        Color::Reset => CrosstermColor::Reset,
        Color::Black => CrosstermColor::Black,
        Color::Red => CrosstermColor::Red,
        Color::Green => CrosstermColor::Green,
        Color::Yellow => CrosstermColor::Yellow,
        Color::Blue => CrosstermColor::Blue,
        Color::Magenta => CrosstermColor::Magenta,
        Color::Cyan => CrosstermColor::Cyan,
        Color::Gray => CrosstermColor::Grey,
        Color::DarkGray => CrosstermColor::DarkGrey,
        Color::LightRed => CrosstermColor::Red,
        Color::LightGreen => CrosstermColor::Green,
        Color::LightYellow => CrosstermColor::Yellow,
        Color::LightBlue => CrosstermColor::Blue,
        Color::LightMagenta => CrosstermColor::Magenta,
        Color::LightCyan => CrosstermColor::Cyan,
        Color::White => CrosstermColor::White,
        Color::Indexed(value) => CrosstermColor::AnsiValue(value),
        Color::Rgb(r, g, b) => CrosstermColor::Rgb { r, g, b },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::style::Color as C;

    #[test]
    fn default_config_total_is_materially_shorter_than_the_old_timeline() {
        assert_eq!(SplashConfig::default().total(), Duration::from_millis(600));
        assert!(SplashConfig::default().total() < Duration::from_millis(700));
    }

    #[test]
    fn phase_advances_across_every_region() {
        let mut splash = Splash::new(SplashConfig::default());
        assert_eq!(splash.phase, Phase::Prep);

        splash.update(Duration::from_millis(40));
        assert_eq!(splash.phase, Phase::Scanning);

        splash.update(Duration::from_millis(420));
        assert_eq!(splash.phase, Phase::Hold);
        assert!(!splash.is_finished());

        splash.update(Duration::from_millis(140));
        assert_eq!(splash.phase, Phase::Done);
        assert!(splash.is_finished());
    }

    #[test]
    fn scan_stays_above_logo_during_prep() {
        let mut splash = Splash::new(SplashConfig::default());
        splash.update(Duration::from_millis(20));
        assert_eq!(splash.phase, Phase::Prep);
        assert_eq!(revealed_rows(splash.scan.y, LOGO_HEIGHT), 0);
    }

    #[test]
    fn logo_is_fully_revealed_when_holding() {
        let mut splash = Splash::new(SplashConfig::default());
        splash.update(Duration::from_millis(460));
        assert_eq!(splash.phase, Phase::Hold);
        assert_eq!(revealed_rows(splash.scan.y, LOGO_HEIGHT), LOGO_HEIGHT);
    }

    #[test]
    fn completed_timeline_reports_finished() {
        let mut splash = Splash::new(SplashConfig::default());
        splash.update(SplashConfig::default().total());
        assert_eq!(splash.phase, Phase::Done);
        assert!(splash.is_finished());
    }

    #[test]
    fn crossterm_color_maps_representative_colors() {
        assert_eq!(crossterm_color(Color::Reset), C::Reset);
        assert_eq!(crossterm_color(Color::White), C::White);
        assert_eq!(crossterm_color(Color::DarkGray), C::DarkGrey);
        assert_eq!(crossterm_color(Color::Gray), C::Grey);
        assert_eq!(crossterm_color(Color::Indexed(196)), C::AnsiValue(196));
        assert_eq!(
            crossterm_color(Color::Rgb(0, 128, 255)),
            C::Rgb {
                r: 0,
                g: 128,
                b: 255
            }
        );
    }

    #[test]
    fn render_has_no_background_and_uses_line_color_for_every_glyph() {
        // Distinct colors so each requirement is provable from the output bytes:
        // background = red, old highlight (text) = yellow, line color = blue.
        let theme = Theme {
            background: Color::Rgb(255, 0, 0),
            highlight: Color::Rgb(255, 255, 0),
            border_focused: Color::Rgb(0, 0, 255),
            ..Theme::default()
        };

        let mut splash = Splash::new(SplashConfig::default());
        splash.resize(120, 40);
        // Jump to the hold phase so the whole logo is revealed.
        splash.update(Duration::from_millis(500));

        let mut out: Vec<u8> = Vec::new();
        splash.render(&mut out, &theme).unwrap();
        let buf = String::from_utf8_lossy(&out);

        // 1. No background color is emitted at all.
        assert!(
            !buf.contains("48;"),
            "background color leaked into splash: {buf:?}"
        );

        // 2. Every glyph uses the line color (border_focused), not the old
        //    per-role colors.
        assert!(
            buf.contains("38;2;0;0;255"),
            "line color (border_focused) missing: {buf:?}"
        );
        assert!(
            !buf.contains("38;2;255;255;0"),
            "old highlight (text) color still used: {buf:?}"
        );

        // 3. Art is horizontally centered relative to the terminal. Derive the
        //    expected position from the same constants the renderer uses.
        let (sx, sy) = center_xy(logo_width(), LOGO_HEIGHT, 120, 40);
        let sy = sy.saturating_sub(VERTICAL_OFFSET);
        let expected = format!("\x1b[{};{}H", sy + 1, sx + 1);
        assert!(
            buf.contains(&expected),
            "art is not horizontally centered (expected {expected:?}): {buf:?}"
        );
    }

    #[test]
    fn render_centers_the_scanline_and_makes_it_wider_than_the_art() {
        let theme = Theme::default();
        let mut splash = Splash::new(SplashConfig::default());
        splash.resize(120, 40);
        // Stay in the prep phase: no art row revealed yet, only the scan line is
        // on screen, so its position and width are easy to observe.
        splash.update(Duration::ZERO);

        let mut out: Vec<u8> = Vec::new();
        splash.render(&mut out, &theme).unwrap();
        let buf = String::from_utf8_lossy(&out);

        // Derive the intended geometry from the same constants the renderer
        // uses, so a future art swap cannot silently break this test.
        let (sx, sy) = center_xy(logo_width(), LOGO_HEIGHT, 120, 40);
        let sy = sy.saturating_sub(VERTICAL_OFFSET);
        let art_center = sx + logo_width() / 2;
        let scan_width = (logo_width() + 2 * SCAN_OVERHANG).min(120);
        let scan_x = art_center
            .saturating_sub(scan_width / 2)
            .min(120 - scan_width);

        // The scan line must share the art's center and be wider than it. Its
        // first row sits at start_y (prep) and crossterm coordinates are 1-based.
        let expected = format!("\x1b[{};{}H", sy + 1, scan_x + 1);
        assert!(
            buf.contains(&expected),
            "scan line is not centered on the art (expected {expected:?}): {buf:?}"
        );

        // Exactly the intended scan width: box-drawing glyphs.
        let count = buf.matches("─").count();
        assert_eq!(count, scan_width, "scan line width is unexpected: {count}");
    }
}
