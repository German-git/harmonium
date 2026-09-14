//! Embedded lyrics extraction through lofty.
//!
//! This path runs on demand, only when the local `.lrc` search came up empty
//! and the panel needs a document. It never touches the metadata batch that
//! feeds the playlist, keeping the tracking scan free of lyrics work.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use lofty::file::TaggedFileExt;
use lofty::probe::Probe;
use lofty::tag::ItemKey;

/// Read embedded lyrics from an audio file, degrading to `None`.
///
/// The `UnsyncLyrics` key covers ID3v2 USLT frames and the MP4 `©lyr` atom,
/// while `Lyrics` covers Vorbis and APE comments. Decoding failures are
/// logged and return `None` so a broken file never reaches the UI as an
/// error.
pub fn read_embedded_lyrics(path: &Path) -> Option<String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "cannot open file for embedded lyrics lookup"
            );
            return None;
        }
    };

    let tagged = Probe::new(BufReader::new(file))
        .guess_file_type()
        .map_err(|error| {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "cannot guess file type for embedded lyrics"
            )
        })
        .ok()
        .and_then(|probe| {
            probe
                .read()
                .map_err(|error| {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "cannot read tags for embedded lyrics"
                    )
                })
                .ok()
        })?;

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let value = tag
        .get_string(ItemKey::UnsyncLyrics)
        .or_else(|| tag.get_string(ItemKey::Lyrics))?;
    // Whitespace-only payloads count as absent but the returned value keeps
    // its original shape (line breaks included) for the LRC parser.
    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;
    use lofty::config::WriteOptions;
    use lofty::tag::{TagExt, TagType};
    use std::fs;
    use std::io::Write;

    /// Write a fixture file containing an ID3v2 tag plus one minimal MPEG
    /// frame.
    ///
    /// `dump_to` serializes the tag into the file; the single MPEG frame
    /// header makes the file decodable as MPEG audio, so the probe accepts
    /// the whole file and the embedded lyrics lookup can run.
    fn write_tagged_bare_audio(path: &std::path::Path, lyrics: &str, key_name: &str) {
        use lofty::tag::{ItemKey, Tag};
        let mut tag = Tag::new(TagType::Id3v2);
        let key = match key_name {
            "lyrics" => ItemKey::Lyrics,
            _ => ItemKey::UnsyncLyrics,
        };
        tag.insert_text(key, lyrics.to_string());

        let mut bytes = Vec::new();
        tag.dump_to(&mut bytes, WriteOptions::default())
            .expect("tag serializes");
        // Several MPEG1 Layer III frames (128kbps / 44100Hz, 417 bytes each):
        // the parser needs a chain of frames before the next sync word to
        // accept the file as decodable audio.
        for _ in 0..8 {
            bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            bytes.extend(std::iter::repeat_n(0, 417 - 4));
        }
        let mut file = fs::File::create(path).expect("fixture file");
        file.write_all(&bytes).expect("fixture write");
        file.flush().expect("flush");
    }

    #[test]
    fn reads_unsync_lyrics_from_an_id3v2_tag() {
        let root = unique_temp_dir("lyrics-embedded");
        let path = root.join("song.mp3");
        write_tagged_bare_audio(&path, "[00:01.00]embedded text\n", "unsync");

        let lyrics = read_embedded_lyrics(&path);
        assert_eq!(lyrics.as_deref(), Some("[00:01.00]embedded text\n"));
    }

    #[test]
    fn missing_file_yields_none() {
        let root = unique_temp_dir("lyrics-embedded-missing");
        let path = root.join("ghost.mp3");
        assert_eq!(read_embedded_lyrics(&path), None);
    }

    #[test]
    fn untracked_corrupt_file_yields_none() {
        let root = unique_temp_dir("lyrics-embedded-corrupt");
        let path = root.join("broken.mp3");
        fs::write(&path, b"definitely not audio bytes").expect("fixture");
        assert_eq!(read_embedded_lyrics(&path), None);
    }
}
