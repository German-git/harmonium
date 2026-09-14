//! Character-by-character karaoke timing engine.
//!
//! Pure timing math for the lyrics panel: it decides when every character
//! of every timed line is "reached" during playback. The module has no UI
//! dependency so the interpolation rules stay exhaustively testable, and it
//! never fails: lines with word timing that do not align with their text
//! degrade to the plain line interpolation.

use std::sync::Arc;

use unicode_width::UnicodeWidthChar;

use super::{LyricsDocument, LyricsLine};

/// Gap applied to the last timed line when no next line exists.
pub const DEFAULT_LINE_MS: i64 = 4000;

/// Highlight characters this many milliseconds before their exact time,
/// so the visual front leads the audio by a hair.
///
/// Tunable: bump or lower the value until the read feels right.
pub const ANTICIPATION_MS: i64 = 400;

/// Per-character reach times for a line, aligned with `line.text` chars.
///
/// `None` when the line is untimed (plain text). The last char always
/// reaches exactly `end`, where `end` is the start of the next timed line,
/// or `start + DEFAULT_LINE_MS` when there is none. When the next line
/// starts before the current one the span collapses to zero so all chars
/// reach at the same instant, which reads as an immediate flip instead of
/// a backwards clock.
pub fn line_char_times(line: &LyricsLine, next_line_start: Option<i64>) -> Option<Vec<i64>> {
    let start = line.timestamp_ms?;
    let count = line.text.chars().count();
    if count == 0 {
        return Some(Vec::new());
    }
    let end = resolve_end(start, next_line_start);
    if line.words.is_empty() {
        return Some(line_interpolation(start, end, count));
    }
    // Word timing wins while it survives validation; any mismatch with the
    // body text degrades the whole line to the plain interpolation so the
    // panel never flips haphazardly on odd files.
    word_char_times(line, start, end, count)
        .filter(|times| times.len() == count)
        .or_else(|| Some(line_interpolation(start, end, count)))
}

/// Count of leading characters already reached at `elapsed_ms`.
///
/// Times are monotonic within a line, so the reached prefix is the first
/// run of chars whose reach time is `<= elapsed_ms`. Returns `None` for
/// untimed lines, mirroring [`line_char_times`].
pub fn reached_char_count(
    line: &LyricsLine,
    elapsed_ms: i64,
    next_line_start: Option<i64>,
) -> Option<usize> {
    let times = line_char_times(line, next_line_start)?;
    Some(times.iter().take_while(|&&time| time <= elapsed_ms).count())
}

/// Elapsed clock shifted forward by [`ANTICIPATION_MS`], the single source
/// of truth for "when is it sung".
///
/// The row painter and the active line follow both compare this value
/// against the real reach times, so one constant governs the whole lead.
pub fn effective_elapsed(elapsed_ms: i64) -> i64 {
    elapsed_ms + ANTICIPATION_MS
}

/// Direction of the follow move, used to pick the anchor margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowDirection {
    /// Playback advances: keep two rows visible below the active line.
    Down,
    /// Playback jumped backwards: keep two rows visible above the active.
    Up,
}

/// Index of the line being sung: the last timed line whose start is not
/// after the effective elapsed time; the first timed line when playback
/// has not reached the first one yet; `None` for untimed documents.
///
/// The effective time uses [`effective_elapsed`] so the follow trips at
/// the same instant the highlight does.
pub fn active_line_index(document: &LyricsDocument, elapsed_ms: i64) -> Option<usize> {
    let effective = effective_elapsed(elapsed_ms);
    let mut first_timed: Option<usize> = None;
    let mut active: Option<usize> = None;
    for (index, line) in document.lines.iter().enumerate() {
        if let Some(start) = line.timestamp_ms {
            if first_timed.is_none() {
                first_timed = Some(index);
            }
            if start <= effective {
                active = Some(index);
            }
        }
    }
    active.or(first_timed)
}

/// Cached equivalent of [`active_line_index`]. The timed starts are monotonic
/// and already derived during layout construction, so each frame only performs
/// a logarithmic partition lookup.
pub fn active_line_index_from_starts(
    timed_starts: &[(usize, i64)],
    line_count: usize,
    elapsed_ms: i64,
) -> Option<usize> {
    let effective = effective_elapsed(elapsed_ms);
    let first = timed_starts.first().map(|(index, _)| *index)?;
    let position = timed_starts.partition_point(|(_, start)| *start <= effective);
    timed_starts
        .get(position.saturating_sub(1))
        .map(|(index, _)| *index)
        .or((first < line_count).then_some(first))
}

/// Scroll offset that keeps the active line on screen with the direction
/// aware margin: two rows below it when following down, two rows above it
/// when following up (up to `total - viewport`).
pub fn follow_scroll(
    active: usize,
    viewport: usize,
    total: usize,
    direction: FollowDirection,
) -> usize {
    if viewport == 0 {
        return 0;
    }
    let anchor_margin = match direction {
        FollowDirection::Down => viewport.saturating_sub(3),
        FollowDirection::Up => 2,
    };
    active
        .saturating_sub(anchor_margin)
        .min(total.saturating_sub(viewport))
}

/// Physical-row layout of the document at a given display width: how many
/// rows each line needs and where each one starts, so follow logic can
/// operate in physical rows instead of document lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentLayout {
    /// Physical rows produced for each document line.
    pub rows_per_line: Arc<Vec<usize>>,
    /// Cumulative physical rows before each document line.
    pub row_starts: Arc<Vec<usize>>,
    /// Total physical rows.
    pub total_rows: usize,
}

/// Layout for the document at `width` display columns.
///
/// Long lines become multiple physical rows, so the follow offset (which
/// moves in terminal rows) reads the real row count instead of naively
/// assuming one row per document line.
pub fn layout_document(lines: &[LyricsLine], width: usize) -> DocumentLayout {
    let mut rows_per_line = Vec::with_capacity(lines.len());
    let mut total_rows = 0usize;
    for line in lines {
        let rows = wrap_rows_for(&line.text, width).len();
        rows_per_line.push(rows);
        total_rows += rows;
    }
    let mut row_starts = Vec::with_capacity(lines.len());
    let mut running = 0usize;
    for &rows in &rows_per_line {
        row_starts.push(running);
        running += rows;
    }
    DocumentLayout {
        rows_per_line: Arc::new(rows_per_line),
        row_starts: Arc::new(row_starts),
        total_rows,
    }
}

/// Timestamp of the next timed line for every document line.
///
/// Untimed lines inherit the next timed line's start, while the final timed
/// line and fully untimed documents produce `None`.
pub fn next_line_starts(document: &LyricsDocument) -> Vec<Option<i64>> {
    let mut out = vec![None; document.lines.len()];
    let mut next: Option<i64> = None;
    for index in (0..document.lines.len()).rev() {
        out[index] = next;
        if let Some(ms) = document.lines[index].timestamp_ms {
            next = Some(ms);
        }
    }
    out
}

/// Char-boundary ranges of the physical rows `text` needs at `width`.
///
/// Each range is a half-open pair of char indexes into `text`, so callers
/// can safely splice the text and colour each chunk without splitting a
/// Unicode char. A text that does not fit width at all still yields at
/// least one row, and empty text yields exactly one empty row so the
/// vertical rhythm of untimed rows is preserved.
pub fn wrap_rows_for(text: &str, width: usize) -> Vec<(usize, usize)> {
    let width = width.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![(0, 0)];
    }

    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut row_start = 0usize;
    let mut col = 0usize;
    let mut index = 0usize;
    while index < chars.len() {
        let char_width = chars[index].width().unwrap_or(0);
        // A char wider than the whole row always owns its own row, so tight
        // terminals never produce an infinite loop on a single glyph.
        if char_width > width {
            if col > 0 {
                rows.push((row_start, index));
            }
            rows.push((index, index + 1));
            row_start = index + 1;
            col = 0;
            index += 1;
            continue;
        }
        if col > 0 && char_width > 0 && col + char_width > width {
            rows.push((row_start, index));
            row_start = index;
            col = 0;
        }
        col += char_width;
        index += 1;
    }
    if index > row_start {
        rows.push((row_start, index));
    }
    rows
}

/// Split `text` at `n` chars, respecting Unicode char boundaries.
///
/// `n == 0` yields an empty left half, `n` beyond the length returns the
/// whole text on the left, mirroring how the panel treats fully reached
/// rows without needing a special case.
pub fn split_at_chars(text: &str, n: usize) -> (&str, &str) {
    let boundary = text
        .char_indices()
        .nth(n)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.split_at(boundary)
}

/// End of the line window: the next line start when it is after the
/// beginning of this one, otherwise the default gap.
///
/// `max` against `start` makes an inverted order collapse to a zero span.
fn resolve_end(start: i64, next_line_start: Option<i64>) -> i64 {
    next_line_start
        .unwrap_or(start + DEFAULT_LINE_MS)
        .max(start)
}

/// Uniform reach times for a line with `count` chars spanning `start..end`.
///
/// Char `i` (0-based) reaches at `start + span * (i + 1) / count`, so the
/// first char never reaches before `start` and the last one lands exactly
/// on `end`.
fn line_interpolation(start: i64, end: i64, count: usize) -> Vec<i64> {
    let span = end - start;
    (0..count)
        .map(|index| start + span * (index as i64 + 1) / count as i64)
        .collect()
}

/// Clamp `value` into `low..=high`, tolerating a zero-length window.
fn clamp(value: i64, low: i64, high: i64) -> i64 {
    value.clamp(low, high)
}

/// Reach times from word timing, or `None` when the words do not match the
/// body text in order (degrade to the whole-line interpolation).
///
/// The text is cut into segments in file order: the prefix before the
/// first word, each word, the separator after it, and the trailing text.
/// A word spans `wstart..wend`, where `wstart` clamps its own tag into the
/// line window and `wend` clamps the next tag after it (or the line end
/// for the last one); prefixes, separators and trailing text inherit the
/// time of the segment they follow.
fn word_char_times(line: &LyricsLine, start: i64, end: i64, count: usize) -> Option<Vec<i64>> {
    let words = &line.words;
    if words.is_empty() {
        return None;
    }

    // Locate every word sequentially in the body as char ranges, gluing the
    // word list to the exact text the parser stripped the tags from.
    let mut search_from = 0usize;
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(words.len());
    for word in words {
        if word.text.is_empty() {
            return None;
        }
        let rel = line.text[search_from..].find(&word.text)?;
        let start_byte = search_from + rel;
        let end_byte = start_byte + word.text.len();
        let start_char = line.text[..start_byte].chars().count();
        let end_char = line.text[..end_byte].chars().count();
        if end_char > count {
            return None;
        }
        spans.push((start_char, end_char));
        search_from = end_byte;
    }

    // Resolve the window of every word before emitting chars so each
    // segment can read both its own bounds and the following tag.
    let mut wstarts: Vec<i64> = Vec::with_capacity(words.len());
    let mut wends: Vec<i64> = Vec::with_capacity(words.len());
    for (index, word) in words.iter().enumerate() {
        let wstart = clamp(word.start_ms, start, end);
        let wend = match words.get(index + 1) {
            Some(next) => clamp(next.start_ms, wstart, end),
            None => end,
        };
        wstarts.push(wstart);
        wends.push(wend);
    }

    let mut times: Vec<i64> = Vec::with_capacity(count);
    // The prefix before the first word stays at the line start.
    if let Some((first_char_start, _)) = spans.first()
        && *first_char_start > 0
    {
        times.extend(std::iter::repeat_n(start, *first_char_start));
    }
    for (span_index, &(char_start, char_end)) in spans.iter().enumerate() {
        let span_len = char_end - char_start;
        let wstart = wstarts[span_index];
        let wend = wends[span_index];
        for local in 0..span_len {
            let raw = wstart + (wend - wstart) * (local as i64 + 1) / span_len as i64;
            times.push(raw);
        }
        let next_start = spans.get(span_index + 1).map_or(count, |next| next.0);
        for _ in char_end..next_start {
            times.push(wend);
        }
    }
    // Out-of-order word timestamps would make the clock run backwards on
    // that line: refuse the timing and let the caller fall back.
    if times.windows(2).all(|pair| pair[0] <= pair[1]) {
        Some(times)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::{LyricsDocument, LyricsLine, LyricsWord};

    fn line(timestamp_ms: Option<i64>, text: &str, words: Vec<LyricsWord>) -> LyricsLine {
        LyricsLine {
            timestamp_ms,
            text: text.to_string(),
            words,
        }
    }

    #[test]
    fn plain_line_interpolates_uniformly_and_lands_on_the_next_line() {
        let line = line(Some(10000), "hola", Vec::new());

        let times = line_char_times(&line, Some(14000)).expect("timed line reports times");

        assert_eq!(times, vec![11000, 12000, 13000, 14000]);
    }

    #[test]
    fn elapsed_before_start_reaches_nothing() {
        let line = line(Some(10000), "hola", Vec::new());

        assert_eq!(
            reached_char_count(&line, 9999, Some(14000)),
            Some(0),
            "an instant before the line must reach zero chars"
        );
        assert_eq!(
            reached_char_count(&line, 10000, Some(14000)),
            Some(0),
            "the first char reaches strictly after the line start"
        );
    }

    #[test]
    fn elapsed_at_the_last_char_reaches_everything() {
        let line = line(Some(10000), "hola", Vec::new());

        assert_eq!(reached_char_count(&line, 14000, Some(14000)), Some(4));
        assert_eq!(reached_char_count(&line, 50000, Some(14000)), Some(4));
    }

    #[test]
    fn word_timing_uses_the_word_windows_and_separators_inherit_the_previous_end() {
        let line = line(
            Some(10000),
            "la casa",
            vec![
                LyricsWord {
                    start_ms: 11000,
                    text: "la".to_string(),
                },
                LyricsWord {
                    start_ms: 15000,
                    text: "casa".to_string(),
                },
            ],
        );

        let times = line_char_times(&line, Some(20000)).expect("worded line reports times");

        assert_eq!(
            times,
            vec![
                13000, // "l" of "la": 11000 + half the word window
                15000, // "a" of "la", the word window end
                15000, // separator " " inherits the previous word end
                16250, // "c" of "casa"
                17500, // "a"
                18750, // "s"
                20000, // final "a" lands on the line window end
            ]
        );
    }

    #[test]
    fn words_that_do_not_match_the_body_degrade_to_line_interpolation() {
        let line = line(
            Some(10000),
            "la casa",
            vec![
                LyricsWord {
                    start_ms: 11000,
                    text: "la".to_string(),
                },
                LyricsWord {
                    start_ms: 13000,
                    text: "azul".to_string(),
                },
            ],
        );

        let times = line_char_times(&line, Some(20000)).expect("degraded line still reports times");

        assert_eq!(times, line_interpolation(10000, 20000, 7));
        assert_eq!(times.len(), 7, "the shape must match the whole text");
    }

    #[test]
    fn inverted_next_line_start_collapses_to_a_zero_span() {
        let line = line(Some(10000), "hola", Vec::new());

        let times =
            line_char_times(&line, Some(5000)).expect("stale next line still times the row");

        assert_eq!(times, vec![10000, 10000, 10000, 10000]);
        assert_eq!(reached_char_count(&line, 10000, Some(5000)), Some(4));
    }

    #[test]
    fn untimed_lines_report_no_timing() {
        let line = line(None, "intro", Vec::new());

        assert_eq!(line_char_times(&line, Some(9000)), None);
        assert_eq!(reached_char_count(&line, 5000, None), None);
    }

    #[test]
    fn empty_text_reports_an_empty_timing() {
        let line = line(Some(10000), "", Vec::new());

        assert_eq!(line_char_times(&line, Some(14000)), Some(vec![]));
        assert_eq!(reached_char_count(&line, 20000, Some(14000)), Some(0));
    }

    #[test]
    fn split_at_chars_respects_unicode_boundaries() {
        assert_eq!(split_at_chars("áéí", 1), ("á", "éí"));
        assert_eq!(split_at_chars("la casa", 0), ("", "la casa"));
        assert_eq!(split_at_chars("la casa", 99), ("la casa", ""));
    }

    #[test]
    fn anticipation_reaches_the_first_char_before_its_exact_time() {
        let line = line(Some(10000), "hola", Vec::new());

        // Plain interpolation lands the first char at start + span/count,
        // 11000 here. The UI feeds effective_elapsed into the reached
        // count, so the boundary sits 200 ms before 11000 and an instant
        // earlier still reads as untouched.
        assert_eq!(
            reached_char_count(
                &line,
                effective_elapsed(11000 - ANTICIPATION_MS),
                Some(14000)
            ),
            Some(1),
            "the first char must highlight ANTICIPATION_MS before its time"
        );
        assert_eq!(
            reached_char_count(
                &line,
                effective_elapsed(11000 - ANTICIPATION_MS - 1),
                Some(14000)
            ),
            Some(0),
            "one earlier than the boundary must still be untouched"
        );
        assert_eq!(
            effective_elapsed(0),
            ANTICIPATION_MS,
            "the shift is exactly the anticipation constant"
        );
    }

    #[test]
    fn follow_scroll_down_keeps_two_rows_below_the_active_line() {
        // Window 2..=11 with the active line in row 9: it sits at slot 7,
        // leaving rows 10 and 11 (two lines) visible beneath it.
        assert_eq!(follow_scroll(9, 10, 100, FollowDirection::Down), 2);
    }

    #[test]
    fn follow_scroll_up_keeps_two_rows_above_the_active_line() {
        // Window 48..=57: the active line sits at slot 2 with rows 48 and
        // 49 visible above it.
        assert_eq!(follow_scroll(50, 10, 100, FollowDirection::Up), 48);
    }

    #[test]
    fn follow_scroll_clamps_to_the_document_edges() {
        assert_eq!(follow_scroll(2, 10, 100, FollowDirection::Down), 0);
        assert_eq!(follow_scroll(99, 10, 100, FollowDirection::Down), 90);
        assert_eq!(follow_scroll(0, 0, 100, FollowDirection::Down), 0);
        assert_eq!(
            follow_scroll(5, 0, 100, FollowDirection::Up),
            0,
            "a zero viewport never scrolls"
        );
    }

    #[test]
    fn active_line_trips_with_the_anticipation_and_skips_untimed_rows() {
        let document = LyricsDocument {
            lines: vec![
                line(Some(10000), "intro", Vec::new()),
                line(None, "verse", Vec::new()),
                line(Some(14000), "chorus", Vec::new()),
            ],
            text: "intro\nverse\nchorus".to_string(),
        };

        assert_eq!(
            active_line_index(&document, 10000 - ANTICIPATION_MS),
            Some(0),
            "the first line becomes active ANTICIPATION_MS before its start"
        );
        assert_eq!(
            active_line_index(&document, 10000 - ANTICIPATION_MS - 1),
            Some(0),
            "before the first start the first timed line is still the focus"
        );
        assert_eq!(
            active_line_index(&document, 12000),
            Some(0),
            "between lines the last started one stays active"
        );
        assert_eq!(
            active_line_index(&document, 14000 - ANTICIPATION_MS),
            Some(2),
            "the next timed line trips exactly at the anticipation boundary"
        );
        assert_eq!(
            active_line_index(&document, 14000 - ANTICIPATION_MS - 1),
            Some(0),
            "one instant earlier the previous timed line keeps the focus"
        );
    }

    #[test]
    fn active_line_is_none_for_untimed_or_empty_documents() {
        let plain = LyricsDocument::from_plain("piano\nintro");
        assert_eq!(active_line_index(&plain, 5000), None);

        let empty = LyricsDocument {
            lines: vec![],
            text: String::new(),
        };
        assert_eq!(active_line_index(&empty, 5000), None);
    }

    #[test]
    fn last_line_uses_the_default_gap() {
        let line = line(Some(10000), "hola", Vec::new());

        let times = line_char_times(&line, None).expect("timed line reports times");

        assert_eq!(
            times,
            vec![11000, 12000, 13000, 14000],
            "the last line spans exactly DEFAULT_LINE_MS"
        );
        assert_eq!(times[3] - line.timestamp_ms.unwrap_or(0), DEFAULT_LINE_MS);
    }

    #[test]
    fn short_line_wraps_to_one_row() {
        let rows = wrap_rows_for("hola", 20);
        assert_eq!(rows, vec![(0, 4)]);
    }

    #[test]
    fn wrap_happens_exactly_at_the_width_boundary() {
        // 10 columns of width: the first 10 chars fit exactly, the 11th
        // starts the next physical row.
        let rows = wrap_rows_for("abcdefghijk", 10);
        assert_eq!(rows, vec![(0, 10), (10, 11)]);
    }

    #[test]
    fn long_line_chunks_rebuild_the_original_text() {
        let text = "I needed you desperately (da-da-da, da-da-da, da-da-da)";
        let rows = wrap_rows_for(text, 12);
        assert!(rows.len() >= 2, "a long line must wrap into several rows");
        let rebuilt: String = rows
            .iter()
            .map(|&(start, end)| {
                text.chars()
                    .skip(start)
                    .take(end - start)
                    .collect::<String>()
            })
            .collect();
        assert_eq!(
            rebuilt, text,
            "concatenating every chunk must restore the text"
        );
    }

    #[test]
    fn empty_text_wraps_to_one_empty_row() {
        assert_eq!(wrap_rows_for("", 10), vec![(0, 0)]);
    }

    #[test]
    fn char_wider_than_width_occupies_its_own_row() {
        let rows = wrap_rows_for("你好", 1);
        assert_eq!(rows, vec![(0, 1), (1, 2)], "each wide glyph owns a row");
    }

    #[test]
    fn layout_document_tracks_physical_rows_with_mixed_lines() {
        let document = LyricsDocument {
            lines: vec![
                line(Some(100), "hola", Vec::new()),
                line(Some(200), "abcdefghijk", Vec::new()),
                line(None, "", Vec::new()),
            ],
            text: String::new(),
        };

        let layout = layout_document(&document.lines, 5);

        assert_eq!(layout.rows_per_line.as_ref(), &[1, 3, 1]);
        assert_eq!(layout.row_starts.as_ref(), &[0, 1, 4]);
        assert_eq!(layout.total_rows, 5);
    }
}
