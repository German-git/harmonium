//! Persisting remote lyrics next to the audio file.
//!
//! Cache files live in a `lyrics/` folder beside the audio file and carry
//! the same base name, so the local search finds them naturally on a later
//! run without touching the network again.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::filesystem::persistence::atomic_replace;

/// Save `content` as `<audio-dir>/lyrics/<audio-stem>.lrc`.
///
/// The directory is created on demand. IO failures are returned to the
/// caller, which decides whether the error is worth surfacing (it is not:
/// lyric caching is optional and a failed write degrades to resolving again
/// next time).
pub fn save_lyrics_file(audio_path: &Path, content: &str) -> io::Result<PathBuf> {
    let stem = audio_path.file_stem().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "audio path has no file stem")
    })?;
    let dir = audio_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("lyrics");
    fs::create_dir_all(&dir)?;
    let mut filename = stem.to_os_string();
    filename.push(".lrc");
    let path = dir.join(filename);
    atomic_replace(&path, content.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::unique_temp_dir;
    use std::ffi::OsString;

    #[test]
    fn writes_into_the_lyrics_subfolder_with_the_audio_stem() {
        let root = unique_temp_dir("lyrics-cache");
        fs::write(root.join("song.mp3"), "audio").expect("audio fixture");

        let saved = save_lyrics_file(&root.join("song.mp3"), "[00:01.00]cached")
            .expect("cache write succeeds");

        assert_eq!(saved, root.join("lyrics/song.lrc"));
        assert_eq!(
            fs::read_to_string(&saved).expect("cache read"),
            "[00:01.00]cached"
        );
    }

    #[test]
    fn a_path_without_a_stem_fails_cleanly() {
        let error = save_lyrics_file(Path::new("/"), "content").expect_err("must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_stem_and_local_lookup_rediscovers_the_cache() {
        use std::os::unix::ffi::OsStringExt;

        let root = unique_temp_dir("lyrics-cache-non-utf8");
        let audio_name = OsString::from_vec(b"song-\xff.mp3".to_vec());
        let audio_path = root.join(&audio_name);
        fs::write(&audio_path, "audio").expect("audio fixture");

        let saved =
            save_lyrics_file(&audio_path, "[00:01.00]cached").expect("cache write succeeds");

        let mut expected_name = OsString::from_vec(b"song-\xff".to_vec());
        expected_name.push(".lrc");
        let expected = root.join("lyrics").join(&expected_name);
        assert_eq!(saved, expected);
        assert_eq!(
            crate::lyrics::locator::find_local_lrc(&audio_path),
            Some(expected)
        );

        let rewritten =
            save_lyrics_file(&audio_path, "[00:02.00]updated").expect("cache replacement succeeds");
        assert_eq!(rewritten, root.join("lyrics").join(&expected_name));
        assert_eq!(
            fs::read_to_string(&rewritten).expect("rewritten cache read"),
            "[00:02.00]updated"
        );
        assert_eq!(
            fs::read_dir(root.join("lyrics"))
                .expect("lyrics directory")
                .count(),
            1,
            "atomic replacement must clean up its temporary file"
        );
    }
}
