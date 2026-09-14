//! Domain error types shared across Harmonium modules.

use std::error::Error;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use thiserror::Error;

/// Errors produced by Harmonium domain layers.
#[derive(Debug, Error)]
pub enum HarmoniumError {
    /// Filesystem or terminal IO failure.
    #[error("cannot complete operation at {path}: {source}")]
    Io {
        /// Resource whose operation failed.
        path: PathBuf,
        /// Underlying operating-system failure.
        #[source]
        source: io::Error,
    },
    /// No usable home directory could be resolved.
    #[error("home directory not found")]
    NoHomeDir,
    /// Playlist name violates the storage naming rules.
    #[error("invalid playlist name: {0}")]
    InvalidPlaylistName(String),
    /// A saved playlist with this name does not exist.
    #[error("playlist not found: {0}")]
    PlaylistNotFound(String),
    /// A saved playlist with this name already exists, so an overwrite is
    /// refused unless the caller explicitly asked for it.
    #[error("playlist already exists: {0}")]
    PlaylistAlreadyExists(String),
    /// A filesystem safety policy rejected an operation.
    #[error("filesystem safety check failed: {0}")]
    Safety(#[source] crate::filesystem::safety::SafetyError),
    /// An operation failed and restoring its previous state also failed.
    #[error("{0}")]
    Rollback(#[source] RollbackError),
}

impl HarmoniumError {
    /// Attach the path known by the current filesystem boundary to an IO error.
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

impl From<crate::filesystem::safety::SafetyError> for HarmoniumError {
    fn from(source: crate::filesystem::safety::SafetyError) -> Self {
        Self::Safety(source)
    }
}

/// Retains both the original operation failure and the failure to restore its
/// previous state. Each cause remains downcastable at trusted boundaries.
#[derive(Debug, Clone)]
pub struct RollbackError {
    operation: &'static str,
    primary: Arc<dyn Error + Send + Sync>,
    rollback: Arc<dyn Error + Send + Sync>,
}

impl RollbackError {
    /// Build an aggregate for one failed operation and its rollback attempt.
    pub fn new<P, R>(operation: &'static str, primary: P, rollback: R) -> Self
    where
        P: Error + Send + Sync + 'static,
        R: Error + Send + Sync + 'static,
    {
        Self {
            operation,
            primary: Arc::new(primary),
            rollback: Arc::new(rollback),
        }
    }

    /// Stable operation context retained for logs and tests.
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// Original operation failure.
    pub fn primary(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.primary.as_ref()
    }

    /// Failure encountered while restoring the previous state.
    pub fn rollback(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.rollback.as_ref()
    }
}

impl fmt::Display for RollbackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed: {}; rollback failed: {}",
            self.operation, self.primary, self.rollback
        )
    }
}

impl Error for RollbackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.primary.as_ref())
    }
}

/// Maximum diagnostic size crossing a worker-to-UI boundary.
pub const MAX_WORKER_DIAGNOSTIC_CHARS: usize = 256;

/// Typed failure envelope used when a worker result crosses into application
/// event plumbing. The source remains inspectable while its display is
/// bounded and privacy-safe.
#[derive(Debug, Clone)]
pub struct WorkerError {
    operation: &'static str,
    source: Arc<dyn Error + Send + Sync>,
}

impl WorkerError {
    /// Wrap a typed worker failure with its stable operation label.
    pub fn new<E>(operation: &'static str, source: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self {
            operation,
            source: Arc::new(source),
        }
    }

    /// Create a typed failure for a bounded, already-classified diagnostic.
    pub fn message(operation: &'static str, message: impl Into<String>) -> Self {
        Self::new(operation, DiagnosticError(message.into()))
    }

    /// Convert the `anyhow` result of a worker body at the event boundary.
    /// `anyhow::Error` intentionally does not implement `std::error::Error`,
    /// so its bounded diagnostic is retained in a small standard-error
    /// adapter while typed domain errors use [`Self::new`] directly.
    pub fn from_anyhow(operation: &'static str, source: anyhow::Error) -> Self {
        Self::new(operation, AnyhowDiagnostic(source.to_string()))
    }

    /// Stable operation context retained for logs and tests.
    pub const fn operation(&self) -> &'static str {
        self.operation
    }

    /// The original typed source, available for downcasting at trusted
    /// boundaries without exposing its raw display to the UI.
    pub fn source_error(&self) -> &(dyn Error + Send + Sync + 'static) {
        self.source.as_ref()
    }

    /// Bounded diagnostic suitable for a notification or a log field.
    pub fn diagnostic(&self) -> String {
        let raw = self.source.to_string();
        let without_os_code = raw.split(" (os error").next().unwrap_or(&raw);
        crate::net::normalize_diagnostic(without_os_code, MAX_WORKER_DIAGNOSTIC_CHARS)
    }
}

impl PartialEq for WorkerError {
    fn eq(&self, other: &Self) -> bool {
        self.operation == other.operation && self.diagnostic() == other.diagnostic()
    }
}

impl Eq for WorkerError {}

impl fmt::Display for WorkerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic())
    }
}

impl Error for WorkerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Result alias used by domain layers.
pub type Result<T> = std::result::Result<T, HarmoniumError>;

#[derive(Debug)]
struct DiagnosticError(String);

impl fmt::Display for DiagnosticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for DiagnosticError {}

#[derive(Debug)]
struct AnyhowDiagnostic(String);

impl fmt::Display for AnyhowDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for AnyhowDiagnostic {}

/// Result carried by worker completion events.
pub type WorkerResult<T> = std::result::Result<T, WorkerError>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{MetaField, MetadataError};

    #[test]
    fn io_display_includes_path_and_preserves_source() {
        let error = HarmoniumError::io(
            "/music/missing.mp3",
            io::Error::new(io::ErrorKind::NotFound, "file is absent"),
        );

        assert_eq!(
            error.to_string(),
            "cannot complete operation at /music/missing.mp3: file is absent"
        );
        let source = std::error::Error::source(&error)
            .expect("IO source is retained")
            .downcast_ref::<io::Error>()
            .expect("source remains an io::Error");
        assert_eq!(source.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn retained_safety_error_converts_without_using_removed_variants() {
        let error: HarmoniumError = crate::filesystem::safety::SafetyError::EmptyName.into();

        assert!(matches!(error, HarmoniumError::Safety(_)));
        assert_eq!(
            error.to_string(),
            "filesystem safety check failed: the file name cannot be empty"
        );
        assert!(matches!(
            HarmoniumError::NoHomeDir,
            HarmoniumError::NoHomeDir
                | HarmoniumError::InvalidPlaylistName(_)
                | HarmoniumError::PlaylistNotFound(_)
                | HarmoniumError::PlaylistAlreadyExists(_)
                | HarmoniumError::Safety(_)
                | HarmoniumError::Io { .. }
        ));
    }

    #[test]
    fn worker_error_preserves_typed_source_and_bounds_redacted_display() {
        let path = "/music/secret.mp3";
        let metadata = MetadataError::InvalidNumber {
            path: path.into(),
            field: MetaField::TrackNumber.label(),
            value: "not-a-number".into(),
        };
        let error = WorkerError::new("metadata-prefill", metadata);
        let rendered = error.to_string();

        assert_eq!(error.operation(), "metadata-prefill");
        assert!(rendered.contains(path));
        assert!(
            error
                .source_error()
                .downcast_ref::<MetadataError>()
                .is_some()
        );

        let secret = format!(
            "resolver failed https://user:password@example.com/live?access_token=secret {}",
            "x".repeat(400)
        );
        let bounded = WorkerError::message("stream-resolve", secret).to_string();
        assert!(bounded.chars().count() <= MAX_WORKER_DIAGNOSTIC_CHARS);
        assert!(!bounded.contains("password"));
        assert!(!bounded.contains("access_token=secret"));
        assert!(bounded.contains("<redacted>") || bounded.contains("example.com/live"));

        let uppercase = WorkerError::message(
            "stream-resolve",
            "resolver failed HTTPS://user:password@example.com/live",
        )
        .to_string();
        assert!(!uppercase.contains("user"));
        assert!(!uppercase.contains("password"));
        assert!(uppercase.contains("https://example.com/live"));
    }

    #[test]
    fn rollback_error_retains_both_typed_causes_and_context() {
        let primary = HarmoniumError::io(
            "/playlists/one.m3u8",
            io::Error::new(io::ErrorKind::PermissionDenied, "primary failure"),
        );
        let rollback = HarmoniumError::io(
            "/playlists/two.m3u8",
            io::Error::new(io::ErrorKind::ReadOnlyFilesystem, "rollback failure"),
        );
        let aggregate = RollbackError::new("playlist-rewrite", primary, rollback);

        assert_eq!(aggregate.operation(), "playlist-rewrite");
        assert!(
            aggregate
                .primary()
                .downcast_ref::<HarmoniumError>()
                .is_some()
        );
        assert!(
            aggregate
                .rollback()
                .downcast_ref::<HarmoniumError>()
                .is_some()
        );
        assert!(aggregate.to_string().contains("primary failure"));
        assert!(aggregate.to_string().contains("rollback failure"));
    }
}
