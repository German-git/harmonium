//! Lofty backed tag writer running inside blocking workers.
//!
//! Same thread contract as the reader: nothing here may run on the UI
//! thread, callers go through `spawn_blocking` on the shared runtime.
//!
//! ## Per tag type semantics (spike findings)
//!
//! - Year is written as [`ItemKey::RecordingDate`], not [`ItemKey::Year`]:
//!   `Year` only maps to the Vorbis `YEAR` key, while `RecordingDate` maps
//!   to ID3v2 `TDRC`, MP4 `©day`, RIFF INFO `ICRD` and Vorbis `DATE`/`YEAR`,
//!   so the value survives every supported family. The reader mirrors this
//!   by reading `RecordingDate` first and `Year` as fallback.
//! - Empty fields remove the tag item instead of writing an empty string,
//!   matching lofty's model where an absent key means an absent tag.
//! - Comment is single valued here: `set_comment` overwrites, which is the
//!   documented lofty behaviour for multi value formats.
//! - The generic [`lofty::tag::Tag`] abstraction converts items to each
//!   format at save time, so no per family branches are needed. For WAV the
//!   primary tag type is `Id3v2` (`FileType::primary_tag_type`), which
//!   supports all ten fields, so a tagless WAV receives a fresh ID3v2 tag.

use std::fs::File;
use std::path::Path;

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::{ItemKey, Tag};

use crate::filesystem::persistence::stage_copy;
use crate::metadata::{MetaField, MetadataError};

struct ValidatedTags {
    fields: [String; 10],
    track_number: Option<u32>,
    disc_number: Option<u32>,
}

impl ValidatedTags {
    fn parse(path: &Path, fields: &[String; 10]) -> Result<Self, MetadataError> {
        let track_number = parse_optional_number(path, MetaField::TrackNumber, &fields[4])?;
        let disc_number = parse_optional_number(path, MetaField::DiscNumber, &fields[5])?;
        let mut fields = fields.clone();
        if let Some(number) = track_number {
            fields[MetaField::TrackNumber.index()] = number.to_string();
        }
        if let Some(number) = disc_number {
            fields[MetaField::DiscNumber.index()] = number.to_string();
        }
        Ok(Self {
            fields,
            track_number,
            disc_number,
        })
    }
}

/// Persist the ten editable fields into the file's primary tag.
///
/// A field left empty removes its tag item; a non empty field overwrites the
/// previous value. Track and disc numbers must parse as `u32`, otherwise the
/// whole write fails before touching the file so a typo cannot silently
/// mangle existing tags.
pub fn write_metadata(path: &Path, fields: &[String; 10]) -> Result<(), MetadataError> {
    let validated = ValidatedTags::parse(path, fields)?;
    let mut staged = stage_copy(path).map_err(|error| MetadataError::Write {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let temporary_path = staged.temporary_path().to_path_buf();

    let mut temporary = File::open(&temporary_path).map_err(|error| MetadataError::Write {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let mut tagged = lofty::read_from(&mut temporary).map_err(|error| MetadataError::Decode {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    drop(temporary);

    // Obtain a mutable primary tag, creating one when the file is tagless
    let primary_type = tagged.primary_tag_type();
    let tag = match tagged.primary_tag_mut() {
        Some(tag) => tag,
        None => {
            tagged.insert_tag(Tag::new(primary_type));
            tagged
                .primary_tag_mut()
                .expect("fresh tag was just inserted")
        }
    };

    for field in MetaField::ALL {
        apply_field(
            tag,
            field,
            &validated.fields[field.index()],
            validated.track_number,
            validated.disc_number,
        );
    }

    tagged
        .save_to_path(&temporary_path, lofty::config::WriteOptions::default())
        .map_err(|error| MetadataError::Write {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;

    let actual = crate::metadata::reader::editable_fields(&temporary_path).map_err(|error| {
        MetadataError::Write {
            path: path.to_path_buf(),
            message: format!("metadata verification failed: {error}"),
        }
    })?;
    if actual != validated.fields {
        return Err(MetadataError::Write {
            path: path.to_path_buf(),
            message: format!(
                "metadata verification failed: requested {:?}, read back {:?}",
                validated.fields, actual
            ),
        });
    }

    staged.commit().map_err(|error| MetadataError::Write {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

/// Apply one field: non empty overwrites, empty removes the tag item.
fn apply_field(
    tag: &mut Tag,
    field: MetaField,
    value: &str,
    track_number: Option<u32>,
    disc_number: Option<u32>,
) {
    use lofty::tag::Accessor;

    if value.is_empty() {
        match field {
            MetaField::Title => tag.remove_title(),
            MetaField::Artist => tag.remove_artist(),
            MetaField::Album => tag.remove_album(),
            MetaField::AlbumArtist => tag.remove_key(ItemKey::AlbumArtist),
            MetaField::TrackNumber => tag.remove_track(),
            MetaField::DiscNumber => tag.remove_disk(),
            MetaField::Genre => tag.remove_genre(),
            MetaField::Year => tag.remove_date(),
            MetaField::Composer => tag.remove_key(ItemKey::Composer),
            MetaField::Comment => tag.remove_comment(),
        }
        return;
    }

    match field {
        MetaField::Title => tag.set_title(value.to_string()),
        MetaField::Artist => tag.set_artist(value.to_string()),
        MetaField::Album => tag.set_album(value.to_string()),
        MetaField::AlbumArtist => {
            tag.insert_text(ItemKey::AlbumArtist, value.to_string());
        }
        MetaField::TrackNumber => {
            if let Some(number) = track_number {
                tag.set_track(number);
            }
        }
        MetaField::DiscNumber => {
            if let Some(number) = disc_number {
                tag.set_disk(number);
            }
        }
        MetaField::Genre => tag.set_genre(value.to_string()),
        MetaField::Year => {
            // See the module docs: RecordingDate is the only key that maps
            // to every supported tag family
            tag.remove_date();
            tag.insert_text(ItemKey::RecordingDate, value.to_string());
        }
        MetaField::Composer => {
            tag.insert_text(ItemKey::Composer, value.to_string());
        }
        MetaField::Comment => tag.set_comment(value.to_string()),
    }
}

/// Parse a numeric field, rejecting garbage with the file context attached.
fn parse_number(path: &Path, field: MetaField, value: &str) -> Result<u32, MetadataError> {
    value
        .trim()
        .parse::<u32>()
        .map_err(|_| MetadataError::InvalidNumber {
            path: path.to_path_buf(),
            field: field.label(),
            value: value.to_string(),
        })
}

fn parse_optional_number(
    path: &Path,
    field: MetaField,
    value: &str,
) -> Result<Option<u32>, MetadataError> {
    (!value.is_empty())
        .then(|| parse_number(path, field, value))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::reader::read_metadata;
    use crate::test_support::wav_bytes;
    use std::fs;

    #[allow(clippy::too_many_arguments)]
    fn ten_fields(
        title: &str,
        artist: &str,
        album: &str,
        album_artist: &str,
        track: &str,
        disc: &str,
        genre: &str,
        year: &str,
        composer: &str,
        comment: &str,
    ) -> [String; 10] {
        [
            title.into(),
            artist.into(),
            album.into(),
            album_artist.into(),
            track.into(),
            disc.into(),
            genre.into(),
            year.into(),
            composer.into(),
            comment.into(),
        ]
    }

    #[test]
    fn full_round_trip_on_a_tagless_wav() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("track.wav");
        fs::write(&path, wav_bytes(&[])).expect("fixture write");

        write_metadata(
            &path,
            &ten_fields(
                "Tom Sawyer",
                "Rush",
                "Moving Pictures",
                "Rush",
                "1",
                "1",
                "Progressive Rock",
                "1981",
                "Geddy Lee",
                "Remastered",
            ),
        )
        .expect("write succeeds");

        let meta = read_metadata(&path).expect("re-read succeeds");
        assert_eq!(meta.title, "Tom Sawyer");
        assert_eq!(meta.artist, "Rush");
        assert_eq!(meta.album, "Moving Pictures");
        assert_eq!(meta.album_artist.as_deref(), Some("Rush"));
        assert_eq!(meta.track_number, Some(1));
        assert_eq!(meta.disc_number, Some(1));
        assert_eq!(meta.genre.as_deref(), Some("Progressive Rock"));
        assert_eq!(meta.year.as_deref(), Some("1981"));
        assert_eq!(meta.composer.as_deref(), Some("Geddy Lee"));
        assert_eq!(meta.comment.as_deref(), Some("Remastered"));
    }

    #[test]
    fn cleared_field_removes_its_tag_item() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("track.wav");
        fs::write(&path, wav_bytes(&[])).expect("fixture write");

        write_metadata(
            &path,
            &ten_fields(
                "Tom Sawyer",
                "Rush",
                "Moving Pictures",
                "Rush",
                "1",
                "1",
                "Progressive Rock",
                "1981",
                "Geddy Lee",
                "Remastered",
            ),
        )
        .expect("first write");

        // Clear Composer and Year: those items must vanish from the file
        write_metadata(
            &path,
            &ten_fields(
                "Tom Sawyer",
                "Rush",
                "Moving Pictures",
                "Rush",
                "1",
                "1",
                "Progressive Rock",
                "",
                "",
                "Remastered",
            ),
        )
        .expect("second write");

        let meta = read_metadata(&path).expect("re-read succeeds");
        assert_eq!(meta.composer, None, "cleared composer must be removed");
        assert_eq!(meta.year, None, "cleared year must be removed");
        assert_eq!(meta.title, "Tom Sawyer", "other fields keep their values");
        assert_eq!(meta.genre.as_deref(), Some("Progressive Rock"));
        assert_eq!(meta.comment.as_deref(), Some("Remastered"));
    }

    #[test]
    fn read_only_file_reports_a_write_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("locked.wav");
        fs::write(&path, wav_bytes(&[])).expect("fixture write");
        let original = fs::read(&path).expect("read original");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).expect("lock fixture");

        let error = write_metadata(
            &path,
            &ten_fields("T", "A", "L", "", "", "", "", "", "", ""),
        )
        .expect_err("read only file must fail");

        match error {
            MetadataError::Write { path: got, .. } => assert_eq!(got, path),
            other => panic!("expected Write error, got {other:?}"),
        }
        assert_eq!(fs::read(&path).expect("read original"), original);
    }

    #[test]
    fn invalid_numeric_field_reports_a_validation_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("track.wav");
        fs::write(&path, wav_bytes(&[])).expect("fixture write");
        let original = fs::read(&path).expect("read original");

        let error = write_metadata(
            &path,
            &ten_fields("T", "A", "L", "", "abc", "", "", "", "", ""),
        )
        .expect_err("garbage track number must fail");

        match error {
            MetadataError::InvalidNumber { field, value, .. } => {
                assert_eq!(field, "Track Number");
                assert_eq!(value, "abc");
            }
            other => panic!("expected InvalidNumber error, got {other:?}"),
        }
        assert_eq!(fs::read(&path).expect("read original"), original);
    }

    #[cfg(unix)]
    #[test]
    fn successful_write_preserves_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("track.wav");
        fs::write(&path, wav_bytes(&[])).expect("fixture write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("set mode");

        write_metadata(
            &path,
            &ten_fields("Title", "", "", "", "", "", "", "", "", ""),
        )
        .expect("write succeeds");

        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o640
        );
    }
}
