//! Harmonium is a terminal UI music player for Linux.
//!
//! The crate root exposes every module so integration tests and future
//! binaries can reuse the same domain logic.

pub mod app;
pub mod artwork;
pub mod audio;
pub mod browser_state;
pub mod command;
pub mod config;
pub mod error;
pub mod event;
pub mod filesystem;
pub mod input;
pub mod lyrics;
pub mod metadata;
pub(crate) mod net;
pub mod playback_mode;
pub mod playlist;
pub mod runtime;
pub mod search;
pub mod state;
pub mod stream;
#[cfg(test)]
mod test_support;
pub mod track;
pub mod ui;

/// Version reported by the footer and by log lines.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
