//! Playlist domain: the playback queue and its M3U persistence.
//!
//! [`Playlist`] is a queue, never a library: it owns an ordered track list
//! plus the current selection cursor and keeps every invariant internal.
//! `manager` handles M3U storage below the data directory.

pub(crate) mod m3u;
pub mod manager;
pub mod navigation;
// The core type shares the module family name by design, mirroring the
// documented phase layout of one file per concept
#[allow(clippy::module_inception)]
pub mod playlist;
pub mod sorter;

pub(crate) use m3u::render_m3u;
pub use manager::{PlaylistName, PlaylistRepository, PlaylistStore, RewriteOutcome};
pub use navigation::{
    NavigationState, SelectionAction, auto_advance_action, manual_next_action,
    manual_previous_target,
};
pub use playlist::Playlist;
