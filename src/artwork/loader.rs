//! Artwork source resolution and decoding, always off the UI thread.
//!
//! Resolution follows the owner mandated order: embedded tag pictures
//! first, then well known cover files beside the track. Every function is
//! tolerant by design, returning `None` instead of failing, because a
//! track without usable artwork is an everyday case and never an error.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use image::DynamicImage;
use lofty::file::TaggedFileExt;
use lofty::probe::Probe;

use crate::artwork::ArtworkError;
use crate::config::ArtworkSource as SourceConfig;
use crate::metadata::TrackMetadata;

/// Cover file names probed beside the track, in strict priority order.
///
/// The order is part of the owner artwork policy and pinned by tests.
const COVER_CANDIDATES: [&str; 7] = [
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "folder.jpg",
    "folder.jpeg",
    "front.jpg",
    "front.png",
];

/// Maximum retained source dimension in pixels on either side.
///
/// The browser overlay expands with the panel width, so the source must be
/// larger than the playlist presentation. A 1024px ceiling covers a wide
/// terminal panel while keeping the retained RGBA bitmap bounded to roughly
/// 4 MiB before protocol encoding. The file-size, declared
/// dimension and decoder-allocation limits below remain the first line of
/// defense against hostile input.
const MAX_RETAINED_SOURCE_SIZE: u32 = 1024;

/// Ceiling for accepting an album art file at all, in bytes.
///
/// A cover is a few hundred KB; anything past this is almost certainly a
/// decompression bomb or a non-image renamed to `cover.jpg`. Rejecting before
/// reading avoids allocating the whole file just to fail the decode.
const MAX_ART_FILE_BYTES: u64 = 20 * 1024 * 1024;

/// Hard ceiling for the image's declared pixel dimensions, checked on the
/// header before the decoder runs.
///
/// `image::load_from_memory` fully decodes the bitmap before any resize, so
/// a crafted 20000x20000 PNG would first allocate ~1.6 GB of RGBA. The
/// decoders also respect an allocation limit, but rejecting on the cheap
/// header read keeps even the in-limits-but-huge case from burning CPU.
const MAX_DECODE_DIMENSION: u32 = 16_000;

/// Raw artwork bytes paired with where they came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtworkSource {
    /// Picture bytes extracted from the track tags.
    Embedded(Vec<u8>),
    /// Cover file sitting next to the track.
    File(PathBuf),
    /// Bytes fetched from Cover Art Archive.
    Remote(Vec<u8>),
}

/// Find the best artwork source for an optional local track path, if one exists.
///
/// Resolution follows the user configured `source_config`:
/// - `Metadata` tries only embedded tag pictures
/// - `Local` tries only directory cover files
/// - `Remote` tries only MusicBrainz/Cover Art Archive
/// - `All` tries embedded, directory, then remote in order
pub fn resolve_source(
    track_path: Option<&Path>,
    source_config: SourceConfig,
    metadata: Option<&TrackMetadata>,
    cache_dir: &Path,
) -> Option<ArtworkSource> {
    match source_config {
        SourceConfig::Metadata => track_path
            .and_then(embedded_picture)
            .map(ArtworkSource::Embedded),
        SourceConfig::Local => track_path
            .and_then(directory_cover)
            .map(ArtworkSource::File),
        SourceConfig::Remote => {
            let artist = metadata.map(|m| m.artist.as_str()).unwrap_or("");
            let album = metadata.map(|m| m.album.as_str()).unwrap_or("");
            crate::artwork::remote::fetch_cover(artist, album, cache_dir).map(ArtworkSource::Remote)
        }
        SourceConfig::All => {
            // Embedded tag pictures win over directory covers because they
            // describe the exact track, then local files, then remote
            if let Some(path) = track_path
                && let Some(bytes) = embedded_picture(path)
            {
                return Some(ArtworkSource::Embedded(bytes));
            }
            if let Some(path) = track_path.and_then(directory_cover) {
                return Some(ArtworkSource::File(path));
            }
            let artist = metadata.map(|m| m.artist.as_str()).unwrap_or("");
            let album = metadata.map(|m| m.album.as_str()).unwrap_or("");
            crate::artwork::remote::fetch_cover(artist, album, cache_dir).map(ArtworkSource::Remote)
        }
    }
}

/// Decode the bytes of a resolved source into an image.
///
/// Reads stay here so the caller decides which thread pays for the IO.
/// Corrupt data surfaces as a structured error which the loader logs and
/// turns into no artwork. Images larger than [`MAX_RETAINED_SOURCE_SIZE`] on
/// any side are resized down while preserving aspect ratio so the protocol
/// receives bounded input without imposing the playlist presentation size on
/// the browser overlay.
///
/// Uncompressed source bytes and raw `File` reads guard against two distinct
/// memory risks: a [`ArtworkSource::File`] that is already gigabytes on disk
/// is rejected by size before it is read into memory, and a crafted PNG/JPEG
/// that inflates to enormous dimensions is refused on its header and again by
/// the decoder's allocation limits — never fully decoded just to be resized
/// afterwards.
pub fn decode_source(
    track_path: &Path,
    source: &ArtworkSource,
) -> Result<DynamicImage, ArtworkError> {
    let bytes = match source {
        ArtworkSource::Embedded(bytes) | ArtworkSource::Remote(bytes) => bytes.clone(),
        ArtworkSource::File(path) => {
            // Refuse oversized/malformed files before allocating the whole
            // buffer, so a multi-GB `cover.jpg` never reaches memory.
            let size = fs::metadata(path)
                .map(|meta| meta.len())
                .map_err(|source| ArtworkError::Io {
                    path: path.clone(),
                    source,
                })?;
            if size > MAX_ART_FILE_BYTES {
                return Err(ArtworkError::Decode {
                    track: track_path.to_path_buf(),
                    message: format!(
                        "cover file {} is {size} bytes, above the {} byte ceiling",
                        path.display(),
                        MAX_ART_FILE_BYTES
                    ),
                });
            }
            fs::read(path).map_err(|source| ArtworkError::Io {
                path: path.clone(),
                source,
            })?
        }
    };

    // Reject declared dimensions larger than any sane cover on the cheap
    // header read, before the decoder materializes the bitmap.
    let dims = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .and_then(|reader| reader.into_dimensions().map_err(std::io::Error::other))
        .ok();
    if let Some((width, height)) = dims
        && (width > MAX_DECODE_DIMENSION || height > MAX_DECODE_DIMENSION)
    {
        return Err(ArtworkError::Decode {
            track: track_path.to_path_buf(),
            message: format!(
                "cover dimensions {width}x{height} exceed the {MAX_DECODE_DIMENSION}px ceiling"
            ),
        });
    }

    // Decode under strict limits so a decompression bomb is refused at decode
    // time with a bounded allocation, rather than exploding the worker.
    let image = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|error| ArtworkError::Decode {
            track: track_path.to_path_buf(),
            message: error.to_string(),
        })
        .and_then(|mut reader| {
            // `Limits` is non-exhaustive, so build it from Default and set the
            // fields we care about rather than using a struct literal.
            let mut limits = image::Limits::default();
            limits.max_image_width = Some(MAX_DECODE_DIMENSION);
            limits.max_image_height = Some(MAX_DECODE_DIMENSION);
            limits.max_alloc = Some(512 * 1024 * 1024);
            reader.limits(limits);
            reader.decode().map_err(|error| ArtworkError::Decode {
                track: track_path.to_path_buf(),
                message: error.to_string(),
            })
        })?;

    // Retain a bounded but browser-appropriate source. Presentation-specific
    // resizing belongs to ratatui-image's target-aware protocol path below.
    if image.width() > MAX_RETAINED_SOURCE_SIZE || image.height() > MAX_RETAINED_SOURCE_SIZE {
        Ok(image.resize(
            MAX_RETAINED_SOURCE_SIZE,
            MAX_RETAINED_SOURCE_SIZE,
            image::imageops::FilterType::Lanczos3,
        ))
    } else {
        Ok(image)
    }
}

/// Pull the first embedded picture from the track tags, if any.
///
/// Probing mirrors `metadata::reader` but stays deliberately shallow: any
/// unreadable or pictureless file simply yields `None` and the directory
/// fallback takes over.
fn embedded_picture(track_path: &Path) -> Option<Vec<u8>> {
    let file = fs::File::open(track_path).ok()?;
    let tagged = Probe::new(BufReader::new(file))
        .guess_file_type()
        .ok()?
        .read()
        .ok()?;

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let picture = tag.pictures().first()?;

    Some(picture.data().to_vec())
}

/// Probe the track parent directory for the well known cover names.
///
/// The candidate list is checked in order and the first existing regular
/// file wins, so `cover.jpg` always beats `front.png` regardless of how
/// the filesystem happens to order its entries.
fn directory_cover(track_path: &Path) -> Option<PathBuf> {
    let directory = track_path.parent()?;

    COVER_CANDIDATES
        .iter()
        .map(|name| directory.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ArtworkSource as SourceConfig;
    use crate::test_support::unique_temp_dir;
    use sha2::Digest;

    /// Smallest valid PNG, written through the encoder so the bytes are
    /// guaranteed to decode again in the assertions.
    fn png_bytes() -> Vec<u8> {
        let image = DynamicImage::new_rgba8(2, 2);
        let mut buffer = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("png encoding");
        buffer.into_inner()
    }

    /// Default cache dir for tests that do not exercise remote fetching.
    fn test_cache_dir() -> PathBuf {
        std::env::temp_dir().join("harmonium-test-cache")
    }

    #[test]
    fn directory_cover_follows_the_owner_priority_order() {
        let root = unique_temp_dir("artwork-order");
        let track = root.join("song.mp3");
        fs::write(&track, b"audio").expect("track fixture");
        // Deliberately create them in reverse priority order so the result
        // can only come from the candidate list, not from filesystem order
        for name in [
            "front.png",
            "front.jpg",
            "folder.png",
            "folder.jpg",
            "cover.png",
        ] {
            fs::write(root.join(name), b"img").expect("cover fixture");
        }

        assert_eq!(
            directory_cover(&track),
            Some(root.join("cover.png")),
            "cover.png beats folder and front variants"
        );

        fs::write(root.join("cover.jpeg"), b"img").expect("cover fixture");
        assert_eq!(directory_cover(&track), Some(root.join("cover.jpeg")));

        fs::write(root.join("cover.jpg"), b"img").expect("cover fixture");
        assert_eq!(directory_cover(&track), Some(root.join("cover.jpg")));
    }

    #[test]
    fn directory_cover_is_none_without_candidates() {
        let root = unique_temp_dir("artwork-none");
        let track = root.join("song.mp3");
        fs::write(&track, b"audio").expect("track fixture");
        fs::write(root.join("notes.txt"), b"text").expect("decoy fixture");

        assert_eq!(directory_cover(&track), None);
    }

    #[test]
    fn directories_named_like_covers_are_ignored() {
        let root = unique_temp_dir("artwork-dirs");
        let track = root.join("song.mp3");
        fs::write(&track, b"audio").expect("track fixture");
        fs::create_dir_all(root.join("cover.jpg")).expect("dir fixture");

        // Only regular files qualify, a matching directory is not a cover
        assert_eq!(directory_cover(&track), None);
    }

    #[test]
    fn corrupt_audio_falls_through_to_the_directory_cover() {
        let root = unique_temp_dir("artwork-fallback");
        let track = root.join("broken.mp3");
        fs::write(&track, b"definitely not audio").expect("track fixture");
        fs::write(root.join("folder.jpg"), b"img").expect("cover fixture");

        // The failed tag probe must not abort resolution, the directory
        // fallback still gets its chance
        assert_eq!(
            resolve_source(Some(&track), SourceConfig::All, None, &test_cache_dir()),
            Some(ArtworkSource::File(root.join("folder.jpg")))
        );
    }

    #[test]
    fn file_source_decodes_into_an_image() {
        let root = unique_temp_dir("artwork-decode");
        let track = root.join("song.mp3");
        let cover = root.join("cover.png");
        fs::write(&track, b"audio").expect("track fixture");
        fs::write(&cover, png_bytes()).expect("cover fixture");

        let source = resolve_source(Some(&track), SourceConfig::All, None, &test_cache_dir())
            .expect("cover resolves");
        let image = decode_source(&track, &source).expect("png decodes");

        assert_eq!(image.width(), 2);
        assert_eq!(image.height(), 2);
    }

    #[test]
    fn local_high_resolution_cover_is_not_reduced_to_playlist_size() {
        let root = unique_temp_dir("artwork-high-resolution");
        let track = root.join("song.mp3");
        let cover = root.join("cover.png");
        fs::write(&track, b"audio").expect("track fixture");

        let image = DynamicImage::new_rgba8(512, 512);
        image.save(&cover).expect("high-resolution cover fixture");

        let decoded = decode_source(&track, &ArtworkSource::File(cover))
            .expect("high-resolution cover decodes");

        assert_eq!(decoded.width(), 512);
        assert_eq!(decoded.height(), 512);
        assert!(
            decoded.width() > 144,
            "browser source must exceed playlist size"
        );
    }

    #[test]
    fn decoded_cover_is_bounded_before_protocol_creation() {
        let track = PathBuf::from("/music/song.mp3");
        let image = DynamicImage::new_rgba8(2_048, 1_024);
        let mut buffer = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("high-resolution png encoding");

        let decoded = decode_source(&track, &ArtworkSource::Embedded(buffer.into_inner()))
            .expect("bounded high-resolution cover decodes");

        assert_eq!(decoded.width(), MAX_RETAINED_SOURCE_SIZE);
        assert_eq!(decoded.height(), MAX_RETAINED_SOURCE_SIZE / 2);
    }

    #[test]
    fn corrupt_image_bytes_fail_with_the_decode_variant() {
        let root = unique_temp_dir("artwork-corrupt");
        let track = root.join("song.mp3");
        let cover = root.join("cover.jpg");
        fs::write(&track, b"audio").expect("track fixture");
        fs::write(&cover, b"this is not a jpeg").expect("cover fixture");

        let source = resolve_source(Some(&track), SourceConfig::All, None, &test_cache_dir())
            .expect("cover resolves");
        let error = decode_source(&track, &source).expect_err("corrupt bytes must fail");

        assert!(
            matches!(error, ArtworkError::Decode { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn embedded_source_decodes_without_touching_the_disk() {
        let track = PathBuf::from("/music/song.mp3");
        let source = ArtworkSource::Embedded(png_bytes());

        let image = decode_source(&track, &source).expect("embedded png decodes");

        assert_eq!(image.width(), 2);
    }

    #[test]
    fn missing_cover_file_reports_the_io_variant() {
        let root = unique_temp_dir("artwork-vanished");
        let track = root.join("song.mp3");
        let ghost = root.join("cover.jpg");

        let error = decode_source(&track, &ArtworkSource::File(ghost.clone()))
            .expect_err("a vanished file must fail");

        match error {
            ArtworkError::Io { path, .. } => assert_eq!(path, ghost),
            other => panic!("expected the Io variant, got {other:?}"),
        }
    }

    #[test]
    fn oversized_cover_file_is_rejected_before_reading() {
        let root = unique_temp_dir("artwork-oversize");
        let track = root.join("song.mp3");
        let cover = root.join("cover.jpg");
        fs::write(&track, b"audio").expect("track fixture");
        // Sparse file larger than the ceiling but cheap to create.
        let file = fs::File::create(&cover).expect("cover create");
        file.set_len(MAX_ART_FILE_BYTES + 1).expect("grow");

        let error = decode_source(&track, &ArtworkSource::File(cover))
            .expect_err("an oversized file must be refused");
        assert!(
            matches!(error, ArtworkError::Decode { .. }),
            "got {error:?}"
        );
    }

    #[test]
    fn huge_declared_dimensions_are_rejected_on_the_header() {
        // A tiny file whose dims (as a crafted PNG) would blow up the decode.
        // Build a valid PNG then re-tag its header dimensions to a huge value.
        let track = PathBuf::from("/music/song.mp3");
        let png = png_bytes();

        // The IHDR width/height are bytes 16..24 after the 8-byte signature.
        let mut bomb = png;
        bomb[16..20].copy_from_slice(&16_001u32.to_be_bytes());
        bomb[20..24].copy_from_slice(&16_001u32.to_be_bytes());

        let error = decode_source(&track, &ArtworkSource::Embedded(bomb))
            .expect_err("huge dims must be refused");
        assert!(
            matches!(error, ArtworkError::Decode { .. }),
            "got {error:?}"
        );

        // The legitimate small PNG still decodes.
        assert_eq!(
            decode_source(&track, &ArtworkSource::Embedded(png_bytes()))
                .unwrap()
                .width(),
            2
        );
    }

    #[test]
    fn metadata_source_config_only_tries_embedded() {
        let root = unique_temp_dir("artwork-metadata-only");
        let track = root.join("song.mp3");
        fs::write(&track, b"audio").expect("track fixture");
        fs::write(root.join("cover.jpg"), b"img").expect("cover fixture");

        // Metadata mode ignores the directory cover
        let result = resolve_source(
            Some(&track),
            SourceConfig::Metadata,
            None,
            &test_cache_dir(),
        );
        assert!(result.is_none());
    }

    #[test]
    fn local_source_config_only_tries_directory() {
        let root = unique_temp_dir("artwork-local-only");
        let track = root.join("song.mp3");
        fs::write(&track, b"audio").expect("track fixture");
        fs::write(root.join("cover.jpg"), b"img").expect("cover fixture");

        let result = resolve_source(Some(&track), SourceConfig::Local, None, &test_cache_dir());
        assert_eq!(result, Some(ArtworkSource::File(root.join("cover.jpg"))));
    }

    #[test]
    fn remote_source_config_returns_none_without_network() {
        let root = unique_temp_dir("artwork-remote-only");
        let track = root.join("song.mp3");
        fs::write(&track, b"audio").expect("track fixture");

        // Remote mode without valid credentials returns None
        let result = resolve_source(Some(&track), SourceConfig::Remote, None, &root);
        assert!(result.is_none());
    }

    #[test]
    fn all_source_config_tries_embedded_then_local() {
        let root = unique_temp_dir("artwork-all-chain");
        let track = root.join("song.mp3");
        fs::write(&track, b"audio").expect("track fixture");
        fs::write(root.join("cover.jpg"), b"img").expect("cover fixture");

        let result = resolve_source(Some(&track), SourceConfig::All, None, &test_cache_dir());
        assert_eq!(result, Some(ArtworkSource::File(root.join("cover.jpg"))));
    }

    #[test]
    fn stream_source_skips_local_probes_but_keeps_remote_resolution() {
        let root = unique_temp_dir("artwork-stream-sources");
        let metadata = TrackMetadata {
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            ..TrackMetadata::default()
        };
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"front-500");
        hasher.update(b"\nArtist\nAlbum");
        let key = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let artwork_dir = root.join("artwork");
        fs::create_dir_all(&artwork_dir).expect("artwork cache dir");
        fs::write(artwork_dir.join(format!("{}.jpg", &key[..16])), png_bytes())
            .expect("cached remote cover");

        assert_eq!(
            resolve_source(None, SourceConfig::Metadata, Some(&metadata), &root),
            None,
            "streams have no embedded tag path"
        );
        assert_eq!(
            resolve_source(None, SourceConfig::Local, Some(&metadata), &root),
            None,
            "streams have no directory path"
        );
        assert!(matches!(
            resolve_source(None, SourceConfig::All, Some(&metadata), &root),
            Some(ArtworkSource::Remote(_))
        ));
    }
}
