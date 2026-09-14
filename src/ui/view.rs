//! Immutable presentation data derived from the authoritative application state.
//!
//! The render tree receives this snapshot instead of reaching into sibling
//! domains.  All domain lookups, sorting, request-state projection, and frame
//! derived values happen before drawing, in `App::tick_frame`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::audio::{PlayStatus, format_clock, progress_ratio};
use crate::filesystem::{EntryKind, is_supported_audio};
pub(crate) use crate::input::{HelpContentCache, HelpContentLine};
use crate::lyrics::{
    effective_elapsed, layout_document, line_char_times, next_line_starts, wrap_rows_for,
};
use crate::playlist::sorter::{ColumnConfig, display_label, now_playing_display_label};
pub(crate) use crate::search::SearchScope;
use crate::state::{AppState, LyricsState};
use crate::track::Track;

pub(crate) use crate::state::{
    AppearanceColumn, DialogMode, Panel, Popup, SettingsDraft, SettingsField, SettingsTab,
};

/// All data consumed by the panel renderers for one completed frame.
#[derive(Debug, Clone)]
pub struct PanelViewModel {
    pub(crate) active_panel: Panel,
    pub(crate) browser: BrowserView,
    pub(crate) playlist: PlaylistView,
    pub(crate) now_playing: NowPlayingView,
    pub(crate) lyrics: LyricsView,
    pub(crate) artwork: ArtworkView,
    pub(crate) footer: FooterView,
    pub(crate) popup: Option<Popup>,
    pub(crate) playlist_manager_scroll_offset: usize,
    pub(crate) dialog: Option<DialogView>,
    pub(crate) alert: Option<String>,
    pub(crate) search_query: Option<String>,
    pub(crate) search_cursor: usize,
    pub(crate) active_playlist_name: Option<String>,
    pub(crate) metadata_labels: Vec<&'static str>,
    pub(crate) help: HelpContentCache,
    pub(crate) applied_theme: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct BrowserView {
    pub(crate) title_location: String,
    pub(crate) entries: Vec<BrowserEntryView>,
    pub(crate) visible_start: usize,
    pub(crate) visible_count: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct BrowserEntryView {
    pub(crate) display_name: String,
    pub(crate) playable: bool,
    pub(crate) marked: bool,
    pub(crate) selected: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PlaylistView {
    pub(crate) title: String,
    pub(crate) rows: Vec<PlaylistRowView>,
    pub(crate) visible_start: usize,
    pub(crate) visible_count: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PlaylistRowView {
    pub(crate) marker: &'static str,
    pub(crate) name: String,
    pub(crate) selected: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct NowPlayingView {
    pub(crate) label: String,
    pub(crate) stream_loading: bool,
    pub(crate) spinner_frame: String,
    pub(crate) elapsed: String,
    pub(crate) duration: String,
    pub(crate) progress: f64,
    pub(crate) status: PlaybackVisualState,
    pub(crate) volume_percent: u8,
    pub(crate) speed: f32,
    pub(crate) repeat_label: String,
    pub(crate) repeat_active: bool,
    pub(crate) shuffle: bool,
    pub(crate) marked_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaybackVisualState {
    Playing,
    Paused,
    Stopped,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LyricsView {
    pub(crate) visible: bool,
    pub(crate) title: String,
    pub(crate) loading: bool,
    pub(crate) error: Option<String>,
    pub(crate) scroll: usize,
    pub(crate) viewport_height: usize,
    pub(crate) elapsed_ms: i64,
    pub(crate) timed: bool,
    pub(crate) layout: LyricsLayoutView,
    pub(crate) lines: Vec<LyricsLineView>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LyricsLayoutView {
    pub(crate) rows_per_line: Arc<Vec<usize>>,
    pub(crate) row_starts: Arc<Vec<usize>>,
    pub(crate) total_rows: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LyricsLineView {
    pub(crate) text: String,
    pub(crate) wrap_ranges: Arc<Vec<(usize, usize)>>,
    pub(crate) char_times: Option<Arc<Vec<i64>>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ArtworkView {
    pub(crate) visible: bool,
    pub(crate) enabled: bool,
    pub(crate) has_artwork: bool,
    pub(crate) playlist_target: ratatui::layout::Size,
    pub(crate) image_size: Option<ratatui::layout::Size>,
    pub(crate) track_index: Option<usize>,
    pub(crate) external_source: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FooterView {
    pub(crate) pending_effects: bool,
    pub(crate) spinner_frame: String,
}

#[derive(Debug, Clone)]
pub(crate) struct DialogView {
    pub(crate) mode: DialogMode,
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) extension: Option<String>,
    pub(crate) error: Option<String>,
}

impl PanelViewModel {
    /// Build the immutable panel snapshot after a frame tick has updated state.
    pub(crate) fn from_state(
        state: &AppState,
        lyrics_width: usize,
        help: &HelpContentCache,
        applied_theme: &str,
        artwork_target: Option<ratatui::layout::Size>,
    ) -> Self {
        let popup = state.popup_dialog.active_popup_ref().cloned();
        let dialog = state.popup_dialog.dialog_ref().map(|dialog| DialogView {
            mode: dialog.mode.clone(),
            input: dialog.input.clone(),
            cursor: dialog.cursor,
            extension: dialog.extension.clone(),
            error: dialog.error.clone(),
        });

        Self {
            active_panel: state.active_panel,
            browser: browser_view(state),
            playlist: playlist_view(state),
            now_playing: now_playing_view(state),
            lyrics: lyrics_view(&state.lyrics, lyrics_width, state.playback.elapsed),
            artwork: artwork_view(state, artwork_target),
            footer: FooterView {
                pending_effects: state.has_pending_effects(),
                spinner_frame: state.frame.spinner.frame_str().to_string(),
            },
            popup,
            playlist_manager_scroll_offset: state.popup_dialog.manager_scroll_offset(),
            dialog,
            alert: state.popup_dialog.alert_message().map(str::to_string),
            search_query: state.popup_dialog.search_query().map(str::to_string),
            search_cursor: state.popup_dialog.search_cursor().unwrap_or(0),
            active_playlist_name: state.active_playlist_name.clone(),
            metadata_labels: crate::metadata::MetaField::ALL
                .iter()
                .map(|field| field.label())
                .collect(),
            help: help.clone(),
            applied_theme: applied_theme.to_string(),
        }
    }

    pub(crate) fn lyrics_visible(&self) -> bool {
        self.lyrics.visible
    }
}

fn browser_view(state: &AppState) -> BrowserView {
    BrowserView {
        title_location: state.browser.current_dir.to_string_lossy().into_owned(),
        entries: state
            .browser
            .entries
            .iter()
            .map(|entry| BrowserEntryView {
                display_name: if entry.kind == EntryKind::Dir {
                    format!("{}/", entry.name)
                } else {
                    entry.name.clone()
                },
                playable: entry.kind != EntryKind::File || is_supported_audio(&entry.path),
                marked: state.browser.selected_entries.contains(&entry.path),
                selected: state
                    .browser
                    .entries
                    .get(state.browser.cursor())
                    .is_some_and(|selected| selected.path == entry.path),
            })
            .collect(),
        visible_start: visible_window_start(
            state.browser.scroll_offset(),
            state.browser.cursor(),
            state.browser.entries.len(),
            usize::from(state.browser.viewport_height.max(1)),
        ),
        visible_count: usize::from(state.browser.viewport_height.max(1)),
    }
}

fn playlist_view(state: &AppState) -> PlaylistView {
    let active = state.active_playlist_name.as_deref().unwrap_or("Unsaved");
    let playing = state.playback.track_index;
    PlaylistView {
        title: format!("{active} ({})", state.playlist.len()),
        rows: state
            .playlist
            .tracks()
            .iter()
            .enumerate()
            .map(|(index, track)| PlaylistRowView {
                marker: if playing == Some(index) { "▶ " } else { "  " },
                name: playlist_entry_name(track, &state.playlist_columns),
                selected: index == state.playlist.cursor(),
            })
            .collect(),
        visible_start: state.playlist_scroll_offset,
        visible_count: usize::from(state.playlist_viewport_height.max(1)),
    }
}

pub(crate) fn visible_window_start(
    prev_offset: usize,
    cursor: usize,
    item_count: usize,
    viewport: usize,
) -> usize {
    if item_count == 0 {
        return 0;
    }
    let visible = viewport.max(1);
    let max_offset = item_count.saturating_sub(visible);
    let cursor = cursor.min(item_count.saturating_sub(1));
    let mut offset = prev_offset.min(max_offset);
    if cursor < offset {
        offset = cursor;
    }
    if cursor >= offset.saturating_add(visible) {
        offset = cursor.saturating_add(1).saturating_sub(visible);
    }
    offset.min(max_offset)
}

fn playlist_entry_name<C: ColumnConfig>(track: &Track, columns: &C) -> String {
    display_label(track, columns)
}

fn now_playing_view(state: &AppState) -> NowPlayingView {
    let playing_track = state
        .playback
        .track_index
        .and_then(|index| state.playlist.tracks().get(index));
    let stream_loading =
        state.async_ops.stream_activity().is_some() && playing_track.is_some_and(Track::is_stream);
    let elapsed = state.playback.elapsed;
    let duration = state.playback.duration;
    let status = match state.playback.status {
        PlayStatus::Playing => PlaybackVisualState::Playing,
        PlayStatus::Paused => PlaybackVisualState::Paused,
        PlayStatus::Stopped => PlaybackVisualState::Stopped,
    };
    NowPlayingView {
        label: playing_track
            .map(|track| now_playing_display_label(track, &state.now_playing_display))
            .unwrap_or_else(|| "-".to_string()),
        stream_loading,
        spinner_frame: state.frame.spinner.frame_str().to_string(),
        elapsed: format_clock(elapsed),
        duration: duration
            .map(format_clock)
            .unwrap_or_else(|| "--:--".to_string()),
        progress: progress_ratio(elapsed, duration),
        status,
        volume_percent: state.playback.volume_percent.as_u16() as u8,
        speed: state.playback.speed.as_f32(),
        repeat_label: state.playback_mode.repeat().label().to_string(),
        repeat_active: !matches!(
            state.playback_mode.repeat(),
            crate::playback_mode::RepeatMode::Off
        ),
        shuffle: state.playback_mode.shuffle(),
        marked_count: state.browser.selected_entries.len(),
    }
}

fn lyrics_view(lyrics: &LyricsState, width: usize, elapsed: Duration) -> LyricsView {
    let Some(document) = lyrics.document.as_ref() else {
        return LyricsView {
            visible: lyrics.visible,
            title: lyrics
                .display_title
                .clone()
                .unwrap_or_else(|| "Lyrics".to_string()),
            loading: lyrics.loading,
            error: lyrics.error.clone(),
            scroll: lyrics.scroll,
            viewport_height: usize::from(lyrics.viewport_height.max(1)),
            elapsed_ms: effective_elapsed(elapsed.as_millis() as i64),
            ..LyricsView::default()
        };
    };

    let width = width.max(1);
    let (layout, wrap_ranges, char_times) = if let Some(cache) = lyrics.layout_cache_for(width) {
        (
            LyricsLayoutView {
                rows_per_line: Arc::clone(&cache.layout.rows_per_line),
                row_starts: Arc::clone(&cache.layout.row_starts),
                total_rows: cache.layout.total_rows,
            },
            Arc::clone(&cache.wrap_ranges),
            Arc::clone(&cache.char_times),
        )
    } else {
        let next_starts = next_line_starts(document);
        let layout = layout_document(&document.lines, width);
        let wrap_ranges = Arc::new(
            document
                .lines
                .iter()
                .map(|line| Arc::new(wrap_rows_for(&line.text, width)))
                .collect(),
        );
        let char_times = Arc::new(
            document
                .lines
                .iter()
                .enumerate()
                .map(|(index, line)| line_char_times(line, next_starts[index]).map(Arc::new))
                .collect(),
        );
        (
            LyricsLayoutView {
                rows_per_line: layout.rows_per_line,
                row_starts: layout.row_starts,
                total_rows: layout.total_rows,
            },
            wrap_ranges,
            char_times,
        )
    };
    let lines = document
        .lines
        .iter()
        .enumerate()
        .map(|(index, line)| LyricsLineView {
            text: line.text.clone(),
            wrap_ranges: Arc::clone(&wrap_ranges[index]),
            char_times: char_times[index].as_ref().map(Arc::clone),
        })
        .collect();

    LyricsView {
        visible: lyrics.visible,
        title: lyrics
            .display_title
            .clone()
            .unwrap_or_else(|| "Lyrics".to_string()),
        loading: lyrics.loading,
        error: lyrics.error.clone(),
        scroll: lyrics.scroll,
        viewport_height: usize::from(lyrics.viewport_height.max(1)),
        elapsed_ms: effective_elapsed(elapsed.as_millis() as i64),
        timed: document
            .lines
            .iter()
            .any(|line| line.timestamp_ms.is_some()),
        layout,
        lines,
    }
}

fn artwork_view(state: &AppState, artwork_target: Option<ratatui::layout::Size>) -> ArtworkView {
    let artwork = &state.artwork;
    let visible = artwork.is_visible();
    let enabled = artwork.is_enabled();
    let has_artwork = artwork.has_artwork();
    let target_size = artwork.playlist_target();
    ArtworkView {
        visible,
        enabled,
        has_artwork,
        playlist_target: target_size,
        image_size: artwork_target.and_then(|target| artwork.image_size(target)),
        track_index: artwork.track_index(),
        external_source: artwork.external_source(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_renderers_have_no_direct_sibling_domain_imports() {
        let sources = [
            include_str!("panels/mod.rs"),
            include_str!("panels/popups.rs"),
            include_str!("panels/settings.rs"),
        ];
        let forbidden = [
            "use crate::app::App",
            "use crate::browser_state",
            "use crate::filesystem",
            "use crate::lyrics",
            "use crate::metadata",
            "use crate::playlist",
            "use crate::search",
            "use crate::track",
        ];
        for source in sources {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            for import in forbidden {
                assert!(
                    !production.contains(import),
                    "obsolete panel import: {import}"
                );
            }
        }
    }

    #[test]
    fn view_snapshot_uses_authoritative_state_values() {
        let mut state = AppState::default();
        state.active_playlist_name = Some("Queue".to_string());
        state.browser.current_dir = PathBuf::from("/music");
        let help = HelpContentCache::new(&crate::config::KeysConfig::default());

        let view = PanelViewModel::from_state(&state, 40, &help, "", None);

        assert_eq!(view.playlist.title, "Queue (0)");
        assert_eq!(view.browser.title_location, "/music");
    }

    #[test]
    fn loading_views_consume_the_shared_frame_spinner() {
        let mut state = AppState::default();
        state.frame.spinner.tick(Duration::from_millis(100));
        let help = HelpContentCache::new(&crate::config::KeysConfig::default());
        let view = PanelViewModel::from_state(&state, 64, &help, "", None);
        let shared_frame = state.frame.spinner.frame_str();

        assert_eq!(view.now_playing.spinner_frame, shared_frame);
        assert_eq!(view.footer.spinner_frame, shared_frame);
    }

    #[test]
    fn cached_lyrics_geometry_is_shared_across_view_snapshots() {
        let mut state = AppState::default();
        state.lyrics.visible = true;
        state
            .lyrics
            .set_document(Some(crate::lyrics::LyricsDocument {
                lines: vec![crate::lyrics::LyricsLine {
                    timestamp_ms: Some(1000),
                    text: "abcdef".to_string(),
                    words: Vec::new(),
                }],
                text: "abcdef".to_string(),
            }));
        state.lyrics.ensure_layout_cache(3);
        let help = HelpContentCache::new(&crate::config::KeysConfig::default());

        let first = PanelViewModel::from_state(&state, 3, &help, "", None);
        let second = PanelViewModel::from_state(&state, 3, &help, "", None);

        assert!(Arc::ptr_eq(
            &first.lyrics.layout.rows_per_line,
            &second.lyrics.layout.rows_per_line
        ));
        assert!(Arc::ptr_eq(
            &first.lyrics.layout.row_starts,
            &second.lyrics.layout.row_starts
        ));
        assert!(Arc::ptr_eq(
            &first.lyrics.lines[0].wrap_ranges,
            &second.lyrics.lines[0].wrap_ranges
        ));
        assert!(Arc::ptr_eq(
            first.lyrics.lines[0]
                .char_times
                .as_ref()
                .expect("char timings"),
            second.lyrics.lines[0]
                .char_times
                .as_ref()
                .expect("char timings")
        ));
        assert_eq!(
            first.lyrics.lines[0].wrap_ranges.as_ref(),
            &[(0, 3), (3, 6)]
        );
        assert_eq!(
            first.lyrics.lines[0]
                .char_times
                .as_deref()
                .map(Vec::as_slice),
            Some(&[1666, 2333, 3000, 3666, 4333, 5000][..])
        );
    }
}
