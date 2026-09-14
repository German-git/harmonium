//! M3U storage for playlists below the data directory.
//!
//! Format policy: UTF-8 `m3u8` files, one absolute or playlist relative
//! path per line, with optional `#EXTINF` metadata written but never
//! required on read. The manager never mutates audio files and only ever
//! creates, reads or renames its own playlist documents.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::config::{Paths, ensure_dir};
use crate::error::{HarmoniumError, Result, Result as DomainResult, RollbackError};
use crate::filesystem::persistence::{StagedReplacement, atomic_replace, stage_replacement};
use crate::filesystem::safety::is_valid_path_component;
use crate::playlist::Playlist;
use crate::playlist::m3u::{self, ExtinfTarget};

/// Extension appended to stored playlists, UTF-8 M3U by convention.
const PLAYLIST_EXTENSION: &str = "m3u8";

/// A validated playlist name that is safe to use as one filesystem component.
///
/// The private representation makes validation a construction invariant: a
/// `PlaylistStore` never receives an unchecked string for a playlist path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlaylistName(String);

impl PlaylistName {
    /// Borrow the validated name as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for PlaylistName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for PlaylistName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for PlaylistName {
    type Error = HarmoniumError;

    fn try_from(name: &str) -> Result<Self> {
        validate_playlist_name(name)?;
        Ok(Self(name.to_string()))
    }
}

impl TryFrom<String> for PlaylistName {
    type Error = HarmoniumError;

    fn try_from(name: String) -> Result<Self> {
        validate_playlist_name(&name)?;
        Ok(Self(name))
    }
}

/// Observable result of one all-or-nothing playlist rewrite operation.
///
/// The original document snapshots are retained privately so a caller that
/// subsequently fails a related filesystem mutation can restore exactly the
/// documents this operation changed, without running a second path-matching
/// policy over the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteOutcome {
    documents: Vec<PendingRewrite>,
}

/// Persistence and transaction boundary for saved playlists.
///
/// The application uses the concrete [`PlaylistStore`] directly, while this
/// trait keeps playlist persistence replaceable for workers and focused tests
/// without introducing dynamic dispatch.
pub trait PlaylistRepository: Send + Sync {
    fn save(&self, name: &PlaylistName, playlist: &Playlist) -> DomainResult<PathBuf>;
    fn save_rendered(&self, name: &PlaylistName, contents: &str) -> DomainResult<PathBuf>;
    fn load(&self, name: &PlaylistName) -> DomainResult<Playlist>;
    fn list_names(&self) -> DomainResult<Vec<String>>;
    fn delete(&self, name: &PlaylistName) -> DomainResult<()>;
    fn rename_playlist(&self, old_name: &PlaylistName, new_name: &PlaylistName)
    -> DomainResult<()>;
    fn rewrite_path_in_all(&self, old: &Path, new: &Path) -> DomainResult<RewriteOutcome>;
    fn rollback_rewrite(&self, outcome: &RewriteOutcome) -> DomainResult<()>;
    fn update_extinf_title(&self, path: &Path, new_title: &str) -> DomainResult<RewriteOutcome>;
    fn update_stream_extinf_title(
        &self,
        url: &url::Url,
        new_title: &str,
    ) -> DomainResult<RewriteOutcome>;
}

impl RewriteOutcome {
    /// Number of playlist documents replaced by the operation.
    pub fn touched(&self) -> usize {
        self.documents.len()
    }

    /// Paths of the playlist documents replaced by the operation.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.documents
            .iter()
            .map(|document| document.path.as_path())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRewrite {
    path: PathBuf,
    original: String,
    rewritten: String,
}

/// Creates, reads and renames playlist files inside one directory.
///
/// Every store clones share the same `write_lock`, so a clone used for async
/// autosave serializes its writes against the synchronous clone driving the
/// command layer. `Arc<Mutex<()>>` is intentionally not `PartialEq`, which is
/// why the derive drops that trait.
#[derive(Debug, Clone)]
pub struct PlaylistStore {
    dir: PathBuf,
    /// Serializes every write so concurrent saves cannot interleave.
    write_lock: Arc<Mutex<()>>,
}

impl PlaylistStore {
    /// Store rooted at the standard data location.
    pub fn from_paths(paths: &Paths) -> Self {
        Self::for_dir(paths.playlists_dir())
    }

    /// Store rooted at an explicit directory.
    ///
    /// The injection point that keeps every manager test off the real user
    /// profile while production always goes through [`Self::from_paths`].
    pub fn for_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Directory this store reads and writes.
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    /// Write `playlist` as `<name>.m3u8`, replacing any previous version.
    ///
    /// The target directory is created lazily so a fresh install can save
    /// without any startup cost. Returns the file written.
    pub fn save(&self, name: &PlaylistName, playlist: &Playlist) -> DomainResult<PathBuf> {
        // Render the M3U text outside the lock: it is a pure in-memory
        // transform of the caller's snapshot and can be costly for large
        // queues. Holding the store lock only for the actual file write keeps
        // the critical section as short as possible, so a concurrent autosave
        // on a slow disk stalls the UI for the minimum time.
        let rendered = m3u::render_m3u(playlist);
        ensure_dir(&self.dir)?;
        // Hold the store lock for the write so a concurrent autosave from the
        // async clone cannot interleave with this one.
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

        let path = self.file_path(name);
        atomic_replace(&path, rendered.as_bytes())
            .map_err(|source| HarmoniumError::io(path.clone(), source))?;
        tracing::debug!(name = %name, path = %path.display(), "playlist saved");
        Ok(path)
    }

    /// Write already-rendered M3U text as `<name>.m3u8`.
    ///
    /// The caller (the autosave path) renders the queue on the UI thread and
    /// hands over the finished text, so the store never has to own or clone a
    /// `Playlist`. Same serialization contract and lock semantics as
    /// [`Self::save`].
    pub fn save_rendered(&self, name: &PlaylistName, contents: &str) -> DomainResult<PathBuf> {
        ensure_dir(&self.dir)?;
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());

        let path = self.file_path(name);
        atomic_replace(&path, contents.as_bytes())
            .map_err(|source| HarmoniumError::io(path.clone(), source))?;
        tracing::debug!(name = %name, path = %path.display(), "playlist saved (rendered)");
        Ok(path)
    }

    /// Load a playlist by name, resolving relative paths against the file.
    pub fn load(&self, name: &PlaylistName) -> DomainResult<Playlist> {
        let path = self.file_path(name);
        let contents =
            fs::read_to_string(&path).map_err(|source| HarmoniumError::io(path.clone(), source))?;
        tracing::debug!(name = %name, path = %path.display(), "playlist loaded");
        m3u::parse_m3u(&contents, &self.dir)
    }

    /// All saved playlist names currently present, stem only, sorted.
    ///
    /// Only `*.m3u8` files count, the extension is stripped, and entries
    /// whose names are not valid UTF-8 are skipped rather than producing a
    /// lossy name that could never be loaded back. Missing directories
    /// (no playlists saved yet) yield an empty list instead of an error.
    pub fn list_names(&self) -> DomainResult<Vec<String>> {
        let mut names: Vec<String> = Vec::new();

        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(names),
            Err(error) => return Err(HarmoniumError::io(self.dir.clone(), error)),
        };

        for entry in entries {
            let entry = entry.map_err(|source| HarmoniumError::io(self.dir.clone(), source))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some(PLAYLIST_EXTENSION) {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                names.push(stem.to_string());
            }
        }

        names.sort_by(|left, right| natord::compare(left, right).then_with(|| left.cmp(right)));
        Ok(names)
    }

    /// Remove a saved playlist file, refusing an unknown name.
    ///
    /// The write lock is held so a delete never races a concurrent save of
    /// the same file. The typed name invariant keeps the directory free of
    /// stray documents.
    pub fn delete(&self, name: &PlaylistName) -> DomainResult<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.file_path(name);
        if !path.exists() {
            return Err(HarmoniumError::PlaylistNotFound(name.to_string()));
        }
        fs::remove_file(&path).map_err(|source| HarmoniumError::io(path.clone(), source))?;
        tracing::debug!(name = %name, path = %path.display(), "playlist deleted");
        Ok(())
    }

    /// Rename a saved playlist file, refusing to overwrite an existing target.
    ///
    /// The move runs under the same per-store write lock as `save` and
    /// `delete`, so a concurrent autosave can never observe a half-renamed
    /// directory. Both names are validated before this method can be called.
    pub fn rename_playlist(
        &self,
        old_name: &PlaylistName,
        new_name: &PlaylistName,
    ) -> DomainResult<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let source = self.file_path(old_name);
        let destination = self.file_path(new_name);
        if !source.exists() {
            return Err(HarmoniumError::PlaylistNotFound(old_name.to_string()));
        }
        if destination.exists() {
            return Err(HarmoniumError::PlaylistAlreadyExists(new_name.to_string()));
        }
        fs::rename(&source, &destination)
            .map_err(|source_error| HarmoniumError::io(source.clone(), source_error))?;
        tracing::debug!(
            old_name = %old_name,
            new_name = %new_name,
            path = %destination.display(),
            "playlist renamed"
        );
        Ok(())
    }

    /// Rewrite every saved playlist so references to `old` now point at `new`.
    ///
    /// Runs under the write lock, mirroring the other mutating store methods,
    /// and returns a typed outcome describing the documents touched. Every
    /// document is read and rendered before any target is staged, then all staged
    /// documents are replaced as one rollback-aware batch. A line matches when its
    /// resolved lexical [`crate::track::TrackLocation`] equals the normalized
    /// old path; resolving relative lines against the store directory matches
    /// how [`crate::playlist::m3u::parse_m3u`] reads them. Matching is IO-free so symlink spellings
    /// retain their identity.
    pub fn rewrite_path_in_all(&self, old: &Path, new: &Path) -> DomainResult<RewriteOutcome> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let old = crate::track::TrackLocation::local(old);
        let outcome =
            self.rewrite_documents(|contents| m3u::rewrite_path(contents, &self.dir, &old, new))?;
        tracing::info!(
            touched = outcome.touched(),
            old = ?old,
            new = ?new,
            "playlist paths rewritten"
        );
        Ok(outcome)
    }

    /// Restore the exact documents changed by a previous rewrite operation.
    ///
    /// This is used by the file-rename worker when the later audio-file rename
    /// fails. It intentionally does not rediscover matches, so rollback cannot
    /// touch an unrelated playlist or depend on the renamed file still being
    /// resolvable through the filesystem.
    pub fn rollback_rewrite(&self, outcome: &RewriteOutcome) -> DomainResult<()> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let pending = outcome
            .documents
            .iter()
            .map(|document| PendingRewrite {
                path: document.path.clone(),
                original: document.rewritten.clone(),
                rewritten: document.original.clone(),
            })
            .collect();
        let _ = commit_rewrites(pending)?;
        Ok(())
    }

    /// Update the EXTINF label of every saved playlist referencing `path`,
    /// setting the trailing display label to `new_title` while preserving the
    /// existing duration. Lines are matched against the normalized lexical
    /// form of `path` exactly like [`rewrite_path_in_all`], so a playlist that
    /// still spells the file by a relative path is rewritten when it resolves
    /// to the same identity.
    ///
    /// Returns a typed outcome describing the documents touched. A playlist whose
    /// EXTINF already carries the new title is left untouched (byte-for-byte equal
    /// after the rewrite), so repeated updates are cheap.
    pub fn update_extinf_title(
        &self,
        path: &Path,
        new_title: &str,
    ) -> DomainResult<RewriteOutcome> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let target = crate::track::TrackLocation::local(path);
        let outcome = self.rewrite_documents(|contents| {
            m3u::rewrite_extinf_title(
                contents,
                ExtinfTarget::Local {
                    base_dir: &self.dir,
                    location: &target,
                },
                new_title,
            )
        })?;
        tracing::info!(
            touched = outcome.touched(),
            path = ?path,
            new_title,
            "playlist EXTINF titles updated"
        );
        Ok(outcome)
    }

    /// Update the EXTINF label of every saved playlist whose body line
    /// matches the given stream URL exactly.
    ///
    /// Stream entries are matched by URL string equality, ignoring trailing
    /// whitespace, so a playlist that carries the canonical URL is rewritten
    /// regardless of how it was originally written. Returns the number of
    /// playlist documents touched, mirroring [`Self::update_extinf_title`].
    pub fn update_stream_extinf_title(
        &self,
        url: &url::Url,
        new_title: &str,
    ) -> DomainResult<RewriteOutcome> {
        let _guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let target = url.as_str().to_string();

        let outcome = self.rewrite_documents(|contents| {
            m3u::rewrite_extinf_title(contents, ExtinfTarget::Stream(&target), new_title)
        })?;
        tracing::info!(
            touched = outcome.touched(),
            url = %crate::net::safe_url(url),
            new_title,
            "stream EXTINF titles updated"
        );
        Ok(outcome)
    }

    fn rewrite_documents<F>(&self, mut rewrite: F) -> DomainResult<RewriteOutcome>
    where
        F: FnMut(&str) -> String,
    {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(RewriteOutcome {
                    documents: Vec::new(),
                });
            }
            Err(error) => return Err(HarmoniumError::io(self.dir.clone(), error)),
        };

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| HarmoniumError::io(self.dir.clone(), source))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some(PLAYLIST_EXTENSION) {
                paths.push(path);
            }
        }
        paths.sort();

        // Read and render every document before staging or replacing any
        // target. A malformed or unreadable later playlist therefore leaves
        // every original document untouched.
        let mut pending = Vec::new();
        for path in paths {
            let original = fs::read_to_string(&path)
                .map_err(|source| HarmoniumError::io(path.clone(), source))?;
            let rewritten = rewrite(&original);
            if rewritten != original {
                pending.push(PendingRewrite {
                    path,
                    original,
                    rewritten,
                });
            }
        }

        commit_rewrites(pending)
    }

    /// Canonical file location of a validated playlist name.
    fn file_path(&self, name: &PlaylistName) -> PathBuf {
        self.dir.join(format!("{name}.{PLAYLIST_EXTENSION}"))
    }
}

impl PlaylistRepository for PlaylistStore {
    fn save(&self, name: &PlaylistName, playlist: &Playlist) -> DomainResult<PathBuf> {
        PlaylistStore::save(self, name, playlist)
    }

    fn save_rendered(&self, name: &PlaylistName, contents: &str) -> DomainResult<PathBuf> {
        PlaylistStore::save_rendered(self, name, contents)
    }

    fn load(&self, name: &PlaylistName) -> DomainResult<Playlist> {
        PlaylistStore::load(self, name)
    }

    fn list_names(&self) -> DomainResult<Vec<String>> {
        PlaylistStore::list_names(self)
    }

    fn delete(&self, name: &PlaylistName) -> DomainResult<()> {
        PlaylistStore::delete(self, name)
    }

    fn rename_playlist(
        &self,
        old_name: &PlaylistName,
        new_name: &PlaylistName,
    ) -> DomainResult<()> {
        PlaylistStore::rename_playlist(self, old_name, new_name)
    }

    fn rewrite_path_in_all(&self, old: &Path, new: &Path) -> DomainResult<RewriteOutcome> {
        PlaylistStore::rewrite_path_in_all(self, old, new)
    }

    fn rollback_rewrite(&self, outcome: &RewriteOutcome) -> DomainResult<()> {
        PlaylistStore::rollback_rewrite(self, outcome)
    }

    fn update_extinf_title(&self, path: &Path, new_title: &str) -> DomainResult<RewriteOutcome> {
        PlaylistStore::update_extinf_title(self, path, new_title)
    }

    fn update_stream_extinf_title(
        &self,
        url: &url::Url,
        new_title: &str,
    ) -> DomainResult<RewriteOutcome> {
        PlaylistStore::update_stream_extinf_title(self, url, new_title)
    }
}

/// Stage every replacement and then commit the complete batch. If a commit
/// fails after one or more targets were installed, restore those targets from
/// their in-memory snapshots before returning the original failure.
fn commit_rewrites(pending: Vec<PendingRewrite>) -> DomainResult<RewriteOutcome> {
    let mut staged: Vec<(PendingRewrite, StagedReplacement)> = Vec::with_capacity(pending.len());
    for document in pending {
        let replacement = stage_replacement(&document.path, document.rewritten.as_bytes())
            .map_err(|source| HarmoniumError::io(document.path.clone(), source))?;
        staged.push((document, replacement));
    }

    let mut committed = Vec::new();
    for index in 0..staged.len() {
        let commit_result = staged[index].1.commit();
        if let Err(error) = commit_result {
            if staged[index].1.committed() {
                committed.push(index);
            }
            let primary = HarmoniumError::io(staged[index].0.path.clone(), error);
            return Err(match rollback_committed(&mut staged, &committed) {
                Some((rollback_path, rollback_error)) => {
                    HarmoniumError::Rollback(RollbackError::new(
                        "playlist-rewrite",
                        primary,
                        HarmoniumError::io(rollback_path, rollback_error),
                    ))
                }
                None => primary,
            });
        }
        committed.push(index);
    }

    let documents = staged.into_iter().map(|(document, _)| document).collect();
    Ok(RewriteOutcome { documents })
}

fn rollback_committed(
    staged: &mut [(PendingRewrite, StagedReplacement)],
    committed: &[usize],
) -> Option<(PathBuf, std::io::Error)> {
    let mut first_error = None;
    for &index in committed.iter().rev() {
        let document = &staged[index].0;
        if let Err(error) = atomic_replace(&document.path, document.original.as_bytes()) {
            first_error.get_or_insert((document.path.clone(), error));
        }
    }
    first_error
}

/// Reject names that could escape the playlists directory or vanish.
///
/// Only the final component is ever user supplied, so separators, dot
/// segments and control characters are the whole attack surface.
fn validate_playlist_name(name: &str) -> Result<()> {
    if !is_valid_path_component(name) || !is_platform_valid_playlist_name(name) {
        return Err(HarmoniumError::InvalidPlaylistName(name.to_string()));
    }
    Ok(())
}

/// Apply the platform restrictions that are not represented by Unix path
/// parsing. The portable policy rejects Windows-invalid punctuation and DOS
/// device names even when Harmonium is running on Unix, so saved playlists can
/// move between supported platforms without changing their identity.
fn is_platform_valid_playlist_name(name: &str) -> bool {
    !name.ends_with('.')
        && !name
            .chars()
            .any(|character| matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        && !is_reserved_windows_device_name(name)
}

fn is_reserved_windows_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::metadata::TrackMetadata;
    use crate::playlist::m3u::{
        ExtinfLabel, ExtinfTarget, LineKind, lex_document, line_kind, parse_m3u, render_document,
        rewrite_path as rewrite_path_lines,
    };
    use crate::test_support::TestTempDir;
    use crate::track::Track;
    use std::time::Duration;

    type TempDir = TestTempDir;

    fn queue(paths: &[&str]) -> Playlist {
        let mut playlist = Playlist::new();
        playlist.extend(paths.iter().map(|path| Track::local(*path)));
        playlist
    }

    fn playlist_name(name: &str) -> PlaylistName {
        PlaylistName::try_from(name).expect("valid playlist name")
    }

    fn rewrite_extinf_titles(
        contents: &str,
        dir: &Path,
        target: &crate::track::TrackLocation,
        new_title: &str,
    ) -> String {
        crate::playlist::m3u::rewrite_extinf_title(
            contents,
            ExtinfTarget::Local {
                base_dir: dir,
                location: target,
            },
            new_title,
        )
    }

    #[test]
    fn save_and_load_round_trips_plain_paths() {
        let temp = TempDir::new("store-roundtrip");
        let store = PlaylistStore::for_dir(temp.path());

        let saved = store
            .save(&playlist_name("evening"), &queue(&["/a.flac", "/b.mp3"]))
            .expect("save");
        assert_eq!(saved, temp.path().join("evening.m3u8"));

        let loaded = store.load(&playlist_name("evening")).expect("load");
        let paths: Vec<PathBuf> = loaded
            .tracks()
            .iter()
            .filter_map(|t| t.path().map(Path::to_path_buf))
            .collect();
        assert_eq!(
            paths,
            ["/a.flac", "/b.mp3"]
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn filesystem_store_is_usable_through_the_repository_boundary() {
        fn save_and_load(repository: &impl PlaylistRepository, name: &PlaylistName) -> Playlist {
            repository
                .save(name, &Playlist::new())
                .expect("save through repository");
            repository.load(name).expect("load through repository")
        }

        let temp = TempDir::new("repository-boundary");
        let store = PlaylistStore::for_dir(temp.path());
        let loaded = save_and_load(&store, &playlist_name("through-trait"));

        assert!(loaded.is_empty());
        assert_eq!(
            store.list_names().expect("list through concrete API"),
            vec!["through-trait"]
        );
    }

    #[test]
    fn extinf_metadata_survives_a_round_trip_as_display_fallbacks() {
        let temp = TempDir::new("store-extinf");
        let store = PlaylistStore::for_dir(temp.path());

        let mut playlist = Playlist::new();
        let mut track = Track::local("/music/song.mp3");
        track.set_metadata(TrackMetadata {
            title: "Tom Sawyer".into(),
            title_tagged: true,
            artist: "Rush".into(),
            album: "Moving Pictures".into(),
            track_number: Some(1),
            duration: Duration::from_millis(253_400),
            bitrate: Some(320),
            sample_rate: Some(44100),
            codec: "MP3".into(),
            format: "MP3".into(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        });
        playlist.extend([track]);

        store
            .save(&playlist_name("tagged"), &playlist)
            .expect("save");
        let raw = fs::read_to_string(temp.path().join("tagged.m3u8")).expect("read");
        assert!(raw.starts_with("#EXTM3U\n"));
        // The EXTINF label is the lofty Title tag only; the artist is no
        // longer concatenated to it.
        assert!(raw.contains("#EXTINF:253,Tom Sawyer\n"));
        // Rounding truncates toward zero, never invents extra seconds
        assert!(!raw.contains("#EXTINF:254"));

        let loaded = store.load(&playlist_name("tagged")).expect("load");
        // The reloaded track keeps the path and its EXTINF label, which the
        // parser promotes to a tagged title so the playlist row matches what
        // the original M3U described. The lofty snapshot will replace it
        // once the metadata worker delivers a real one.
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.tracks()[0].display_name(), "Tom Sawyer");
    }

    #[test]
    fn zero_duration_serializes_as_unknown_not_zero() {
        let temp = TempDir::new("store-zero-duration");
        let store = PlaylistStore::for_dir(temp.path());

        let mut playlist = Playlist::new();
        let mut track = Track::local("/music/untimed.flac");
        track.set_metadata(TrackMetadata {
            title: "Untimed".into(),
            title_tagged: true,
            artist: "X".into(),
            album: "Y".into(),
            track_number: None,
            duration: Duration::ZERO,
            bitrate: None,
            sample_rate: None,
            codec: "FLAC".into(),
            format: "FLAC".into(),
            album_artist: None,
            disc_number: None,
            genre: None,
            year: None,
            composer: None,
            comment: None,
        });
        playlist.extend([track]);

        store.save(&playlist_name("flac"), &playlist).expect("save");
        let raw = fs::read_to_string(temp.path().join("flac.m3u8")).expect("read");
        // Zero duration is treated as unknown (-1) and the label falls back to
        // the display name (the tagged title), never a misleading "0".
        assert!(
            raw.contains("#EXTINF:-1,Untimed\n"),
            "zero duration must be unknown, got: {raw}"
        );
    }

    #[test]
    fn save_escapes_line_breaks_in_tagged_extinf_titles() {
        let temp = TempDir::new("store-extinf-title-break");
        let mut playlist = Playlist::new();
        let mut track = Track::local("/music/song.mp3");
        track.set_title("Title\r\nInjected");
        playlist.extend([track]);

        PlaylistStore::for_dir(temp.path())
            .save(&playlist_name("safe"), &playlist)
            .expect("save");

        let raw = fs::read_to_string(temp.path().join("safe.m3u8")).expect("read");
        assert!(raw.contains("#EXTINF:-1,Title\\r\\nInjected\n"));
        assert_eq!(raw.lines().count(), 3);
    }

    #[test]
    fn relative_entries_resolve_against_the_store_directory() {
        let temp = TempDir::new("store-relative");
        let dir = temp.path();

        fs::create_dir_all(dir.join("sub")).expect("dir");
        let store = PlaylistStore::for_dir(dir);
        fs::write(
            dir.join("portable.m3u8"),
            "#EXTM3U\n#EXTINF:-1,x\nlocal.wav\nsub/inner.ogg\n",
        )
        .expect("fixture");

        let loaded = store.load(&playlist_name("portable")).expect("load");
        assert_eq!(
            loaded.tracks()[0].path(),
            Some(dir.join("local.wav").as_path())
        );
        assert_eq!(
            loaded.tracks()[1].path(),
            Some(dir.join("sub/inner.ogg").as_path())
        );
    }

    #[test]
    fn relative_playlist_entries_use_the_same_lexical_identity_as_direct_adds() {
        let temp = TempDir::new("store-lexical-identity");
        let dir = temp.path();
        let store = PlaylistStore::for_dir(dir);
        fs::write(dir.join("lexical.m3u8"), "#EXTM3U\nnested/../song.mp3\n")
            .expect("playlist fixture");

        let loaded = store.load(&playlist_name("lexical")).expect("load");
        let direct = Track::local(dir.join("song.mp3"));

        assert_eq!(loaded.tracks()[0].track_location(), direct.track_location());
    }

    #[test]
    fn blank_lines_and_unknown_directives_are_ignored() {
        let temp = TempDir::new("store-directives");
        fs::write(
            temp.path().join("messy.m3u8"),
            "\n#EXTM3U\n#PLAYLIST:Evening mix\n   \n/one.mp3\n#SOMETHING:else\n/two.mp3",
        )
        .expect("fixture");

        let loaded = PlaylistStore::for_dir(temp.path())
            .load(&playlist_name("messy"))
            .expect("load");

        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn loading_a_missing_playlist_reports_io_not_found() {
        let temp = TempDir::new("store-missing");

        let error = PlaylistStore::for_dir(temp.path())
            .load(&playlist_name("ghost"))
            .expect_err("missing playlist");

        match error {
            HarmoniumError::Io { source, path } => {
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
                assert!(path.ends_with("ghost.m3u8"));
            }
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn rename_moves_the_document_and_refuses_clobbering() {
        let temp = TempDir::new("store-rename");
        let store = PlaylistStore::for_dir(temp.path());

        store
            .save(&playlist_name("old"), &queue(&["/a.mp3"]))
            .expect("save");

        store
            .rename_playlist(&playlist_name("old"), &playlist_name("new"))
            .expect("rename");
        assert!(!temp.path().join("old.m3u8").exists());
        assert_eq!(store.load(&playlist_name("new")).expect("load").len(), 1);

        store
            .save(&playlist_name("other"), &queue(&[]))
            .expect("second save");
        let clash = store
            .rename_playlist(&playlist_name("new"), &playlist_name("other"))
            .expect_err("target exists");
        match clash {
            HarmoniumError::PlaylistAlreadyExists(got) => assert_eq!(got, "other"),
            other => panic!("expected PlaylistAlreadyExists, got {other:?}"),
        }
        // Both originals survive a rejected rename
        assert!(temp.path().join("new.m3u8").exists());
        assert!(temp.path().join("other.m3u8").exists());

        let missing = store
            .rename_playlist(&playlist_name("ghost"), &playlist_name("anywhere"))
            .expect_err("no source");
        match missing {
            HarmoniumError::PlaylistNotFound(got) => assert_eq!(got, "ghost"),
            other => panic!("expected PlaylistNotFound, got {other:?}"),
        }
    }

    #[test]
    fn dangerous_names_are_rejected_before_any_io_happens() {
        let temp = TempDir::new("store-names");

        for bad in [
            "",
            "  ",
            "../escape",
            "with/slash",
            "back\\slash",
            ".hidden",
            ".",
            "..",
            "trailing ",
            "bad\tname",
            "bad\nname",
            "\0nul",
        ] {
            let error = PlaylistName::try_from(bad).expect_err(bad);
            assert!(
                matches!(error, HarmoniumError::InvalidPlaylistName(ref got) if got == bad),
                "name {bad:?} produced {error}"
            );
        }

        // Conversion happens before the store can perform any filesystem IO.
        let created: Vec<_> = fs::read_dir(temp.path()).expect("listing").collect();
        assert!(created.is_empty(), "rejected names must not create files");
    }

    #[test]
    fn traversal_in_old_and_new_names_fails_before_any_io() {
        let temp = TempDir::new("store-traversal");
        let playlist_dir = temp.path().join("playlists");
        let _store = PlaylistStore::for_dir(&playlist_dir);

        let old = PlaylistName::try_from("../outside").expect_err("unsafe old name");
        let new = PlaylistName::try_from("inside/../target").expect_err("unsafe new name");

        assert!(matches!(old, HarmoniumError::InvalidPlaylistName(_)));
        assert!(matches!(new, HarmoniumError::InvalidPlaylistName(_)));
        assert!(
            !playlist_dir.exists(),
            "rejected names must not create the store directory"
        );
    }

    #[test]
    fn platform_invalid_playlist_names_are_rejected() {
        for bad in [
            "/absolute",
            "C:\\playlists\\name",
            "C:/playlists/name",
            "with:colon",
            "with*wildcard",
            "with?mark",
            "with\"quote",
            "with<angle>",
            "with|pipe",
            "name.",
            "CON.txt",
            "nul",
            "bad\u{0007}name",
            ".",
            "..",
        ] {
            assert!(
                matches!(
                    PlaylistName::try_from(bad),
                    Err(HarmoniumError::InvalidPlaylistName(ref got)) if got == bad
                ),
                "name {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn playlist_name_display_and_as_ref_preserve_unicode() {
        let name = PlaylistName::try_from("日本語-🎵").expect("unicode name");
        let owned = PlaylistName::try_from("別名".to_string()).expect("owned unicode name");
        assert_eq!(name.to_string(), "日本語-🎵");
        assert_eq!(name.as_ref(), "日本語-🎵");
        assert_eq!(owned.as_str(), "別名");
    }

    #[test]
    fn store_operations_accept_validated_unicode_names() {
        let temp = TempDir::new("store-unicode-name");
        let store = PlaylistStore::for_dir(temp.path());
        let name = PlaylistName::try_from("日本語-🎵").expect("unicode name");

        let saved = store.save(&name, &queue(&[])).expect("unicode name");

        assert_eq!(saved, temp.path().join("日本語-🎵.m3u8"));
        assert_eq!(store.load(&name).expect("load unicode name").len(), 0);
        store
            .rename_playlist(&name, &playlist_name("別名"))
            .expect("rename unicode name");
        store
            .delete(&playlist_name("別名"))
            .expect("delete unicode name");
    }

    #[test]
    fn unicode_playlist_names_are_valid_single_components() {
        let temp = TempDir::new("store-unicode-name");
        let store = PlaylistStore::for_dir(temp.path());
        let name = playlist_name("日本語-🎵");

        let saved = store.save(&name, &queue(&[])).expect("unicode name");

        assert_eq!(saved, temp.path().join("日本語-🎵.m3u8"));
        assert_eq!(store.load(&name).expect("load unicode name").len(), 0);
    }

    #[test]
    fn saving_creates_missing_directories_lazily_and_overwrites_cleanly() {
        let temp = TempDir::new("store-lazy");
        let nested = temp.path().join("deep/playlists");
        let store = PlaylistStore::for_dir(&nested);

        store
            .save(&playlist_name("first"), &queue(&["/a.mp3"]))
            .expect("first save");
        assert!(nested.is_dir());

        store
            .save(&playlist_name("first"), &queue(&["/b.mp3"]))
            .expect("overwrite");
        assert_eq!(store.load(&playlist_name("first")).expect("load").len(), 1);
        // The reloaded entry picks up the EXTINF label the serializer wrote
        // (the full file name with extension), so the display name reflects
        // what is on disk rather than collapsing to the stem.
        assert_eq!(
            store.load(&playlist_name("first")).expect("load").tracks()[0].display_name(),
            "b.mp3"
        );
    }

    #[test]
    fn system_store_points_at_the_data_playlists_subdirectory() {
        let paths = Paths::for_home(Path::new("/home/u"));
        let store = PlaylistStore::from_paths(&paths);

        assert_eq!(
            store.directory(),
            Path::new("/home/u/.local/share/harmonium/playlists")
        );
    }

    #[test]
    fn list_names_returns_only_m3u8_stems_sorted() {
        let temp = TempDir::new("store-list");
        fs::write(temp.path().join("beta.m3u8"), "#EXTM3U\n").expect("beta");
        fs::write(temp.path().join("Alpha.m3u8"), "#EXTM3U\n").expect("Alpha");
        fs::write(temp.path().join("alpha.m3u8"), "#EXTM3U\n").expect("alpha");
        fs::write(temp.path().join("Beta.m3u8"), "#EXTM3U\n").expect("Beta");
        // Non playlist files must be ignored
        fs::write(temp.path().join("c.txt"), "nope").expect("c");

        let store = PlaylistStore::for_dir(temp.path());

        assert_eq!(
            store.list_names().expect("playlist directory is readable"),
            vec![
                "Alpha".to_string(),
                "Beta".to_string(),
                "alpha".to_string(),
                "beta".to_string(),
            ]
        );
    }

    #[test]
    fn list_names_uses_natural_order_with_lexical_ties() {
        let temp = TempDir::new("store-natural-order");
        let store = PlaylistStore::for_dir(temp.path());
        for name in [
            "Pearl Jam (copy 10)",
            "Pearl Jam (copy 2)",
            "Pearl Jam",
            "Pearl Jam (copy 1)",
        ] {
            fs::write(temp.path().join(format!("{name}.m3u8")), "#EXTM3U\n")
                .expect("playlist fixture");
        }

        assert_eq!(
            store.list_names().expect("playlist directory is readable"),
            vec![
                "Pearl Jam".to_string(),
                "Pearl Jam (copy 1)".to_string(),
                "Pearl Jam (copy 2)".to_string(),
                "Pearl Jam (copy 10)".to_string(),
            ]
        );
    }

    #[test]
    fn list_names_is_empty_when_nothing_is_saved() {
        let temp = TempDir::new("store-list-empty");
        let store = PlaylistStore::for_dir(temp.path().join("not-created"));

        assert!(
            store
                .list_names()
                .expect("empty playlist directory is readable")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_names_skips_non_utf8_stems() {
        use std::os::unix::ffi::OsStringExt;

        let temp = TempDir::new("store-list-nonutf8");
        fs::write(temp.path().join("good.m3u8"), "#EXTM3U\n").expect("good");
        // A file whose stem is invalid UTF-8 must be skipped, not crash
        let bad = std::ffi::OsString::from_vec(vec![0xff, 0xfe, b'.', b'm', b'3', b'u', b'8']);
        fs::write(temp.path().join(bad), "#EXTM3U\n").expect("bad");

        let store = PlaylistStore::for_dir(temp.path());

        assert_eq!(
            store.list_names().expect("playlist directory is readable"),
            vec!["good".to_string()]
        );
    }

    #[test]
    fn list_names_returns_path_scoped_io_failure_for_an_unreadable_root() {
        let temp = TempDir::new("store-list-failure");
        let root = temp.path().join("not-a-directory");
        fs::write(&root, "fixture").expect("file fixture");
        let store = PlaylistStore::for_dir(&root);

        let error = store
            .list_names()
            .expect_err("a file cannot be listed as a directory");
        match error {
            HarmoniumError::Io { path, source } => {
                assert_eq!(path, root);
                assert_eq!(source.kind(), std::io::ErrorKind::NotADirectory);
            }
            other => panic!("expected path-scoped IO error, got {other:?}"),
        }
    }

    #[test]
    fn rewrite_returns_path_scoped_io_failure_for_an_unreadable_root() {
        let temp = TempDir::new("store-rewrite-failure");
        let root = temp.path().join("not-a-directory");
        fs::write(&root, "fixture").expect("file fixture");
        let store = PlaylistStore::for_dir(&root);

        let error = store
            .rewrite_path_in_all(Path::new("/old.mp3"), Path::new("/new.mp3"))
            .expect_err("a file cannot be listed as a directory");
        match error {
            HarmoniumError::Io { path, source } => {
                assert_eq!(path, root);
                assert_eq!(source.kind(), std::io::ErrorKind::NotADirectory);
            }
            other => panic!("expected path-scoped IO error, got {other:?}"),
        }
    }

    #[test]
    fn delete_removes_the_saved_file() {
        let temp = TempDir::new("store-delete");
        let store = PlaylistStore::for_dir(temp.path());
        store
            .save(&playlist_name("gone"), &queue(&["/x.mp3"]))
            .expect("save");

        store.delete(&playlist_name("gone")).expect("delete");
        assert!(!temp.path().join("gone.m3u8").exists());

        let missing = store
            .delete(&playlist_name("gone"))
            .expect_err("already removed");
        match missing {
            HarmoniumError::PlaylistNotFound(got) => assert_eq!(got, "gone"),
            other => panic!("expected PlaylistNotFound, got {other:?}"),
        }
    }

    #[test]
    fn parse_m3u_routes_urls_to_stream_tracks_with_extinf_as_title() {
        let contents = "#EXTM3U\n#EXTINF:240,Black - Pearl Jam\n/black.mp3\n\
        https://example.com/stream.m3u8\n#EXTINF:100,Some Song\nlocal.mp3\n";
        let parsed = parse_m3u(contents, Path::new("/music")).expect("parse ok");

        // URLs are no longer skipped: they become stream tracks alongside the
        // two local entries. The total queue length grows by one compared
        // with the old local-only behavior.
        assert_eq!(parsed.len(), 3);
        // Local tracks honour the EXTINF label because parsing promotes it to
        // a tagged title, mirroring how third-party m3u8 files describe
        // their entries. The file stem is the fallback only when no EXTINF
        // is present.
        assert_eq!(parsed.tracks()[0].display_name(), "Black - Pearl Jam");
        // The stream entry has no preceding EXTINF label, so it falls back
        // to the URL hostname — exactly as the spec mandates for streams
        // whose metadata is missing.
        assert_eq!(parsed.tracks()[1].display_name(), "example.com");
        assert!(parsed.tracks()[1].is_stream());
        assert_eq!(
            parsed.tracks()[1].display_location(),
            "https://example.com/stream.m3u8"
        );
        // The trailing local entry also picks up its EXTINF label.
        assert_eq!(parsed.tracks()[2].display_name(), "Some Song");
    }

    #[test]
    fn line_kind_accepts_only_supported_stream_urls() {
        for entry in [
            "http://stream.example.com/live",
            "https://stream.example.com/live",
        ] {
            assert_eq!(line_kind(entry), LineKind::Stream, "fixture {entry}");
        }

        for entry in [
            "file:///music/song.flac",
            "ftp://example.com/song.flac",
            "custom://example.com/song.flac",
            "harmonium-local-v1:https://example.com/song.flac",
            "http://[::1",
            "music/song.flac",
        ] {
            assert_eq!(line_kind(entry), LineKind::LocalPath, "fixture {entry}");
        }

        assert_eq!(line_kind("#EXTINF:-1,Title"), LineKind::Directive);
        assert_eq!(line_kind(""), LineKind::Blank);
    }

    #[test]
    fn m3u_keeps_legacy_v1_prefix_paths_opaque() {
        let base = Path::new("/music");
        let contents = "#EXTM3U\n\
            harmonium-local-v1:ordinary/song.mp3\n\
            harmonium-local-v1:https://example.com/live\n";

        let parsed = parse_m3u(contents, base).expect("parse ok");

        assert_eq!(
            parsed.tracks()[0].path(),
            Some(Path::new("/music/harmonium-local-v1:ordinary/song.mp3"))
        );
        assert_eq!(
            parsed.tracks()[1].path(),
            Some(Path::new(
                "/music/harmonium-local-v1:https://example.com/live"
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn newline_in_local_filename_is_escaped_in_extinf_and_body_round_trips() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let temp = TempDir::new("store-newline-label");
        let path = PathBuf::from(OsString::from_vec(
            temp.path()
                .as_os_str()
                .as_bytes()
                .iter()
                .copied()
                .chain([
                    b'/', b'o', b'd', b'd', b'\n', b'n', b'a', b'm', b'e', b'.', b'm', b'p', b'3',
                ])
                .collect(),
        ));
        let mut playlist = Playlist::new();
        playlist.extend([Track::local(path.clone())]);
        let store = PlaylistStore::for_dir(temp.path());

        store
            .save(&playlist_name("newline"), &playlist)
            .expect("save");
        let raw = fs::read_to_string(temp.path().join("newline.m3u8")).expect("read");

        assert!(raw.contains("#EXTINF:-1,odd\\nname.mp3\n"));
        assert!(!raw.contains("#EXTINF:-1,odd\nname.mp3\n"));
        assert_eq!(raw.lines().count(), 3);

        let loaded = store.load(&playlist_name("newline")).expect("reload");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded.tracks()[0].path(), Some(path.as_path()));
        assert_eq!(loaded.tracks()[0].display_name(), "odd\\nname.mp3");
    }

    #[test]
    fn parse_m3u_keeps_unsupported_and_malformed_urls_out_of_stream_tracks() {
        let contents = "#EXTM3U\n\
            #PLAYLIST:Mixed\n\
            #EXTINF:-1,HTTP\n\
            http://stream.example.com/live\n\
            #EXTINF:-1,HTTPS\n\
            https://stream.example.com/live\n\
            #EXTINF:-1,File URL\n\
            file:///music/song.flac\n\
            #EXTINF:-1,FTP URL\n\
            ftp://example.com/song.flac\n\
            #EXTINF:-1,Unknown URL\n\
            custom://example.com/song.flac\n\
            #EXTINF:-1,Malformed URL\n\
            http://[::1\n\
            #EXTINF:-1,Relative path\n\
            music/song.flac\n";
        let base_dir = Path::new("/music");
        let parsed = parse_m3u(contents, base_dir).expect("parse ok");

        assert_eq!(parsed.len(), 7);
        assert!(parsed.tracks()[0].is_stream());
        assert!(parsed.tracks()[1].is_stream());
        for track in &parsed.tracks()[2..] {
            assert!(
                track.is_local(),
                "unsupported URL became a stream: {track:?}"
            );
        }
        assert_eq!(parsed.tracks()[2].display_name(), "File URL");
        assert_eq!(parsed.tracks()[5].display_name(), "Malformed URL");
        assert_eq!(
            parsed.tracks()[6].path(),
            Some(Path::new("/music/music/song.flac"))
        );
    }

    #[test]
    fn parse_m3u_accepts_bom_crlf_indentation_commas_and_missing_final_newline() {
        let contents = "\u{feff}#EXTM3U\r\n  #EXTINF:253,Title, with comma  \r\n\t song.mp3";
        let parsed = parse_m3u(contents, Path::new("/music")).expect("parse ok");

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.tracks()[0].display_name(), "Title, with comma");
        assert_eq!(
            parsed.tracks()[0].path(),
            Some(Path::new("/music/song.mp3"))
        );
    }

    #[test]
    fn local_extinf_rewrite_preserves_bom_crlf_indentation_and_missing_final_newline() {
        let temp = TempDir::new("store-extinf-format");
        let dir = temp.path();
        let original = "\u{feff}#EXTM3U\r\n  #EXTINF:253,Old title, artist  \r\n\t song.mp3";
        fs::write(dir.join("mix.m3u8"), original).expect("playlist fixture");
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");

        let touched = PlaylistStore::for_dir(dir)
            .update_extinf_title(dir.join("song.mp3").as_path(), "New\r\nInjected")
            .expect("title update");

        assert_eq!(touched.touched(), 1);
        assert_eq!(
            fs::read_to_string(dir.join("mix.m3u8")).expect("read"),
            "\u{feff}#EXTM3U\r\n  #EXTINF:253,New\\r\\nInjected  \r\n\t song.mp3"
        );
    }

    #[test]
    fn path_rewrite_preserves_line_endings_and_path_indentation() {
        let dir = Path::new("/music");
        let contents = "\u{feff}#EXTM3U\r\n  #EXTINF:-1,Song\r\n\t old.mp3  ";

        let rewritten = rewrite_path_lines(
            contents,
            dir,
            &crate::track::TrackLocation::local(dir.join("old.mp3")),
            Path::new("/music/new.mp3"),
        );

        assert_eq!(
            rewritten,
            "\u{feff}#EXTM3U\r\n  #EXTINF:-1,Song\r\n\t new.mp3  "
        );
    }

    #[test]
    fn path_rewrite_encodes_unsafe_legacy_body_replacements() {
        let dir = Path::new("/music");
        let contents = "#EXTM3U\nold.mp3\n";
        let new = PathBuf::from("/music/new\nname.mp3");

        let rewritten = rewrite_path_lines(
            contents,
            dir,
            &crate::track::TrackLocation::local(dir.join("old.mp3")),
            &new,
        );

        assert_eq!(
            rewritten,
            format!(
                "#EXTM3U\n{}\n",
                crate::track::TrackLocation::local("new\nname.mp3").to_persisted()
            )
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 48,
            failure_persistence: None,
            max_shrink_iters: 128,
            rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha,
            rng_seed: proptest::test_runner::RngSeed::Fixed(0x4933_3004),
            .. ProptestConfig::default()
        })]

        #[test]
        fn arbitrary_m3u_text_is_total_and_output_is_bounded(
            contents in prop::collection::vec(any::<char>(), 0..=512)
                .prop_map(|characters| characters.into_iter().collect::<String>())
        ) {
            let parsed = parse_m3u(&contents, Path::new("/music"))
                .expect("M3U parsing is non-failing for arbitrary text");

            prop_assert!(parsed.len() <= contents.lines().count());
        }

        #[test]
        fn arbitrary_utf8_m3u_lines_render_byte_identically(
            contents in prop::collection::vec(any::<char>(), 0..=512)
                .prop_map(|characters| characters.into_iter().collect::<String>())
        ) {
            let lines = lex_document(&contents);
            prop_assert_eq!(render_document(&lines), contents);
        }

        #[test]
        fn arbitrary_line_breaking_titles_stay_on_one_physical_line(
            new_title in prop::collection::vec(any::<char>(), 0..=128)
                .prop_map(|characters| characters.into_iter().collect::<String>())
        ) {
            let contents = "#EXTM3U\r\n  #EXTINF:-1,old, label\r\n song.mp3";
            let rewritten = rewrite_extinf_titles(
                contents,
                Path::new("/music"),
                &crate::track::TrackLocation::local("/music/song.mp3"),
                &new_title,
            );
            let lines = lex_document(&rewritten);

            prop_assert_eq!(lines.len(), 3);
            prop_assert_eq!(render_document(&lines), rewritten);
            prop_assert!(!lines[1].body.contains('\r'));
            prop_assert!(!lines[1].body.contains('\n'));
        }
    }

    #[test]
    fn rewrite_path_in_all_preserves_every_byte_outside_matched_lines() {
        let temp = TempDir::new("store-rewrite-golden");
        let dir = temp.path();
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");
        fs::write(dir.join("other.wav"), b"audio").expect("other fixture");

        let absolute = dir.join("song.mp3");
        let contents = format!(
            "#EXTM3U\n\
             #PLAYLIST:Evening mix\n\
             #EXTINF:253,Rush - Tom Sawyer\n\
             song.mp3\n\
             {}\n\
             ./song.mp3\n\
             #EXTINF:-1,Other\n\
             other.wav\n\
             https://example.com/stream.m3u8\n\
             file:///music/song.mp3\n\
             ftp://example.com/song.mp3\n\
             custom://example.com/song.mp3\n\
             http://[::1\n\
             \n\
             # trailing comment",
            absolute.display()
        );
        fs::write(dir.join("evening.m3u8"), &contents).expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let touched = store
            .rewrite_path_in_all(&dir.join("song.mp3"), &dir.join("hit.mp3"))
            .expect("rewrite succeeds");

        assert_eq!(touched.touched(), 1);
        let rewritten = fs::read_to_string(dir.join("evening.m3u8")).expect("read back");
        let absolute_new = dir.join("hit.mp3");
        assert_eq!(
            rewritten,
            format!(
                "#EXTM3U\n\
                 #PLAYLIST:Evening mix\n\
                 #EXTINF:253,Rush - Tom Sawyer\n\
                 hit.mp3\n\
                 {}\n\
                 hit.mp3\n\
                 #EXTINF:-1,Other\n\
                 other.wav\n\
                 https://example.com/stream.m3u8\n\
                 file:///music/song.mp3\n\
                 ftp://example.com/song.mp3\n\
                 custom://example.com/song.mp3\n\
                 http://[::1\n\
                 \n\
                 # trailing comment",
                absolute_new.display()
            ),
            "directives, EXTINF, URLs, blank lines and comments must survive verbatim"
        );
    }

    #[test]
    fn rewrite_path_in_all_leaves_unrelated_playlists_untouched() {
        let temp = TempDir::new("store-rewrite-untouched");
        let dir = temp.path();
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");
        let untouched = "#EXTM3U\n#EXTINF:-1,Mix\nother.wav\n";
        fs::write(dir.join("mix.m3u8"), untouched).expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let touched = store
            .rewrite_path_in_all(&dir.join("song.mp3"), &dir.join("hit.mp3"))
            .expect("rewrite succeeds");

        assert_eq!(touched.touched(), 0, "no playlist references the old path");
        assert_eq!(
            fs::read_to_string(dir.join("mix.m3u8")).expect("read back"),
            untouched
        );
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_path_in_all_does_not_follow_symlinks_for_identity() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new("store-rewrite-symlink");
        let dir = temp.path();
        fs::create_dir(dir.join("real")).expect("real directory");
        symlink(dir.join("real"), dir.join("link")).expect("directory symlink");
        fs::write(dir.join("real/song.mp3"), b"audio").expect("song fixture");
        fs::write(dir.join("mix.m3u8"), "#EXTM3U\nlink/song.mp3\n").expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let outcome = store
            .rewrite_path_in_all(&dir.join("real/song.mp3"), &dir.join("real/renamed.mp3"))
            .expect("rewrite succeeds");

        assert_eq!(outcome.touched(), 0);
        assert_eq!(
            fs::read_to_string(dir.join("mix.m3u8")).expect("read"),
            "#EXTM3U\nlink/song.mp3\n"
        );
    }

    #[test]
    fn rewrite_path_in_all_returns_zero_without_playlists() {
        let temp = TempDir::new("store-rewrite-empty");
        let store = PlaylistStore::for_dir(temp.path());

        let touched = store
            .rewrite_path_in_all(
                Path::new("/nowhere/song.mp3"),
                Path::new("/nowhere/hit.mp3"),
            )
            .expect("empty store is not an error");

        assert_eq!(touched.touched(), 0);
    }

    #[test]
    fn rewrite_path_in_all_aborts_on_an_unreadable_playlist() {
        let temp = TempDir::new("store-rewrite-broken");
        let dir = temp.path();
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");
        // A directory wearing the m3u8 extension cannot be read as text
        fs::create_dir(dir.join("broken.m3u8")).expect("broken fixture");

        let store = PlaylistStore::for_dir(dir);
        let error = store
            .rewrite_path_in_all(&dir.join("song.mp3"), &dir.join("hit.mp3"))
            .expect_err("unreadable playlist must abort the rewrite");

        match error {
            HarmoniumError::Io { source, path } => {
                assert_eq!(source.kind(), std::io::ErrorKind::IsADirectory);
                assert!(path.ends_with("broken.m3u8"));
            }
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[test]
    fn rewrite_path_in_all_keeps_previous_documents_on_malformed_input() {
        let temp = TempDir::new("store-rewrite-malformed");
        let dir = temp.path();
        let original = format!("#EXTM3U\n{}\n", dir.join("song.mp3").display());
        fs::write(dir.join("a.m3u8"), &original).expect("valid playlist");
        fs::write(dir.join("b.m3u8"), [0xff, 0xfe, 0xfd]).expect("malformed playlist");
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");

        let error = PlaylistStore::for_dir(dir)
            .rewrite_path_in_all(&dir.join("song.mp3"), &dir.join("hit.mp3"))
            .expect_err("invalid UTF-8 must abort before any replacement");
        assert!(
            matches!(error, HarmoniumError::Io { source, .. } if source.kind() == std::io::ErrorKind::InvalidData)
        );
        assert_eq!(
            fs::read(dir.join("a.m3u8")).expect("read valid playlist"),
            original.as_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rewrite_path_in_all_keeps_every_document_when_a_later_target_is_read_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new("store-rewrite-read-only");
        let dir = temp.path();
        let first = format!("#EXTM3U\n{}\n", dir.join("song.mp3").display());
        let second = format!("#EXTM3U\n{}\n", dir.join("song.mp3").display());
        fs::write(dir.join("a.m3u8"), &first).expect("first playlist");
        fs::write(dir.join("b.m3u8"), &second).expect("second playlist");
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");
        fs::set_permissions(dir.join("b.m3u8"), fs::Permissions::from_mode(0o444))
            .expect("read-only playlist");

        let error = PlaylistStore::for_dir(dir)
            .rewrite_path_in_all(&dir.join("song.mp3"), &dir.join("hit.mp3"))
            .expect_err("read-only target must abort the batch");
        assert!(
            matches!(error, HarmoniumError::Io { source, .. } if source.kind() == std::io::ErrorKind::PermissionDenied)
        );
        assert_eq!(
            fs::read(dir.join("a.m3u8")).expect("read first"),
            first.as_bytes()
        );
        assert_eq!(
            fs::read(dir.join("b.m3u8")).expect("read second"),
            second.as_bytes()
        );

        fs::set_permissions(dir.join("b.m3u8"), fs::Permissions::from_mode(0o644))
            .expect("restore playlist permissions");
    }

    #[test]
    fn rewrite_outcome_rolls_back_exact_documents_without_rediscovery() {
        let temp = TempDir::new("store-rewrite-rollback");
        let dir = temp.path();
        let playlist = dir.join("mix.m3u8");
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");
        let original = format!("#EXTM3U\n{}\n", dir.join("song.mp3").display());
        fs::write(&playlist, &original).expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let outcome = store
            .rewrite_path_in_all(&dir.join("song.mp3"), &dir.join("hit.mp3"))
            .expect("rewrite");
        assert_eq!(outcome.touched(), 1);
        assert!(
            fs::read_to_string(&playlist)
                .expect("rewritten playlist")
                .contains("hit.mp3")
        );

        store.rollback_rewrite(&outcome).expect("rollback");
        assert_eq!(
            fs::read_to_string(playlist).expect("restored playlist"),
            original
        );
    }

    /// When a file is renamed and the metadata editor has no Title tag, the
    /// EXTINF label of every saved playlist that references the old path
    /// must be replaced with the new file name (full filename including
    /// extension) while the duration stays untouched.
    #[test]
    fn update_extinf_title_rewrites_only_matched_paths() {
        let temp = TempDir::new("store-extinf-rename");
        let dir = temp.path();
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");
        fs::write(dir.join("other.mp3"), b"audio").expect("other fixture");
        fs::write(
            dir.join("evening.m3u8"),
            "#EXTM3U\n#PLAYLIST:Evening mix\n\
             #EXTINF:253,Rush - Tom Sawyer\nsong.mp3\n\
             #EXTINF:200,Black - Pearl Jam\nother.mp3\n\
             #EXTINF:-1,File URL\nfile:///music/song.mp3\n\
             #EXTINF:-1,FTP URL\nftp://example.com/song.mp3\n\
             #EXTINF:-1,Unknown URL\ncustom://example.com/song.mp3\n\
             #EXTINF:-1,Malformed URL\nhttp://[::1\n",
        )
        .expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let touched = store
            .update_extinf_title(&dir.join("song.mp3"), "song-renamed.mp3")
            .expect("title update");

        assert_eq!(
            touched.touched(),
            1,
            "exactly the matching playlist is touched"
        );
        let raw = fs::read_to_string(dir.join("evening.m3u8")).expect("read");
        // The duration stays intact; only the label after the comma changes.
        assert!(
            raw.contains("#EXTINF:253,song-renamed.mp3\n"),
            "the matched entry picks up the new file name, got: {raw}"
        );
        assert!(
            !raw.contains("Tom Sawyer\n"),
            "the old EXTINF label must not survive the rewrite"
        );
        // The unmatched line keeps its existing EXTINF label verbatim.
        assert!(
            raw.contains("#EXTINF:200,Black - Pearl Jam\n"),
            "the unrelated entry must not be touched, got: {raw}"
        );
        for label in ["File URL", "FTP URL", "Unknown URL", "Malformed URL"] {
            assert!(
                raw.contains(&format!("#EXTINF:-1,{label}\n")),
                "unsupported URL label must survive unchanged: {raw}"
            );
        }
        // The directive above the playlist survives untouched.
        assert!(raw.contains("#PLAYLIST:Evening mix\n"));
    }

    /// When the metadata editor writes a new Title tag, every saved playlist
    /// that references the file must pick up the new title on its EXTINF
    /// line, preserving the duration.
    #[test]
    fn update_extinf_title_propagates_metadata_writes() {
        let temp = TempDir::new("store-extinf-metadata");
        let dir = temp.path();
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");
        fs::write(
            dir.join("evening.m3u8"),
            "#EXTM3U\n#EXTINF:253,song.mp3\nsong.mp3\n",
        )
        .expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let touched = store
            .update_extinf_title(&dir.join("song.mp3"), "Tom Sawyer")
            .expect("title update");

        assert_eq!(touched.touched(), 1);
        let raw = fs::read_to_string(dir.join("evening.m3u8")).expect("read");
        assert!(
            raw.contains("#EXTINF:253,Tom Sawyer\n"),
            "the new title replaces the file name on the EXTINF line, got: {raw}"
        );
        // The path below stays untouched: only the label above it changes.
        assert!(
            raw.contains("song.mp3\n"),
            "the path line must survive the title update, got: {raw}"
        );
    }

    /// A playlist whose EXTINF already carries the requested title is left
    /// byte-for-byte untouched (the rewrite compares the buffer against the
    /// input before touching disk).
    #[test]
    fn update_extinf_title_is_a_noop_when_label_already_matches() {
        let temp = TempDir::new("store-extinf-noop");
        let dir = temp.path();
        fs::write(dir.join("song.mp3"), b"audio").expect("song fixture");
        let original = "#EXTM3U\n#EXTINF:253,Tom Sawyer\nsong.mp3\n";
        fs::write(dir.join("evening.m3u8"), original).expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let touched = store
            .update_extinf_title(&dir.join("song.mp3"), "Tom Sawyer")
            .expect("title update");

        assert_eq!(
            touched.touched(),
            0,
            "the playlist is unchanged so nothing is touched"
        );
        let after = fs::read_to_string(dir.join("evening.m3u8")).expect("read");
        assert_eq!(after, original, "the file must remain byte-identical");
    }

    /// An EXTINF line that lacks a comma (malformed input) is left alone:
    /// there is no label to rewrite without breaking the duration prefix.
    #[test]
    fn update_extinf_title_leaves_malformed_lines_untouched() {
        let temp = TempDir::new("store-extinf-malformed");
        let dir = temp.path();
        fs::write(dir.join("song.mp3"), b"audio").expect("source fixture");
        fs::write(dir.join("weird.m3u8"), "#EXTM3U\n#EXTINF:253\nsong.mp3\n")
            .expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let touched = store
            .update_extinf_title(&dir.join("song.mp3"), "Tom Sawyer")
            .expect("title update");

        assert_eq!(
            touched.touched(),
            0,
            "no label to rewrite means no write at all"
        );
        let after = fs::read_to_string(dir.join("weird.m3u8")).expect("read");
        assert!(after.contains("#EXTINF:253\n"));
    }

    #[test]
    fn extinf_label_rewrite_escapes_line_breaks_at_the_shared_boundary() {
        let line = lex_document("#EXTINF:253,old title\n")
            .into_iter()
            .next()
            .expect("one line");
        let rewritten = line
            .with_extinf_label(&ExtinfLabel::new("new\n#EXTINF:0,injected\rtitle"))
            .expect("EXTINF label");
        assert_eq!(
            render_document(&[rewritten]),
            "#EXTINF:253,new\\n#EXTINF:0,injected\\rtitle\n"
        );
    }

    /// Stream EXTINF titles are keyed by the canonical URL string. A rename
    /// of the EXTINF label that precedes a URL body line updates every
    /// saved playlist that references the URL.
    #[test]
    fn update_stream_extinf_title_rewrites_every_matching_playlist() {
        let temp = TempDir::new("store-stream-extinf");
        let dir = temp.path();
        // Two playlists, each referencing the same stream URL with the
        // original EXTINF label. A third entry points at a different
        // stream URL and must stay untouched.
        fs::write(
            dir.join("morning.m3u8"),
            "#EXTM3U\n#EXTINF:-1,Old Stream Title\n\
             https://stream.example.com/live\n",
        )
        .expect("playlist fixture");
        fs::write(
            dir.join("evening.m3u8"),
            "#EXTM3U\n#EXTINF:-1,Old Stream Title\n\
             https://stream.example.com/live\n\
             #EXTINF:-1,Other Stream\n\
             https://other.example.com/audio\n\
             #EXTINF:-1,File URL\n\
             file:///music/song.mp3\n\
             #EXTINF:-1,FTP URL\n\
             ftp://example.com/song.mp3\n\
             #EXTINF:-1,Unknown URL\n\
             custom://example.com/song.mp3\n\
             #EXTINF:-1,Malformed URL\n\
             http://[::1\n",
        )
        .expect("playlist fixture");

        let store = PlaylistStore::for_dir(dir);
        let url = url::Url::parse("https://stream.example.com/live").expect("valid url");
        let touched = store
            .update_stream_extinf_title(&url, "My Radio")
            .expect("title update");

        assert_eq!(
            touched.touched(),
            2,
            "both playlists referencing the URL are touched"
        );
        let morning = fs::read_to_string(dir.join("morning.m3u8")).expect("read");
        let evening = fs::read_to_string(dir.join("evening.m3u8")).expect("read");
        assert!(morning.contains("#EXTINF:-1,My Radio\n"));
        assert!(evening.contains("#EXTINF:-1,My Radio\n"));
        // The unrelated entry must keep its original label.
        assert!(evening.contains("#EXTINF:-1,Other Stream\n"));
        assert!(evening.contains("https://other.example.com/audio\n"));
        for label in ["File URL", "FTP URL", "Unknown URL", "Malformed URL"] {
            assert!(
                evening.contains(&format!("#EXTINF:-1,{label}\n")),
                "unsupported URL label must survive unchanged: {evening}"
            );
        }
    }

    #[test]
    fn stream_extinf_rewrite_preserves_bom_crlf_and_missing_final_newline() {
        let temp = TempDir::new("store-stream-extinf-format");
        let url = url::Url::parse("https://stream.example.com/live").expect("valid url");
        let original = "\u{feff}#EXTM3U\r\n  #EXTINF:-1,Old title, station  \r\n\t https://stream.example.com/live";
        fs::write(temp.path().join("radio.m3u8"), original).expect("playlist fixture");

        let touched = PlaylistStore::for_dir(temp.path())
            .update_stream_extinf_title(&url, "New\nTitle")
            .expect("title update");

        assert_eq!(touched.touched(), 1);
        assert_eq!(
            fs::read_to_string(temp.path().join("radio.m3u8")).expect("read"),
            "\u{feff}#EXTM3U\r\n  #EXTINF:-1,New\\nTitle  \r\n\t https://stream.example.com/live"
        );
    }

    /// A playlist mixing local files and stream URLs round-trips through
    /// the M3U8 store without losing the stream identity. The serializer
    /// writes the URL verbatim and the parser rebuilds a stream `Track`
    /// from the same URL on reload.
    #[test]
    fn mixed_local_and_stream_playlist_round_trips() {
        use crate::stream::{StreamKind, TrackSource};
        let temp = TempDir::new("store-mixed-roundtrip");
        let dir = temp.path();
        fs::write(dir.join("local.mp3"), b"audio").expect("local fixture");

        let store = PlaylistStore::for_dir(dir);
        let mut playlist = Playlist::new();
        playlist.extend([
            crate::track::Track::local(dir.join("local.mp3")),
            crate::track::Track::from_stream(
                url::Url::parse("https://stream.example.com/live").expect("valid url"),
                StreamKind::Http,
            ),
        ]);
        store
            .save(&playlist_name("mixed"), &playlist)
            .expect("save");

        let raw = fs::read_to_string(dir.join("mixed.m3u8")).expect("read");
        // Local file path appears verbatim
        assert!(raw.contains(&format!(
            "#EXTINF:-1,{}\n",
            dir.join("local.mp3").file_name().unwrap().to_string_lossy()
        )));
        // Stream URL is written as-is, no path normalisation
        assert!(raw.contains("https://stream.example.com/live\n"));

        let loaded = store.load(&playlist_name("mixed")).expect("load");
        assert_eq!(loaded.len(), 2);
        // Local entry keeps its path.
        assert_eq!(
            loaded.tracks()[0].path(),
            Some(dir.join("local.mp3").as_path())
        );
        // Stream entry keeps its URL and source kind.
        assert_eq!(
            loaded.tracks()[1].source(),
            &TrackSource::Stream {
                url: url::Url::parse("https://stream.example.com/live").expect("valid url"),
                kind: StreamKind::Http,
                prepared: None,
            }
        );
        assert!(loaded.tracks()[1].is_stream());
    }

    #[test]
    fn url_looking_local_path_stays_local_when_m3u_is_reloaded() {
        use crate::stream::StreamKind;

        let temp = TempDir::new("store-url-looking-local");
        let dir = temp.path();
        let url_text = "https://stream.example.com/live";
        let mut playlist = Playlist::new();
        playlist.extend([
            Track::local(url_text),
            Track::from_stream(
                url::Url::parse(url_text).expect("valid URL"),
                StreamKind::Http,
            ),
        ]);

        let store = PlaylistStore::for_dir(dir);
        store
            .save(&playlist_name("mixed"), &playlist)
            .expect("save");

        let raw = fs::read_to_string(dir.join("mixed.m3u8")).expect("read");
        assert!(raw.contains(
            "harmonium-local-v2:68747470733a2f2f73747265616d2e6578616d706c652e636f6d2f6c697665\n"
        ));
        assert!(raw.contains("https://stream.example.com/live\n"));

        let loaded = store.load(&playlist_name("mixed")).expect("load");
        assert!(loaded.tracks()[0].is_local());
        assert_eq!(
            loaded.tracks()[0].path(),
            Some(dir.join(url_text).as_path())
        );
        assert!(loaded.tracks()[1].is_stream());
        assert_eq!(loaded.tracks()[1].display_location(), url_text);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_local_paths_round_trip_through_m3u() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let temp = TempDir::new("store-nonutf8");
        let path = PathBuf::from(OsString::from_vec(
            temp.path()
                .as_os_str()
                .as_bytes()
                .iter()
                .copied()
                .chain([b'/', 0xff, b'.', b'm', b'p', b'3'])
                .collect(),
        ));
        let mut playlist = Playlist::new();
        playlist.extend([Track::local(path.clone())]);
        let store = PlaylistStore::for_dir(temp.path());

        store
            .save(&playlist_name("nonutf8"), &playlist)
            .expect("save");
        let raw = fs::read_to_string(temp.path().join("nonutf8.m3u8")).expect("read");
        assert!(raw.contains("harmonium-local-v2:"));

        let loaded = store.load(&playlist_name("nonutf8")).expect("load");
        assert_eq!(loaded.tracks()[0].path(), Some(path.as_path()));
    }
}
