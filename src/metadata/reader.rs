//! Lofty backed extraction running inside blocking workers.
//!
//! Nothing here may be called from the UI thread: decoding is CPU bound IO,
//! so callers go through `spawn_blocking` on the shared runtime. The batch
//! helpers exist because one user action can queue many files at once and
//! per file events would flood the bridge bus.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::prelude::*;
use lofty::probe::Probe;

use crate::metadata::{MetadataError, TrackMetadata, UNKNOWN_ALBUM, UNKNOWN_ARTIST};

/// Outcome of a whole extraction batch, ready for the bridge bus.
#[derive(Debug, Default)]
pub struct MetadataBatch {
    /// One snapshot per successfully decoded path.
    pub loaded: Vec<(PathBuf, TrackMetadata)>,
    /// How many paths failed with a structured [`MetadataError`].
    pub failed: usize,
}

impl MetadataBatch {
    /// Whether nothing was extracted and nothing failed.
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty() && self.failed == 0
    }
}

/// Extract metadata for every path, never failing the whole batch.
///
/// Corrupt or unreadable entries only increment [`MetadataBatch::failed`]
/// and emit a warning log each, so a single broken download cannot hide
/// the tags of every other track queued by the same action.
pub fn collect_metadata(paths: Vec<PathBuf>) -> MetadataBatch {
    let mut batch = MetadataBatch::default();

    for path in paths {
        match read_metadata(&path) {
            Ok(metadata) => batch.loaded.push((path, metadata)),
            Err(error) => {
                tracing::warn!("{error}");
                batch.failed += 1;
            }
        }
    }

    batch
}

/// Extract tags and properties from a single audio file.
pub fn read_metadata(path: &Path) -> Result<TrackMetadata, MetadataError> {
    // Opening separately keeps real io::Error kinds such as NotFound or
    // PermissionDenied intact instead of flattening them into decode noise
    let file = File::open(path).map_err(|source| MetadataError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let tagged = Probe::new(BufReader::new(file))
        .guess_file_type()
        .map_err(|source| MetadataError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .read()
        .map_err(|error| MetadataError::Decode {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

    Ok(build_snapshot(path, &tagged))
}

/// Raw tag values for the ten editable fields, in [`MetaField::ALL`] order.
///
/// Unlike [`read_metadata`] this applies no display fallbacks: a tagless file
/// yields ten empty strings so the metadata editor opens with every field
/// editable, and an absent artist stays absent instead of becoming
/// `Unknown Artist`.
pub fn editable_fields(path: &Path) -> Result<[String; 10], MetadataError> {
    let file = File::open(path).map_err(|source| MetadataError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let tagged = Probe::new(BufReader::new(file))
        .guess_file_type()
        .map_err(|source| MetadataError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .read()
        .map_err(|error| MetadataError::Decode {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    // Same order as MetaField::ALL so the form, the prefill and the writer
    // all agree on which index means which field
    Ok([
        tag.and_then(Accessor::title)
            .map(|value| value.into_owned())
            .unwrap_or_default(),
        tag.and_then(Accessor::artist)
            .map(|value| value.into_owned())
            .unwrap_or_default(),
        tag.and_then(Accessor::album)
            .map(|value| value.into_owned())
            .unwrap_or_default(),
        tag.and_then(|tag| tag.get_string(lofty::tag::ItemKey::AlbumArtist))
            .map(str::to_string)
            .unwrap_or_default(),
        tag.and_then(Accessor::track)
            .map(|number| number.to_string())
            .unwrap_or_default(),
        tag.and_then(Accessor::disk)
            .map(|number| number.to_string())
            .unwrap_or_default(),
        tag.and_then(Accessor::genre)
            .map(|value| value.into_owned())
            .unwrap_or_default(),
        tag.and_then(|tag| {
            tag.get_string(lofty::tag::ItemKey::RecordingDate)
                .or_else(|| tag.get_string(lofty::tag::ItemKey::Year))
        })
        .map(str::to_string)
        .unwrap_or_default(),
        tag.and_then(|tag| tag.get_string(lofty::tag::ItemKey::Composer))
            .map(str::to_string)
            .unwrap_or_default(),
        tag.and_then(Accessor::comment)
            .map(|value| value.into_owned())
            .unwrap_or_default(),
    ])
}

/// Flatten lofty's tag plus properties model into the domain snapshot.
fn build_snapshot(path: &Path, tagged: &lofty::file::TaggedFile) -> TrackMetadata {
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    // The flag records whether the title came from a real `Title` tag (true)
    // or from the file stem fallback (false). The m3u8 EXTINF and the
    // rename flow need to distinguish the two so the fallback label can be
    // replaced with the full file name when the user renames the track.
    let raw_title = tag
        .and_then(Accessor::title)
        .map(|value| value.into_owned())
        .filter(|value| !value.trim().is_empty());
    let title_tagged = raw_title.is_some();
    let title = raw_title.unwrap_or_else(|| fallback_title(path));

    let artist = non_empty_tag(tag, Accessor::artist).unwrap_or_else(|| UNKNOWN_ARTIST.into());
    let album = non_empty_tag(tag, Accessor::album).unwrap_or_else(|| UNKNOWN_ALBUM.into());
    let track_number = tag.and_then(Accessor::track);
    // The remaining editable fields stay raw (no display fallbacks): the
    // metadata editor must show exactly what the file carries so a cleared
    // field removes the tag instead of reappearing as a sentinel.
    let album_artist = tag.and_then(|tag| tag.get_string(lofty::tag::ItemKey::AlbumArtist));
    let disc_number = tag.and_then(Accessor::disk);
    let genre = non_empty_tag(tag, Accessor::genre);
    let year = tag.and_then(|tag| {
        tag.get_string(lofty::tag::ItemKey::RecordingDate)
            .or_else(|| tag.get_string(lofty::tag::ItemKey::Year))
    });
    let composer = tag.and_then(|tag| tag.get_string(lofty::tag::ItemKey::Composer));
    let comment = non_empty_tag(tag, Accessor::comment);

    let properties = tagged.properties();
    let (format, codec) = describe(tagged.file_type());

    TrackMetadata {
        title,
        title_tagged,
        artist,
        album,
        album_artist: album_artist.map(str::to_string),
        track_number,
        disc_number,
        genre,
        year: year.map(str::to_string),
        composer: composer.map(str::to_string),
        comment,
        duration: properties.duration(),
        bitrate: properties.overall_bitrate(),
        sample_rate: properties.sample_rate(),
        codec,
        format,
    }
}

/// Read an accessor string treating blank values as absent.
fn non_empty_tag(
    tag: Option<&lofty::tag::Tag>,
    reader: fn(&lofty::tag::Tag) -> Option<std::borrow::Cow<'_, str>>,
) -> Option<String> {
    let value = tag.and_then(reader)?.into_owned();
    (!value.trim().is_empty()).then_some(value)
}

/// File stem as the last line title fallback.
fn fallback_title(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Map lofty's file type onto display names.
///
/// Lofty merges container and codec into one enum, so both labels derive
/// from the same source. Keeping them separate fields preserves the domain
/// vocabulary for phase 4 playback wiring.
fn describe(file_type: FileType) -> (String, String) {
    let pair = |format: &str, codec: &str| (format.to_string(), codec.to_string());
    match file_type {
        FileType::Mpeg => pair("MP3", "MP3"),
        FileType::Flac => pair("FLAC", "FLAC"),
        FileType::Wav => pair("WAV", "PCM"),
        FileType::Aiff => pair("AIFF", "PCM"),
        FileType::Opus => pair("Opus", "Opus"),
        FileType::Vorbis => pair("Ogg Vorbis", "Vorbis"),
        FileType::Speex => pair("Ogg Speex", "Speex"),
        FileType::Aac => pair("AAC", "AAC"),
        FileType::Mp4 => pair("MP4 audio", "MP4"),
        FileType::Ape => pair("Monkey's Audio", "APE"),
        FileType::Mpc => pair("Musepack", "Musepack"),
        FileType::WavPack => pair("WavPack", "WavPack"),
        // Non exhaustive upstream: unknown or custom formats degrade to
        // their own name so snapshots never carry empty labels
        FileType::Custom(name) => pair(name, name),
        other => {
            let name = format!("{other:?}");
            (name.clone(), name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;
    use std::fs;
    use std::time::Duration;

    /// Byte length of a RIFF chunk payload padded to an even boundary.
    fn riff_chunk(id: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::with_capacity(8 + payload.len());
        chunk.extend_from_slice(id);
        chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        chunk.extend_from_slice(payload);
        if payload.len() % 2 == 1 {
            chunk.push(0);
        }
        chunk
    }

    /// Null terminated ASCII used by every INFO subchunk value.
    fn info_entry(id: &[u8; 4], value: &str) -> Vec<u8> {
        let mut payload = value.as_bytes().to_vec();
        payload.push(0);
        riff_chunk(id, &payload)
    }

    /// Build a decodable mono 16 bit PCM WAV of exactly one second.
    ///
    /// The optional tags exercise the INFO chunk branch, while leaving them off
    /// exercises every fallback path in the snapshot builder.
    fn wav_file(title: Option<&str>, artist: Option<&str>) -> Vec<u8> {
        let mut tags = Vec::new();
        if let Some(title) = title {
            tags.push(("INAM", title));
        }
        if let Some(artist) = artist {
            tags.push(("IART", artist));
        }
        wav_with_info(&tags)
    }

    /// WAV with arbitrary RIFF INFO subchunks, one per `(id, value)` pair.
    ///
    /// Kept separate so the metadata-edit tests can seed every INFO field the
    /// RIFF tag family supports (album, track, genre, year, composer, comment).
    fn wav_with_info(tags: &[(&str, &str)]) -> Vec<u8> {
        const SAMPLE_RATE: u32 = 44100;
        const SAMPLES: usize = SAMPLE_RATE as usize;

        let mut fmt_payload = Vec::new();
        fmt_payload.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        fmt_payload.extend_from_slice(&1_u16.to_le_bytes()); // mono
        fmt_payload.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        fmt_payload.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
        fmt_payload.extend_from_slice(&2_u16.to_le_bytes()); // block align
        fmt_payload.extend_from_slice(&16_u16.to_le_bytes()); // bits

        let data_payload = vec![0_u8; SAMPLES * 2];

        let mut info = b"INFO".to_vec();
        for (id, value) in tags {
            info.extend(info_entry(
                id.as_bytes().try_into().expect("4 byte id"),
                value,
            ));
        }

        let mut body = b"WAVE".to_vec();
        body.extend(riff_chunk(b"fmt ", &fmt_payload));
        if info.len() > 4 {
            body.extend(riff_chunk(b"LIST", &info));
        }
        body.extend(riff_chunk(b"data", &data_payload));

        let mut file = b"RIFF".to_vec();
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);
        file
    }

    fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, bytes).expect("fixture write");
        path
    }

    #[test]
    fn tagged_wav_yields_full_snapshot() {
        let root = unique_temp_dir("meta-tagged");
        let path = write_fixture(
            &root,
            "song.wav",
            &wav_with_info(&[
                ("INAM", "Tom Sawyer"),
                ("IART", "Rush"),
                ("IPRD", "Moving Pictures"),
                ("IPRT", "1"),
                ("IGNR", "Progressive Rock"),
                ("ICRD", "1981"),
                ("IMUS", "Geddy Lee"),
                ("ICMT", "Remastered"),
            ]),
        );

        let meta = read_metadata(&path).expect("extraction succeeds");

        assert_eq!(meta.title, "Tom Sawyer");
        assert_eq!(meta.artist, "Rush");
        assert_eq!(meta.album, "Moving Pictures");
        assert_eq!(meta.track_number, Some(1));
        assert_eq!(meta.genre.as_deref(), Some("Progressive Rock"));
        assert_eq!(meta.year.as_deref(), Some("1981"));
        assert_eq!(meta.composer.as_deref(), Some("Geddy Lee"));
        assert_eq!(meta.comment.as_deref(), Some("Remastered"));
        // The RIFF INFO tag family cannot express these two fields
        assert_eq!(meta.album_artist, None, "RiffInfo has no album artist key");
        assert_eq!(meta.disc_number, None, "RiffInfo has no disc number key");
        assert_eq!(meta.duration, Duration::from_secs(1));
        assert_eq!(meta.sample_rate, Some(44100));
        assert!(meta.bitrate.is_some(), "PCM byte rate yields a bitrate");
        assert_eq!(meta.codec, "PCM");
        assert_eq!(meta.format, "WAV");
    }

    #[test]
    fn untagged_wav_falls_back_to_the_file_stem() {
        let root = unique_temp_dir("meta-untagged");
        let path = write_fixture(&root, "fallback-name.wav", &wav_file(None, None));

        let meta = read_metadata(&path).expect("extraction succeeds");

        assert_eq!(meta.title, "fallback-name");
        assert_eq!(meta.artist, "Unknown Artist");
        assert_eq!(meta.album, "Unknown Album");
        // Every optional editable field stays absent on a tagless file
        assert_eq!(meta.album_artist, None);
        assert_eq!(meta.disc_number, None);
        assert_eq!(meta.genre, None);
        assert_eq!(meta.year, None);
        assert_eq!(meta.composer, None);
        assert_eq!(meta.comment, None);
    }

    #[test]
    fn missing_files_fail_with_the_io_variant() {
        let root = unique_temp_dir("meta-missing");
        let missing = root.join("ghost.mp3");

        let error = read_metadata(&missing).expect_err("missing file must fail");

        match &error {
            MetadataError::Io { path, source } => {
                assert_eq!(path, &missing);
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected the Io variant, got {other:?}"),
        }
    }

    #[test]
    fn corrupt_content_fails_gracefully_without_panicking() {
        let root = unique_temp_dir("meta-corrupt");
        let garbage = write_fixture(&root, "broken.mp3", b"definitely not audio");
        let empty = write_fixture(&root, "empty.flac", b"");

        for path in [garbage, empty] {
            let error = read_metadata(&path).expect_err("corrupt input must fail");
            assert!(
                matches!(
                    error,
                    MetadataError::Decode { .. } | MetadataError::Io { .. }
                ),
                "{path:?} produced {error}"
            );
        }
    }

    #[test]
    fn batches_keep_going_after_individual_failures() {
        let root = unique_temp_dir("meta-batch");
        let good = write_fixture(&root, "good.wav", &wav_file(Some("Good"), None));
        let bad = write_fixture(&root, "bad.mp3", b"junk");

        let batch = collect_metadata(vec![bad.clone(), good.clone()]);

        assert_eq!(batch.failed, 1, "only the corrupt entry counts as failed");
        assert_eq!(batch.loaded.len(), 1);
        let (loaded_path, meta) = &batch.loaded[0];
        assert_eq!(loaded_path, &good);
        assert_eq!(meta.title, "Good");
    }

    #[test]
    fn editable_fields_are_raw_without_display_fallbacks() {
        let root = unique_temp_dir("meta-editable");
        let tagged = write_fixture(
            &root,
            "tagged.wav",
            &wav_with_info(&[("INAM", "Tom Sawyer"), ("IART", "Rush"), ("ICRD", "1981")]),
        );

        let fields = editable_fields(&tagged).expect("tagged read");

        assert_eq!(fields[0], "Tom Sawyer");
        assert_eq!(fields[1], "Rush");
        assert_eq!(fields[7], "1981", "year comes from RecordingDate");
        assert_eq!(fields[4], "", "no track number tag");

        let untagged = write_fixture(&root, "untagged.wav", &wav_with_info(&[]));
        let fields = editable_fields(&untagged).expect("untagged read");

        assert!(
            fields.iter().all(|field| field.is_empty()),
            "a tagless file must open with every field empty, got {fields:?}"
        );
    }
}
