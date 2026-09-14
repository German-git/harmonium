//! Lossless, UTF-8 M3U line handling.
//!
//! This module owns pure M3U parsing, rendering, and document transformations.
//! It may use playlist domain values, but it never performs filesystem IO, so
//! callers can rewrite one field without normalizing the document.

use std::path::{Path, PathBuf};

use crate::error::Result as DomainResult;
use crate::playlist::Playlist;
use crate::stream::classify;
use crate::track::{Track, TrackLocation};

/// The physical line ending that terminated an M3U line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineEnding {
    /// Unix line feed.
    Lf,
    /// Windows carriage-return/line-feed pair.
    CrLf,
    /// The document ended without a line terminator.
    None,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::None => "",
        }
    }
}

/// One physical M3U line, split into preserved indentation, content, and EOL.
///
/// `indent` includes a UTF-8 BOM when it occurs at the start of the document.
/// `body` contains the rest of the line, including trailing whitespace. This
/// lets callers use `entry()` for classification while rendering untouched
/// lines byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct M3uLine {
    pub(crate) indent: String,
    pub(crate) body: String,
    pub(crate) eol: LineEnding,
}

impl M3uLine {
    fn from_text(text: &str, eol: LineEnding, first_line: bool) -> Self {
        let (bom, remainder) = if first_line {
            text.strip_prefix('\u{feff}')
                .map_or(("", text), |remainder| ("\u{feff}", remainder))
        } else {
            ("", text)
        };
        let indent_len = remainder.len() - remainder.trim_start().len();

        Self {
            indent: format!("{bom}{}", &remainder[..indent_len]),
            body: remainder[indent_len..].to_string(),
            eol,
        }
    }

    /// The trimmed semantic content used by the M3U parser and rewriters.
    pub(crate) fn entry(&self) -> &str {
        self.body.trim()
    }

    /// Whether this line is an EXTINF directive after indentation/whitespace.
    pub(crate) fn is_extinf(&self) -> bool {
        self.entry().starts_with("#EXTINF:")
    }

    /// Replace the semantic entry while retaining indentation and trailing
    /// whitespace from the original line.
    pub(crate) fn with_entry(&self, entry: &str) -> Self {
        let trailing_start = self.body.trim_end().len();
        let mut body = String::with_capacity(entry.len() + self.body.len() - trailing_start);
        body.push_str(entry);
        body.push_str(&self.body[trailing_start..]);

        Self {
            indent: self.indent.clone(),
            body,
            eol: self.eol,
        }
    }

    /// Replace the text after the first comma of an EXTINF line.
    pub(crate) fn with_extinf_label(&self, label: &ExtinfLabel) -> Option<Self> {
        let comma = self.body.find(',')?;
        let trailing_start = self.body.trim_end().len();
        let mut body = String::with_capacity(
            comma + 1 + label.as_str().len() + self.body.len() - trailing_start,
        );
        body.push_str(&self.body[..=comma]);
        body.push_str(label.as_str());
        body.push_str(&self.body[trailing_start..]);

        Some(Self {
            indent: self.indent.clone(),
            body,
            eol: self.eol,
        })
    }

    fn render_into(&self, output: &mut String) {
        output.push_str(&self.indent);
        output.push_str(&self.body);
        output.push_str(self.eol.as_str());
    }
}

/// A display label guaranteed not to contain a physical line break.
///
/// M3U has no portable escaping standard for display labels. Harmonium keeps
/// the existing visible `\\r`/`\\n` escape spelling at this serialization
/// boundary and intentionally does not decode it while parsing, so identity
/// and body bytes remain unrelated to presentation labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExtinfLabel(String);

impl ExtinfLabel {
    /// Escape physical line breaks before a label can be placed on one line.
    pub(crate) fn new(label: &str) -> Self {
        Self(label.replace('\r', r"\r").replace('\n', r"\n"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lex a UTF-8 M3U document into physical lines without normalizing it.
pub(crate) fn lex_document(contents: &str) -> Vec<M3uLine> {
    let bytes = contents.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut first_line = true;
    let mut index = 0;

    while index < bytes.len() {
        let (eol, width) = match bytes[index] {
            b'\n' => (LineEnding::Lf, 1),
            b'\r' if index + 1 < bytes.len() && bytes[index + 1] == b'\n' => (LineEnding::CrLf, 2),
            _ => {
                index += 1;
                continue;
            }
        };

        lines.push(M3uLine::from_text(&contents[start..index], eol, first_line));
        first_line = false;
        index += width;
        start = index;
    }

    if start < contents.len() {
        lines.push(M3uLine::from_text(
            &contents[start..],
            LineEnding::None,
            first_line,
        ));
    }

    lines
}

/// Render lexed lines exactly as they were split, including missing final EOL.
pub(crate) fn render_document(lines: &[M3uLine]) -> String {
    let capacity = lines
        .iter()
        .map(|line| line.indent.len() + line.body.len() + line.eol.as_str().len())
        .sum();
    let mut output = String::with_capacity(capacity);
    for line in lines {
        line.render_into(&mut output);
    }
    output
}

/// The semantic kind of one trimmed M3U line.
///
/// Only URLs accepted by the stream classifier are streams. Valid URLs with
/// unsupported schemes intentionally fall through to `LocalPath`, preserving
/// them as local playlist entries rather than forcing them into a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    Blank,
    Directive,
    Stream,
    LocalPath,
}

/// Classify one trimmed M3U line without applying filesystem semantics.
pub(crate) fn line_kind(entry: &str) -> LineKind {
    if entry.is_empty() {
        return LineKind::Blank;
    }
    if entry.starts_with('#') {
        return LineKind::Directive;
    }
    if TrackLocation::persisted_local_path(entry).is_some() {
        return LineKind::LocalPath;
    }

    match url::Url::parse(entry) {
        Ok(url) if classify(&url).is_some() => LineKind::Stream,
        _ => LineKind::LocalPath,
    }
}

/// Serialize a queue into extended M3U text with a trailing newline.
pub(crate) fn render_m3u(playlist: &Playlist) -> String {
    let mut out = String::from("#EXTM3U\n");
    for track in playlist.tracks() {
        // The M3U convention marks unknown durations as -1, so the u64 from
        // the domain must be narrowed explicitly instead of silently wrapping.
        // A decoded-but-zero duration is also unknown: a real string is never
        // exactly 0 seconds, so write the -1 sentinel instead of a misleading 0.
        let seconds = track
            .metadata()
            .filter(|meta| !meta.duration.is_zero())
            .map(|meta| i64::try_from(meta.duration.as_secs()).unwrap_or(-1))
            .unwrap_or(-1);
        // EXTINF label: tagged title wins, otherwise a source-appropriate
        // fallback. Local tracks use the file name with its extension, while
        // stream tracks use the URL hostname when one exists.
        let title = track
            .metadata()
            .filter(|meta| meta.title_tagged && !meta.title.is_empty())
            .map(|meta| meta.title.clone())
            .unwrap_or_else(|| fallback_label(track));
        out.push_str(&format!(
            "#EXTINF:{seconds},{}\n",
            ExtinfLabel::new(&title).as_str()
        ));
        // Body lines are a UTF-8 serialization boundary. Local identities use
        // a tagged byte encoding so arbitrary Unix path bytes survive the M3U
        // round trip; stream URLs retain their established spelling.
        out.push_str(&track.track_location().to_persisted());
        out.push('\n');
    }
    out
}

/// Source-appropriate EXTINF fallback label when metadata is empty.
fn fallback_label(track: &Track) -> String {
    use crate::stream::TrackSource;

    match track.source() {
        TrackSource::Local(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned()),
        TrackSource::Stream { url, .. } => url
            .host_str()
            .map(str::to_owned)
            .unwrap_or_else(|| url.as_str().to_string()),
    }
}

/// Parse M3U text back into a queue.
///
/// Unknown comment lines are skipped on purpose: the format allows arbitrary
/// directives and being strict here would reject third-party exports. Relative
/// paths resolve against the explicit playlist directory; no directory access
/// or canonicalization is performed.
pub(crate) fn parse_m3u(contents: &str, base_dir: &Path) -> DomainResult<Playlist> {
    let mut playlist = Playlist::new();
    let mut pending_label: Option<String> = None;

    for line in lex_document(contents) {
        let entry = line.entry();
        match line_kind(entry) {
            LineKind::Blank => continue,
            LineKind::Directive => {
                if let Some(label) = entry.strip_prefix("#EXTINF:") {
                    // `#EXTINF:<seconds>,<display>`; only use the trailing
                    // display label (the duration stays unknown until metadata
                    // arrives).
                    pending_label = label
                        .split_once(',')
                        .map(|(_, label)| label.to_string())
                        .or_else(|| Some(label.to_string()));
                }
                continue;
            }
            LineKind::Stream => {
                let url = url::Url::parse(entry).expect("classified stream must be a valid URL");
                let kind = classify(&url).expect("classified stream must have a stream kind");
                let mut track = Track::from_stream(url, kind);
                if let Some(label) = pending_label.take()
                    && !label.trim().is_empty()
                {
                    track.set_title(label.trim());
                }
                playlist.extend([track]);
            }
            LineKind::LocalPath => {
                let persisted_path = TrackLocation::persisted_local_path(entry);
                let path = persisted_path
                    .as_deref()
                    .unwrap_or_else(|| Path::new(entry));
                let resolved = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    base_dir.join(path)
                };
                let mut track = Track::local(resolved);
                if let Some(label) = pending_label.take()
                    && !label.trim().is_empty()
                {
                    track.set_title(label.trim());
                }
                playlist.extend([track]);
            }
        }
    }

    Ok(playlist)
}

/// Rewrite local path entries while preserving their original spelling style.
///
/// Relative entries are replaced by the new file name, while absolute entries
/// receive the new absolute path. Only the matching lexical identity changes;
/// all other physical bytes remain untouched.
pub(crate) fn rewrite_path(contents: &str, dir: &Path, old: &TrackLocation, new: &Path) -> String {
    let mut lines = lex_document(contents);
    for line in &mut lines {
        let entry = line.entry();
        if !matches!(line_kind(entry), LineKind::LocalPath) {
            continue;
        }

        let persisted_path = TrackLocation::persisted_local_path(entry);
        let path_entry = persisted_path
            .as_deref()
            .unwrap_or_else(|| Path::new(entry));
        let relative = !path_entry.is_absolute();
        let resolved = if relative {
            dir.join(path_entry)
        } else {
            path_entry.to_path_buf()
        };
        if TrackLocation::local(resolved) != *old {
            continue;
        }

        let replacement_path = if relative {
            new.file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| new.to_path_buf())
        } else {
            new.to_path_buf()
        };
        let replacement = if persisted_path.is_some() {
            TrackLocation::local(replacement_path).to_persisted()
        } else if path_body_is_safe(&replacement_path) {
            replacement_path.to_string_lossy().into_owned()
        } else {
            TrackLocation::local(replacement_path).to_persisted()
        };
        *line = line.with_entry(&replacement);
    }
    render_document(&lines)
}

/// Identity used by the single EXTINF rewrite algorithm.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ExtinfTarget<'a> {
    Local {
        base_dir: &'a Path,
        location: &'a TrackLocation,
    },
    Stream(&'a str),
}

/// Rewrite EXTINF labels for either local paths or stream URLs.
///
/// The following physical line decides whether a buffered EXTINF belongs to
/// the selected entry. Its duration and every surrounding byte are preserved.
pub(crate) fn rewrite_extinf_title(
    contents: &str,
    target: ExtinfTarget<'_>,
    new_title: &str,
) -> String {
    let lines = lex_document(contents);
    let mut rendered = Vec::with_capacity(lines.len());
    let mut pending_extinf: Option<M3uLine> = None;
    let label = ExtinfLabel::new(new_title);

    for line in lines {
        if line.is_extinf() {
            if let Some(previous) = pending_extinf.take() {
                rendered.push(previous);
            }
            pending_extinf = Some(line);
            continue;
        }

        if let Some(extinf_line) = pending_extinf.take() {
            if extinf_matches(line.entry(), target) {
                let rewritten = extinf_line.with_extinf_label(&label).unwrap_or(extinf_line);
                rendered.push(rewritten);
            } else {
                rendered.push(extinf_line);
            }
        }
        rendered.push(line);
    }

    if let Some(extinf_line) = pending_extinf {
        rendered.push(extinf_line);
    }

    render_document(&rendered)
}

fn extinf_matches(entry: &str, target: ExtinfTarget<'_>) -> bool {
    match target {
        ExtinfTarget::Local { base_dir, location } => {
            if !matches!(line_kind(entry), LineKind::LocalPath) {
                return false;
            }
            let persisted_path = TrackLocation::persisted_local_path(entry);
            let path_entry = persisted_path
                .as_deref()
                .unwrap_or_else(|| Path::new(entry));
            let resolved = if path_entry.is_absolute() {
                path_entry.to_path_buf()
            } else {
                base_dir.join(path_entry)
            };
            TrackLocation::local(resolved) == *location
        }
        ExtinfTarget::Stream(url) => matches!(line_kind(entry), LineKind::Stream) && entry == url,
    }
}

fn path_body_is_safe(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|path| !path.contains(['\r', '\n']))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_render_preserves_bom_indentation_endings_and_final_eol() {
        let contents = "\u{feff}#EXTM3U\r\n  #EXTINF:-1,one, two  \r\n\t song.mp3\nlast";
        let lines = lex_document(contents);

        assert_eq!(lines[0].indent, "\u{feff}");
        assert_eq!(lines[0].body, "#EXTM3U");
        assert_eq!(lines[1].indent, "  ");
        assert_eq!(lines[1].body, "#EXTINF:-1,one, two  ");
        assert_eq!(lines[1].eol, LineEnding::CrLf);
        assert_eq!(lines[2].eol, LineEnding::Lf);
        assert_eq!(lines[3].eol, LineEnding::None);
        assert_eq!(render_document(&lines), contents);
    }

    #[test]
    fn extinf_label_escapes_line_breaks() {
        let label = ExtinfLabel::new("new\r\nlabel");
        assert_eq!(label.as_str(), r"new\r\nlabel");
    }

    #[test]
    fn parser_and_extinf_rewriter_are_pure_document_transforms() {
        let base = Path::new("/music");
        let contents = "\u{feff}#EXTM3U\r\n  #EXTINF:253,Old local  \r\n\t song.mp3\r\n\
            #EXTINF:-1,Old stream\r\n\
            https://stream.example.com/live";
        let parsed = parse_m3u(contents, base).expect("parse");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.tracks()[0].display_name(), "Old local");
        assert_eq!(parsed.tracks()[1].display_name(), "Old stream");

        let local = rewrite_extinf_title(
            contents,
            ExtinfTarget::Local {
                base_dir: base,
                location: &TrackLocation::local("/music/song.mp3"),
            },
            "New local\nTitle",
        );
        let rewritten = rewrite_extinf_title(
            &local,
            ExtinfTarget::Stream("https://stream.example.com/live"),
            "New stream",
        );

        assert_eq!(
            rewritten,
            "\u{feff}#EXTM3U\r\n  #EXTINF:253,New local\\nTitle  \r\n\t song.mp3\r\n\
             #EXTINF:-1,New stream\r\n\
             https://stream.example.com/live"
        );
    }

    #[test]
    fn path_rewriter_preserves_relative_and_absolute_spelling() {
        let base = Path::new("/music");
        let contents = "#EXTM3U\nold.mp3\n/music/old.mp3\nother.mp3\n";
        let rewritten = rewrite_path(
            contents,
            base,
            &TrackLocation::local("/music/old.mp3"),
            Path::new("/music/new.mp3"),
        );

        assert_eq!(rewritten, "#EXTM3U\nnew.mp3\n/music/new.mp3\nother.mp3\n");
    }
}
