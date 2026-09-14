//! LRC lyrics parsing and formatting.
//!
//! The parser accepts the standard `[mm:ss.xx]` and `[mm:ss.xxx]` timestamp
//! shapes, supports the repeated timestamp convention (one lyric text applied
//! to every timestamp listed on the line) and tolerates malformed lines by
//! skipping them instead of failing the whole document. Enhanced LRC word
//! tags (`<mm:ss.xx>` inside a line) are stripped from the body while their
//! per-word times are preserved on the [`LyricsWord`] column of each line.

use std::collections::HashMap;

use super::{LyricsDocument, LyricsLine, LyricsWord};

/// Result of parsing one LRC document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LrcParsed {
    /// Body lines in file order, timestamps applied from the tags.
    pub lines: Vec<LyricsLine>,
    /// Metadata tags such as `[ti:]`, `[ar:]`, `[al:]` or `[offset:]`.
    ///
    /// Keys are lowercased and trimmed; unknown keys are preserved so custom
    /// tag vocabulary never gets silently discarded.
    pub meta: HashMap<String, String>,
}

impl LrcParsed {
    /// Convert the parsed rows into a full [`LyricsDocument`].
    ///
    /// The document text is the plain body with every timestamp removed, so
    /// consumers that only render text can ignore the tags entirely.
    pub fn into_document(self) -> LyricsDocument {
        let text = self
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        LyricsDocument {
            lines: self.lines,
            text,
        }
    }
}

/// Parse LRC content without ever failing.
///
/// Timestamp tags drive the line timing, metadata tags fill the `meta` map,
/// and any line that does not parse as either becomes an untimed body line.
/// Lines that start an unclosed bracket are skipped as malformed.
pub fn parse_lrc(content: &str) -> LrcParsed {
    let mut parsed = LrcParsed::default();
    // The `[offset:±ms]` tag shifts every timestamp that follows it. The
    // offset may appear with a sign and must apply to the lines after it.
    let mut offset_ms: i64 = 0;

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            parsed.lines.push(LyricsLine {
                timestamp_ms: None,
                text: String::new(),
                words: Vec::new(),
            });
            continue;
        }
        if !line.starts_with('[') {
            parsed.lines.push(LyricsLine {
                timestamp_ms: None,
                text: line.to_string(),
                words: Vec::new(),
            });
            continue;
        }
        // An unclosed bracket opener is malformed: dropping the line keeps
        // the rest of the document readable instead of corrupting the body.
        if line.find(']').is_none() {
            continue;
        }

        let (line_times, meta, rest) = split_leading_tags(line);

        for (key, value) in &meta {
            if key == "offset"
                && let Ok(offset) = value.parse::<i64>()
            {
                // Clamp so a malicious `/ huge` offset cannot overflow the
                // per-line additions below.
                offset_ms = offset.clamp(-MAX_TIMESTAMP_MS, MAX_TIMESTAMP_MS);
            }
            parsed.meta.insert(key.clone(), value.clone());
        }

        if !line_times.is_empty() {
            let (body, words) = split_word_tags(rest, offset_ms);
            for resolved_ms in line_times {
                parsed.lines.push(LyricsLine {
                    timestamp_ms: Some(
                        resolved_ms
                            .saturating_add(offset_ms)
                            .clamp(0, MAX_TIMESTAMP_MS),
                    ),
                    text: body.clone(),
                    words: words.clone(),
                });
            }
        } else if !rest.is_empty() {
            // Tags that are neither timestamps nor metadata leave the
            // remainder as a plain line, so headings like "[Verse 1]" in
            // plain lyrics keep their text.
            parsed.lines.push(LyricsLine {
                timestamp_ms: None,
                text: rest.to_string(),
                words: Vec::new(),
            });
        }
    }

    parsed
}

/// Split the successive `[..]` tags at the start of `line`.
///
/// Returns the resolved timestamps, the metadata map and the remaining body
/// text. A tag that is neither a timestamp nor a `key:value` pair stops the
/// scan: it is not part of the tag block, the whole line stays as body text
/// (so `[Verse 1]` works in plain lyrics).
fn split_leading_tags(line: &str) -> (Vec<i64>, Vec<(String, String)>, &str) {
    let mut timestamps = Vec::new();
    let mut meta = Vec::new();
    let mut byte_pos = 0usize;

    while line[byte_pos..].starts_with('[') {
        let Some(rel_end) = line[byte_pos..].find(']') else {
            break;
        };
        let mut end = byte_pos + rel_end;
        let content = &line[byte_pos + 1..end].trim();
        if content.is_empty() {
            break;
        }
        match parse_time_stamp(content) {
            Some(ms) => timestamps.push(ms),
            None => {
                // A `digits:digits[.digits]` shape that failed to parse (for
                // example `[00:99.00]`: minutes are fine but seconds exceed 59)
                // is a malformed timestamp, not a metadata tag. Drop the tag
                // and keep the rest of the line as plain body text, so the
                // karaoke shows it as an untimed, always-visible line instead
                // of swallowing it as metadata.
                if looks_like_timestamp(content) {
                    byte_pos = end + 1;
                    continue;
                }
                let Some((key, value)) = content.split_once(':') else {
                    // Not a tag: keep the whole line as body text.
                    break;
                };
                let key = key.trim().to_lowercase();
                if key.is_empty() {
                    break;
                }
                let mut value = value.trim().to_string();
                // Values sometimes carry bracketed suffixes such as
                // "[Official Audio]". The first `]` then closes the inner
                // bracket and leaves a stray `]` behind, so when the
                // leftover has a closing bracket but no new tag opener the
                // real tag closes at the last `]` of the line.
                let leftover = &line[end + 1..];
                if leftover.contains(']')
                    && !leftover.contains('[')
                    && end + 1 < line.len()
                    && let Some(last) = line.rfind(']')
                    && last > byte_pos + 1
                    && let Some((_, full_value)) = line[byte_pos + 1..last].split_once(':')
                {
                    value = full_value.trim().to_string();
                    end = last;
                }
                meta.push((key, value));
            }
        }
        byte_pos = end + 1;
    }

    (timestamps, meta, &line[byte_pos..])
}

/// Whether `content` has the syntactic shape of an LRC timestamp
/// (`mm:ss` or `mm:ss.cc`, digits only) regardless of its range validity.
///
/// Used to tell a malformed timestamp apart from a `key:value` metadata tag:
/// `[00:99.00]` looks like a time but is out of range, while `[author:me]`
/// is genuine metadata. Only the former is treated as body text.
fn looks_like_timestamp(content: &str) -> bool {
    let Some((minutes, rest)) = content.split_once(':') else {
        return false;
    };
    if minutes.is_empty() || !minutes.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let (seconds, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    if seconds.is_empty() || !seconds.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    fraction.is_empty() || fraction.chars().all(|c| c.is_ascii_digit())
}

/// Highest timestamp a lyric document may carry: 24 hours.
///
/// LRC timestamps describe positions inside a song, so nothing legitimate
/// approaches this bound. It exists to stop a malformed tag such as
/// `[99999999999999999:01.00]` from overflowing `i64` — which would panic in
/// debug builds and wrap silently in release, corrupting the karaoke clock.
const MAX_TIMESTAMP_MS: i64 = 24 * 60 * 60 * 1_000;

/// Parse one `mm:ss.cc` style timestamp into milliseconds.
///
/// The fractional part accepts 1-3 digits (centiseconds or milliseconds),
/// longer tails are truncated. `None` when the shape is not a valid time.
fn parse_time_stamp(content: &str) -> Option<i64> {
    let (minutes, seconds) = content.split_once(':')?;
    let minutes: i64 = minutes.trim().parse().ok()?;
    let (seconds, fraction) = match seconds.split_once('.') {
        Some((head, tail)) => (head, Some(tail)),
        None => (seconds, None),
    };
    let seconds: i64 = seconds.trim().parse().ok()?;
    if !(0..60).contains(&seconds) {
        return None;
    }
    let fraction_ms = match fraction {
        Some(digits) if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) => {
            let digits: Vec<char> = digits.chars().take(3).collect();
            let parsed: i64 = digits.iter().collect::<String>().parse().ok()?;
            // Scale "5" -> 500ms, "50" -> 500ms, "500" -> 500ms.
            parsed * (10_i64.pow(3 - digits.len() as u32))
        }
        _ => 0,
    };
    // Clamp the minute field before the multiplication so a huge value cannot
    // overflow the arithmetic; the result is further clamped into the sane
    // document range so downstream consumers never see a negative wrap.
    let minutes = minutes.clamp(0, MAX_TIMESTAMP_MS / 60_000);
    Some((minutes * 60_000 + seconds * 1_000 + fraction_ms).clamp(0, MAX_TIMESTAMP_MS))
}

/// Format one timestamp as `mm:ss.cc`, the canonical LRC shape without the
/// surrounding brackets, used inside word tags.
fn format_time_stamp_content(ms: i64) -> String {
    let ms = ms.max(0);
    let minutes = ms / 60_000;
    let seconds = (ms % 60_000) / 1_000;
    let centis = (ms % 1_000) / 10;
    format!("{minutes:02}:{seconds:02}.{centis:02}")
}

/// Format one timestamp as `[mm:ss.cc]`, the canonical LRC shape.
fn format_time_stamp(ms: i64) -> String {
    format!("[{}]", format_time_stamp_content(ms))
}

/// Split Enhanced LRC word tags (`<mm:ss.cc>`) off a line body.
///
/// Returns the body with the tags removed and the per-word list: each word
/// takes the text between the closing `>` of its tag and the next `<` (or
/// the end of the line), trimmed, and its own start time shifted by the
/// document offset. Only spans that parse as a timestamp are treated as
/// tags: any other `'<'` content is kept untouched so angle brackets in
/// plain text survive. Empty words are dropped, so a line whose tags never
/// parse produces an empty word list.
fn split_word_tags(text: &str, offset_ms: i64) -> (String, Vec<LyricsWord>) {
    let mut body = String::with_capacity(text.len());
    let mut words = Vec::new();
    let mut body_start = 0usize;
    let mut position = 0usize;

    while position < text.len() {
        if text.as_bytes()[position] == b'<'
            && let Some(rel_end) = text[position..].find('>')
        {
            let end = position + rel_end;
            if let Some(tag_ms) = parse_time_stamp(&text[position + 1..end]) {
                body.push_str(&text[body_start..position]);
                let next_lt = text[end + 1..]
                    .find('<')
                    .map(|rel| end + 1 + rel)
                    .unwrap_or(text.len());
                let word_text = text[end + 1..next_lt].trim();
                if !word_text.is_empty() {
                    words.push(LyricsWord {
                        start_ms: tag_ms.saturating_add(offset_ms).clamp(0, MAX_TIMESTAMP_MS),
                        text: word_text.to_string(),
                    });
                }
                body_start = end + 1;
                position = end + 1;
                continue;
            }
        }
        position += 1;
    }
    body.push_str(&text[body_start..]);

    // The metadata-style spacing after the line timestamp is not part of
    // the story and would break char alignment with the stored words.
    let body = if words.is_empty() {
        body
    } else {
        body.trim().to_string()
    };
    (body, words)
}

/// Serialize a document into LRC text.
///
/// `[ti:]`/`[ar:]` headers are written when the metadata is known, timed
/// lines keep their `[mm:ss.xx]` prefix and untimed lines stay plain. Lines
/// whose word timing is known are written as Enhanced LRC (`<mm:ss.cc>` tags
/// in front of each word), so persisting a remote document keeps the word
/// granularity instead of the line-level fallback. Lines whose words do not
/// align with their body text degrade to the plain timestamped row,
/// mirroring the timing engine.
pub fn format_lrc(document: &LyricsDocument, title: Option<&str>, artist: Option<&str>) -> String {
    let mut out = String::new();

    if let Some(value) = title.map(str::trim).filter(|v| !v.is_empty()) {
        out.push_str(&format!("[ti:{value}]\n"));
    }
    if let Some(value) = artist.map(str::trim).filter(|v| !v.is_empty()) {
        out.push_str(&format!("[ar:{value}]\n"));
    }

    for line in &document.lines {
        match line.timestamp_ms {
            Some(ms) if !line.words.is_empty() => {
                out.push_str(&format_time_stamp(ms));
                // The enhanced body is rebuilt from the stored words so a
                // save/parse roundtrip restores both words and their times.
                out.push_str(
                    &worded_body(line).unwrap_or_else(|| line.text.trim_end().to_string()),
                );
                out.push('\n');
            }
            Some(ms) => {
                out.push_str(&format_time_stamp(ms));
                out.push_str(line.text.trim_end());
                out.push('\n');
            }
            None => {
                out.push_str(line.text.trim_end());
                out.push('\n');
            }
        }
    }
    out
}

/// Rebuild the Enhanced LRC body of one line.
///
/// The words are located sequentially in the body text so the text between
/// them (prefixes and separators) is preserved untouched, and each word is
/// prefixed with its own `<mm:ss.cc>` tag. `None` when a word does not show
/// up in order, letting the caller degrade to the plain line.
fn worded_body(line: &LyricsLine) -> Option<String> {
    let mut out = String::with_capacity(line.text.len() + line.words.len() * 12);
    let mut search_from = 0usize;
    for word in &line.words {
        if word.text.is_empty() {
            return None;
        }
        let rel = line.text.get(search_from..)?.find(&word.text)?;
        let word_start = search_from + rel;
        out.push_str(&line.text[search_from..word_start]);
        out.push('<');
        out.push_str(&format_time_stamp_content(word.start_ms));
        out.push('>');
        out.push_str(&word.text);
        search_from = word_start + word.text.len();
    }
    out.push_str(&line.text[search_from..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn bounded_text() -> impl Strategy<Value = String> {
        prop::collection::vec(any::<char>(), 0..=256)
            .prop_map(|characters| characters.into_iter().collect())
    }

    #[test]
    fn parses_standard_timed_lines() {
        let input = "[00:01.50]First line\n[01:02.00]Second line\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(
            parsed.lines[0],
            LyricsLine {
                timestamp_ms: Some(1500),
                text: "First line".to_string(),
                words: Vec::new(),
            }
        );
        assert_eq!(
            parsed.lines[1],
            LyricsLine {
                timestamp_ms: Some(62000),
                text: "Second line".to_string(),
                words: Vec::new(),
            }
        );
        assert!(parsed.meta.is_empty());
    }

    #[test]
    fn parses_stripped_timestamp_shape() {
        let parsed = parse_lrc("[0:01.00]short\n");
        assert_eq!(parsed.lines[0].timestamp_ms, Some(1000));
    }

    #[test]
    fn huge_minute_field_is_clamped_instead_of_overflowing() {
        // i64::MAX minutes would overflow `minutes * 60_000` (panic in debug,
        // wrap in release). The parser must clamp to the document ceiling
        // rather than panic or produce a negative/garbage timestamp.
        let input = "[9223372036854775807:59.99]boom\n".to_string();
        let parsed = parse_lrc(&input);
        assert_eq!(parsed.lines.len(), 1);
        let ms = parsed.lines[0].timestamp_ms.expect("timestamp must parse");
        assert!((0..=super::MAX_TIMESTAMP_MS).contains(&ms));
        assert_eq!(ms, super::MAX_TIMESTAMP_MS);
    }

    #[test]
    fn huge_offset_is_clamped_and_cannot_overflow_the_line_addition() {
        // An extreme `[offset:...]` must not overflow `resolved_ms + offset_ms`.
        let input = "[offset:9223372036854775807][00:01.00]first\n".to_string();
        let parsed = parse_lrc(&input);
        // The line keeps a sane, non-overflowed timestamp.
        assert_eq!(parsed.lines[0].timestamp_ms, Some(super::MAX_TIMESTAMP_MS));
    }

    #[test]
    fn word_timestamp_plus_huge_offset_does_not_overflow() {
        // Enhanced word tags add the offset too; a huge offset must saturate
        // rather than wrap into a negative start.
        let input = "[offset:9223372036854775807][00:01.00]<00:01.00>word\n".to_string();
        let parsed = parse_lrc(&input);
        assert_eq!(parsed.lines[0].words.len(), 1);
        let start = parsed.lines[0].words[0].start_ms;
        assert!(start > 0, "saturated start must stay positive");
    }

    #[test]
    fn parses_enhanced_word_tags_into_plain_body() {
        let input = "[00:12.00]Hello <00:12.34>world<00:12.50>!\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(
            parsed.lines[0].text, "Hello world!",
            "word tags must be stripped leaving the words in place"
        );
        assert_eq!(
            parsed.lines[0].words,
            vec![
                LyricsWord {
                    start_ms: 12340,
                    text: "world".to_string(),
                },
                LyricsWord {
                    start_ms: 12500,
                    text: "!".to_string(),
                },
            ],
            "each word keeps the time of its own tag"
        );
        assert_eq!(parsed.lines[0].timestamp_ms, Some(12000));
    }

    #[test]
    fn word_tags_carry_their_own_times_with_the_offset() {
        let input = "[offset:100]\n[00:10.00] <00:11.50>la <00:13.00>casa azul\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines.len(), 1);
        let line = &parsed.lines[0];
        assert_eq!(line.timestamp_ms, Some(10100));
        assert_eq!(line.text, "la casa azul");
        assert_eq!(
            line.words,
            vec![
                LyricsWord {
                    start_ms: 11600,
                    text: "la".to_string(),
                },
                LyricsWord {
                    start_ms: 13100,
                    text: "casa azul".to_string(),
                },
            ],
            "the tag itself must be excluded from the word text"
        );
    }

    #[test]
    fn repeated_timestamps_clone_the_word_list() {
        let input = "[00:03.00][00:47.00]Hi <00:04.00>there\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.lines[0].text, "Hi there");
        assert_eq!(
            parsed.lines[0].words,
            vec![LyricsWord {
                start_ms: 4000,
                text: "there".to_string(),
            }]
        );
        assert_eq!(
            parsed.lines[0].words, parsed.lines[1].words,
            "both clones of the line must share the word list"
        );
    }

    #[test]
    fn non_timestamp_angle_brackets_stay_in_the_body() {
        let input = "[00:10.00]hello <notatim> world\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines[0].text, "hello <notatim> world");
        assert!(
            parsed.lines[0].words.is_empty(),
            "a tag that is not a timestamp is ordinary text"
        );
    }

    #[test]
    fn standard_lrc_lines_have_no_words() {
        let parsed = parse_lrc("[00:01.50]plain line\none\n");

        assert!(
            parsed.lines.iter().all(|line| line.words.is_empty()),
            "timed and untimed plain lines carry no word list"
        );
    }

    #[test]
    fn repeats_the_text_for_every_timestamp_on_the_line() {
        let input = "[00:03.00][00:47.00]Chorus line\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(
            parsed.lines[0],
            LyricsLine {
                timestamp_ms: Some(3000),
                text: "Chorus line".to_string(),
                words: Vec::new(),
            }
        );
        assert_eq!(
            parsed.lines[1],
            LyricsLine {
                timestamp_ms: Some(47000),
                text: "Chorus line".to_string(),
                words: Vec::new(),
            }
        );
    }

    #[test]
    fn applies_the_offset_shift_to_content_after_it() {
        let input = "[offset:+1200]\n[00:01.00]Shifted\n[00:02.00]Later\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines[0].timestamp_ms, Some(1000 + 1200));
        assert_eq!(parsed.lines[1].timestamp_ms, Some(2000 + 1200));
        assert_eq!(
            parsed.meta.get("offset"),
            Some(&"+1200".to_string()),
            "the raw tag value is preserved, the sign is applied to timestamps only"
        );
    }

    #[test]
    fn offset_may_shift_earlier() {
        let input = "[offset:-500]\n[00:02.00]Early\n";
        let parsed = parse_lrc(input);
        assert_eq!(parsed.lines[0].timestamp_ms, Some(1500));
    }

    #[test]
    fn negative_offset_clamps_line_to_zero_and_roundtrips_stably() {
        let input = "[offset:-2000]\n[00:01.00]Early\n";
        let parsed = parse_lrc(input);
        let timestamp = parsed.lines[0].timestamp_ms.expect("timestamp must parse");

        assert_eq!(parsed.meta.get("offset"), Some(&"-2000".to_string()));
        assert_eq!(timestamp, 0);
        assert!((0..=MAX_TIMESTAMP_MS).contains(&timestamp));

        let formatted = format_lrc(&parsed.clone().into_document(), None, None);
        let reparsed = parse_lrc(&formatted);
        let reparsed_timestamp = reparsed.lines[0]
            .timestamp_ms
            .expect("formatted timestamp must parse");

        assert_eq!(reparsed_timestamp, timestamp);
        assert!((0..=MAX_TIMESTAMP_MS).contains(&reparsed_timestamp));
        assert_eq!(reparsed.lines, parsed.lines);
    }

    #[test]
    fn negative_offset_clamps_enhanced_word_to_zero_and_roundtrips_stably() {
        let input = "[offset:-1500]\n[00:02.00]Line <00:01.00>early\n";
        let parsed = parse_lrc(input);
        let line = &parsed.lines[0];
        let line_timestamp = line.timestamp_ms.expect("timestamp must parse");
        let word_timestamp = line.words[0].start_ms;

        assert_eq!(line_timestamp, 500);
        assert_eq!(word_timestamp, 0);
        assert!((0..=MAX_TIMESTAMP_MS).contains(&line_timestamp));
        assert!((0..=MAX_TIMESTAMP_MS).contains(&word_timestamp));

        let formatted = format_lrc(&parsed.clone().into_document(), None, None);
        let reparsed = parse_lrc(&formatted);
        let reparsed_line = &reparsed.lines[0];

        assert_eq!(reparsed.lines, parsed.lines);
        assert_eq!(reparsed_line.timestamp_ms, Some(line_timestamp));
        assert_eq!(reparsed_line.words[0].start_ms, word_timestamp);
        assert!((0..=MAX_TIMESTAMP_MS).contains(&reparsed_line.timestamp_ms.unwrap()));
        assert!((0..=MAX_TIMESTAMP_MS).contains(&reparsed_line.words[0].start_ms));
    }

    #[test]
    fn collects_metadata_tags_and_skips_them_from_the_body() {
        let input = "[ti:My Song]\n[ar:An Artist]\n[al:An Album]\n[00:01.00]body\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].text, "body");
        assert_eq!(parsed.meta.get("ti"), Some(&"My Song".to_string()));
        assert_eq!(parsed.meta.get("ar"), Some(&"An Artist".to_string()));
        assert_eq!(parsed.meta.get("al"), Some(&"An Album".to_string()));
    }

    #[test]
    fn meta_values_keep_bracketed_suffixes_without_stray_lines() {
        // Regression: the value "[Official Audio]" contains a `]`, so the
        // first closing bracket is the inner one and a bare `]` used to
        // leak into the body as a visible line.
        let input = "[ti:BL3SS & Tchami - R 2 ME [Official Audio]]\n[ar:BL3SS]\n[00:01.02] You change your mind\n";
        let parsed = parse_lrc(input);

        assert_eq!(
            parsed.meta.get("ti"),
            Some(&"BL3SS & Tchami - R 2 ME [Official Audio]".to_string()),
            "the value must extend to the last closing bracket"
        );
        assert_eq!(parsed.meta.get("ar"), Some(&"BL3SS".to_string()));
        assert_eq!(parsed.lines.len(), 1, "no stray `]` line may remain");
        assert_eq!(parsed.lines[0].timestamp_ms, Some(1_020));
        assert_eq!(parsed.lines[0].text, " You change your mind");
    }

    #[test]
    fn consecutive_meta_tags_are_not_merged_by_the_bracket_fix() {
        let input = "[ti:A][ar:B]\n[00:01.00]body\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.meta.get("ti"), Some(&"A".to_string()));
        assert_eq!(parsed.meta.get("ar"), Some(&"B".to_string()));
        assert_eq!(parsed.lines.len(), 1);
    }

    #[test]
    fn treats_bracketed_plain_headings_as_body() {
        let input = "[Verse 1]\nSome words\n[00:02.00]Timed\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines.len(), 3);
        assert_eq!(
            parsed.lines[0],
            LyricsLine {
                timestamp_ms: None,
                text: "[Verse 1]".to_string(),
                words: Vec::new(),
            }
        );
        assert_eq!(parsed.lines[1].text, "Some words");
        assert_eq!(parsed.lines[2].timestamp_ms, Some(2000));
    }

    #[test]
    fn skips_malformed_unclosed_bracket_lines() {
        let input = "[00:01.00]ok\n[00:02.00 broken\n[00:03.00]still ok\n";
        let parsed = parse_lrc(input);

        assert_eq!(parsed.lines.len(), 2, "only the malformed line is dropped");
        assert_eq!(parsed.lines[0].timestamp_ms, Some(1000));
        assert_eq!(parsed.lines[1].timestamp_ms, Some(3000));
    }

    #[test]
    fn malformed_timestamp_keeps_the_body_as_a_plain_line() {
        let parsed = parse_lrc("[00:99.00]never valid\n[00:01.00]valid\n");
        assert_eq!(
            parsed.lines.len(),
            2,
            "the broken tag becomes a normal line"
        );
        assert_eq!(
            parsed.lines[0],
            LyricsLine {
                timestamp_ms: None,
                text: "never valid".to_string(),
                words: Vec::new(),
            }
        );
        assert_eq!(parsed.lines[1].timestamp_ms, Some(1000));
    }

    #[test]
    fn roundtrip_through_format_keeps_timestamps_and_body() {
        let input = "[ti:Title]\n[ar:Artist]\n[00:01.20]one\n[00:02.10]two\n";
        let parsed = parse_lrc(input);
        let document = parsed.clone().into_document();

        let formatted = format_lrc(&document, Some("Title"), Some("Artist"));

        let reparsed = parse_lrc(&formatted);
        assert_eq!(
            reparsed.lines, parsed.lines,
            "timestamps and body must survive a format/parse roundtrip"
        );
        assert_eq!(reparsed.meta.get("ti"), Some(&"Title".to_string()));
        assert_eq!(reparsed.meta.get("ar"), Some(&"Artist".to_string()));
    }

    #[test]
    fn format_lrc_emits_headers_and_timed_lines() {
        let document = LyricsDocument {
            lines: vec![
                LyricsLine {
                    timestamp_ms: Some(1200),
                    text: "one".to_string(),
                    words: Vec::new(),
                },
                LyricsLine {
                    timestamp_ms: None,
                    text: "plain".to_string(),
                    words: Vec::new(),
                },
            ],
            text: "one\nplain".to_string(),
        };

        let out = format_lrc(&document, Some("The Title"), Some("The Artist"));

        assert!(out.contains("[ti:The Title]"));
        assert!(out.contains("[ar:The Artist]"));
        assert!(out.contains("[00:01.20]one"));
        assert!(out.contains("\nplain\n"));
    }

    #[test]
    fn format_lrc_omits_empty_headers() {
        let document = LyricsDocument {
            lines: vec![],
            text: String::new(),
        };
        let out = format_lrc(&document, Some("   "), None);
        assert!(!out.contains("[ti:"));
    }

    #[test]
    fn format_lrc_emits_enhanced_word_tags() {
        let document = parse_lrc("[00:12.00]Hello <00:12.34>world<00:12.50>!\n").into_document();

        let out = format_lrc(&document, None, None);

        assert_eq!(out, "[00:12.00]Hello <00:12.34>world<00:12.50>!\n");
    }

    #[test]
    fn format_lrc_roundtrips_enhanced_words() {
        let input = "[00:12.00]Hello <00:12.34>world<00:12.50>!\n";
        let parsed = parse_lrc(input);

        let out = format_lrc(&parsed.clone().into_document(), None, None);

        let reparsed = parse_lrc(&out);
        assert_eq!(
            reparsed.lines, parsed.lines,
            "words and their start times must survive a format/parse roundtrip"
        );
    }

    #[test]
    fn format_lrc_roundtrips_multi_word_lines_without_offsets() {
        let input = "[00:12.00]la <00:12.34>casa <00:12.50>azul\n";
        let parsed = parse_lrc(input);

        let out = format_lrc(&parsed.clone().into_document(), None, None);

        let reparsed = parse_lrc(&out);
        assert_eq!(reparsed.lines, parsed.lines);
        assert_eq!(reparsed.lines[0].text, "la casa azul");
    }

    #[test]
    fn format_lrc_degrades_when_words_do_not_match_the_body() {
        // The word list references "azul" which is not in the body, so the
        // line falls back to the plain timestamped text.
        let document = LyricsDocument {
            lines: vec![LyricsLine {
                timestamp_ms: Some(12000),
                text: "la casa".to_string(),
                words: vec![LyricsWord {
                    text: "azul".to_string(),
                    start_ms: 12340,
                }],
            }],
            text: "la casa".to_string(),
        };

        let out = format_lrc(&document, None, None);

        assert_eq!(out, "[00:12.00]la casa\n");
    }

    #[test]
    fn into_document_joins_lines_without_timestamps() {
        let doc = parse_lrc("[00:01.00]alpha\nbeta\n").into_document();
        assert_eq!(doc.text, "alpha\nbeta");
        assert_eq!(doc.lines[0].text, "alpha");
        assert_eq!(doc.lines[1].text, "beta");
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 48,
            failure_persistence: None,
            max_shrink_iters: 128,
            rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x4933_3001),
            .. ProptestConfig::default()
        })]

        #[test]
        fn arbitrary_lrc_text_is_total_and_timestamp_bounded(input in bounded_text()) {
            let parsed = parse_lrc(&input);

            for line in parsed.lines {
                if let Some(timestamp) = line.timestamp_ms {
                    prop_assert!((0..=MAX_TIMESTAMP_MS).contains(&timestamp));
                }
                for word in line.words {
                    prop_assert!((0..=MAX_TIMESTAMP_MS).contains(&word.start_ms));
                }
            }
        }

        #[test]
        fn generated_ordered_lrc_timestamps_keep_input_order(
            timestamps in prop::collection::vec(0i64..=MAX_TIMESTAMP_MS / 10, 0..=16)
                .prop_map(|values| values.into_iter().map(|value| value * 10).collect::<Vec<_>>())
        ) {
            let mut timestamps = timestamps;
            timestamps.sort_unstable();
            let input = timestamps
                .iter()
                .map(|timestamp| format!("[{}]line", format_time_stamp_content(*timestamp)))
                .collect::<Vec<_>>()
                .join("\n");

            let parsed = parse_lrc(&input);
            let actual = parsed
                .lines
                .iter()
                .filter_map(|line| line.timestamp_ms)
                .collect::<Vec<_>>();

            prop_assert!(actual.windows(2).all(|pair| pair[0] <= pair[1]));
            prop_assert_eq!(actual, timestamps);
        }
    }
}
