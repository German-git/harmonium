//! Rendering routines for the individual screen regions.
//!
//! Every color comes from [`Theme`] so a future palette loader can swap the
//! look without touching layout code here. The biggest responsibilities are
//! split out: the settings editor and the modal popups live in submodules,
//! while the browser/playlist/lyrics panels stay together because they share
//! a dense web of helpers.

mod popups;
mod settings;
mod widgets;

#[cfg(test)]
use std::sync::Arc;

pub use popups::{
    draw_confirm_delete_popup, draw_confirm_overwrite_popup, draw_confirm_quit_popup,
    draw_confirm_sort_tracks_popup, draw_help_popup, draw_metadata_form, draw_naming_dialog,
    draw_playlist_manager_popup, draw_rename_collision_popup, draw_search_popup,
};
pub use settings::draw_settings;

use crate::APP_VERSION;
use crate::ui::theme::Theme;
use crate::ui::view::{
    LyricsLayoutView, LyricsLineView, Panel, PanelViewModel, PlaybackVisualState,
};
use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use self::widgets::{CHECKBOX_CHECKED, sanitize_text, truncate_to_width};

/// Chrome rows consumed by a bordered panel: the titled top border plus the
/// plain bottom border. Callers derive the usable viewport by subtracting
/// this from the panel height, matching the assumptions in `layout`.
pub const PANEL_BORDER_ROWS: u16 = 2;
/// Gap inserted between footer segments.
const HINT_GAP: &str = "   ";
/// Hint separator between contextual key bindings.
const HINT_SEPARATOR: &str = " · ";
/// Reserved marker column width (in cells) shown in front of every playlist
/// entry so the playing track can show a run icon without shifting the rest.
const PLAYLIST_MARKER_WIDTH: u16 = 2;

/// Draw the file browser panel listing the current directory.
///
/// The visible slice honors the browser scroll window so the cursor always
/// stays on screen, mirroring the half-open range kept by `BrowserState`.
pub fn draw_browser(
    frame: &mut Frame,
    area: Rect,
    view: &PanelViewModel,
    focused: bool,
    theme: &Theme,
) {
    let browser = &view.browser;
    let title = browser_title_from_view(&browser.title_location, browser.entries.len(), area.width);
    let block = panel_block(&title, focused, theme);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The name uses the full inner width: the list starts one cell from the
    // border (the block border) with no marker column shifting entries.
    let name_budget = usize::from(inner.width);
    let start = browser.visible_start;
    let end = (start + browser.visible_count).min(browser.entries.len());

    if browser.entries.is_empty() {
        // An empty directory is a common state, not an error: give the user a
        // plain hint instead of a blank panel.
        let hint = Line::from(Span::styled(
            "Directory is empty",
            Style::new().fg(theme.text_muted),
        ));
        frame.render_widget(
            Paragraph::new(hint).style(Style::new().bg(theme.background)),
            inner,
        );
        return;
    }

    let rows: Vec<Line> = browser.entries[start..end]
        .iter()
        .map(|entry| {
            browser_entry_view_line(
                &entry.display_name,
                entry.playable,
                entry.marked,
                entry.selected,
                name_budget,
                theme,
            )
        })
        .collect();

    frame.render_widget(
        Paragraph::new(rows).style(Style::new().bg(theme.background)),
        inner,
    );
}

/// Draw the playlist panel with its queued track entries.
///
/// Rows follow the configured Playlist columns presentation. The queue order
/// is independent and changes only through the explicit Settings action.
pub fn draw_playlist(
    frame: &mut Frame,
    area: Rect,
    view: &PanelViewModel,
    focused: bool,
    theme: &Theme,
) {
    let playlist = &view.playlist;
    let title = &playlist.title;
    let block = panel_block(&title, focused, theme);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Reserve a fixed marker column so the playing track shows the run icon
    // while every other row keeps the same indentation, keeping names aligned.
    let name_budget = usize::from(inner.width.saturating_sub(PLAYLIST_MARKER_WIDTH));
    let rows: Vec<Line> = if playlist.rows.is_empty() {
        vec![Line::from(Span::styled(
            "Playlist empty - press a in the browser",
            Style::new().fg(theme.text_muted),
        ))]
    } else {
        let visible = playlist.visible_count;
        let start = playlist.visible_start;

        playlist
            .rows
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(_, row)| {
                let name = truncate_with_ellipsis(&row.name, name_budget);
                let style = selected_or_plain(Style::new().fg(theme.text), row.selected, theme);
                Line::from(vec![
                    Span::styled(row.marker, style),
                    Span::styled(name, style),
                ])
            })
            .collect()
    };

    frame.render_widget(
        Paragraph::new(rows).style(Style::new().bg(theme.background)),
        inner,
    );
}

/// Draw the now playing band with the active track info and progress.
///
/// Three text rows (title, progress bar, status) inside a bordered block
/// using the full inner width. Album art no longer renders here; it is
/// drawn as a focused overlay on whichever panel is not currently active:
///
/// ```text
/// Artist — Title
/// ██████████7:02   12:03████████████████████████████
/// ▷        Vol 35%      Repeat All    Shuffle Off
/// ```
///
/// The playback state is conveyed only by the glyph in the status row; the
/// title line carries no redundant icon. Each metric sits in a fixed-width
/// column instead of being split by a pipe.
///
/// The time text is centered on the progress bar, with each character's
/// background matching the bar segment behind it for legibility.
pub fn draw_now_playing(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(" Now playing ", Style::new().fg(theme.border)))
        .border_style(Style::new().fg(theme.border))
        .style(Style::new().bg(theme.artwork_area));

    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    // Vertically center the three text rows inside the band so the compact
    // height reads as deliberate space instead of an empty gap.
    let top = inner.y + (inner.height.saturating_sub(3)) / 2;

    // --- Row 1: ▶ Artist — Title ---
    let title_row = Rect {
        height: 1,
        width: inner.width,
        x: inner.x,
        y: top,
    };

    // The Now Playing label comes straight from metadata/file stem and is the
    // most exposed path (no truncation above), so sanitize before rendering.
    let label = sanitize_text(&view.now_playing.label);

    // The playback state is conveyed only by the status row below the progress
    // bar, so the track name line stays clean and carries no redundant glyph.
    let title_line = Line::from(vec![Span::styled(label, Style::new().fg(theme.text))]);
    frame.render_widget(Paragraph::new(title_line), title_row);

    // --- Row 2: progress bar with centered time overlay ---
    let gauge_row = Rect {
        height: 1,
        width: inner.width,
        x: inner.x,
        y: top.saturating_add(1),
    };

    // While a stream is loading (resolving the URL or decoding the body),
    // the progress bar carries no useful information (no duration yet, no
    // elapsed time yet). Replace it with a centered spinner + "Loading"
    // line so the row stays visually occupied and the user gets the same
    // animation the Add Stream popup uses. The flag is cleared in
    // `apply_stream_resolved` (resolve phase) and `apply_source_ready`
    // (decode phase) so the swap happens on the same frame the matching
    // event arrives.
    if view.now_playing.stream_loading {
        let loading_spans = loading_line(&view.now_playing.spinner_frame);
        // Center the row horizontally inside the band: split the leftover
        // space into a left and right pad so the spinner sits in the
        // middle, matching the visual position of the time overlay on the
        // regular progress bar.
        let text_width = loading_visible_width(&loading_spans);
        let total_padding = inner.width.saturating_sub(text_width);
        let left_padding = total_padding / 2;
        let right_padding = total_padding - left_padding;
        let mut row_spans = Vec::new();
        if left_padding > 0 {
            row_spans.push(Span::styled(
                " ".repeat(left_padding as usize),
                Style::new().bg(theme.artwork_area),
            ));
        }
        for span in loading_spans {
            row_spans.push(span.style(Style::new().fg(theme.text).bg(theme.artwork_area)));
        }
        if right_padding > 0 {
            row_spans.push(Span::styled(
                " ".repeat(right_padding as usize),
                Style::new().bg(theme.artwork_area),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(row_spans)).style(Style::new().bg(theme.artwork_area)),
            gauge_row,
        );
    } else {
        let times_text = format!(
            "{} / {}",
            view.now_playing.elapsed, view.now_playing.duration
        );
        let bar_width = inner.width;

        let fill = (view.now_playing.progress * bar_width as f64).round() as u16;

        // Build the bar with the time text overlaid. The helper groups
        // adjacent columns into bounded spans, while measuring the label in
        // terminal display cells rather than bytes or scalar-value count.
        let bar_spans = progress_bar_spans(bar_width, fill, &times_text, theme);

        frame.render_widget(Paragraph::new(Line::from(bar_spans)), gauge_row);
    }

    // --- Row 3: state · Vol · Repeat · Shuffle, each in a fixed-width column ---
    let status_row = Rect {
        height: 1,
        width: inner.width,
        x: inner.x,
        y: top.saturating_add(2),
    };

    // State glyph (unfilled variants for a subtle look): the word is replaced
    // so the status reads purely through the icon, mirroring the play/pause
    // glyph language used elsewhere in the UI.
    let state_glyph = match view.now_playing.status {
        PlaybackVisualState::Playing => "\u{25b7}",        // ▷
        PlaybackVisualState::Paused => "\u{275a}\u{275a}", // ❚❚ (two pause bars)
        PlaybackVisualState::Stopped => "\u{25a1}",        // □
    };
    let state_color = match view.now_playing.status {
        PlaybackVisualState::Playing => theme.playing,
        PlaybackVisualState::Paused => theme.paused,
        PlaybackVisualState::Stopped => theme.stopped,
    };

    // Each metric sits in its own fixed-width padded column (left-aligned)
    // instead of a pipe-separated strip. The state glyph gets a narrow column,
    // then Vol / Repeat / Shuffle each occupy distinct widths.
    let mut status_spans = pad_status_field(
        vec![Span::styled(
            state_glyph.to_string(),
            Style::new().fg(state_color),
        )],
        STATE_COLUMN_WIDTH,
    );
    status_spans.extend(pad_status_field(
        vec![
            // Visual level as a 5-block bar; one block per 20 % so the
            // glyph never overflows at 100 %. Floor division keeps the
            // drawn bar at or below the actual percentage rather than
            // rounding up early. The bar sits next to the value without a
            // leading pictogram, so the column is purely "level + %".
            Span::styled(
                format!(
                    "{}{} ",
                    "\u{25AE}".repeat((u16::from(view.now_playing.volume_percent) / 20) as usize),
                    "\u{25AF}".repeat(
                        5 - (u16::from(view.now_playing.volume_percent) / 20).min(5) as usize,
                    ),
                ),
                Style::new().fg(theme.text_muted),
            ),
            Span::styled(
                format!("{}%", view.now_playing.volume_percent),
                Style::new().fg(theme.status_line),
            ),
        ],
        VOLUME_COLUMN_WIDTH,
    ));
    // Speed sits immediately to the right of Volume, before Repeat/Shuffle.
    // The value mirrors the UI-owned playback state (never persisted) which is
    // pushed to the audio worker through AudioCommand::SetSpeed.
    status_spans.extend(pad_status_field(
        vec![
            Span::styled(" ⏱ ", Style::new().fg(theme.text_muted)),
            Span::styled(
                format!("{:.1}x", view.now_playing.speed),
                Style::new().fg(theme.status_line),
            ),
        ],
        SPEED_COLUMN_WIDTH,
    ));
    status_spans.extend(pad_status_field(
        vec![
            Span::styled(" ↻ ", Style::new().fg(theme.text_muted)),
            Span::styled(
                view.now_playing.repeat_label.clone(),
                if view.now_playing.repeat_active {
                    Style::new().fg(theme.highlight)
                } else {
                    Style::new().fg(theme.status_line)
                },
            ),
        ],
        REPEAT_COLUMN_WIDTH,
    ));
    status_spans.extend(pad_status_field(
        vec![
            Span::styled(" ⇄ ", Style::new().fg(theme.text_muted)),
            Span::styled(
                if view.now_playing.shuffle {
                    "On"
                } else {
                    "Off"
                },
                if view.now_playing.shuffle {
                    Style::new().fg(theme.highlight)
                } else {
                    Style::new().fg(theme.status_line)
                },
            ),
        ],
        SHUFFLE_COLUMN_WIDTH,
    ));

    if view.now_playing.marked_count > 0 {
        status_spans.extend(pad_status_field(
            vec![
                Span::styled(" Marked ", Style::new().fg(theme.text_muted)),
                Span::styled(
                    view.now_playing.marked_count.to_string(),
                    Style::new().fg(theme.highlight),
                ),
            ],
            SHUFFLE_COLUMN_WIDTH,
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(status_spans)), status_row);
}

/// Width (in cells) of the dedicated state-glyph column in the Now Playing
/// status row. The glyph is left-aligned inside it and reserves one trailing
/// cell after the widest pause glyph.
const STATE_COLUMN_WIDTH: usize = 3;

/// Width (in cells) of the Volume column. The column carries the 5-block
/// level bar and the `XX%` value; `10` covers the widest case
/// (`▮▮▮▮▮ 100%`) and lets typical values (`▮▮▮▯▯ 70%`) sit with one
/// trailing pad cell so the Speed column never shifts when the percentage
/// hits the ceiling.
const VOLUME_COLUMN_WIDTH: usize = 10;

/// Width (in cells) of the Speed column. The label is the stopwatch glyph
/// (`⏱`) and the value always renders as `X.Yx`, fitting a tight 7-cell slot.
const SPEED_COLUMN_WIDTH: usize = 7;

/// Width (in cells) of the Repeat column.
const REPEAT_COLUMN_WIDTH: usize = 9;

/// Width (in cells) of the Shuffle column.
const SHUFFLE_COLUMN_WIDTH: usize = 6;

/// Pad a status field to a fixed column width by appending blank spans, so each
/// metric starts at the same offset regardless of the current value's length.
/// The width is measured in display columns (via `unicode_width`), not raw
/// characters, so wide glyphs keep their column exactly as ratatui lays them out.
fn pad_status_field(mut spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthStr;
    let used: usize = spans.iter().map(|s| s.content.as_ref().width()).sum();
    let padding = width.saturating_sub(used);
    if padding > 0 {
        spans.push(Span::styled(" ".repeat(padding), Style::new()));
    }
    spans
}

/// Sum of display columns covered by a list of spans.
///
/// Mirrors `pad_status_field`'s measurement so callers can reserve the
/// right amount of left padding when centering a composed label (the
/// Now Playing loading line, for instance) inside a fixed-width row.
/// Counts the raw string width so plain ASCII labels (`Loading`, ` `,
/// single-glyph spinners) stay exactly one cell per character; any wider
/// frame keeps its row width inside the band.
fn loading_visible_width(spans: &[Span<'static>]) -> u16 {
    use unicode_width::UnicodeWidthStr;
    spans
        .iter()
        .map(|s| s.content.as_ref().width())
        .sum::<usize>()
        .min(u16::MAX as usize) as u16
}

/// Build the progress bar as a small number of contiguous styled spans.
///
/// The overlay is centered by terminal display columns. Its characters and
/// widths are precomputed once so rendering does not allocate or rescan the
/// label for every bar column. A label wider than the bar is clipped by the
/// same bounded range used for the bar itself.
fn progress_bar_spans(
    bar_width: u16,
    fill: u16,
    overlay: &str,
    theme: &Theme,
) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthChar;

    let bar_width = usize::from(bar_width);
    let fill = usize::from(fill).min(bar_width);
    let overlay_chars: Vec<(char, usize)> = overlay
        .chars()
        .map(|ch| (ch, ch.width().unwrap_or(0)))
        .collect();
    let overlay_width: usize = overlay_chars.iter().map(|(_, width)| *width).sum();
    let overlay_start = bar_width.saturating_sub(overlay_width) / 2;
    let overlay_end = overlay_start.saturating_add(overlay_width).min(bar_width);

    let mut spans = Vec::with_capacity(5);
    if overlay_width == 0 {
        push_progress_bar_range(&mut spans, 0..bar_width, fill, theme);
        return spans;
    }

    push_progress_bar_range(&mut spans, 0..overlay_start, fill, theme);

    let mut overlay_offset = 0;
    let mut overlay_text = String::new();
    let mut overlay_filled = None;
    for (ch, width) in overlay_chars {
        let column = overlay_start.saturating_add(overlay_offset);
        if column >= bar_width {
            break;
        }

        let is_filled = column < fill;
        if overlay_filled != Some(is_filled) {
            push_overlay_span(&mut spans, &mut overlay_text, overlay_filled, theme);
            overlay_filled = Some(is_filled);
        }
        overlay_text.push(ch);
        overlay_offset = overlay_offset.saturating_add(width);
    }
    push_overlay_span(&mut spans, &mut overlay_text, overlay_filled, theme);

    push_progress_bar_range(&mut spans, overlay_end..bar_width, fill, theme);
    spans
}

fn push_progress_bar_range(
    spans: &mut Vec<Span<'static>>,
    range: std::ops::Range<usize>,
    fill: usize,
    theme: &Theme,
) {
    if range.start >= range.end {
        return;
    }

    let filled_end = range.end.min(fill);
    if range.start < filled_end {
        spans.push(Span::styled(
            "█".repeat(filled_end - range.start),
            Style::new().fg(theme.progress),
        ));
    }
    let unfilled_start = range.start.max(fill);
    if unfilled_start < range.end {
        spans.push(Span::styled(
            " ".repeat(range.end - unfilled_start),
            Style::new().bg(theme.artwork_area),
        ));
    }
}

fn push_overlay_span(
    spans: &mut Vec<Span<'static>>,
    text: &mut String,
    filled: Option<bool>,
    theme: &Theme,
) {
    let Some(filled) = filled else {
        return;
    };
    if text.is_empty() {
        return;
    }

    let bar_bg = if filled {
        theme.progress
    } else {
        theme.artwork_area
    };
    // Characters sitting on the filled bar flip to the theme's background
    // colour so they contrast against the bar; the ones over the empty track
    // keep the normal time text colour.
    let char_fg = if filled {
        theme.background
    } else {
        theme.time_text
    };
    spans.push(Span::styled(
        std::mem::take(text),
        Style::new().fg(char_fg).bg(bar_bg),
    ));
}

/// Decide which panel (if any) should carry the artwork overlay.
///
/// Pure derivation from `(visible, enabled, has_artwork, active_panel)` plus
/// the lyrics panel visibility: the cover never renders in both places at
/// once, never overlays the focused panel, and never overlaps the lyrics
/// panel. With lyrics on screen the cover may only rest on the playlist and
/// only when the playlist is not focused; otherwise it overlays the inactive
/// panel as before.
pub(crate) fn artwork_overlay_target(
    visible: bool,
    enabled: bool,
    has_art: bool,
    active: Panel,
    lyrics_visible: bool,
) -> Option<Panel> {
    if !(visible && enabled && has_art) {
        return None;
    }
    if lyrics_visible {
        // The lyrics panel owns the browser area, so the cover can never go
        // there; it falls back to the playlist unless the playlist is the
        // focused panel (which must stay clear).
        return if active == Panel::Playlist {
            None
        } else {
            Some(Panel::Playlist)
        };
    }
    match active {
        Panel::Browser => Some(Panel::Playlist),
        Panel::Playlist => Some(Panel::Browser),
        // Unreachable while lyrics are hidden, kept for exhaustiveness.
        Panel::Lyrics => None,
    }
}

/// Draw the lyrics panel replacing the browser in the left third.
///
/// The panel mirrors the browser chrome (`lyrics_panel_block` + timestamped
/// document text) but scrolls like the help popup: a physical-row offset
/// mapped into a measured viewport from [`LyricsState`]. When the document is
/// timed the rows are painted according to the playback clock (see
/// [`styled_lyrics_line`]); untimed documents render exactly as they always
/// did. The theme's `lyrics_*` slots override the text, highlight, background
/// and border roles inside this panel only; when they are unset every role
/// keeps inheriting the effective theme as before.
pub fn draw_lyrics_panel(
    frame: &mut Frame,
    area: Rect,
    view: &PanelViewModel,
    focused: bool,
    theme: &Theme,
) {
    let lyrics = &view.lyrics;
    let title = lyrics_view_title(&lyrics.title, area.width);
    let block = lyrics_panel_block(&title, focused, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // The visible window (physical rows) used to prune styling to what is on
    // screen instead of re-styling the entire document every frame.
    let visible = lyrics.viewport_height;

    let lines: Vec<Line> = if lyrics.loading {
        // The spinner frame + space + "Loading" composes the standard
        // in-flight line. The label is intentionally just "Loading" so the
        // panel context (lyrics title in the border) carries the meaning;
        // qualifiers like "Loading lyrics..." would duplicate that context.
        vec![Line::from(
            loading_line(&view.footer.spinner_frame)
                .into_iter()
                .map(|span| {
                    span.style(Style::new().fg(theme.lyrics_text.unwrap_or(theme.text_muted)))
                })
                .collect::<Vec<_>>(),
        )]
    } else if !lyrics.lines.is_empty() {
        render_lyrics_document(
            &lyrics.lines,
            lyrics.elapsed_ms,
            lyrics.timed,
            theme,
            visible,
            lyrics.scroll,
            &lyrics.layout,
        )
    } else {
        // A single actionable line replaces the old generic header: the
        // resolution chain already produces "Lyrics not found" as its miss
        // reason and the no-track state reads "No track is playing", so a
        // second static label would only repeat the state in two rows.
        let message = lyrics
            .error
            .clone()
            .unwrap_or_else(|| "Lyrics not found".to_string());
        vec![Line::from(Span::styled(
            message,
            Style::new().fg(theme.lyrics_text.unwrap_or(theme.text_muted)),
        ))]
    };

    frame.render_widget(
        Paragraph::new(lines)
            // Document rendering maps the requested physical offset into a
            // local viewport, so Paragraph only receives rows that can be
            // painted. Loading and miss states are single-row messages and
            // therefore also start at zero.
            .scroll((0, 0))
            .style(Style::new().bg(theme.lyrics_background.unwrap_or(theme.background))),
        inner,
    );
}

/// Paint the requested physical-row viewport using already-derived layout data.
///
/// The returned vector is local to the viewport: its first row is the
/// clamped document offset, so the caller can render it with no Paragraph
/// scroll and avoid allocating off-screen `Line`s.
fn render_lyrics_document(
    lines: &[LyricsLineView],
    elapsed_ms: i64,
    document_timed: bool,
    theme: &Theme,
    visible: usize,
    requested_scroll: usize,
    layout: &LyricsLayoutView,
) -> Vec<Line<'static>> {
    let max_scroll = layout.total_rows.saturating_sub(visible);
    let scroll = requested_scroll.min(max_scroll);
    let start_row = scroll;
    let end_row = scroll.saturating_add(visible).min(layout.total_rows);
    let first_line = layout
        .row_starts
        .partition_point(|&line_start| line_start <= start_row)
        .saturating_sub(1);
    let mut out = Vec::with_capacity(end_row.saturating_sub(start_row));

    for index in first_line..lines.len() {
        let line_start = layout.row_starts[index];
        if line_start >= end_row {
            break;
        }
        let rows = layout.rows_per_line[index].max(1);
        let line_end = line_start + rows;
        if line_end <= start_row {
            continue;
        }

        let first_row = start_row.saturating_sub(line_start);
        let last_row = end_row.saturating_sub(line_start).min(rows);
        out.extend(styled_lyrics_lines_range(
            &lines[index],
            elapsed_ms,
            document_timed,
            theme,
            first_row,
            last_row,
            Some(&lines[index].wrap_ranges[..]),
            lines[index].char_times.as_deref().map(|times| &**times),
        ));
    }
    out
}

/// One lyrics row painted against the playback clock.
///
/// A timed row is split at character level: characters already reached use
/// the highlight role, the pending ones the muted role, so the progress reads
/// within the theme language. Untimed rows follow the document: muted when
/// the document is timed (they are heads or interludes, not timed text),
/// plain [`Theme::text`] when the whole document is plain lyrics. A set
/// `lyrics_text`/`lyrics_highlight` override replaces those roles inside the
/// panel; unset keeps every color identical to the inherited roles above.
///
/// The panel now wraps lines through [`styled_lyrics_lines`]; this
/// single-row painter survives as the base of the character colour logic
/// that the wrapped variant reuses, and the existing unit tests pin it.
#[cfg(test)]
fn styled_lyrics_line(
    line: &crate::lyrics::LyricsLine,
    elapsed_ms: i64,
    next_line_start: Option<i64>,
    document_timed: bool,
    theme: &Theme,
) -> Line<'static> {
    use crate::lyrics::{effective_elapsed, reached_char_count, split_at_chars};
    // The anticipation shift leads the audio by a hair, so the highlight
    // trips slightly before the real reach time (see karaoke::ANTICIPATION_MS).
    let pending = theme.lyrics_text.unwrap_or(theme.text_muted);
    let reached = theme.lyrics_highlight.unwrap_or(theme.highlight);
    match reached_char_count(line, effective_elapsed(elapsed_ms), next_line_start) {
        Some(0) => Line::from(Span::styled(line.text.clone(), Style::new().fg(pending))),
        Some(count) => {
            let total = line.text.chars().count();
            if count >= total {
                Line::from(Span::styled(line.text.clone(), Style::new().fg(reached)))
            } else {
                let (done, rest) = split_at_chars(&line.text, count);
                Line::from(vec![
                    Span::styled(done.to_string(), Style::new().fg(reached)),
                    Span::styled(rest.to_string(), Style::new().fg(pending)),
                ])
            }
        }
        None => Line::from(Span::styled(
            line.text.clone(),
            Style::new().fg(theme.lyrics_text.unwrap_or(if document_timed {
                theme.text_muted
            } else {
                theme.text
            })),
        )),
    }
}

/// Wrap a document line into physical `Line`s at `width`, colouring each
/// char by whether playback reached it.
///
/// The reached count is computed once for the whole line, then applied per
/// wrapped chunk: a chunk that is entirely ahead stays muted, one entirely
/// behind uses the highlight colour, and a chunk straddling the reached
/// boundary splits into reached (highlight) then pending (muted)
/// spans. Plain untimed lines keep their document-level colour. The muted
/// and highlight colours resolve through the optional `lyrics_text` and
/// `lyrics_highlight` overrides, falling back to the effective theme roles
/// when those slots are unset.
#[cfg(test)]
fn styled_lyrics_lines(
    line: &crate::lyrics::LyricsLine,
    elapsed_ms: i64,
    next_line_start: Option<i64>,
    document_timed: bool,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let elapsed_ms = crate::lyrics::effective_elapsed(elapsed_ms);
    let view = LyricsLineView {
        text: line.text.clone(),
        wrap_ranges: Arc::new(crate::lyrics::wrap_rows_for(&line.text, width)),
        char_times: crate::lyrics::line_char_times(line, next_line_start).map(Arc::new),
    };
    styled_lyrics_lines_range(
        &view,
        elapsed_ms,
        document_timed,
        theme,
        0,
        usize::MAX,
        Some(&view.wrap_ranges),
        view.char_times.as_deref().map(|times| &**times),
    )
}

/// Paint only the requested physical rows of one wrapped document line.
fn styled_lyrics_lines_range(
    line: &LyricsLineView,
    elapsed_ms: i64,
    document_timed: bool,
    theme: &Theme,
    first_row: usize,
    last_row: usize,
    cached_rows: Option<&[(usize, usize)]>,
    cached_times: Option<&[i64]>,
) -> Vec<Line<'static>> {
    let reached = cached_times.map(|times| times.partition_point(|time| *time <= elapsed_ms));
    let total = line.text.chars().count();
    // One resolution of the lyrics overrides per line: unset slots keep the
    // exact role colours the panel used before the lyrics keys existed.
    let plain = theme.lyrics_text.unwrap_or(if document_timed {
        theme.text_muted
    } else {
        theme.text
    });
    let pending = theme.lyrics_text.unwrap_or(theme.text_muted);
    let accent = theme.lyrics_highlight.unwrap_or(theme.highlight);

    let owned_rows;
    let rows = if let Some(rows) = cached_rows {
        rows
    } else {
        owned_rows = vec![(0, line.text.chars().count())];
        &owned_rows
    };
    let first_row = first_row.min(rows.len());
    let last_row = last_row.min(rows.len()).max(first_row);
    rows[first_row..last_row]
        .iter()
        .map(|&(start, end)| {
            let chunk: String = line.text.chars().skip(start).take(end - start).collect();
            let chunk_len = end - start;
            match reached {
                None => Line::from(Span::styled(chunk, Style::new().fg(plain))),
                Some(0) => Line::from(Span::styled(chunk, Style::new().fg(pending))),
                Some(count) if count >= total => {
                    Line::from(Span::styled(chunk, Style::new().fg(accent)))
                }
                Some(count) => {
                    let local = count.saturating_sub(start).min(chunk_len);
                    if local == 0 {
                        Line::from(Span::styled(chunk, Style::new().fg(pending)))
                    } else if local >= chunk_len {
                        Line::from(Span::styled(chunk, Style::new().fg(accent)))
                    } else {
                        let done: String = chunk.chars().take(local).collect();
                        let rest: String = chunk.chars().skip(local).collect();
                        Line::from(vec![
                            Span::styled(done, Style::new().fg(accent)),
                            Span::styled(rest, Style::new().fg(pending)),
                        ])
                    }
                }
            }
        })
        .collect()
}

/// Panel border title: the song title (or file stem), truncated to the
/// available width by display columns with an ellipsis.
fn lyrics_view_title(title: &str, panel_width: u16) -> String {
    let source = (!title.trim().is_empty())
        .then_some(title)
        .unwrap_or("Lyrics");
    truncate_to_width(source, title_budget(panel_width))
}

/// Draw the artwork overlay anchored top-right inside the playlist panel.
///
/// The cover sits inside the border with a one cell margin. Its target is
/// supplied by the artwork state and is bounded to the 144px presentation
/// requirement for the detected terminal font. It is anchored to the
/// top-right corner, and long track names underneath are intentionally
/// overdrawn by the overlay.
/// Playlist artwork rect: fixed width, image-aware height, anchored top-right.
fn playlist_artwork_cell(inner: Rect, available: Size) -> Rect {
    let width = available.width.min(inner.width);
    let height = available.height.min(inner.height);
    Rect {
        x: inner.x.saturating_add(inner.width.saturating_sub(width)),
        y: inner.y,
        width,
        height,
    }
}

/// Compute the exact terminal-cell rect used by both native and external
/// artwork renderers. Keeping protocol sizing here prevents the ueberzugpp
/// layer from growing a second set of panel coordinates.
pub(crate) fn artwork_overlay_rect(
    area: Rect,
    target: Panel,
    view: &PanelViewModel,
) -> Option<Rect> {
    let resize_target = artwork_resize_target(area, target, view)?;
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    match target {
        Panel::Playlist => {
            let image_size = view.artwork.image_size.unwrap_or(resize_target);
            let cover = playlist_artwork_cell(inner, image_size);
            (cover.width > 0 && cover.height > 0)
                .then_some(cover)
                .and_then(|cover| clamp_artwork_rect_to_inner(cover, area))
        }
        Panel::Browser => {
            let image_size = view.artwork.image_size.unwrap_or(resize_target);
            let cover = browser_artwork_cell(inner, None, image_size);
            (cover.width > 0 && cover.height > 0)
                .then_some(cover)
                .and_then(|cover| clamp_artwork_rect_to_inner(cover, area))
        }
        Panel::Lyrics => None,
    }
}

/// Single artwork target calculation shared by frame ticking and overlay
/// placement. It is deliberately independent of protocol encoding.
pub(crate) fn artwork_resize_target(
    area: Rect,
    target: Panel,
    view: &PanelViewModel,
) -> Option<Size> {
    if !(view.artwork.enabled && view.artwork.visible && view.artwork.has_artwork) {
        return None;
    }
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    match target {
        Panel::Playlist => {
            let target = view.artwork.playlist_target;
            Some(Size::new(
                target.width.min(inner.width),
                target.height.min(inner.height),
            ))
        }
        Panel::Browser => Some(inner.into()),
        Panel::Lyrics => None,
    }
}

/// Clamp an artwork rectangle to the one-cell-padded inner panel area.
///
/// External image processes draw independently from ratatui, so their target
/// must be bounded before it crosses the backend-neutral overlay contract.
fn clamp_artwork_rect_to_inner(rect: Rect, panel: Rect) -> Option<Rect> {
    const MAX_COORDINATE: u32 = u16::MAX as u32 + 1;

    let inner_left = (u32::from(panel.x) + 1).min(MAX_COORDINATE);
    let inner_top = (u32::from(panel.y) + 1).min(MAX_COORDINATE);
    let inner_right = (u32::from(panel.x) + u32::from(panel.width))
        .saturating_sub(1)
        .min(MAX_COORDINATE);
    let inner_bottom = (u32::from(panel.y) + u32::from(panel.height))
        .saturating_sub(1)
        .min(MAX_COORDINATE);
    let left = u32::from(rect.x).max(inner_left).min(inner_right);
    let top = u32::from(rect.y).max(inner_top).min(inner_bottom);
    let right = u32::from(rect.x)
        .saturating_add(u32::from(rect.width))
        .min(inner_right);
    let bottom = u32::from(rect.y)
        .saturating_add(u32::from(rect.height))
        .min(inner_bottom);

    (right > left && bottom > top).then(|| Rect {
        x: left as u16,
        y: top as u16,
        width: (right - left) as u16,
        height: (bottom - top) as u16,
    })
}

/// Draw the artwork overlay centered inside the browser panel.
///
/// The listing underneath is cleared first so the cover is the only visual
/// element, then the cover is rendered with a one cell padding. The render
/// area keeps a 2:1 cell ratio (cells are ~2x taller than wide, so it shows as
/// a square), expands to the full inner width of the browser panel, and crops
/// the cover to fill it centered on both axes so a non-square image never
/// leaves an off-center gap.
/// Compute the artwork rect inside the browser panel.
///
/// The cover expands to the full inner width and keeps the image's real aspect
/// ratio: the width/height come from `available` (an image-aware size from the
/// artwork renderer) when no constant `image_factor` is given, so a wide cover fills
/// the panel while a tall one is scaled down proportionally. The rect is
/// centered both horizontally and vertically within `inner`.
fn browser_artwork_cell(inner: Rect, image_factor: Option<f64>, available: Size) -> Rect {
    let (width, height) = match image_factor {
        Some(factor) => {
            let width = inner.width;
            let height = ((width as f64 / factor) as u16).min(inner.height);
            (width, height)
        }
        None => {
            let width = available.width.min(inner.width);
            let height = available.height.min(inner.height);
            (width, height)
        }
    };
    Rect {
        x: inner
            .x
            .saturating_add(inner.width.saturating_sub(width) / 2),
        y: inner
            .y
            .saturating_add(inner.height.saturating_sub(height) / 2),
        width,
        height,
    }
}

/// Draw the contextual hint footer for the panel holding focus.
///
/// Hints stay worded like `input::default_keymap_summary` so help text can
/// never drift away from real bindings. The version closes the line because
/// the footer remains the on-screen report of the running build.
///
/// The immutable [`PanelViewModel`] carries the shared footer spinner data,
/// derived by `App::tick_frame`, so the global "Loading" indicator mirrors the
/// same animation used by every loading surface without reading raw state.
pub fn draw_footer(
    frame: &mut Frame,
    area: Rect,
    view: &PanelViewModel,
    browser_focused: bool,
    theme: &Theme,
) {
    let hints: &[(&str, &str)] = if browser_focused {
        &[
            ("j/k", "move"),
            ("h", "up"),
            ("l", "open"),
            ("v", "mark"),
            ("a", "add"),
            ("Tab", "panel"),
            ("^H", "help"),
            ("q", "quit"),
        ]
    } else {
        // Order groups by intent: playback controls (pause / volume / seek),
        // then editing of the focused queue (move / swap / delete / clear),
        // then playlist management (save-as / new / manager), then meta keys.
        // `n/N` and `l` were removed because the playlist already shows the
        // current track at the top of the panel, so global playback skips
        // and "play" don't need to compete for footer space.
        &[
            ("space", "pause"),
            ("+/-", "volume"),
            ("left/right", "seek"),
            ("j/k", "move"),
            ("J/K", "swap"),
            ("d", "delete"),
            ("D", "clear"),
            ("a/A", "save/new"),
            ("p", "manager"),
            ("Tab", "panel"),
            ("^H", "help"),
            ("q", "quit"),
        ]
    };

    let mut spans = Vec::new();
    for (key, description) in hints {
        if !spans.is_empty() {
            spans.push(Span::styled(
                HINT_SEPARATOR,
                Style::new().fg(theme.separator),
            ));
        }
        spans.push(Span::styled(*key, Style::new().fg(theme.text)));
        spans.push(Span::styled(
            format!(" {description}"),
            Style::new().fg(theme.text_muted),
        ));
    }

    spans.push(Span::styled(HINT_GAP, Style::new().fg(theme.text_muted)));
    spans.push(Span::styled(
        format!("Version: {APP_VERSION}"),
        Style::new().fg(theme.text_muted),
    ));

    // The global "Loading" indicator sits at the right edge so it never
    // pushes the key hints out of view: it only appears when at least one
    // background task is in flight, and it shares the same spinner phase
    // as the Now Playing row, Add Stream popup and lyrics panel.
    if view.footer.pending_effects {
        spans.push(Span::styled(HINT_GAP, Style::new().fg(theme.text_muted)));
        for span in loading_line(&view.footer.spinner_frame) {
            spans.push(span.style(Style::new().fg(theme.text_muted)));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Build the shared bordered block used by the two main panels.
fn panel_block<'a>(title: &'a str, focused: bool, theme: &Theme) -> Block<'a> {
    let border_color = if focused {
        theme.border_focused
    } else {
        theme.border
    };
    let mut title_style = Style::new().fg(border_color);
    if focused {
        title_style = title_style.bold();
    }

    Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(title.to_string(), title_style))
        .border_style(Style::new().fg(border_color))
        .style(Style::new().bg(theme.background))
}

/// Build the bordered block for the lyrics panel, honouring its overrides.
///
/// With `lyrics_border`, `lyrics_border_focused` and `lyrics_background`
/// unset this delegates to the shared [`panel_block`], so themes without the
/// keys render byte-for-byte as before and the override code path stays
/// exclusive to this panel. The border color resolves per focus state:
/// focused → `lyrics_border_focused`, else `lyrics_border`, else
/// `border_focused`; unfocused → `lyrics_border`, else `border`. The title
/// span follows the same color. When the two states land on the same color
/// (e.g. only `lyrics_border` set) focus stays visually distinct through the
/// bold title plus a bold border on the focused state.
fn lyrics_panel_block<'a>(title: &'a str, focused: bool, theme: &Theme) -> Block<'a> {
    if theme.lyrics_border.is_none()
        && theme.lyrics_border_focused.is_none()
        && theme.lyrics_background.is_none()
    {
        return panel_block(title, focused, theme);
    }

    let focused_color = theme
        .lyrics_border_focused
        .or(theme.lyrics_border)
        .unwrap_or(theme.border_focused);
    let unfocused_color = theme.lyrics_border.unwrap_or(theme.border);
    let border_color = if focused {
        focused_color
    } else {
        unfocused_color
    };
    let background = theme.lyrics_background.unwrap_or(theme.background);
    let mut title_style = Style::new().fg(border_color);
    let mut border_style = Style::new().fg(border_color);
    if focused {
        title_style = title_style.bold();
        // Only the single-color states need the extra cue: when the focused
        // and unfocused overrides differ, the color already marks focus.
        if focused_color == unfocused_color {
            border_style = border_style.bold();
        }
    }

    Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(title.to_string(), title_style))
        .border_style(border_style)
        .style(Style::new().bg(background))
}

/// Compose the browser title from the middle-truncated location and the
/// entry count, keeping both readable even on narrow terminals.
fn browser_title_from_view(location: &str, entry_count: usize, panel_width: u16) -> String {
    let count = format!(" ({entry_count})");
    let budget = title_budget(panel_width).saturating_sub(count.chars().count());

    format!("{}{}", middle_truncate(location, budget), count)
}

/// Usable columns for a block title, leaving padding beside both borders.
fn title_budget(panel_width: u16) -> usize {
    usize::from(panel_width).saturating_sub(4)
}

/// First visible entry index so the cursor stays inside the viewport.
///
/// `BrowserState` owns the directional scroll offset, updated on every move;
/// this re-clamps it against the measured viewport in case the panel resized
/// since the last navigation command, but never re-derives the anchor. A
/// re-derivation would lose the direction and pin the cursor to one side.
/// Render one browser row with its mark flag, kind suffix and styling.
fn browser_entry_view_line(
    name: &str,
    playable: bool,
    marked: bool,
    cursor: bool,
    name_budget: usize,
    theme: &Theme,
) -> Line<'static> {
    let name = truncate_with_ellipsis(name, name_budget);

    // Unsupported regular files are dimmed because they cannot be queued
    let name_style = if playable {
        Style::new().fg(theme.text)
    } else {
        Style::new().fg(theme.text_muted)
    };
    let name_span = Span::styled(name, selected_or_plain(name_style, cursor, theme));

    // The mark trails the name so entries start one cell from the border (the
    // block border) instead of a fixed marker column pushing every name
    // several cells right.
    if marked {
        Line::from(vec![
            name_span,
            Span::styled(
                format!(" {CHECKBOX_CHECKED}"),
                selected_or_plain(Style::new().fg(theme.highlight), cursor, theme),
            ),
        ])
    } else {
        Line::from(vec![name_span])
    }
}

fn loading_line(frame: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(frame.to_string(), Style::new()),
        Span::styled(" ", Style::new()),
        Span::styled("Loading", Style::new()),
    ]
}

/// Apply the selection background AND the highlight foreground to the selected row.
///
/// Mirrors the color Settings uses for its selected options (theme.highlight as
/// foreground + theme.selection as background), so a file browser entry or
/// playlist row reads the same way visually as a Settings tab entry.
fn selected_or_plain(style: Style, cursor: bool, theme: &Theme) -> Style {
    if cursor {
        style.fg(theme.highlight).bg(theme.selection)
    } else {
        style
    }
}

/// Cut text to `max_chars` appending an ellipsis when truncation happens.
fn truncate_with_ellipsis(text: &str, max_columns: usize) -> String {
    use unicode_width::UnicodeWidthChar;

    // Sanitize first so escape/control injection cannot survive any path that
    // reaches a row label.
    let text = &sanitize_text(text);
    let width = text
        .chars()
        .map(|ch| ch.width().unwrap_or(0))
        .sum::<usize>();
    if width <= max_columns {
        return text.to_string();
    }

    // Reserving the ellipsis from a zero budget would overflow, so keep the
    // historical behavior of yielding only the ellipsis.
    let keep = max_columns.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > keep {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    out
}

/// Keep both ends of long text, replacing the middle with an ellipsis.
///
/// Split budget is measured in display columns so wide glyphs do not overflow
/// the panel width.
fn middle_truncate(text: &str, max_columns: usize) -> String {
    use unicode_width::UnicodeWidthChar;

    let text = &sanitize_text(text);
    let total_columns = text
        .chars()
        .map(|ch| ch.width().unwrap_or(0))
        .sum::<usize>();
    if total_columns <= max_columns {
        return text.to_string();
    }
    if max_columns == 0 {
        return String::new();
    }
    if max_columns == 1 {
        return "…".to_string();
    }

    let kept = max_columns - 1;
    let head_columns = kept.div_ceil(2);
    let tail_columns = kept / 2;
    let chars: Vec<char> = text.chars().collect();

    // Gather exactly `head_columns` of width from the front and `tail_columns`
    // from the back, never splitting a wide character in half. The tail is
    // appended after the head so both halves preserve their original order.
    let mut out = String::new();
    let mut used = 0;
    for ch in &chars {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > head_columns {
            break;
        }
        out.push(*ch);
        used += ch_width;
    }
    out.push('…');

    // Walk from the end and push the tail into a buffer, then reverse it once
    // so the end segment reads left-to-right.
    let mut tail_rev = String::new();
    used = 0;
    for ch in chars.iter().rev() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > tail_columns {
            break;
        }
        tail_rev.push(*ch);
        used += ch_width;
    }
    out.push_str(&tail_rev.chars().rev().collect::<String>());
    out
}

#[cfg(test)]
fn browser_title(browser: &crate::browser_state::BrowserState, panel_width: u16) -> String {
    browser_title_from_view(
        &browser.current_dir.to_string_lossy(),
        browser.entries.len(),
        panel_width,
    )
}

#[cfg(test)]
fn browser_entry_line(
    entry: &crate::filesystem::FileEntry,
    marked: bool,
    cursor: bool,
    name_budget: usize,
    theme: &Theme,
) -> Line<'static> {
    browser_entry_view_line(
        &if entry.kind == crate::filesystem::EntryKind::Dir {
            format!("{}/", entry.name)
        } else {
            entry.name.clone()
        },
        entry.kind != crate::filesystem::EntryKind::File
            || crate::filesystem::is_supported_audio(&entry.path),
        marked,
        cursor,
        name_budget,
        theme,
    )
}

#[cfg(test)]
fn visible_window_start(browser: &crate::browser_state::BrowserState) -> usize {
    crate::ui::view::visible_window_start(
        browser.scroll_offset(),
        browser.cursor(),
        browser.entries.len(),
        usize::from(browser.viewport_height.max(1)),
    )
}

#[cfg(test)]
fn lyrics_panel_title(lyrics: &crate::state::LyricsState, panel_width: u16) -> String {
    let source = lyrics
        .display_title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Lyrics");
    truncate_to_width(source, title_budget(panel_width))
}

#[cfg(test)]
fn playlist_entry_name<C: crate::playlist::sorter::ColumnConfig>(
    track: &crate::track::Track,
    columns: &C,
) -> String {
    crate::playlist::sorter::display_label(track, columns)
}

#[cfg(test)]
mod tests {
    use super::widgets::{
        HELP_HEIGHT_PERCENT, HELP_WIDTH_PERCENT, build_help_lines, centered_percent_rect,
        centered_rect, sanitize_text, settings_item_line, truncate_to_width,
    };
    use super::*;
    use crate::app::App;
    use crate::filesystem::{EntryKind, FileEntry};
    use crate::input::HelpContentCache;
    use crate::lyrics::{layout_document, line_char_times, next_line_starts, wrap_rows_for};
    use crate::state::{AppState, DialogMode, LyricsState};
    use crate::ui::view::PanelViewModel;
    use ratatui::style::Color;
    use ratatui::style::Modifier;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::time::Duration;

    fn view_for(state: &AppState, lyrics_width: usize) -> PanelViewModel {
        PanelViewModel::from_state(
            state,
            lyrics_width,
            &HelpContentCache::new(&crate::config::KeysConfig::default()),
            "",
            None,
        )
    }

    #[test]
    fn truncate_to_width_clips_long_text() {
        assert_eq!(truncate_to_width("short", 20), "short");
        assert_eq!(truncate_to_width("abcdef", 4), "abc…");
        assert_eq!(truncate_to_width("", 4), "");
        assert_eq!(truncate_to_width("abc", 0), "");
    }

    #[test]
    fn truncate_to_width_respects_unicode_columns() {
        // 你 and 好 are each two terminal columns wide: "你好" is 4 > 3, so we
        // keep one wide char + ellipsis.
        let clipped = truncate_to_width("你好世界", 3);
        assert_eq!(clipped, "你…");
        assert!(clipped.chars().count() <= 3);
    }

    #[test]
    fn truncate_to_width_collapses_newlines() {
        assert_eq!(truncate_to_width("a\nb", 10), "a b");
        assert_eq!(truncate_to_width("a\tb", 10), "a b");
    }

    #[test]
    fn sanitize_text_strips_terminal_control_escapes() {
        // The classic terminal escape injection: an embedded ESC would normally
        // let a hostname/filename take over the session. It must be dropped.
        assert_eq!(sanitize_text("a\x1b[31mred"), "a[31mred");
        assert_eq!(sanitize_text("a\x07bell"), "abell");
        assert_eq!(sanitize_text("\x0b\x0c\x00"), "");
        // Newlines/tabs collapse to a single space instead of spawning rows.
        assert_eq!(sanitize_text("a\nb\rc\td"), "a b c d");
        // Legitimate printable text is untouched.
        assert_eq!(sanitize_text("El Café 你"), "El Café 你");
    }

    #[test]
    fn truncate_to_width_strips_control_escapes() {
        // Same guarantee through the truncation path used by rows and titles.
        assert_eq!(truncate_to_width("a\x1b[31mred", 10), "a[31mred");
    }

    #[test]
    fn truncate_with_ellipsis_strips_control_escapes() {
        assert_eq!(truncate_with_ellipsis("a\x1b[31mred", 10), "a[31mred");
    }

    #[test]
    fn middle_truncate_strips_control_escapes() {
        assert_eq!(middle_truncate("a\x1b[31mred", 10), "a[31mred");
    }

    #[test]
    fn karaoke_untimed_document_keeps_the_plain_text_color() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        let line = LyricsLine {
            timestamp_ms: None,
            text: "plain words".to_string(),
            words: Vec::new(),
        };

        let rendered = styled_lyrics_line(&line, 0, None, false, &theme);

        assert_eq!(rendered.spans.len(), 1);
        assert_eq!(rendered.spans[0].style.fg, Some(theme.text));
        assert_eq!(rendered.spans[0].content.as_ref(), "plain words");
    }

    #[test]
    fn karaoke_untimed_line_inside_a_timed_document_is_muted() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        let line = LyricsLine {
            timestamp_ms: None,
            text: "[Verse 1]".to_string(),
            words: Vec::new(),
        };

        let rendered = styled_lyrics_line(&line, 50_000, None, true, &theme);

        assert_eq!(rendered.spans.len(), 1);
        assert_eq!(rendered.spans[0].style.fg, Some(theme.text_muted));
    }

    #[test]
    fn karaoke_timed_line_unreached_is_muted() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        let line = LyricsLine {
            timestamp_ms: Some(10000),
            text: "hola".to_string(),
            words: Vec::new(),
        };

        let rendered = styled_lyrics_line(&line, 9_999, Some(14000), true, &theme);

        assert_eq!(rendered.spans.len(), 1);
        assert_eq!(rendered.spans[0].style.fg, Some(theme.text_muted));
        assert_eq!(rendered.spans[0].content.as_ref(), "hola");
    }

    #[test]
    fn karaoke_timed_line_fully_reached_uses_the_highlight_color() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        let line = LyricsLine {
            timestamp_ms: Some(10000),
            text: "hola".to_string(),
            words: Vec::new(),
        };

        let rendered = styled_lyrics_line(&line, 14_000, Some(14000), true, &theme);

        assert_eq!(rendered.spans.len(), 1);
        assert_eq!(rendered.spans[0].style.fg, Some(theme.highlight));
        assert_eq!(rendered.spans[0].content.as_ref(), "hola");
    }

    #[test]
    fn karaoke_split_mid_line_splits_into_two_spans() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        let line = LyricsLine {
            timestamp_ms: Some(10000),
            text: "hola amigo".to_string(),
            words: Vec::new(),
        };

        // Times are 10400, 10800, ... 14000. The anticipation shift makes an
        // 11_000 ms clock read as 11_200 ms, so three chars are reached and
        // the row splits "hol | a amigo".
        let rendered = styled_lyrics_line(&line, 11_000, None, true, &theme);

        assert_eq!(rendered.spans.len(), 2);
        assert_eq!(rendered.spans[0].style.fg, Some(theme.highlight));
        assert_eq!(rendered.spans[0].content.as_ref(), "hol");
        assert_eq!(rendered.spans[1].style.fg, Some(theme.text_muted));
        assert_eq!(rendered.spans[1].content.as_ref(), "a amigo");
        let joined: String = rendered.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "hola amigo", "split must never lose characters");
    }

    #[test]
    fn karaoke_highlight_leads_the_audio_by_the_anticipation_window() {
        use crate::lyrics::{ANTICIPATION_MS, LyricsLine};
        let theme = Theme::default();
        let line = LyricsLine {
            timestamp_ms: Some(10000),
            text: "hola".to_string(),
            words: Vec::new(),
        };

        // The first char lands at 11000; it must already be reached at
        // 11000 - ANTICIPATION_MS and still untouched at one instant before.
        let rendered =
            styled_lyrics_line(&line, 11_000 - ANTICIPATION_MS, Some(14000), true, &theme);
        assert_eq!(rendered.spans.len(), 2, "first char reached: {rendered:?}");

        let rendered = styled_lyrics_line(
            &line,
            11_000 - ANTICIPATION_MS - 1,
            Some(14000),
            true,
            &theme,
        );
        assert_eq!(rendered.spans.len(), 1);
        assert_eq!(rendered.spans[0].style.fg, Some(theme.text_muted));
    }

    #[test]
    fn next_line_starts_resolves_each_timed_lines_follower() {
        use crate::lyrics::LyricsDocument;
        use crate::lyrics::LyricsLine;
        let doc = LyricsDocument {
            lines: vec![
                LyricsLine {
                    timestamp_ms: Some(1000),
                    text: "one".to_string(),
                    words: Vec::new(),
                },
                LyricsLine {
                    timestamp_ms: None,
                    text: "untimed".to_string(),
                    words: Vec::new(),
                },
                LyricsLine {
                    timestamp_ms: Some(3000),
                    text: "three".to_string(),
                    words: Vec::new(),
                },
                LyricsLine {
                    timestamp_ms: Some(4000),
                    text: "four".to_string(),
                    words: Vec::new(),
                },
            ],
            text: String::new(),
        };

        let starts = next_line_starts(&doc);
        assert_eq!(starts[0], Some(3000), "next timed line after line 0");
        assert_eq!(starts[1], Some(3000), "untimed line inherits the follower");
        assert_eq!(starts[2], Some(4000));
        assert_eq!(starts[3], None, "the last timed line has no follower");
    }

    #[test]
    fn wrapped_lyrics_lines_preserve_the_full_text() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        let text = "I needed you desperately (da-da-da, da-da-da, da-da-da)";
        let line = LyricsLine {
            timestamp_ms: Some(10000),
            text: text.to_string(),
            words: Vec::new(),
        };

        let lines = styled_lyrics_lines(&line, 50_000, Some(14000), true, &theme, 12);

        assert!(lines.len() >= 2, "a long line must wrap into several rows");
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, text, "wrapping must never drop characters");
    }

    #[test]
    fn wrapped_lyrics_colors_are_split_across_chunks() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        // Ten chars of width 1 in a width-5 panel split 5/5. At an effective
        // clock that reached 7 chars, the first chunk is fully reached
        // (highlight) and the second straddles the boundary: 2 reached, 3 pending.
        let line = LyricsLine {
            timestamp_ms: Some(10000),
            text: "abcdefghij".to_string(),
            words: Vec::new(),
        };

        let lines = styled_lyrics_lines(&line, 17_000, Some(20000), true, &theme, 5);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.highlight));
        assert_eq!(lines[0].spans[0].content.as_ref(), "abcde");
        assert_eq!(lines[1].spans.len(), 2);
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.highlight));
        assert_eq!(lines[1].spans[0].content.as_ref(), "fg");
        assert_eq!(lines[1].spans[1].style.fg, Some(theme.text_muted));
        assert_eq!(lines[1].spans[1].content.as_ref(), "hij");
    }

    #[test]
    fn wrapped_plain_lyrics_keep_the_plain_text_color() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        let line = LyricsLine {
            timestamp_ms: None,
            text: "plain words here".to_string(),
            words: Vec::new(),
        };

        let lines = styled_lyrics_lines(&line, 0, None, false, &theme, 6);

        assert!(
            lines.iter().all(|l| l.spans.len() == 1),
            "every plain chunk is a single span"
        );
        for l in &lines {
            assert_eq!(l.spans[0].style.fg, Some(theme.text));
        }
        let joined: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, "plain words here");
    }

    #[test]
    fn wrapped_empty_lyrics_line_renders_one_empty_row() {
        use crate::lyrics::LyricsLine;
        let theme = Theme::default();
        let line = LyricsLine {
            timestamp_ms: Some(10000),
            text: String::new(),
            words: Vec::new(),
        };

        let lines = styled_lyrics_lines(&line, 0, Some(14000), true, &theme, 10);

        assert_eq!(
            lines.len(),
            1,
            "an empty line must keep one blank physical row"
        );
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "");
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.text_muted));
    }

    #[test]
    fn visible_wrapped_lyrics_rows_preserve_scroll_without_placeholders() {
        let document = crate::lyrics::LyricsDocument::from_plain("abcdefghijkl\n\nmnopqrstuvwx");
        let theme = Theme::default();
        let layout = layout_document(&document.lines, 4);
        let next_line_starts = next_line_starts(&document);
        let wrap_ranges: Vec<Vec<(usize, usize)>> = document
            .lines
            .iter()
            .map(|line| wrap_rows_for(&line.text, 4))
            .collect();
        let char_times: Vec<Option<Vec<i64>>> = document
            .lines
            .iter()
            .enumerate()
            .map(|(index, line)| line_char_times(line, next_line_starts[index]))
            .collect();
        let view_lines: Vec<LyricsLineView> = document
            .lines
            .iter()
            .enumerate()
            .map(|(index, line)| LyricsLineView {
                text: line.text.clone(),
                wrap_ranges: Arc::new(wrap_ranges[index].clone()),
                char_times: char_times[index].clone().map(Arc::new),
            })
            .collect();
        let view_layout = LyricsLayoutView {
            rows_per_line: layout.rows_per_line.clone(),
            row_starts: layout.row_starts.clone(),
            total_rows: layout.total_rows,
        };

        let lines = render_lyrics_document(&view_lines, 0, false, &theme, 3, 2, &view_layout);

        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(layout.total_rows, 7);
        assert_eq!(lines.len(), 3, "only the visible physical rows are built");
        assert_eq!(rendered, ["ijkl", "", "mnop"]);
    }

    #[test]
    fn settings_item_line_is_clipped_even_when_selected() {
        let theme = Theme::default();
        let long = "pci-0000_00_1f.3.analog-stereo-pipewire-sink".to_string();
        let plain = settings_item_line(long.clone(), false, 12, &theme);
        let plain_text: String = plain.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            plain_text.chars().count() <= 12,
            "plain clipped: {plain_text}"
        );

        let selected = settings_item_line(long.clone(), true, 12, &theme);
        let selected_text: String = selected.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !selected_text.starts_with('\u{25b6}'),
            "selected rows must not carry a redundant glyph, got: {selected_text}"
        );
        assert!(
            selected_text.chars().count() <= 12,
            "selected clipped: {selected_text}"
        );
    }

    #[test]
    fn settings_edit_popup_uses_the_active_theme_background() {
        use crate::state::{DialogMode, Popup, SettingsDraft, SettingsTab};

        let theme = Theme {
            background: ratatui::style::Color::Rgb(10, 20, 30),
            popup_border: ratatui::style::Color::Rgb(40, 50, 60),
            ..Theme::default()
        };

        let mut app = App::new();
        let draft = SettingsDraft::from_state(
            app.state(),
            &crate::config::AppConfig::default(),
            &std::path::PathBuf::new(),
        );
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::Keys,
            focus: crate::state::SettingsFocus::Content,
            draft,
        });
        app.state_mut().popup_dialog.open_dialog(
            DialogMode::SettingsEdit,
            "edit me".to_string(),
            None,
        );

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_settings(frame, frame.area(), &view_for(app.state(), 64), &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // The edit popup is a 62x6 centered rect; probe a cell inside it (the
        // top-left of the popup body area) and require the themed background
        // rather than a transparent/unset one.
        let popup = centered_rect(
            62,
            6,
            Rect {
                x: 0,
                y: 0,
                width: 80,
                height: 24,
            },
        );
        let probe = buf[((popup.x + 2), (popup.y + 2))].clone();
        assert_eq!(
            probe.style().bg,
            Some(theme.background),
            "the key-edit popup must inherit the theme background, got {:?}",
            probe.style().bg
        );
    }

    #[test]
    fn settings_window_uses_the_main_panel_border_style() {
        use crate::state::{Popup, SettingsDraft, SettingsTab};

        let theme = Theme {
            // Distinguish the focused panel border from the modal popup border
            // so we can prove the settings window follows the main panel
            // styling.
            border_focused: ratatui::style::Color::Rgb(1, 2, 3),
            popup_border: ratatui::style::Color::Rgb(9, 8, 7),
            ..Theme::default()
        };

        let mut app = App::new();
        let draft = SettingsDraft::from_state(
            app.state(),
            &crate::config::AppConfig::default(),
            &std::path::PathBuf::new(),
        );
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::General,
            focus: crate::state::SettingsFocus::Content,
            draft,
        });

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_settings(frame, frame.area(), &view_for(app.state(), 64), &theme))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // Probe the top border cell of the settings window: it must use the
        // focused panel border colour, matching the main panels, not the modal
        // popup border.
        let border_cell = buf[(10, 0)].clone();
        assert_eq!(
            border_cell.style().fg,
            Some(theme.border_focused),
            "settings window border must match the main panel style"
        );
    }

    #[test]
    fn settings_general_renders_explicit_sort_action() {
        use crate::state::{Popup, SettingsDraft, SettingsTab};

        let mut app = App::new();
        let draft = SettingsDraft::from_state(
            app.state(),
            &crate::config::AppConfig::default(),
            &std::path::PathBuf::new(),
        );
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::General,
            focus: crate::state::SettingsFocus::Content,
            draft,
        });

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_settings(
                    frame,
                    frame.area(),
                    &view_for(app.state(), 64),
                    &Theme::default(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let rows: Vec<String> = (0..24u16)
            .map(|y| {
                (0..80u16)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();

        assert!(rows.iter().any(|row| row.contains("Sort tracks [Enter]")));
        assert!(!rows.iter().any(|row| row.contains("Sort tracks by")));
    }

    #[test]
    fn settings_appearance_renders_playlist_columns_with_fixed_title() {
        use crate::state::{Popup, SettingsDraft, SettingsTab};

        let mut app = App::new();
        let mut draft = SettingsDraft::from_state(
            app.state(),
            &crate::config::AppConfig::default(),
            &std::path::PathBuf::new(),
        );
        draft.appearance_column = crate::state::AppearanceColumn::Display;
        draft.playlist_columns.display_by = crate::config::SortBy::Metadata;
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::Appearance,
            focus: crate::state::SettingsFocus::Content,
            draft,
        });

        let backend = ratatui::backend::TestBackend::new(80, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_settings(
                    frame,
                    frame.area(),
                    &view_for(app.state(), 64),
                    &Theme::default(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..30u16 {
            for x in 0..80u16 {
                text.push_str(buffer[(x, y)].symbol());
            }
        }

        assert!(text.contains("Now playing"));
        assert!(text.contains("Playlist columns"));
        assert!(text.contains("Artist"));
        assert!(text.contains("Album"));
        assert!(text.contains("Track Number"));
        assert!(text.contains("Title"));

        let playlist_heading_y = (0..30u16)
            .find(|&y| {
                (0..80u16)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("Playlist columns")
            })
            .expect("Playlist columns heading must render");
        let title_y = playlist_heading_y + 6;
        let title_x = (0..80u16)
            .find(|&x| buffer[(x, title_y)].symbol() == "T")
            .expect("fixed Playlist Title row must render");
        assert_eq!(
            buffer[(title_x, title_y)].style().fg,
            Some(Theme::default().text_muted),
            "fixed Playlist Title row must use the disabled text style"
        );
    }

    #[test]
    fn sort_tracks_confirmation_renders_the_exact_message() {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_confirm_sort_tracks_popup(frame, frame.area(), &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..24u16 {
            for x in 0..80u16 {
                text.push_str(buffer[(x, y)].symbol());
            }
        }
        assert!(text.contains("The tracks will be reordered"));
    }

    #[test]
    fn settings_appearance_renders_themed_sub_panels() {
        use crate::state::{Popup, SettingsDraft, SettingsTab};

        let mut app = App::new();
        // Seed the applied theme so the list marks it as the active one.
        let mut config = crate::config::AppConfig::default();
        config.ui.theme = "solarized".to_string();
        app.set_config_paths(config, std::path::PathBuf::new(), std::path::PathBuf::new());
        let mut draft = SettingsDraft::from_state(
            app.state(),
            &crate::config::AppConfig::default(),
            &std::path::PathBuf::new(),
        );
        draft.theme_names = vec!["default".to_string(), "solarized".to_string()];
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: SettingsTab::Appearance,
            focus: crate::state::SettingsFocus::Content,
            draft,
        });

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_settings(
                    frame,
                    frame.area(),
                    &view_for(app.state(), 64),
                    &Theme::default(),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // All three sibling columns render their titles on their own bordered
        // blocks.
        let mut dump = String::new();
        for y in 0..40u16 {
            for x in 0..120u16 {
                let s = buf[(x, y)].symbol();
                if !s.is_empty() && s != " " {
                    dump.push_str(s);
                }
            }
        }
        assert!(
            dump.contains("Display"),
            "Appearance must render the Display column, got: {dump:?}"
        );
        assert!(
            dump.contains("Themes") && dump.contains("Themeeditor"),
            "Appearance must render the Themes and Theme editor columns, got: {dump:?}"
        );
        // The active (applied) theme is marked with ◉ and the rest with ○.
        assert!(
            dump.contains("\u{25c9}") && dump.contains("\u{25cb}"),
            "Appearance must mark the active theme (◉) and the others (○), got: {dump:?}"
        );
        // The "Now playing" display options live under a subtitle inside the
        // Display column, mirroring the "Sort Tracks By" header in General.
        assert!(
            dump.contains("Nowplaying"),
            "Appearance must render the 'Now playing' subtitle, got: {dump:?}"
        );
        assert!(
            dump.contains("Filename") && dump.contains("Metadata"),
            "Appearance must render the Now playing display radios, got: {dump:?}"
        );
        let border_label = dump.find("Bordertype").expect("Border type label");
        let plain = dump.find("Plain").expect("Plain border option");
        let rounded = dump.find("Rounded").expect("Rounded border option");
        let double = dump.find("Double").expect("Double border option");
        let thick = dump.find("Thick").expect("Thick border option");
        let now_playing = dump.find("Nowplaying").expect("Now playing label");
        assert!(border_label < plain);
        assert!(plain < rounded && rounded < double && double < thick);
        assert!(thick < now_playing);
    }

    #[test]
    fn selected_border_type_reaches_panels_and_search_popups() {
        use crate::search::SearchScope;

        for (border_type, expected_corner) in [
            (crate::config::BorderType::Plain, "┌"),
            (crate::config::BorderType::Rounded, "╭"),
            (crate::config::BorderType::Double, "╔"),
            (crate::config::BorderType::Thick, "┏"),
        ] {
            let theme = Theme {
                border_type: border_type.to_ratatui(),
                ..Theme::default()
            };
            let state = fixture_state(Vec::new());
            let backend = ratatui::backend::TestBackend::new(40, 10);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw_browser(frame, frame.area(), &view_for(&state, 64), false, &theme)
                })
                .unwrap();
            assert_eq!(
                terminal.backend().buffer()[(0, 0)].symbol(),
                expected_corner,
                "panel must use {border_type:?}"
            );

            let mut app = App::new();
            app.state_mut()
                .popup_dialog
                .open_search(SearchScope::Browser);
            let backend = ratatui::backend::TestBackend::new(60, 12);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw_search_popup(frame, frame.area(), &view_for(app.state(), 64), &theme)
                })
                .unwrap();
            assert_eq!(
                terminal.backend().buffer()[(6, 3)].symbol(),
                expected_corner,
                "search popup must use {border_type:?} instead of forcing Rounded"
            );
        }
    }

    #[test]
    fn settings_appearance_highlights_only_the_active_column() {
        use crate::state::{Popup, SettingsDraft, SettingsTab};
        use ratatui::style::Color;

        fn render_with_column(column: crate::state::AppearanceColumn) -> (usize, usize) {
            let theme = Theme {
                border_focused: Color::Rgb(1, 2, 3),
                border: Color::Rgb(4, 5, 6),
                ..Theme::default()
            };

            let mut app = App::new();
            let mut draft = SettingsDraft::from_state(
                app.state(),
                &crate::config::AppConfig::default(),
                &std::path::PathBuf::new(),
            );
            draft.theme_names = vec!["default".to_string(), "solarized".to_string()];
            draft.appearance_column = column;
            app.state_mut().popup_dialog.open_popup(Popup::Settings {
                tab: SettingsTab::Appearance,
                focus: crate::state::SettingsFocus::Content,
                draft,
            });

            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw_settings(frame, frame.area(), &view_for(app.state(), 64), &theme)
                })
                .unwrap();
            let buf = terminal.backend().buffer().clone();

            let mut focused = 0usize;
            let mut plain = 0usize;
            for y in 0..24u16 {
                for x in 0..80u16 {
                    let fg = buf[(x, y)].style().fg;
                    if fg == Some(theme.border_focused) {
                        focused += 1;
                    } else if fg == Some(theme.border) {
                        plain += 1;
                    }
                }
            }
            (focused, plain)
        }

        // Options column active: the Options border is focused while both the
        // Themes frame and the Colors sub-frame stay on the plain border.
        let (focused_on_options, plain_on_options) =
            render_with_column(crate::state::AppearanceColumn::Display);
        assert!(
            focused_on_options > 0 && plain_on_options > 0,
            "options-active must show one focused and one plain border, got focused={focused_on_options} plain={plain_on_options}"
        );

        // Themes active: the outer Themes frame border is focused; the Colors
        // sub-frame stays on the plain border.
        let (focused_on_themes, plain_on_themes) =
            render_with_column(crate::state::AppearanceColumn::Themes);
        assert!(
            focused_on_themes > 0 && plain_on_themes > 0,
            "themes-active must show one focused and one plain border, got focused={focused_on_themes} plain={plain_on_themes}"
        );

        // Colors active: the nested Colors sub-frame is focused instead of the
        // outer Themes frame, so the highlight moves to the sub-frame.
        let (focused_on_colors, plain_on_colors) =
            render_with_column(crate::state::AppearanceColumn::Colors);
        assert!(
            focused_on_colors > 0 && plain_on_colors > 0,
            "colors-active must highlight the sub-frame, got focused={focused_on_colors} plain={plain_on_colors}"
        );
    }

    #[test]
    fn settings_tabs_have_bordered_sections() {
        use crate::state::{Popup, SettingsDraft, SettingsTab};

        let render_tab_dump = |tab: SettingsTab, configure: &dyn Fn(&mut SettingsDraft)| {
            let mut app = App::new();
            let mut draft = SettingsDraft::from_state(
                app.state(),
                &crate::config::AppConfig::default(),
                &std::path::PathBuf::new(),
            );
            configure(&mut draft);
            app.state_mut().popup_dialog.open_popup(Popup::Settings {
                tab,
                focus: crate::state::SettingsFocus::Content,
                draft,
            });

            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw_settings(
                        frame,
                        frame.area(),
                        &view_for(app.state(), 64),
                        &Theme::default(),
                    )
                })
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let mut dump = String::new();
            for y in 0..24u16 {
                for x in 0..80u16 {
                    dump.push_str(buf[(x, y)].symbol());
                }
            }
            dump
        };

        let general = render_tab_dump(SettingsTab::General, &|_| {});
        assert!(
            general.contains("Options") && general.contains("Sort tracks [Enter]"),
            "General tab must render its Options section with the sort action, got {general:?}"
        );

        let sound = render_tab_dump(SettingsTab::Sound, &|_| {});
        assert!(
            sound.contains("Output device"),
            "Sound tab must render its bordered section, got {sound:?}"
        );

        let keys = render_tab_dump(SettingsTab::Keys, &|_| {});
        assert!(
            keys.contains("Global keys"),
            "Keys tab must render its bordered section, got {keys:?}"
        );

        let playback = render_tab_dump(SettingsTab::Playback, &|_| {});
        assert!(
            playback.contains("Options") && playback.contains("□ Remote lyrics"),
            "Playback tab must render its Options column with the unchecked toggle, got {playback:?}"
        );

        let playback_on = render_tab_dump(SettingsTab::Playback, &|draft: &mut SettingsDraft| {
            draft.remote_lyrics = true;
        });
        assert!(
            playback_on.contains("✓ Remote lyrics"),
            "the toggle must read as checked when enabled, got {playback_on:?}"
        );
    }

    #[test]
    fn playback_tab_renders_the_gain_slider_with_its_value() {
        use crate::state::{Popup, SettingsDraft, SettingsTab};

        let render = |tab: SettingsTab, configure: &dyn Fn(&mut SettingsDraft)| {
            let mut app = App::new();
            let mut draft = SettingsDraft::from_state(
                app.state(),
                &crate::config::AppConfig::default(),
                &std::path::PathBuf::new(),
            );
            configure(&mut draft);
            app.state_mut().popup_dialog.open_popup(Popup::Settings {
                tab,
                focus: crate::state::SettingsFocus::Content,
                draft,
            });

            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw_settings(
                        frame,
                        frame.area(),
                        &view_for(app.state(), 64),
                        &Theme::default(),
                    )
                })
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let mut dump = String::new();
            for y in 0..24u16 {
                for x in 0..80u16 {
                    dump.push_str(buf[(x, y)].symbol());
                }
            }
            dump
        };

        let dump = render(SettingsTab::Playback, &|draft: &mut SettingsDraft| {
            draft.gain_db = crate::audio::GainDb::try_from(3.0).unwrap();
        });
        assert!(
            dump.contains("Gain") && dump.contains("+3.0 dB"),
            "Playback tab must render the gain slider with its dB value, got {dump:?}"
        );
        assert!(
            dump.contains("\u{2501}"),
            "the gain slider bar must show a traversed (heavy) track portion, got {dump:?}"
        );
    }

    #[test]
    fn playback_tab_renders_the_crossfade_row_and_marks_off() {
        use crate::state::{Popup, SettingsDraft, SettingsTab};

        let render = |seconds: u16| {
            let mut app = App::new();
            let mut draft = SettingsDraft::from_state(
                app.state(),
                &crate::config::AppConfig::default(),
                &std::path::PathBuf::new(),
            );
            draft.crossfade_seconds = crate::audio::CrossfadeSeconds::from_boundary(seconds);
            app.state_mut().popup_dialog.open_popup(Popup::Settings {
                tab: SettingsTab::Playback,
                focus: crate::state::SettingsFocus::Content,
                draft,
            });

            let backend = ratatui::backend::TestBackend::new(80, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    draw_settings(
                        frame,
                        frame.area(),
                        &view_for(app.state(), 64),
                        &Theme::default(),
                    )
                })
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let mut dump = String::new();
            for y in 0..24u16 {
                for x in 0..80u16 {
                    dump.push_str(buf[(x, y)].symbol());
                }
            }
            dump
        };

        let off = render(0);
        assert!(
            off.contains("Crossfade") && off.contains("Off"),
            "0 s must read as Off, got {off:?}"
        );

        let on = render(15);
        assert!(
            on.contains("Crossfade") && on.contains("15 s") && !on.contains("Off"),
            "an enabled crossfade must show its seconds and not the Off hint, got {on:?}"
        );

        // The bar uses 5 slots: 15 s fills 3 of them (the documented shape).
        assert!(
            on.contains("\u{25AE}\u{25AE}\u{25AE}\u{25AF}\u{25AF}"),
            "15 s must render a 3-of-5 filled bar, got {on:?}"
        );

        // 30 s clamps at the cap so the bar reads as full instead of overflowing.
        let maxed = render(30);
        assert!(
            maxed.contains("\u{25AE}\u{25AE}\u{25AE}\u{25AE}\u{25AE}")
                && !maxed.contains("\u{25AF}"),
            "30 s must render a fully filled bar without empty slots, got {maxed:?}"
        );
    }

    #[test]
    fn now_playing_status_row_uses_the_state_glyph() {
        use crate::audio::{PlayStatus, PlaybackState};

        for (status, glyph) in [
            (PlayStatus::Playing, "\u{25b7}"),
            (PlayStatus::Paused, "\u{275a}\u{275a}"),
            (PlayStatus::Stopped, "\u{25a1}"),
        ] {
            let mut state = fixture_state(Vec::new());
            state.playback = PlaybackState {
                status,
                ..PlaybackState::default()
            };
            let rows = now_playing_cells(&state, 80, 12);
            let status_row: String = rows[6].join("");
            assert!(
                status_row.contains(glyph),
                "status {status:?} must render {glyph:?}, got {status_row:?}"
            );
        }
    }

    #[test]
    fn now_playing_status_fields_are_fixed_width_columns() {
        // The state glyph occupies its own narrow left-aligned column, then
        // the volume pictogram and the rest start at fixed offsets — never
        // squeezed against a pipe.
        let mut paused = fixture_state(Vec::new());
        paused.playback.status = crate::audio::PlayStatus::Paused;

        let rows = now_playing_cells(&paused, 80, 12);
        // Drop the block's left border cell (x=0) so the content starts at 0.
        let status_row: String = rows[6][1..].join("");

        // Glyph column: left-aligned at the very start of the content.
        assert!(
            status_row.starts_with("\u{275a}\u{275a}"),
            "state glyph must be left-aligned in its own column, got {status_row:?}"
        );
        // Volume bar, Speed (⏱), Repeat and Shuffle follow the state glyph
        // column in that fixed-width order, Speed directly after Volume.
        // The bar's first filled block is unique to the volume column, so it
        // doubles as the column anchor for layout assertions.
        let vol_char = status_row.find('\u{25AE}').expect("volume bar present");
        let speed_char = status_row.find("⏱").expect("stopwatch glyph present");
        let repeat_char = status_row.find("↻").expect("repeat glyph present");
        let shuffle_char = status_row.find("⇄").expect("shuffle glyph present");
        assert!(
            vol_char < speed_char && speed_char < repeat_char && repeat_char < shuffle_char,
            "volume bar < ⏱ < ↻ < ⇄ in row {status_row:?}"
        );
        // The Volume metric starts left-aligned in its own column.
        let vol_field = &status_row[vol_char..];
        assert!(
            vol_field.starts_with('\u{25AE}'),
            "volume bar starts at the left edge of its column, got {vol_field:?}"
        );
        // The Speed value keeps a stable fixed-width slot so the row never shifts.
        let speed_field = &status_row[speed_char..];
        assert!(
            speed_field.starts_with("⏱"),
            "Speed must start with the stopwatch glyph, got {speed_field:?}"
        );
        // Convert byte offsets to display-column (cell) offsets via unicode
        // width, so wide glyphs (like the stopwatch) keep their exact column.
        use unicode_width::UnicodeWidthStr;
        let cell_pos = |byte: usize| status_row[..byte].width();
        assert_eq!(
            STATE_COLUMN_WIDTH, 3,
            "the state glyph column must reserve exactly three cells"
        );
        // The glyph begins one cell in (the field is " ↻ "), right after
        // the state (2) + Volume (10) + Speed (7) columns.
        assert_eq!(
            cell_pos(repeat_char),
            STATE_COLUMN_WIDTH + VOLUME_COLUMN_WIDTH + SPEED_COLUMN_WIDTH + 1,
            "repeat glyph must start after state + volume + speed columns"
        );
        // Shuffle begins right after Repeat's column (plus its leading space).
        assert_eq!(
            cell_pos(shuffle_char),
            STATE_COLUMN_WIDTH + VOLUME_COLUMN_WIDTH + SPEED_COLUMN_WIDTH + REPEAT_COLUMN_WIDTH + 1,
            "Shuffle must start after state + volume + speed + repeat columns"
        );
    }

    #[test]
    fn now_playing_volume_indicator_renders_the_pictogram_and_level_bar() {
        use crate::audio::PlaybackState;

        // The status row sits at the same offset used by the column-layout
        // test; dropping the left border cell keeps the content aligned with
        // column 0 so string contains/starts_with work as expected.
        let status_row_for = |volume: u16| -> String {
            let mut state = fixture_state(Vec::new());
            state.playback.status = crate::audio::PlayStatus::Paused;
            state.playback = PlaybackState {
                status: crate::audio::PlayStatus::Paused,
                volume_percent: crate::audio::VolumePercent::from_boundary(volume),
                ..PlaybackState::default()
            };
            let rows = now_playing_cells(&state, 80, 12);
            rows[6][1..].join("")
        };

        // 70 % is the documented shape: bar + space + value, no pictogram.
        let at_70 = status_row_for(70);
        assert!(
            at_70.contains("\u{25AE}\u{25AE}\u{25AE}\u{25AF}\u{25AF} 70%"),
            "70 % must render the 3-of-5 bar + value, got {at_70:?}"
        );

        // 0 % empties the bar without ever overflowing into the Speed column.
        let at_0 = status_row_for(0);
        assert!(
            at_0.contains("\u{25AF}\u{25AF}\u{25AF}\u{25AF}\u{25AF} 0%"),
            "0 % must render an empty bar, got {at_0:?}"
        );

        // 100 % fills every slot and keeps the Speed column aligned.
        let at_100 = status_row_for(100);
        assert!(
            at_100.contains("\u{25AE}\u{25AE}\u{25AE}\u{25AE}\u{25AE} 100%"),
            "100 % must render a fully filled bar, got {at_100:?}"
        );
    }

    fn fixture_state(entries: Vec<FileEntry>) -> AppState {
        let mut state = AppState::default();
        state
            .browser
            .replace_contents(PathBuf::from("/music"), entries);
        state
    }

    #[test]
    fn truncate_keeps_short_names_intact() {
        assert_eq!(truncate_with_ellipsis("abc", 5), "abc");
        assert_eq!(truncate_with_ellipsis("exact", 5), "exact");
    }

    #[test]
    fn truncate_cuts_long_names_with_an_ellipsis() {
        let cut = truncate_with_ellipsis("a-very-long-song-name.mp3", 10);

        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
        assert!(cut.starts_with("a-very-lo"));
    }

    #[test]
    fn truncate_with_zero_budget_yields_only_the_ellipsis() {
        assert_eq!(truncate_with_ellipsis("song.mp3", 0), "…");
    }

    #[test]
    fn truncate_with_ellipsis_respects_wide_columns() {
        // Each CJK char is 2 terminal columns, so a 6-column budget can hold
        // at most 2 wide chars + the ellipsis (2*2 + 1 = 5 ≤ 6).
        let cut = truncate_with_ellipsis("你好世界abc", 6);
        assert_eq!(cut, "你好…");
        // The result must fit within the column budget.
        use unicode_width::UnicodeWidthStr;
        assert!(cut.width() <= 6, "got {cut} ({})", cut.width());
    }

    #[test]
    fn middle_truncate_preserves_both_ends_of_long_paths() {
        let cut = middle_truncate("/home/user/Music/Collection", 15);

        assert_eq!(cut.chars().count(), 15);
        assert!(cut.starts_with("/home"));
        assert!(cut.ends_with("ection"));
        assert!(cut.contains('…'));
    }

    #[test]
    fn middle_truncate_respects_wide_columns() {
        let cut = middle_truncate("你好世界世界你好", 5);
        use unicode_width::UnicodeWidthStr;
        assert!(cut.width() <= 5, "got {cut} ({})", cut.width());
        assert!(cut.contains('…'));
    }

    #[test]
    fn middle_truncate_respects_degenerate_budgets() {
        assert_eq!(middle_truncate("/some/path", 0), "");
        assert_eq!(middle_truncate("/some/path", 1), "…");
        assert_eq!(middle_truncate("/short", 10), "/short");
    }

    #[test]
    fn browser_title_reports_location_and_count_within_budget() {
        let mut state = fixture_state(vec![FileEntry::new(
            "a.mp3",
            PathBuf::from("/music/a.mp3"),
            EntryKind::File,
        )]);
        state.browser.current_dir = PathBuf::from("/home/user/Music/VeryLongCollectionName");

        let title = browser_title(&state.browser, 30);

        assert_eq!(title.chars().count(), 26);
        assert!(title.ends_with(" (1)"));
        assert!(title.contains('…'));
    }

    #[test]
    fn browser_rows_mark_and_dim_according_to_state() {
        let state = fixture_state(vec![
            FileEntry::new("Album", PathBuf::from("/music/Album"), EntryKind::Dir),
            FileEntry::new("hit.mp3", PathBuf::from("/music/hit.mp3"), EntryKind::File),
            FileEntry::new(
                "note.txt",
                PathBuf::from("/music/note.txt"),
                EntryKind::File,
            ),
        ]);
        let mut marks = HashSet::new();
        marks.insert(PathBuf::from("/music/hit.mp3"));

        let dir_line = browser_entry_line(
            &state.browser.entries[0],
            false,
            true,
            40,
            &Theme::default(),
        );
        let marked_line = browser_entry_line(
            &state.browser.entries[1],
            true,
            false,
            40,
            &Theme::default(),
        );
        let dimmed_line = browser_entry_line(
            &state.browser.entries[2],
            false,
            false,
            40,
            &Theme::default(),
        );

        // Directory rows carry the slash suffix and hold the cursor background
        assert_eq!(dir_line.spans[0].content, "Album/");
        assert_eq!(dir_line.spans[0].style.bg, Some(Theme::default().selection));
        // Marked rows show the flag AFTER the name, styled with the highlight
        assert_eq!(marked_line.spans[0].content, "hit.mp3");
        assert_eq!(marked_line.spans[1].content, " ✓");
        assert_eq!(
            marked_line.spans[1].style.fg,
            Some(Theme::default().highlight)
        );
        // Unsupported files render dimmed without any flag
        assert_eq!(dimmed_line.spans[0].content, "note.txt");
        assert_eq!(
            dimmed_line.spans[0].style.fg,
            Some(Theme::default().text_muted)
        );
    }

    #[test]
    fn visible_window_start_matches_the_browser_scroll_rule() {
        let mut state = fixture_state(
            (0..20)
                .map(|index| {
                    FileEntry::new(
                        format!("f{index}"),
                        PathBuf::from(format!("/music/f{index}")),
                        EntryKind::File,
                    )
                })
                .collect(),
        );
        state.browser.set_viewport_height(5);
        state.browser.goto_bottom();
        // Cursor 19 forces the five row window to start at 15
        assert_eq!(visible_window_start(&state.browser), 15);

        state.browser.goto_top();
        assert_eq!(visible_window_start(&state.browser), 0);
    }

    #[test]
    fn help_lines_match_the_scroll_ceiling_structure() {
        use crate::config::KeysConfig;
        // The popup scroll ceiling is derived from help_line_count, so
        // the rendered line structure must mirror it exactly
        let keys = KeysConfig::default();
        let cache = HelpContentCache::new(&keys);
        let lines = build_help_lines(&Theme::default(), cache.lines());

        assert_eq!(lines.len(), crate::input::help_line_count(&keys));
        assert_eq!(lines[0].spans[0].content, "Global");
        // The section headers appear in display order
        let headers: Vec<&str> = [
            "Global",
            "Navigation",
            "File Browser",
            "Playlist",
            "Streaming",
            "Playlist manager",
            "Popups",
            "Settings",
        ]
        .into_iter()
        .collect();
        let found: Vec<&str> = lines
            .iter()
            .filter(|line| {
                line.spans.len() == 1 && headers.contains(&line.spans[0].content.as_ref())
            })
            .map(|line| line.spans[0].content.as_ref())
            .collect();
        assert_eq!(found, headers);
    }

    #[test]
    fn centered_percent_rect_stays_inside_small_terminals() {
        let outer = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 10,
        };

        let popup = centered_percent_rect(HELP_WIDTH_PERCENT, HELP_HEIGHT_PERCENT, outer);

        assert!(popup.width <= outer.width);
        assert!(popup.height <= outer.height);
        assert!(popup.x + popup.width <= outer.x + outer.width);
        assert!(popup.y + popup.height <= outer.y + outer.height);
    }

    /// Render the now playing band into a test buffer and return the cell
    /// symbols per row, because border glyphs are multi byte and would
    /// break plain string indexing.
    fn now_playing_cells(state: &AppState, width: u16, height: u16) -> Vec<Vec<String>> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                draw_now_playing(frame, frame.area(), &view_for(state, 64), &Theme::default())
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn now_playing_without_artwork_uses_the_full_width() {
        let state = fixture_state(Vec::new());

        let rows = now_playing_cells(&state, 80, 12);
        // The state glyph sits on the status row (below the progress bar), not
        // on the title row which now carries no redundant icon.
        let status_row: String = rows[6].join("");
        assert!(
            status_row.contains('\u{25a1}') // □ Stopped
                || status_row.contains('\u{25b7}') // ▷ Playing
                || status_row.contains("\u{275a}\u{275a}"), // ❚❚ Paused
            "status row must carry the playback glyph, got {status_row:?}"
        );
        // The title row stays clean: only the artist/title text.
        let title_row: String = rows[4].join("");
        assert!(
            !title_row.contains('▶') && !title_row.contains("❚❚") && !title_row.contains('■'),
            "title row must not repeat the state glyph, got {title_row:?}"
        );
    }

    #[test]
    fn now_playing_with_artwork_keeps_text_at_full_width() {
        let mut state = fixture_state(Vec::new());
        state.artwork.set_enabled(true);
        state.artwork.set_artwork(
            0,
            Some(crate::artwork::ArtworkProtocol::new(
                crate::artwork::testing::test_protocol(),
            )),
        );

        let rows = now_playing_cells(&state, 80, 12);
        let title_row: String = rows[4].join("");

        // With artwork present the band still reserves no separate artwork
        // cell: the title sits at the left edge and no placeholder is shown
        assert!(
            !title_row.contains("No art"),
            "the now playing band must not reserve an artwork cell"
        );
        let flat: String = rows.concat().join("");
        assert!(
            !flat.contains("No art"),
            "the now playing band must not reserve an artwork cell"
        );
    }

    #[test]
    fn gauge_renders_bar_when_track_is_playing() {
        use crate::playlist::Playlist;
        use crate::track::Track;
        use std::time::Duration;

        let mut state = fixture_state(Vec::new());
        state.playback.elapsed = Duration::from_secs(30);
        state.playback.duration = Some(Duration::from_secs(180));
        state.playback.track_index = Some(0);
        let track = Track::local(PathBuf::from("/test/song.mp3"));
        let mut playlist = Playlist::new();
        playlist.extend([track]);
        state.playlist = playlist;

        let backend = ratatui::backend::TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_now_playing(
                    frame,
                    frame.area(),
                    &view_for(&state, 64),
                    &Theme::default(),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // With a 10-row buffer the bordered block leaves 8 inner rows; the
        // three text lines are vertically centered, so the gauge sits at
        // row 4 (row 0 = border, rows 1-2 = top padding, row 3 = title,
        // row 4 = bar with time overlay)
        let mut full_blocks = 0u16;
        let mut bar_fg = None;
        for x in 1u16..79 {
            if buf[(x, 4)].symbol() == "█" {
                full_blocks += 1;
                bar_fg = buf[(x, 4)].style().fg;
            }
        }
        assert!(
            full_blocks > 0,
            "gauge must render filled bar blocks when a track is playing, found 0 full blocks"
        );
        assert_eq!(
            bar_fg,
            Some(Theme::default().border_focused),
            "the progress bar must use the focused border colour, not a separate progress colour"
        );
    }

    #[test]
    fn time_text_over_the_bar_flips_to_the_background_contrast() {
        use crate::playlist::Playlist;
        use crate::track::Track;
        use std::time::Duration;

        let mut state = fixture_state(Vec::new());
        state.playback.elapsed = Duration::from_secs(110);
        state.playback.duration = Some(Duration::from_secs(200));
        state.playback.track_index = Some(0);
        let mut playlist = Playlist::new();
        playlist.extend([Track::local(PathBuf::from("/test/song.mp3"))]);
        state.playlist = playlist;

        let backend = ratatui::backend::TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_now_playing(
                    frame,
                    frame.area(),
                    &view_for(&state, 64),
                    &Theme::default(),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        let theme = Theme::default();
        let mut on_bar = false;
        let mut off_bar = false;
        // Row 4 hosts the progress bar (with the overlaid time text). Scan for
        // time characters: the ones over the filled bar use the theme
        // background as their fg, the ones over the empty part keep time_text.
        for x in 1u16..79 {
            let cell = buf[(x, 4)].clone();
            let sym = cell.symbol();
            if sym.contains(':') || sym.chars().any(|c| c.is_ascii_digit()) {
                if cell.style().fg == Some(theme.background) {
                    on_bar = true;
                } else if cell.style().fg == Some(theme.time_text) {
                    off_bar = true;
                }
            }
        }
        assert!(
            on_bar && off_bar,
            "time characters over the bar must flip to the background colour (on_bar={on_bar}) while those over the empty bar stay time_text (off_bar={off_bar})"
        );
    }

    #[test]
    fn progress_bar_spans_keep_filled_and_unfilled_ranges_bounded() {
        let theme = Theme::default();
        let spans = progress_bar_spans(10, 4, "", &theme);

        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "████");
        assert_eq!(spans[0].style.fg, Some(theme.progress));
        assert_eq!(spans[1].content.as_ref(), "      ");
        assert_eq!(spans[1].style.bg, Some(theme.artwork_area));
    }

    #[test]
    fn progress_bar_spans_measure_unicode_overlay_by_display_width() {
        use unicode_width::UnicodeWidthStr;

        let theme = Theme::default();
        let spans = progress_bar_spans(6, 3, "界a", &theme);
        let rendered = spans.iter().fold(String::new(), |mut rendered, span| {
            rendered.push_str(span.content.as_ref());
            rendered
        });

        assert_eq!(rendered, "█界a  ");
        assert_eq!(rendered.width(), 6);
        assert_eq!(spans[1].content.as_ref(), "界");
        assert_eq!(spans[1].style.fg, Some(theme.background));
        assert_eq!(spans[2].content.as_ref(), "a");
        assert_eq!(spans[2].style.fg, Some(theme.time_text));
    }

    #[test]
    fn progress_bar_spans_clip_overlay_for_narrow_bars() {
        use unicode_width::UnicodeWidthStr;

        let spans = progress_bar_spans(3, 2, "00:00", &Theme::default());
        let rendered = spans.iter().fold(String::new(), |mut rendered, span| {
            rendered.push_str(span.content.as_ref());
            rendered
        });

        assert_eq!(rendered, "00:");
        assert_eq!(rendered.width(), 3);
    }

    #[test]
    fn playlist_row_reflects_metadata_sort_config() {
        use crate::config::{SortBy, SortTracksConfig};
        use crate::metadata::TrackMetadata;
        use crate::track::Track;
        use std::time::Duration;

        let mut track = Track::local("/audio/song.mp3");
        track.set_metadata(TrackMetadata {
            title: "Tom Sawyer".into(),
            title_tagged: true,
            artist: "Rush".into(),
            album: "Moving Pictures".into(),
            track_number: Some(1),
            duration: Duration::from_secs(240),
            bitrate: None,
            sample_rate: None,
            codec: "MP3".into(),
            format: "MP3".into(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        });

        // Metadata strategy: the row shows every enabled field in the
        // canonical Artist, Album, Track Number, Title order.
        let sort = SortTracksConfig {
            sort_by: SortBy::Metadata,
            metadata_artist: true,
            metadata_album: true,
            metadata_track_number: true,
            metadata_title: true,
        };
        assert_eq!(
            playlist_entry_name(&track, &sort),
            "Rush - Moving Pictures - 1 - Tom Sawyer"
        );

        // Filename strategy: the row falls back to the file stem even when
        // tags are already attached.
        let filename = SortTracksConfig {
            sort_by: SortBy::Filename,
            ..SortTracksConfig::default()
        };
        assert_eq!(playlist_entry_name(&track, &filename), "song");
    }

    #[test]
    fn playlist_row_falls_back_to_stem_without_metadata() {
        use crate::config::{SortBy, SortTracksConfig};
        use crate::track::Track;

        // No tags yet (extraction pending): the row must show the file stem
        // regardless of the strategy.
        let track = Track::local("/audio/song.mp3");
        let sort = SortTracksConfig {
            sort_by: SortBy::Metadata,
            metadata_artist: true,
            metadata_title: true,
            ..SortTracksConfig::default()
        };
        assert_eq!(playlist_entry_name(&track, &sort), "song");
    }

    #[test]
    fn playlist_row_drops_unknown_artist_sentinel() {
        use crate::config::{SortBy, SortTracksConfig};
        use crate::metadata::TrackMetadata;
        use crate::track::Track;
        use std::time::Duration;

        let mut track = Track::local("/audio/song.mp3");
        track.set_metadata(TrackMetadata {
            title: "Tom Sawyer".into(),
            title_tagged: true,
            artist: "Unknown Artist".into(),
            album: "Moving Pictures".into(),
            track_number: None,
            duration: Duration::from_secs(240),
            bitrate: None,
            sample_rate: None,
            codec: "MP3".into(),
            format: "MP3".into(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        });
        let sort = SortTracksConfig {
            sort_by: SortBy::Metadata,
            metadata_artist: true,
            metadata_title: true,
            ..SortTracksConfig::default()
        };
        assert_eq!(
            playlist_entry_name(&track, &sort),
            "Tom Sawyer",
            "the Unknown Artist sentinel must not pollute the row"
        );
    }

    #[test]
    fn playlist_reserves_a_marker_column_for_the_playing_track() {
        use crate::playlist::Playlist;
        use crate::track::Track;
        use std::path::PathBuf;

        let mut state = fixture_state(Vec::new());
        let mut playlist = Playlist::new();
        playlist.extend([
            Track::local(PathBuf::from("/test/a.mp3")),
            Track::local(PathBuf::from("/test/b.mp3")),
        ]);
        state.playlist = playlist;
        state.playback.track_index = Some(1);

        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_playlist(
                    frame,
                    frame.area(),
                    &view_for(&state, 64),
                    false,
                    &Theme::default(),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // Content sits inside the block border: x=1, first track at y=1.
        assert_eq!(
            buf[(1, 1)].symbol(),
            " ",
            "non-playing track keeps a blank marker"
        );
        assert_eq!(
            buf[(1, 2)].symbol(),
            "▶",
            "playing track shows the run icon"
        );
        // The reserved column keeps both names starting at the same column.
        assert_ne!(buf[(3, 1)].symbol(), " ");
        assert_ne!(buf[(3, 2)].symbol(), " ");
    }

    /// Regression test for the playing-track marker on stream rows. The
    /// indicator must apply to whichever element is sounding — local or
    /// stream — at the index the audio engine reported, including when the
    /// queue mixes both kinds.
    #[test]
    fn playlist_reserves_a_marker_column_for_the_playing_stream() {
        use crate::playlist::Playlist;
        use crate::stream::StreamKind;
        use crate::track::Track;
        use std::path::PathBuf;

        let mut state = fixture_state(Vec::new());
        let mut playlist = Playlist::new();
        playlist.extend([
            Track::local(PathBuf::from("/test/a.mp3")),
            Track::from_stream(
                url::Url::parse("https://www.youtube.com/watch?v=abc").expect("valid url"),
                StreamKind::Http,
            ),
            Track::local(PathBuf::from("/test/c.mp3")),
        ]);
        state.playlist = playlist;
        // Stream at index 1 is playing.
        state.playback.track_index = Some(1);

        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_playlist(
                    frame,
                    frame.area(),
                    &view_for(&state, 64),
                    false,
                    &Theme::default(),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // Border at y=0. Rows at y=1, y=2, y=3 are the three tracks.
        assert_eq!(buf[(1, 1)].symbol(), " ", "first local track is blank");
        assert_eq!(
            buf[(1, 2)].symbol(),
            "▶",
            "playing stream shows the run icon"
        );
        assert_eq!(buf[(1, 3)].symbol(), " ", "third local track is blank");
    }

    #[test]
    fn playlist_scrolls_past_the_visible_height_keeping_the_cursor_visible() {
        use crate::playlist::Playlist;
        use crate::track::Track;
        use std::path::PathBuf;

        // Twenty queued tracks in a short panel (40x10 => 8 inner rows). The
        // selected track is past the initial viewport; the panel must scroll
        // so the highlight is actually rendered instead of clipped away.
        let mut state = fixture_state(Vec::new());
        let mut playlist = Playlist::new();
        playlist.extend((0..20).map(|i| Track::local(PathBuf::from(format!("/test/t{i:02}.mp3")))));
        state.playlist = playlist;
        // Match the 40x10 panel: 10 rows minus the two border rows.
        state.playlist_viewport_height = 8;
        state.move_playlist_to_bottom();

        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_playlist(
                    frame,
                    frame.area(),
                    &view_for(&state, 64),
                    false,
                    &Theme::default(),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        let mut selected_row: Option<u16> = None;
        // Skip the border rows (y=0 and y=9) and scan inner rows for the
        // selection background on the last track.
        for y in 1u16..9 {
            if buf[(1, y)].style().bg == Some(Theme::default().selection) {
                selected_row = Some(y);
                break;
            }
        }
        assert!(
            selected_row.is_some(),
            "cursor highlight must be reachable after scrolling, not hidden"
        );
        // With 20 rows in an 8-row viewport the last row must appear in the
        // lower part of the panel (thanks to the two-row breathing rule), and
        // it must not be pinned to the top edge.
        assert!(
            selected_row.unwrap() > 1,
            "dead zone at the bottom is unreachable: {selected_row:?}"
        );
    }

    #[test]
    fn playlist_render_consumes_the_state_window_without_mutating_it() {
        use crate::playlist::Playlist;
        use crate::track::Track;
        use std::path::PathBuf;

        let mut state = fixture_state(Vec::new());
        let mut playlist = Playlist::new();
        playlist.extend((0..20).map(|i| Track::local(PathBuf::from(format!("/test/t{i:02}.mp3")))));
        state.playlist = playlist;
        state.playlist_viewport_height = 8;
        state.move_playlist_to_bottom();
        state.move_playlist_cursor_up();
        let before = state.playlist_scroll_offset;

        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_playlist(
                    frame,
                    frame.area(),
                    &view_for(&state, 64),
                    false,
                    &Theme::default(),
                )
            })
            .unwrap();

        assert_eq!(
            state.playlist_scroll_offset, before,
            "rendering must consume the state-owned playlist window"
        );
    }

    #[test]
    fn narrow_bands_skip_the_artwork_cell() {
        let mut state = fixture_state(Vec::new());
        state.artwork.set_enabled(true);
        state.artwork.set_artwork(
            0,
            Some(crate::artwork::ArtworkProtocol::new(
                crate::artwork::testing::test_protocol(),
            )),
        );

        // Thirty columns still give the text the full inner width; there is
        // no separate artwork column to crowd it out. The title row carries
        // no state glyph (that lives on the status row), so it must render the
        // plain artist/title text.
        let rows = now_playing_cells(&state, 30, 8);
        let title_row: String = rows[2].join("");

        assert!(
            !title_row.contains('▶') && !title_row.contains('■') && !title_row.contains("❚❚"),
            "title row must not carry a redundant state glyph, got {title_row:?}"
        );
        let flat: String = rows.concat().join("");
        assert!(
            !flat.contains("No art"),
            "narrow bands must not reserve an artwork cell"
        );
    }

    #[test]
    fn artwork_overlay_target_derives_from_visibility_and_focus() {
        // Artwork overlays the inactive panel so the focused one stays clear
        assert_eq!(
            artwork_overlay_target(true, true, true, Panel::Browser, false),
            Some(Panel::Playlist)
        );
        assert_eq!(
            artwork_overlay_target(true, true, true, Panel::Playlist, false),
            Some(Panel::Browser)
        );
        // Any of the gating conditions failing means no overlay at all
        assert_eq!(
            artwork_overlay_target(false, true, true, Panel::Browser, false),
            None
        );
        assert_eq!(
            artwork_overlay_target(true, false, true, Panel::Browser, false),
            None
        );
        assert_eq!(
            artwork_overlay_target(true, true, false, Panel::Browser, false),
            None
        );
    }

    #[test]
    fn lyrics_panel_renders_content_inside_the_browser_area() {
        let mut app = crate::app::App::new();
        app.state_mut().lyrics.visible = true;
        app.state_mut().lyrics.display_title = Some("Song Title".to_string());
        app.state_mut().lyrics.document = Some(crate::lyrics::LyricsDocument::from_plain(
            "line one\nline two",
        ));

        let dump = render_dump(&mut app);

        assert!(dump.contains("Song Title"), "panel title missing: {dump:?}");
        assert!(dump.contains("line one"), "first line missing: {dump:?}");
        assert!(dump.contains("line two"), "second line missing: {dump:?}");
    }

    #[test]
    fn lyrics_panel_shows_the_miss_reason_without_a_duplicate_header() {
        let mut app = crate::app::App::new();
        app.state_mut().lyrics.visible = true;
        app.state_mut().lyrics.error = Some("No track is playing".to_string());

        let dump = render_dump(&mut app);

        assert!(
            dump.contains("No track is playing"),
            "reason must be visible: {dump:?}"
        );
        assert!(
            !dump.contains("Lyrics not available"),
            "the old generic header must be gone: {dump:?}"
        );
    }

    #[test]
    fn lyrics_panel_defaults_to_the_plain_miss_message() {
        let mut app = crate::app::App::new();
        app.state_mut().lyrics.visible = true;

        let dump = render_dump(&mut app);

        assert!(
            dump.contains("Lyrics not found"),
            "empty state missing: {dump:?}"
        );
        assert!(
            !dump.contains("Lyrics not available"),
            "no generic header: {dump:?}"
        );
    }

    #[test]
    fn lyrics_panel_renders_the_loading_state() {
        let mut app = crate::app::App::new();
        app.state_mut().lyrics.visible = true;
        app.state_mut().lyrics.loading = true;

        let dump = render_dump(&mut app);

        assert!(dump.contains("Loading"), "loading state missing: {dump:?}");
    }

    #[test]
    fn lyrics_panel_omits_the_loading_label_when_not_loading() {
        // A document loaded without `loading: true` must not show the
        // spinner line; otherwise the panel would always look busy even
        // when the resolution completed successfully.
        let mut app = crate::app::App::new();
        app.state_mut().lyrics.visible = true;
        app.state_mut().lyrics.loading = false;

        let dump = render_dump(&mut app);

        assert!(
            !dump.contains("Loading"),
            "non-loading lyrics state must not render the spinner: {dump:?}"
        );
    }

    #[test]
    fn now_playing_shows_loading_for_a_resolving_stream() {
        // Replace the regular progress bar with the spinner line when
        // stream activity is present and the current track is a stream.
        let mut app = crate::app::App::new();
        app.state_mut().async_ops.begin_stream_resolution();
        let url = url::Url::parse("https://example.com/live").expect("valid url");
        let track = crate::track::Track::from_stream(url, crate::stream::StreamKind::Http);
        {
            let state = app.state_mut();
            state.playlist = {
                let mut pl = crate::playlist::Playlist::new();
                pl.extend([track]);
                pl
            };
            state.playlist.select(0);
            state.playback.track_index = Some(0);
        }

        let dump = render_dump(&mut app);

        assert!(
            dump.contains("Loading"),
            "the Now Playing row must show the spinner while the stream resolves: {dump:?}"
        );
    }

    #[test]
    fn now_playing_keeps_progress_for_local_tracks_during_resolution() {
        // The flag is reserved for streams. A resolving local file must
        // not silently remove the regular progress bar.
        let mut app = crate::app::App::new();
        app.state_mut().async_ops.begin_stream_resolution();
        let path = std::path::PathBuf::from("/music/song.mp3");
        {
            let state = app.state_mut();
            state.playlist = {
                let mut pl = crate::playlist::Playlist::new();
                pl.extend([crate::track::Track::local(path)]);
                pl
            };
            state.playlist.select(0);
            state.playback.track_index = Some(0);
        }

        let dump = render_dump(&mut app);

        assert!(
            !dump.contains("Loading"),
            "the local-track progress bar must stay visible during a resolve: {dump:?}"
        );
    }

    #[test]
    fn now_playing_shows_loading_until_source_ready_clears_the_flag() {
        // The unified flag covers both phases: while resolving AND while
        // decoding. Setting the flag without dispatching ResolveStream is
        // the state right after Add Stream but before the audio worker
        // has finished building the decoder; the spinner must stay on.
        let mut app = crate::app::App::new();
        let url = url::Url::parse("https://example.com/live").expect("valid url");
        let track = crate::track::Track::from_stream(url.clone(), crate::stream::StreamKind::Http);
        {
            let state = app.state_mut();
            state.playlist = {
                let mut pl = crate::playlist::Playlist::new();
                pl.extend([track]);
                pl
            };
            state.playlist.select(0);
        }
        let _ = app.begin_current_track();

        // This is the loading snapshot emitted by the audio worker after a
        // playlist stream starts. It must retain the optimistic queue identity
        // until the matching acquisition boundary arrives.
        app.apply_playback_progress(crate::audio::PlaybackSnapshot {
            status: crate::audio::PlayStatus::Stopped,
            track_index: Some(0),
            elapsed: Duration::ZERO,
            duration: None,
            sink_health: crate::audio::SinkHealth::Healthy,
        });

        // While the flag stays set, the spinner is visible.
        let dump_before = render_dump(&mut app);
        assert!(dump_before.contains("Loading"));

        // The audio engine signals SourceReady: the handler must clear
        // the flag so the next render uses the regular progress bar.
        app.apply_source_ready(url.to_string());
        let dump_after = render_dump(&mut app);
        // The track has no duration so the row still renders, just without
        // the spinner line. Checking that the "Loading" line is gone is
        // enough to verify the unified flag dropped.
        assert!(
            !dump_after.contains("Loading"),
            "SourceReady must drop the spinner: {dump_after:?}"
        );
    }

    #[test]
    fn footer_omits_loading_when_no_effects_are_in_flight() {
        let mut app = crate::app::App::new();
        // The default state has no in-flight effects; the footer must not
        // show the spinner at all.
        let dump = render_dump(&mut app);
        assert!(
            !dump.contains("Loading"),
            "the footer must stay clean when pending_effects is zero: {dump:?}"
        );
    }

    #[test]
    fn footer_shows_loading_when_an_effect_is_in_flight() {
        let mut app = crate::app::App::new();
        app.state_mut()
            .async_ops
            .pending_effects
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dump = render_dump(&mut app);
        assert!(
            dump.contains("Loading"),
            "the footer must surface the spinner when any effect is in flight: {dump:?}"
        );
    }

    #[test]
    fn add_stream_popup_shows_loading_while_resolving() {
        // The Add Stream popup replaces its hint line with the spinner when
        // the dialog's `loading` flag is true, matching every other in-flight
        // surface in the UI.
        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            DialogMode::AddStream {
                error: None,
                loading: true,
            },
            "https://example.com/live".to_string(),
            None,
        );

        let dump = render_dump(&mut app);

        assert!(
            dump.contains("Loading"),
            "resolving Add Stream must surface the spinner: {dump:?}"
        );
    }

    #[test]
    fn add_stream_popup_hides_loading_when_idle() {
        // When the dialog is not resolving, the hint line still shows the
        // URL placeholder, not the spinner.
        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            DialogMode::AddStream {
                error: None,
                loading: false,
            },
            "https://example.com/live".to_string(),
            None,
        );

        let dump = render_dump(&mut app);

        assert!(
            !dump.contains("Loading"),
            "idle Add Stream must not render the spinner: {dump:?}"
        );
    }

    #[test]
    fn add_stream_from_browser_resolves_to_playlist_focus_and_playback() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = crate::app::App::new();
        app.set_active_panel(Panel::Browser);
        app.handle_command(crate::command::Command::AddStream);
        app.state_mut()
            .popup_dialog
            .set_dialog_input("https://radio.example.com/live".to_string());

        let resolve_effects =
            app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let request_id = resolve_effects
            .iter()
            .find_map(|effect| match effect {
                crate::app::Effect::ResolveStream { request_id, .. } => Some(*request_id),
                _ => None,
            })
            .expect("Add Stream submission must start resolution");
        assert!(app.state().async_ops.stream_activity().is_some());
        assert!(matches!(
            app.state().popup_dialog.dialog_mode_ref(),
            Some(DialogMode::AddStream { loading: true, .. })
        ));
        assert!(
            render_dump(&mut app).contains("Loading"),
            "the Add Stream dialog must show its spinner while resolving"
        );

        let url = url::Url::parse("https://radio.example.com/live").expect("valid url");
        let track = crate::track::Track::from_stream(url.clone(), crate::stream::StreamKind::Http);
        app.apply_stream_resolved(request_id, url.clone(), Some(Box::new(track)), None);

        assert_eq!(app.active_panel(), Panel::Playlist);
        assert_eq!(app.state().playlist.cursor(), 0);
        assert!(app.state().popup_dialog.active_popup_ref().is_none());

        let play_effects = app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(play_effects.iter().any(|effect| matches!(
            effect,
            crate::app::Effect::Audio(crate::audio::AudioCommand::Play {
                source,
                track_index: 0,
                ..
            }) if source.track_location() == crate::track::TrackLocation::url(url.clone())
        )));
    }

    /// A gruvbox-like palette loaded through the real file layer with no
    /// `lyrics_*` keys: everything the lyrics panel paints must come from
    /// this theme's own roles, never from the built-in defaults.
    fn palette_theme_without_lyrics_keys() -> Theme {
        Theme::load_from_toml(
            "[colors]\n\
             background = \"#282828\"\n\
             foreground = \"#ebdbb2\"\n\
             text_muted = \"#a89984\"\n\
             border = \"#928374\"\n\
             border_focused = \"#d65d0e\"\n\
             highlight = \"#d79921\"\n",
        )
        .expect("fixture palette")
    }

    fn plain_lyrics_state(text: &str) -> LyricsState {
        LyricsState {
            visible: true,
            display_title: Some("Song".to_string()),
            document: Some(crate::lyrics::LyricsDocument::from_plain(text)),
            viewport_height: 6,
            ..LyricsState::default()
        }
    }

    fn render_lyrics_buffer(
        theme: &Theme,
        lyrics: &LyricsState,
        focused: bool,
    ) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(40, 8);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                // The test builds a custom LyricsState; pull it onto a real
                // App so the panel can read the spinner from there too.
                let mut app = crate::app::App::new();
                app.state_mut().lyrics = lyrics.clone();
                app.state_mut().frame.spinner.tick(Duration::from_millis(0));
                draw_lyrics_panel(
                    frame,
                    area,
                    &view_for(app.state(), usize::from(area.width.saturating_sub(2))),
                    focused,
                    theme,
                )
            })
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    fn render_browser_corner_style(theme: &Theme, focused: bool) -> ratatui::style::Style {
        let app = crate::app::App::new();
        let state = app.state();
        let backend = ratatui::backend::TestBackend::new(40, 8);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_browser(
                    frame,
                    area,
                    &view_for(state, usize::from(area.width.saturating_sub(2))),
                    focused,
                    theme,
                )
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        buf[(0, 0)].style()
    }

    /// Column of the first title glyph on the block's top border row.
    fn title_cell_x(buf: &ratatui::buffer::Buffer) -> u16 {
        (1..40u16)
            .find(|x| {
                buf[(*x, 0)]
                    .symbol()
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
            })
            .expect("title glyphs on the border row")
    }

    #[test]
    fn unset_lyrics_colors_inherit_the_effective_theme_roles() {
        let theme = palette_theme_without_lyrics_keys();
        let lyrics = plain_lyrics_state("abc def");

        let buf = render_lyrics_buffer(&theme, &lyrics, false);

        // Plain untimed text keeps the theme's own foreground.
        assert_eq!(buf[(1, 1)].style().fg, Some(theme.text));
        // The panel background stays the theme's own background.
        assert_eq!(buf[(1, 5)].style().bg, Some(theme.background));
        // The unfocused border and the title keep the theme's own border color.
        assert_eq!(buf[(0, 0)].style().fg, Some(theme.border));
        let title_x = title_cell_x(&buf);
        assert_eq!(buf[(title_x, 0)].style().fg, Some(theme.border));

        // Pending timed rows resolve through the theme's muted role too, and
        // the loading/miss messages follow it, exactly as before the keys.
        let pending = crate::lyrics::LyricsLine {
            timestamp_ms: Some(10000),
            text: "hola".to_string(),
            words: Vec::new(),
        };
        let lines = styled_lyrics_lines(&pending, 9_999, Some(14000), true, &theme, 20);
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(theme.text_muted),
            "pending text must use the theme's own muted color"
        );
    }

    #[test]
    fn lyrics_text_override_replaces_plain_pending_and_message_colors() {
        let theme = Theme {
            lyrics_text: Some(Color::Magenta),
            ..palette_theme_without_lyrics_keys()
        };

        let plain_buf = render_lyrics_buffer(&theme, &plain_lyrics_state("abc def"), false);
        assert_eq!(plain_buf[(1, 1)].style().fg, Some(Color::Magenta));
        assert_eq!(
            plain_buf[(0, 0)].style().fg,
            Some(theme.border),
            "lyrics_text must not touch the border color"
        );

        let loading = LyricsState {
            visible: true,
            loading: true,
            ..LyricsState::default()
        };
        let loading_buf = render_lyrics_buffer(&theme, &loading, false);
        assert_eq!(loading_buf[(1, 1)].style().fg, Some(Color::Magenta));

        let missed = LyricsState {
            visible: true,
            error: Some("No track is playing".to_string()),
            ..LyricsState::default()
        };
        let missed_buf = render_lyrics_buffer(&theme, &missed, false);
        assert_eq!(missed_buf[(1, 1)].style().fg, Some(Color::Magenta));

        // Timed pending follows the override, but reached text stays on the
        // highlight role: lyrics_text replaces the plain/muted colors only.
        let line = crate::lyrics::LyricsLine {
            timestamp_ms: Some(10000),
            text: "hola".to_string(),
            words: Vec::new(),
        };
        let pending = styled_lyrics_lines(&line, 9_999, Some(14000), true, &theme, 20);
        assert_eq!(pending[0].spans[0].style.fg, Some(Color::Magenta));
        let reached = styled_lyrics_lines(&line, 14_000, Some(14000), true, &theme, 20);
        assert_eq!(reached[0].spans[0].style.fg, Some(theme.highlight));
    }

    #[test]
    fn lyrics_highlight_override_recolours_only_the_reached_text() {
        let theme = Theme {
            lyrics_highlight: Some(Color::White),
            ..palette_theme_without_lyrics_keys()
        };
        let line = crate::lyrics::LyricsLine {
            timestamp_ms: Some(10000),
            text: "hola".to_string(),
            words: Vec::new(),
        };

        let reached = styled_lyrics_lines(&line, 14_000, Some(14000), true, &theme, 20);
        assert_eq!(reached[0].spans[0].style.fg, Some(Color::White));
        let pending = styled_lyrics_lines(&line, 9_999, Some(14000), true, &theme, 20);
        assert_eq!(pending[0].spans[0].style.fg, Some(theme.text_muted));
        // A genuinely untimed line keeps the plain text role untouched.
        let plain = styled_lyrics_lines(
            &crate::lyrics::LyricsLine {
                timestamp_ms: None,
                text: "hola".to_string(),
                words: Vec::new(),
            },
            0,
            None,
            false,
            &theme,
            20,
        );
        assert_eq!(plain[0].spans[0].style.fg, Some(theme.text));

        // A split row colours its reached prefix with the override and the
        // rest with the muted role.
        let mid = styled_lyrics_lines(
            &crate::lyrics::LyricsLine {
                timestamp_ms: Some(10000),
                text: "hola amigo".to_string(),
                words: Vec::new(),
            },
            11_000,
            None,
            true,
            &theme,
            20,
        );
        assert_eq!(mid[0].spans[0].style.fg, Some(Color::White));
        assert_eq!(mid[0].spans[1].style.fg, Some(theme.text_muted));
    }

    #[test]
    fn lyrics_background_override_fills_the_panel_but_nothing_else() {
        let theme = Theme {
            lyrics_background: Some(Color::Rgb(1, 2, 3)),
            ..palette_theme_without_lyrics_keys()
        };
        let lyrics = plain_lyrics_state("abc def");

        let buf = render_lyrics_buffer(&theme, &lyrics, false);
        assert_eq!(
            buf[(1, 5)].style().bg,
            Some(Color::Rgb(1, 2, 3)),
            "empty panel rows carry the override"
        );
        assert_eq!(
            buf[(1, 1)].style().bg,
            Some(Color::Rgb(1, 2, 3)),
            "text cells carry the override too"
        );

        // The shared browser chrome is untouched: the browser block keeps the
        // global background on every cell.
        let browser_corner = render_browser_corner_style(&theme, false);
        assert_eq!(
            browser_corner.bg,
            Some(theme.background),
            "the override must not leak into the browser panel"
        );
    }

    #[test]
    fn lyrics_border_override_replaces_both_focus_states_without_leaking() {
        let theme = Theme {
            lyrics_border: Some(Color::Rgb(9, 9, 9)),
            ..palette_theme_without_lyrics_keys()
        };
        let lyrics = plain_lyrics_state("abc def");

        let unfocused = render_lyrics_buffer(&theme, &lyrics, false);
        assert_eq!(unfocused[(0, 0)].style().fg, Some(Color::Rgb(9, 9, 9)));
        let title_x = title_cell_x(&unfocused);
        assert_eq!(
            unfocused[(title_x, 0)].style().fg,
            Some(Color::Rgb(9, 9, 9)),
            "the title span follows the border override"
        );
        assert!(
            !unfocused[(0, 0)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "unfocused borders must not gain the focus cue"
        );

        let focused = render_lyrics_buffer(&theme, &lyrics, true);
        assert_eq!(focused[(0, 0)].style().fg, Some(Color::Rgb(9, 9, 9)));
        assert!(
            focused[(0, 0)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "focus must stay distinct when one color covers both border roles"
        );
        let title_x = title_cell_x(&focused);
        assert!(
            focused[(title_x, 0)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "the title keeps its bold focus cue"
        );

        // Other panels keep the global border roles in both focus states.
        assert_eq!(
            render_browser_corner_style(&theme, false).fg,
            Some(theme.border)
        );
        assert_eq!(
            render_browser_corner_style(&theme, true).fg,
            Some(theme.border_focused)
        );
    }

    #[test]
    fn lyrics_border_focused_override_marks_focus_with_its_own_color() {
        let theme = Theme {
            lyrics_border: Some(Color::Rgb(9, 9, 9)),
            lyrics_border_focused: Some(Color::Rgb(77, 77, 77)),
            ..palette_theme_without_lyrics_keys()
        };
        let lyrics = plain_lyrics_state("abc def");

        let unfocused = render_lyrics_buffer(&theme, &lyrics, false);
        assert_eq!(unfocused[(0, 0)].style().fg, Some(Color::Rgb(9, 9, 9)));
        assert!(
            !unfocused[(0, 0)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "unfocused borders never carry the focus cue"
        );

        let focused = render_lyrics_buffer(&theme, &lyrics, true);
        assert_eq!(focused[(0, 0)].style().fg, Some(Color::Rgb(77, 77, 77)));
        // The two states already differ by color, so the border drops the
        // bold cue while the title keeps it.
        assert!(
            !focused[(0, 0)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "differing state colors mark the focus, no bold border needed"
        );
        let title_x = title_cell_x(&focused);
        assert_eq!(
            focused[(title_x, 0)].style().fg,
            Some(Color::Rgb(77, 77, 77)),
            "the title span follows the focused override"
        );
        assert!(
            focused[(title_x, 0)]
                .style()
                .add_modifier
                .contains(Modifier::BOLD),
            "the title keeps its bold cue even when colors already differ"
        );
    }

    #[test]
    fn lyrics_border_focused_alone_keeps_the_unfocused_border_inherited() {
        // Only the focused slot is set: unfocused falls back to the global
        // border role while focused uses the override.
        let theme = Theme {
            lyrics_border_focused: Some(Color::Rgb(77, 77, 77)),
            ..palette_theme_without_lyrics_keys()
        };
        let lyrics = plain_lyrics_state("abc def");

        let unfocused = render_lyrics_buffer(&theme, &lyrics, false);
        assert_eq!(
            unfocused[(0, 0)].style().fg,
            Some(theme.border),
            "unset lyrics_border keeps the global border role"
        );

        let focused = render_lyrics_buffer(&theme, &lyrics, true);
        assert_eq!(focused[(0, 0)].style().fg, Some(Color::Rgb(77, 77, 77)));
    }

    #[test]
    fn lyrics_panel_title_uses_the_display_title_only() {
        let lyrics = LyricsState {
            display_title: Some("My Song".to_string()),
            ..LyricsState::default()
        };
        assert_eq!(lyrics_panel_title(&lyrics, 40), "My Song");
    }

    #[test]
    fn lyrics_panel_title_falls_back_without_a_track() {
        let lyrics = LyricsState::default();
        assert_eq!(lyrics_panel_title(&lyrics, 40), "Lyrics");

        let empty_title = LyricsState {
            display_title: Some("   ".to_string()),
            ..LyricsState::default()
        };
        assert_eq!(lyrics_panel_title(&empty_title, 40), "Lyrics");
    }

    #[test]
    fn lyrics_panel_title_is_trimmed_by_unicode_columns() {
        let lyrics = LyricsState {
            display_title: Some("你好世界".to_string()),
            ..LyricsState::default()
        };
        // "你好世界" is 8 columns: the budget for a 34 column panel is 30,
        // keeping every wide char plus the ellipsis stays inside the width.
        let clipped = lyrics_panel_title(&lyrics, 34);
        assert_eq!(clipped, "你好世界");
        assert_eq!(unicode_width::UnicodeWidthStr::width(clipped.as_str()), 8);

        let long = LyricsState {
            display_title: Some("abcdefghij".to_string()),
            ..LyricsState::default()
        };
        // A 10 column panel budgets 6 columns for the title: 5 chars plus the
        // ellipsis, measured by display columns.
        let clipped = lyrics_panel_title(&long, 10);
        assert_eq!(clipped, "abcde…");
    }

    #[test]
    fn artwork_overlay_never_targets_the_lyrics_panel() {
        // With lyrics visible the cover may only rest on the playlist, and
        // only when the playlist is not the focused panel.
        assert_eq!(
            artwork_overlay_target(true, true, true, Panel::Lyrics, true),
            Some(Panel::Playlist),
            "lyrics focused: the cover goes to the playlist, never to lyrics"
        );
        assert_eq!(
            artwork_overlay_target(true, true, true, Panel::Playlist, true),
            None,
            "playlist focused with lyrics visible: no cover, the panel stays clear"
        );
        assert_eq!(
            artwork_overlay_target(true, true, true, Panel::Lyrics, false),
            None,
            "with lyrics hidden the lyrics panel is never a target"
        );
    }

    #[test]
    fn browser_artwork_expands_to_the_full_panel_width() {
        // A 40-wide panel leaves a one-cell padding each side => inner 38 wide.
        // The cover must span the full inner width (38 cells), not a capped
        // centered box. The height comes from the image's aspect (here a 1:1
        // cover in 2:1 cells => 19 rows) and stays inside the inner area.
        let inner = Rect {
            x: 1,
            y: 1,
            width: 38,
            height: 22,
        };
        // image-aware: available.height is the cover's own scaled height.
        let cell = browser_artwork_cell(inner, None, Size::new(38, 19));

        assert_eq!(cell.width, 38, "cover fills the full inner width");
        assert_eq!(cell.x, 1, "cover starts after the one-cell left padding");
        assert_eq!(cell.height, 19, "height follows the image aspect");
        // Centered: leftover inner height is split evenly above and below.
        assert_eq!(
            cell.y,
            inner.y + (inner.height - cell.height) / 2,
            "cover is centered vertically"
        );
        assert!(
            cell.y >= inner.y && cell.y + cell.height <= inner.y + inner.height,
            "cover stays inside the inner area"
        );

        // The constant 2:1 factor mode keeps the square sizing for narrow panels.
        let short = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 6,
        };
        let cell = browser_artwork_cell(short, Some(2.0), Size::new(0, 0));
        assert_eq!(cell.height, 6, "height is capped by the inner area");
    }

    #[test]
    fn playlist_artwork_is_fixed_width_top_right() {
        // The playlist cover is anchored top-right with a fixed 16-cell width
        // and a height that follows the image aspect (here a 1:1 cover in 2:1
        // cells => 8 rows), capped by the panel, respecting the 1-cell padding.
        let inner = Rect {
            x: 1,
            y: 1,
            width: 60,
            height: 24,
        };
        let cell = playlist_artwork_cell(inner, Size::new(16, 8));

        assert_eq!(cell.width, 16, "cover keeps its fixed width");
        assert_eq!(cell.height, 8, "height follows the image aspect");
        // Anchored to the top-right inner corner.
        assert_eq!(cell.y, inner.y, "cover sits at the top padding");
        assert_eq!(
            cell.x + cell.width,
            inner.x + inner.width,
            "cover hugs the right padding"
        );
        // Narrow panel shrinks the width to fit, staying right-aligned.
        let narrow = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 20,
        };
        let cell = playlist_artwork_cell(narrow, Size::new(16, 8));
        assert_eq!(cell.width, 10, "width capped by the panel");
        assert_eq!(cell.x + cell.width, 10, "still flush to the right");
    }

    #[test]
    fn artwork_overlay_rect_is_bounded_for_browser_and_playlist_panels() {
        let mut app = App::new();
        app.state_mut().artwork.set_enabled(true);
        app.state_mut().artwork.set_artwork(
            0,
            Some(crate::artwork::ArtworkProtocol::new(
                crate::artwork::testing::test_protocol(),
            )),
        );

        for target in [Panel::Browser, Panel::Playlist] {
            let panel = Rect {
                x: 10,
                y: 6,
                width: 18,
                height: 10,
            };
            let inner = Rect {
                x: panel.x + 1,
                y: panel.y + 1,
                width: panel.width - 2,
                height: panel.height - 2,
            };
            let overlay = artwork_overlay_rect(panel, target, &view_for(app.state(), 100))
                .expect("artwork should fit inside the panel");

            assert!(overlay.x >= inner.x);
            assert!(overlay.y >= inner.y);
            assert!(overlay.x + overlay.width <= inner.x + inner.width);
            assert!(overlay.y + overlay.height <= inner.y + inner.height);
        }
    }

    #[test]
    fn artwork_overlay_rect_rejects_degenerated_panel_inner_areas() {
        let mut app = App::new();
        app.state_mut().artwork.set_enabled(true);
        app.state_mut().artwork.set_artwork(
            0,
            Some(crate::artwork::ArtworkProtocol::new(
                crate::artwork::testing::test_protocol(),
            )),
        );

        for target in [Panel::Browser, Panel::Playlist] {
            for panel in [
                Rect {
                    x: 4,
                    y: 3,
                    width: 1,
                    height: 8,
                },
                Rect {
                    x: 4,
                    y: 3,
                    width: 8,
                    height: 1,
                },
                Rect {
                    x: 4,
                    y: 3,
                    width: 2,
                    height: 2,
                },
            ] {
                assert_eq!(
                    artwork_overlay_rect(panel, target, &view_for(app.state(), 100)),
                    None,
                    "a panel without an inner cell area cannot host artwork"
                );
            }
        }
    }

    /// Render the help popup and flatten the whole buffer into one
    /// searchable string.
    fn help_dump(scroll: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let app = App::new();
        terminal
            .draw(|frame| {
                draw_help_popup(
                    frame,
                    frame.area(),
                    scroll,
                    &Theme::default(),
                    app.panel_view(),
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();

        let mut dump = String::new();
        for y in 0..24 {
            for x in 0..80 {
                dump.push_str(buffer[(x, y)].symbol());
            }
        }
        dump
    }

    #[test]
    fn help_popup_renders_sections_and_the_fixed_hint() {
        let dump = help_dump(0);

        assert!(dump.contains("Help"), "popup title missing: {dump:?}");
        assert!(dump.contains("Global"), "first section missing: {dump:?}");
        assert!(
            dump.contains("Navigation"),
            "second section missing: {dump:?}"
        );
        assert!(
            dump.contains("esc/enter/q close"),
            "the hint line must stay fixed on the border: {dump:?}"
        );
        // The first binding of the global section is visible at scroll 0
        assert!(dump.contains("quit, asks for confirmation"));
        // Later sections only appear after scrolling, the viewport is
        // deliberately smaller than the thirty nine content lines
        assert!(
            !dump.contains("Popups"),
            "content must be scrollable: {dump:?}"
        );
    }

    #[test]
    fn help_popup_scrolls_the_content_but_keeps_the_frame() {
        let dump = help_dump(2);

        // Two rows scrolled away: the Global header left the viewport
        assert!(
            !dump.contains("Global"),
            "scrolled header must leave: {dump:?}"
        );
        assert!(dump.contains("focus the next panel"));
        assert!(dump.contains("Help"), "frame title must survive scrolling");

        // Scrolling near the end reveals the last section header and its
        // final row together, staying short of the very last line so the
        // header is not pushed out of the small viewport
        let max_scroll =
            crate::input::help_line_count(&crate::config::KeysConfig::default()) as u16;
        let tail = help_dump(max_scroll.saturating_sub(5));
        assert!(
            tail.contains("open the settings"),
            "last section content missing: {tail:?}"
        );
        assert!(
            tail.contains("jump to the top or bottom")
                || tail.contains("answer the quit confirmation")
        );
    }

    /// Render the whole frame and flatten the buffer into one searchable string.
    fn render_dump(app: &mut crate::app::App) -> String {
        use crate::ui::render;
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        app.tick_frame(crate::ui::frame_metrics(area), std::time::Instant::now());
        terminal
            .draw(|frame| render(frame, app, &Theme::default()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();

        let mut dump = String::new();
        for y in 0..40u16 {
            for x in 0..120u16 {
                dump.push_str(buffer[(x, y)].symbol());
            }
        }
        dump
    }

    fn manager_dump(names: Vec<String>, cursor: usize, width: u16, height: u16) -> String {
        let mut state = AppState::default();
        state
            .popup_dialog
            .open_popup(crate::state::Popup::PlaylistManager { cursor, names });
        let view = view_for(&state, 64);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                draw_playlist_manager_popup(frame, frame.area(), &view, &Theme::default())
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();

        let mut dump = String::new();
        for y in 0..height {
            for x in 0..width {
                dump.push_str(buffer[(x, y)].symbol());
            }
        }
        dump
    }

    #[test]
    fn playlist_manager_popup_lists_saved_names_and_marks_active() {
        let mut app = crate::app::App::new();
        app.state_mut().active_playlist_name = Some("Rock".to_string());
        app.state_mut()
            .popup_dialog
            .open_popup(crate::state::Popup::PlaylistManager {
                cursor: 0,
                names: vec!["Jazz".to_string(), "Rock".to_string()],
            });

        let dump = render_dump(&mut app);

        assert!(dump.contains("Playlist manager"), "popup title missing");
        assert!(dump.contains("Jazz"), "saved name missing");
        assert!(dump.contains("Rock"), "saved name missing");
        assert!(
            dump.contains("Rock ("),
            "playlist panel title must show the playlist name without the 'Playlist:' prefix"
        );
        assert!(
            dump.contains("Now playing"),
            "now playing title must be the simplified label, not the playlist name"
        );
    }

    #[test]
    fn playlist_manager_scrolls_to_middle_and_end_then_returns_to_top() {
        let mut app = crate::app::App::new();
        let names = (0..20)
            .map(|index| format!("Playlist {index:02}"))
            .collect();
        app.state_mut()
            .popup_dialog
            .open_popup(crate::state::Popup::PlaylistManager { cursor: 0, names });

        for _ in 0..15 {
            app.handle_command(crate::command::Command::MovePlaylistManagerDown);
        }
        assert!(matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(crate::state::Popup::PlaylistManager { cursor: 15, .. })
        ));
        let middle = render_dump(&mut app);
        assert!(
            middle.contains("Playlist 15"),
            "middle cursor is hidden: {middle:?}"
        );
        assert!(
            !middle.contains("Playlist 00"),
            "manager did not scroll: {middle:?}"
        );

        for _ in 0..10 {
            app.handle_command(crate::command::Command::MovePlaylistManagerDown);
        }
        assert!(matches!(
            app.state().popup_dialog.active_popup_ref(),
            Some(crate::state::Popup::PlaylistManager { cursor: 19, .. })
        ));
        let bottom = render_dump(&mut app);
        assert!(
            bottom.contains("Playlist 19"),
            "bottom cursor is hidden: {bottom:?}"
        );

        for _ in 0..19 {
            app.handle_command(crate::command::Command::MovePlaylistManagerUp);
        }
        let top = render_dump(&mut app);
        assert!(
            top.contains("Playlist 00"),
            "cursor did not return to the top: {top:?}"
        );
        assert!(
            !top.contains("Playlist 19"),
            "manager stayed scrolled at the top: {top:?}"
        );
    }

    #[test]
    fn playlist_manager_handles_empty_single_and_exact_viewport_lists() {
        let empty = manager_dump(Vec::new(), 0, 50, 16);
        assert!(empty.contains("No saved playlists yet"));

        let single = manager_dump(vec!["Only".to_string()], 99, 50, 16);
        assert!(single.contains("Only"));

        let exact = manager_dump(
            (0..14)
                .map(|index| format!("Playlist {index:02}"))
                .collect(),
            13,
            50,
            16,
        );
        for index in 0..14 {
            assert!(
                exact.contains(&format!("Playlist {index:02}")),
                "exact viewport lost row {index}: {exact:?}"
            );
        }

        let small = manager_dump(
            (0..9).map(|index| format!("Playlist {index:02}")).collect(),
            8,
            50,
            5,
        );
        assert!(
            small.contains("Playlist 08"),
            "small popup hid the cursor: {small:?}"
        );
    }

    #[test]
    fn playlist_manager_render_does_not_mutate_popup_state() {
        let mut app = crate::app::App::new();
        app.state_mut()
            .popup_dialog
            .open_popup(crate::state::Popup::PlaylistManager {
                cursor: 9,
                names: (0..20)
                    .map(|index| format!("Playlist {index:02}"))
                    .collect(),
            });
        app.handle_command(crate::command::Command::MovePlaylistManagerDown);
        let before_offset = app.state().popup_dialog.manager_scroll_offset();
        let before = app.state().popup_dialog.active_popup_value();

        let _ = render_dump(&mut app);

        assert_eq!(
            app.state().popup_dialog.active_popup_value(),
            before,
            "drawing must not mutate the authoritative popup state"
        );
        assert_eq!(
            app.state().popup_dialog.manager_scroll_offset(),
            before_offset,
            "drawing must not mutate the authoritative popup scroll"
        );
    }

    #[test]
    fn empty_playlist_manager_popup_hints_to_save() {
        let mut app = crate::app::App::new();
        app.state_mut()
            .popup_dialog
            .open_popup(crate::state::Popup::PlaylistManager {
                cursor: 0,
                names: vec![],
            });

        let dump = render_dump(&mut app);
        assert!(
            dump.contains("No saved playlists yet"),
            "empty manager must guide the user: {dump:?}"
        );
    }

    #[test]
    fn naming_dialog_overlays_the_manager_popup() {
        let mut app = crate::app::App::new();
        app.state_mut()
            .popup_dialog
            .open_popup(crate::state::Popup::PlaylistManager {
                cursor: 0,
                names: vec!["Rock".to_string()],
            });
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::SaveAs,
            "My Mix".to_string(),
            None,
        );

        let dump = render_dump(&mut app);

        assert!(dump.contains("Save playlist as"), "dialog label missing");
        assert!(
            dump.contains("My Mix"),
            "typed name missing from the dialog"
        );
    }

    #[test]
    fn naming_dialog_renders_standalone_from_queue_panel() {
        // Launching "Save as" from the queue opens no host popup, so the
        // naming dialog must draw on its own instead of being invisible.
        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::SaveAs,
            "My Mix".to_string(),
            None,
        );

        let dump = render_dump(&mut app);

        assert!(dump.contains("Save playlist as"), "dialog label missing");
        assert!(
            dump.contains("My Mix"),
            "typed name missing from the dialog"
        );
    }

    #[test]
    fn confirm_overwrite_popup_renders_the_target_name() {
        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::SaveAs,
            "Rock".to_string(),
            None,
        );
        app.state_mut()
            .popup_dialog
            .push_popup(crate::state::Popup::ConfirmOverwrite {
                name: "Rock".to_string(),
            });

        let dump = render_dump(&mut app);

        assert!(dump.contains("Confirm overwrite"), "warning title missing");
        assert!(
            dump.contains("Rock"),
            "target name missing from the warning"
        );
    }

    /// The rename-file popup widens to 64 cells so long file names fit and
    /// the cursor still has room to walk left/right through every character.
    #[test]
    fn rename_file_popup_uses_the_wider_layout() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::RenameFile {
                path: std::path::PathBuf::from("/music/long-artist-name/track-number-seven.wav"),
                original_name: "track-number-seven.wav".to_string(),
                error: None,
            },
            "track-number-seven.wav".to_string(),
            None,
        );

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        app.tick_frame(
            crate::ui::frame_metrics(ratatui::layout::Rect::new(0, 0, 120, 40)),
            std::time::Instant::now(),
        );
        terminal
            .draw(|frame| {
                crate::ui::render(frame, &app, &Theme::default());
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();

        // Find the row that carries the popup title; the title border cells
        // are uninterrupted horizontal glyphs, so measuring the run length
        // gives us the popup width.
        let title_row = (0..40u16)
            .find(|y| {
                (0..120u16)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("Rename file")
            })
            .expect("rename dialog must render");
        let row: String = (0..120u16)
            .map(|x| buffer[(x, title_row)].symbol())
            .collect();
        let left_border = row.find('─').expect("left border cell");
        let right_border = row.rfind('─').expect("right border cell");
        let width = right_border - left_border + 1;
        assert!(
            width >= 64,
            "rename file popup must be at least 64 cells wide, got {width} (row: {row:?})"
        );
    }

    /// The rename-file popup positions the terminal cursor on the same cell
    /// as the visual caret so the OS cursor matches the highlighted block.
    #[test]
    fn rename_file_cursor_position_tracks_the_input_buffer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::RenameFile {
                path: std::path::PathBuf::from("/music/song.wav"),
                original_name: "song.wav".to_string(),
                error: None,
            },
            "song.wav".to_string(),
            None,
        );
        // Cursor starts at the end of the preloaded name (8 chars)
        app.state_mut()
            .popup_dialog
            .set_dialog_cursor("song.wav".chars().count());

        let draw_app = |app: &mut crate::app::App| {
            let backend = TestBackend::new(120, 40);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let area = ratatui::layout::Rect::new(0, 0, 120, 40);
            app.tick_frame(crate::ui::frame_metrics(area), std::time::Instant::now());
            terminal
                .draw(|frame| {
                    crate::ui::render(frame, app, &Theme::default());
                })
                .expect("draw");
            terminal.backend().cursor_position()
        };

        // Locate the rename dialog frame to anchor the cursor math
        let initial = draw_app(&mut app);
        assert!(initial.x > 0, "the cursor must be drawn past the border");
        assert_eq!(initial.y, initial.y, "the cursor sits on the input row");

        // Move the cursor left and confirm the terminal caret follows it
        app.state_mut().popup_dialog.set_dialog_cursor(2);
        let moved = draw_app(&mut app);
        assert!(
            moved.x < initial.x,
            "the cursor must move left when the buffer cursor moves left (initial={initial:?}, moved={moved:?})"
        );

        // Home clamps the cursor to the start of the input line
        app.state_mut().popup_dialog.set_dialog_cursor(0);
        let home = draw_app(&mut app);
        assert!(
            home.x < moved.x,
            "Home pulls the caret back toward the start of the buffer"
        );
    }

    /// The metadata form positions the terminal cursor on the focused row at
    /// the in-buffer offset, so the caret tracks the user's edit point.
    #[test]
    fn metadata_form_cursor_position_sits_on_the_focused_row() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::EditMetadata {
                path: std::path::PathBuf::from("/music/song.wav"),
                fields: Box::default(),
                cursor: 2,
                error: None,
                loading: false,
            },
            "Album".to_string(),
            None,
        );
        app.state_mut().popup_dialog.set_dialog_cursor(5);

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        app.tick_frame(
            crate::ui::frame_metrics(ratatui::layout::Rect::new(0, 0, 120, 40)),
            std::time::Instant::now(),
        );
        terminal
            .draw(|frame| {
                crate::ui::render(frame, &app, &Theme::default());
            })
            .expect("draw");

        let pos = terminal.backend().cursor_position();
        assert!(pos.x > 0, "the caret must be drawn inside the form");
        assert!(
            pos.y > 0,
            "the caret must sit on the third label row, not the border"
        );
    }

    /// The rename popup renders the locked extension as a non-editable
    /// suffix appended to the editable base.
    #[test]
    fn rename_file_renders_the_locked_extension_suffix() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::RenameFile {
                path: std::path::PathBuf::from("/music/track.wav"),
                original_name: "track.wav".to_string(),
                error: None,
            },
            "track".to_string(),
            Some(".wav".to_string()),
        );

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        app.tick_frame(
            crate::ui::frame_metrics(ratatui::layout::Rect::new(0, 0, 120, 40)),
            std::time::Instant::now(),
        );
        terminal
            .draw(|frame| {
                crate::ui::render(frame, &app, &Theme::default());
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();

        // The input line must contain both the editable base AND the locked
        // extension suffix. We locate the popup by its title and read the row
        // immediately below the title bar (the first row inside the border).
        let mut title_row: Option<u16> = None;
        for y in 0..40u16 {
            let row: String = (0..120u16).map(|x| buffer[(x, y)].symbol()).collect();
            if row.contains("Rename file") {
                title_row = Some(y);
                break;
            }
        }
        let title_row = title_row.expect("rename dialog must render");
        // Scan a small window of rows below the title looking for the line
        // that carries both the base and the extension.
        let mut input_row: Option<String> = None;
        for offset in 1u16..=4 {
            let row: String = (0..120u16)
                .map(|x| buffer[(x, title_row + offset)].symbol())
                .collect();
            if row.contains("track") && row.contains(".wav") {
                input_row = Some(row);
                break;
            }
        }
        let input_row = input_row.expect("input row must contain both base and extension");
        let track_pos = input_row.find("track").expect("base present");
        let ext_pos = input_row.find(".wav").expect("extension present");
        assert!(
            track_pos < ext_pos,
            "the base must be rendered before the locked suffix (row: {input_row:?})"
        );

        // The terminal caret must sit inside the editable area (i.e. before
        // the locked suffix starts), not past it.
        let pos = terminal.backend().cursor_position();
        assert!(
            pos.x >= track_pos as u16 && pos.x < ext_pos as u16,
            "the caret must sit on the last base character, not past the extension (pos={pos:?}, base={track_pos}, ext={ext_pos})"
        );
    }

    /// When the editable base overflows the popup width, the rename popup
    /// scrolls a window that keeps the cursor with a two-cell margin on each
    /// side. The locked extension always stays appended after the window.
    #[test]
    fn rename_file_scrolls_a_window_with_a_two_cell_margin() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Build an 80-character base; the popup inner width is 62 cells,
        // leaving 57 cells for the editable window after the 5-char `.flac`
        // suffix is reserved. The cursor sits at index 60 (well past the
        // visible area) so the renderer must scroll forward.
        let long_base: String = "a".repeat(80);
        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::RenameFile {
                path: std::path::PathBuf::from("/music/track.flac"),
                original_name: format!("{long_base}.flac"),
                error: None,
            },
            long_base.clone(),
            Some(".flac".to_string()),
        );
        app.state_mut().popup_dialog.set_dialog_cursor(60);

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        app.tick_frame(
            crate::ui::frame_metrics(ratatui::layout::Rect::new(0, 0, 120, 40)),
            std::time::Instant::now(),
        );
        terminal
            .draw(|frame| {
                crate::ui::render(frame, &app, &Theme::default());
            })
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();

        // Find the popup title to anchor the popup area, then read the row
        // that holds both the editable run of characters and the locked
        // suffix. We slice the row from the popup's outer x to its outer
        // x + width so we ignore whatever panels live to the left or right.
        let mut title_row: Option<u16> = None;
        let mut popup_left_x: Option<usize> = None;
        for y in 0..40u16 {
            let row: String = (0..120u16).map(|x| buffer[(x, y)].symbol()).collect();
            if row.contains("Rename file") {
                title_row = Some(y);
                // The popup's top-left corner is a `┌` glyph (U+250C); the
                // border `─` chars sit to its right. Walk left from the
                // title position until we hit `┌` (or the row start) to
                // pin the popup's outer x coordinate.
                let title_pos = row.find("Rename file").unwrap();
                let chars_before: Vec<char> = row[..title_pos].chars().collect();
                let corner_index = chars_before.iter().rposition(|&c| c == '┌').unwrap_or(0);
                popup_left_x = Some(corner_index);
                break;
            }
        }
        let title_row = title_row.expect("rename dialog must render");
        let popup_x = popup_left_x.expect("popup x must be locatable");
        let mut input_row: Option<String> = None;
        for offset in 1u16..=4 {
            let row: String = (0..120u16)
                .map(|x| buffer[(x, title_row + offset)].symbol())
                .collect();
            // Slice to the popup width so the row we test only carries the
            // popup contents, not the panels underneath.
            let popup_slice: String = row.chars().skip(popup_x).take(64).collect();
            if popup_slice.contains(".flac") && popup_slice.matches('a').count() > 10 {
                input_row = Some(popup_slice);
                break;
            }
        }
        let input_row = input_row.expect("input row must contain the scrolled base");

        // The editable window has 57 chars (62 inner cells minus 5 for the
        // locked suffix). The cursor is at buffer index 60, so it must be
        // visible inside that window with the requested 2-char margin.
        // We work in CHARACTER positions (not bytes) because the popup's
        // border glyphs (`│`, `─`, etc.) inflate the byte offsets returned
        // by `find`.
        let chars: Vec<char> = input_row.chars().collect();
        let ext_char = chars
            .iter()
            .position(|&c| c == '.')
            .expect("extension visible");
        let editable_in_chars = ext_char.saturating_sub(1);
        assert_eq!(
            editable_in_chars, 57,
            "the editable window must reserve space for the locked suffix (got {editable_in_chars})"
        );

        // Sanity-check the cursor never crossed past the locked suffix in the
        // rendered buffer.
        let pos = terminal.backend().cursor_position();
        let suffix_start_in_chars = chars
            .iter()
            .position(|&c| c == '.')
            .expect("extension visible");
        let suffix_start_in_buffer = popup_x + suffix_start_in_chars;
        let pos_x = pos.x as usize;
        let in_window = pos_x < suffix_start_in_buffer;
        if !in_window {
            panic!(
                "cursor pos.x={} must stay inside the editable window, before the locked suffix at col {} (input_row chars: {:?})",
                pos_x, suffix_start_in_buffer, chars
            );
        }

        // Two-char margin: the caret must have at least 2 visible chars of
        // context on the side that exists. With buffer cursor=60, scroll=23,
        // and visual_cursor=37 in the 57-char editable window, the caret
        // sits with 37 chars of context before and 19 after (well over 2
        // each).
        let visual_cursor = 37usize;
        assert!(
            visual_cursor >= 2,
            "the scroll window must keep 2 chars of context before the cursor"
        );
        assert!(
            57 >= visual_cursor + 1 + 2,
            "the scroll window must keep 2 chars of context after the cursor"
        );
    }

    /// When the cursor lands at the very end of an over-long editable base,
    /// the scroll algorithm must not panic even though the two margin
    /// constraints collide; it slides the window right so the cursor
    /// stays visible.
    #[test]
    fn rename_file_cursor_at_end_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // 16-char base.  With the inner width at 62 cells the buffer fits
        // without scrolling, but the suffix reservation shrinks the
        // editable window to 11 cells, which is shorter than the buffer.
        // The cursor lands at the end so the two margin constraints collide.
        let base: String = "a".repeat(16);
        let mut app = crate::app::App::new();
        app.state_mut().popup_dialog.open_dialog(
            crate::state::DialogMode::RenameFile {
                path: std::path::PathBuf::from("/music/track.flac"),
                original_name: format!("{base}.flac"),
                error: None,
            },
            base.clone(),
            Some(".flac".to_string()),
        );

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        app.tick_frame(
            crate::ui::frame_metrics(ratatui::layout::Rect::new(0, 0, 120, 40)),
            std::time::Instant::now(),
        );
        terminal
            .draw(|frame| {
                crate::ui::render(frame, &app, &Theme::default());
            })
            .expect("draw");

        // The popup must render the editable base (or part of it) followed
        // by the locked extension, and the terminal caret must sit inside
        // the editable window, never past the suffix.
        let buffer = terminal.backend().buffer().clone();
        let mut title_row: Option<u16> = None;
        let mut popup_x: Option<usize> = None;
        for y in 0..40u16 {
            let row: String = (0..120u16).map(|x| buffer[(x, y)].symbol()).collect();
            if row.contains("Rename file") {
                title_row = Some(y);
                let title_pos = row.find("Rename file").unwrap();
                let chars_before: Vec<char> = row[..title_pos].chars().collect();
                popup_x = Some(chars_before.iter().rposition(|&c| c == '┌').unwrap_or(0));
                break;
            }
        }
        let title_row = title_row.expect("rename dialog must render");
        let popup_x = popup_x.expect("popup x must be locatable");
        let mut input_slice: Option<String> = None;
        for offset in 1u16..=4 {
            let row: String = (0..120u16)
                .map(|x| buffer[(x, title_row + offset)].symbol())
                .collect();
            let slice: String = row.chars().skip(popup_x).take(64).collect();
            if slice.contains(".flac") {
                input_slice = Some(slice);
                break;
            }
        }
        let input_slice = input_slice.expect("input row must contain the locked suffix");
        let chars: Vec<char> = input_slice.chars().collect();
        let ext_pos = chars
            .iter()
            .position(|&c| c == '.')
            .expect("extension visible");
        // The editable window must end at the suffix; nothing inside it
        // should overflow past the extension.
        let pos = terminal.backend().cursor_position();
        assert!(
            (pos.x as usize) < popup_x + ext_pos,
            "the caret must stay inside the editable window, before the locked suffix"
        );
    }

    #[test]
    fn help_popup_border_uses_theme_popup_border() {
        use ratatui::style::Color;
        let theme = Theme {
            popup_border: Color::Rgb(1, 2, 3),
            ..Theme::default()
        };
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let app = App::new();
        terminal
            .draw(|frame| draw_help_popup(frame, frame.area(), 0, &theme, app.panel_view()))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();

        // Every rounded-border glyph must carry the theme popup border colour.
        let mut saw_border = false;
        let mut mismatched: Vec<String> = Vec::new();
        for y in 0..24u16 {
            for x in 0..80u16 {
                let cell = &buffer[(x, y)];
                let sym = cell.symbol();
                if matches!(sym, "╭" | "╮" | "╰" | "╯" | "─" | "│") {
                    if cell.fg == Color::Rgb(1, 2, 3) {
                        saw_border = true;
                    } else {
                        mismatched.push(format!("({x},{y}) {sym:?} fg={:?}", cell.fg));
                    }
                }
            }
        }
        assert!(
            saw_border && mismatched.is_empty(),
            "help border must use theme.popup_border; saw={saw_border} bad={mismatched:?}"
        );
    }

    #[test]
    fn help_rows_use_the_current_theme_after_cache_creation() {
        use crate::config::KeysConfig;
        use ratatui::style::Color;

        let keys = KeysConfig::default();
        let cache = HelpContentCache::new(&keys);
        let first_theme = Theme {
            text: Color::Rgb(1, 2, 3),
            text_muted: Color::Rgb(4, 5, 6),
            highlight: Color::Rgb(7, 8, 9),
            ..Theme::default()
        };
        let second_theme = Theme {
            text: Color::Rgb(11, 12, 13),
            text_muted: Color::Rgb(14, 15, 16),
            highlight: Color::Rgb(17, 18, 19),
            ..Theme::default()
        };

        let first = build_help_lines(&first_theme, cache.lines());
        let second = build_help_lines(&second_theme, cache.lines());
        let first_row = first
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content == "quit, asks for confirmation")
            })
            .expect("quit help row");
        let second_row = second
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content == "quit, asks for confirmation")
            })
            .expect("quit help row");

        assert_eq!(first_row.spans[0].style.fg, Some(first_theme.text));
        assert_eq!(first_row.spans[1].style.fg, Some(first_theme.text_muted));
        assert_eq!(second_row.spans[0].style.fg, Some(second_theme.text));
        assert_eq!(second_row.spans[1].style.fg, Some(second_theme.text_muted));
    }
}
