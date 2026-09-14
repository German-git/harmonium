# Changelog

All notable changes to Harmonium will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Added Page Up and Page Down navigation to the saved-playlist manager.

### Changed

- Reworked playlist sorting to use natural track comparison while preserving the selected track.

### Fixed

- Preserved the popup renderer import boundary.
- Reserved three cells for playback glyphs in the playlist view.
- Preserved saved-playlist manager scroll context across navigation and popup updates.

> Current package baseline: `0.1.0`, as declared in `Cargo.toml`. No released version is recorded in the current repository history.
