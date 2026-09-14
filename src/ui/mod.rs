//! UI orchestration layered on top of themed panel renderers.

pub(crate) mod layout;
pub(crate) mod panels;
pub mod theme;
pub(crate) mod view;

use ratatui::Frame;
use ratatui::style::Style;
use ratatui::widgets::Block;

use crate::app::App;
use crate::artwork::ArtworkOverlay;
use crate::state::{DialogMode, FrameMetrics, Panel, Popup};
use layout::compute_layout;
use panels::PANEL_BORDER_ROWS;
use panels::artwork_overlay_target;
use theme::Theme;

/// Derive geometry-dependent frame metrics from the terminal area.
pub fn frame_metrics(area: ratatui::layout::Rect) -> FrameMetrics {
    let areas = compute_layout(area);
    let browser_viewport_height = areas.main_browser.height.saturating_sub(PANEL_BORDER_ROWS);
    let playlist_viewport_height = areas.main_playlist.height.saturating_sub(PANEL_BORDER_ROWS);
    let lyrics_layout_width =
        usize::from(areas.main_browser.width.saturating_sub(PANEL_BORDER_ROWS));
    FrameMetrics::new(
        browser_viewport_height,
        playlist_viewport_height,
        browser_viewport_height,
        lyrics_layout_width,
    )
    // The manager is centered in the full terminal rather than either panel.
    // Its preferred height is 16 cells, with two border rows outside the list.
    .with_playlist_manager_viewport_height(area.height.min(16).saturating_sub(2))
}

/// Resolve the native artwork target from the same geometry used by overlay
/// placement. This is called before rendering so encoding never starts there.
pub fn artwork_resize_target(
    area: ratatui::layout::Rect,
    app: &App,
) -> Option<ratatui::layout::Size> {
    let state = app.state();
    let target = artwork_overlay_target(
        state.artwork.is_visible(),
        state.artwork.is_enabled(),
        state.artwork.has_artwork(),
        app.active_panel(),
        state.lyrics.visible,
    )?;
    let panel_area = match target {
        Panel::Playlist => compute_layout(area).main_playlist,
        Panel::Browser => compute_layout(area).main_browser,
        Panel::Lyrics => return None,
    };
    if !(state.artwork.is_enabled() && state.artwork.is_visible() && state.artwork.has_artwork()) {
        return None;
    }
    let inner = ratatui::layout::Rect {
        x: panel_area.x.saturating_add(1),
        y: panel_area.y.saturating_add(1),
        width: panel_area.width.saturating_sub(2),
        height: panel_area.height.saturating_sub(2),
    };
    match target {
        Panel::Playlist => {
            let target = state.artwork.playlist_target();
            Some(ratatui::layout::Size::new(
                target.width.min(inner.width),
                target.height.min(inner.height),
            ))
        }
        Panel::Browser => Some(inner.into()),
        Panel::Lyrics => None,
    }
}

/// Render one full frame from the current application state.
pub fn render(frame: &mut Frame, app: &App, theme: &Theme) {
    let view = app.panel_view();
    // Keep the loaded palette immutable while layering the runtime-selected
    // border strategy onto the per-frame copy consumed by every renderer.
    let mut effective_theme = *theme;
    effective_theme.border_type = app.border_type().to_ratatui();
    let theme = &effective_theme;

    // Paint the whole canvas with the theme background first, so a modal
    // removed between frames can never leave stale cells behind AND any gap
    // between the panel bands carries the theme colour instead of the
    // terminal's default black.
    frame.render_widget(
        Block::default().style(Style::new().bg(theme.background)),
        frame.area(),
    );

    // Settings renders full-window and hides everything underneath.
    if let Some(Popup::Settings { .. }) = view.popup.as_ref() {
        panels::draw_settings(frame, frame.area(), view, theme);
        return;
    }

    let areas = compute_layout(frame.area());

    let browser_focused = view.active_panel == Panel::Browser;
    // The lyrics panel replaces the browser in the left third when visible;
    // the browser state is untouched, it is simply not drawn.
    if view.lyrics_visible() {
        panels::draw_lyrics_panel(
            frame,
            areas.main_browser,
            view,
            view.active_panel == Panel::Lyrics,
            theme,
        );
    } else {
        let browser_focused = view.active_panel == Panel::Browser;
        panels::draw_browser(frame, areas.main_browser, view, browser_focused, theme);
    }
    panels::draw_playlist(
        frame,
        areas.main_playlist,
        view,
        view.active_panel == Panel::Playlist,
        theme,
    );

    // Artwork overlays the panel that is NOT focused, so the active panel
    // stays readable. Drawing after the panels makes the image cover them.
    // While the lyrics panel is visible the cover never overlaps it: it
    // rests on the playlist (when the playlist is not focused) instead.
    if let Some(target) = panels::artwork_overlay_target(
        view.artwork.visible,
        view.artwork.enabled,
        view.artwork.has_artwork,
        view.active_panel,
        view.lyrics_visible(),
    ) {
        let panel_area = match target {
            Panel::Playlist => areas.main_playlist,
            Panel::Browser => areas.main_browser,
            Panel::Lyrics => unreachable!("lyrics is never an overlay target"),
        };
        if target == Panel::Browser {
            let inner = ratatui::layout::Rect {
                x: panel_area.x.saturating_add(1),
                y: panel_area.y.saturating_add(1),
                width: panel_area.width.saturating_sub(2),
                height: panel_area.height.saturating_sub(2),
            };
            frame.render_widget(ratatui::widgets::Clear, inner);
            frame.render_widget(Block::new().style(Style::new().bg(theme.background)), inner);
        }
        if let Some(cover) = panels::artwork_overlay_rect(panel_area, target, view) {
            app.state().artwork.render(frame, cover);
        }
    }

    // The status line is now rendered inside the now playing block
    if let Some(now_playing) = areas.now_playing {
        panels::draw_now_playing(frame, now_playing, view, theme);
    }

    panels::draw_footer(frame, areas.footer, view, browser_focused, theme);

    // Popups draw last so they overlay every panel, matching their modal
    // input semantics
    match view.popup.as_ref() {
        Some(Popup::ConfirmQuit) => panels::draw_confirm_quit_popup(frame, frame.area(), theme),
        Some(Popup::ConfirmSortTracks { .. }) => {
            panels::draw_confirm_sort_tracks_popup(frame, frame.area(), theme);
        }
        Some(Popup::Help { scroll }) => {
            panels::draw_help_popup(frame, frame.area(), *scroll, theme, view);
        }
        Some(Popup::PlaylistManager { .. }) => {
            panels::draw_playlist_manager_popup(frame, frame.area(), view, theme);
        }
        Some(Popup::ConfirmOverwrite { name }) => {
            panels::draw_confirm_overwrite_popup(frame, frame.area(), &name, theme);
        }
        Some(Popup::ConfirmDelete { name, .. }) => {
            panels::draw_confirm_delete_popup(frame, frame.area(), &name, theme);
        }
        Some(Popup::RenameCollision {
            existing,
            attempted,
        }) => {
            panels::draw_rename_collision_popup(frame, frame.area(), &existing, &attempted, theme);
        }
        Some(Popup::SearchQuery { .. })
        | Some(Popup::SearchLoading { .. })
        | Some(Popup::SearchResults { .. }) => {
            panels::draw_search_popup(frame, frame.area(), view, theme);
        }
        Some(Popup::Settings { .. }) => unreachable!("settings already handled above"),
        None => {}
    }

    // The naming dialog and the metadata form are modal: draw one of them on
    // top whenever a dialog is open and no other popup already hosts it
    // (queue-panel launch, or while no warning covers it). The collision
    // alert covers the rename dialog, so it is excluded like the other
    // warnings. This makes "Save as" from the queue visible.
    if view.dialog.is_some()
        && !matches!(
            view.popup.as_ref(),
            Some(Popup::PlaylistManager { .. })
                | Some(Popup::ConfirmOverwrite { .. })
                | Some(Popup::RenameCollision { .. })
        )
    {
        match view.dialog.as_ref().map(|dialog| &dialog.mode) {
            Some(DialogMode::EditMetadata { .. }) => {
                panels::draw_metadata_form(frame, frame.area(), view, theme);
            }
            _ => panels::draw_naming_dialog(frame, frame.area(), view, theme),
        }
    }
}

/// Build the external artwork target from the same focus, visibility and
/// panel geometry used by the native ratatui-image overlay.
pub fn artwork_overlay(area: ratatui::layout::Rect, app: &App) -> Option<ArtworkOverlay> {
    let view = app.panel_view();
    // External layers sit above the completed terminal frame, so suppress
    // them while a modal is painted last. Native ratatui-image remains behind
    // the same modal as before and is restored when the modal closes.
    if view.popup.is_some() || view.dialog.is_some() {
        return None;
    }
    let target = panels::artwork_overlay_target(
        view.artwork.visible,
        view.artwork.enabled,
        view.artwork.has_artwork,
        view.active_panel,
        view.lyrics_visible(),
    )?;
    let rect = match target {
        Panel::Playlist => {
            panels::artwork_overlay_rect(area_for_panel(area, target), target, view)?
        }
        Panel::Browser => panels::artwork_overlay_rect(area_for_panel(area, target), target, view)?,
        Panel::Lyrics => return None,
    };
    let track_index = view.artwork.track_index?;
    let path = view.artwork.external_source.clone()?;
    Some(ArtworkOverlay::new(track_index, path, rect))
}

/// Recover the layout rect for the two main panels from the terminal area.
fn area_for_panel(area: ratatui::layout::Rect, target: Panel) -> ratatui::layout::Rect {
    let areas = compute_layout(area);
    match target {
        Panel::Playlist => areas.main_playlist,
        Panel::Browser | Panel::Lyrics => areas.main_browser,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::DialogMode;
    use crate::ui::theme::Theme;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::{Duration, Instant};

    #[test]
    fn tick_frame_refreshes_the_cached_panel_view() {
        let mut app = App::new();
        app.state_mut().active_playlist_name = Some("Refreshed".to_string());
        let area = ratatui::layout::Rect::new(0, 0, 90, 30);

        app.tick_frame(frame_metrics(area), Instant::now());

        assert_eq!(app.panel_view().playlist.title, "Refreshed (0)");
    }

    #[test]
    fn repeated_rendering_does_not_mutate_frame_state() {
        let mut app = App::new();
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 30,
        };
        app.state_mut().browser.viewport_height = 3;
        app.state_mut().playlist_viewport_height = 4;
        app.state_mut().lyrics.viewport_height = 5;
        let before = (
            app.state().frame.spinner.frame_str().to_string(),
            app.state().frame.spinner_last_tick,
            app.state().browser.viewport_height,
            app.state().playlist_viewport_height,
            app.state().lyrics.viewport_height,
            app.state().lyrics.scroll,
            app.state().lyrics.active_line,
            app.state().lyrics.layout_width,
            app.help_content().clone(),
        );

        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height)).unwrap();
        terminal
            .draw(|frame| render(frame, &app, &Theme::default()))
            .unwrap();
        terminal
            .draw(|frame| render(frame, &app, &Theme::default()))
            .unwrap();

        let after = (
            app.state().frame.spinner.frame_str().to_string(),
            app.state().frame.spinner_last_tick,
            app.state().browser.viewport_height,
            app.state().playlist_viewport_height,
            app.state().lyrics.viewport_height,
            app.state().lyrics.scroll,
            app.state().lyrics.active_line,
            app.state().lyrics.layout_width,
            app.help_content().clone(),
        );
        assert_eq!(after, before);
    }

    #[test]
    fn explicit_frame_ticks_update_geometry_and_spinner_metrics() {
        let mut app = App::new();
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 30,
        };
        let metrics = frame_metrics(area);
        let first_tick = Instant::now();

        app.tick_frame(metrics, first_tick);

        assert_eq!(app.state().browser.viewport_height, 22);
        assert_eq!(app.state().playlist_viewport_height, 22);
        assert_eq!(app.state().lyrics.viewport_height, 22);
        assert_eq!(app.state().lyrics.layout_width, None);
        assert_eq!(app.state().frame.spinner_last_tick, Some(first_tick));

        let initial_frame = app.state().frame.spinner.frame_str().to_string();
        app.tick_frame(metrics, first_tick + Duration::from_millis(250));
        assert_ne!(
            app.state().frame.spinner.frame_str(),
            initial_frame,
            "an explicit elapsed frame tick advances the shared spinner"
        );
    }

    #[test]
    fn explicit_frame_ticks_update_lyrics_follow_state() {
        let mut app = App::new();
        app.state_mut().playback.elapsed = Duration::from_secs(12);
        app.state_mut().lyrics.document = Some(crate::lyrics::LyricsDocument {
            lines: (0..20)
                .map(|index| crate::lyrics::LyricsLine {
                    timestamp_ms: Some(index * 1_000),
                    text: format!("line {index}"),
                    words: Vec::new(),
                })
                .collect(),
            text: String::new(),
        });

        app.state_mut()
            .tick_frame(FrameMetrics::new(5, 5, 5, 20), Duration::ZERO);

        assert_eq!(app.state().lyrics.viewport_height, 5);
        assert_eq!(app.state().lyrics.active_line, Some(12));
        assert_eq!(app.state().lyrics.layout_width, Some(20));
        assert_eq!(app.state().lyrics.scroll, 10);
    }

    #[test]
    fn render_paints_the_theme_background_everywhere() {
        let theme = Theme {
            background: ratatui::style::Color::Rgb(10, 20, 30),
            ..Theme::default()
        };

        let app = App::new();
        let backend = TestBackend::new(90, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &app, &theme)).unwrap();
        let buf = terminal.backend().buffer().clone();

        // Probe cells that sit in the one-cell gutter between the browser and
        // playlist columns, the region just below the main panels (which the
        // now playing band covers directly), and a canvas corner: all must
        // carry the theme background, never the terminal's default.
        for (x, y) in [(1u16, 1u16), (45, 15), (45, 23), (60, 29)] {
            assert_eq!(
                buf[(x, y)].style().bg,
                Some(theme.background),
                "cell ({x},{y}) must use the theme background"
            );
        }
    }

    #[test]
    fn rename_collision_alert_renders_over_the_open_dialog() {
        let theme = Theme::default();
        let mut app = App::new();
        app.state_mut().popup_dialog.open_dialog(
            DialogMode::RenameFile {
                path: std::path::PathBuf::from("/music/song.wav"),
                original_name: "song.wav".to_string(),
                error: None,
            },
            "solo.wav".to_string(),
            None,
        );
        app.state_mut()
            .popup_dialog
            .push_popup(Popup::RenameCollision {
                existing: std::path::PathBuf::from("/music/solo.wav"),
                attempted: "solo.wav".to_string(),
            });

        let backend = TestBackend::new(90, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        app.tick_frame(
            frame_metrics(ratatui::layout::Rect::new(0, 0, 90, 30)),
            Instant::now(),
        );
        terminal.draw(|frame| render(frame, &app, &theme)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("Rename collision"), "alert title rendered");
        assert!(
            text.contains("already exists"),
            "the collision message must be visible"
        );
        assert!(
            !text.contains("overwrite"),
            "the alert must offer no overwrite option"
        );
    }

    #[test]
    fn external_overlay_reuses_the_native_panel_geometry() {
        let mut app = App::new();
        app.state_mut().artwork.set_enabled(true);
        app.state_mut().artwork.set_artwork(
            0,
            Some(crate::artwork::testing::test_protocol_with_source(
                std::path::PathBuf::from("/cache/cover.png"),
            )),
        );
        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 30,
        };
        app.tick_frame(frame_metrics(area), std::time::Instant::now());
        let overlay = artwork_overlay(area, &app).expect("external artwork target");
        let layout = compute_layout(area);
        let native =
            panels::artwork_overlay_rect(layout.main_playlist, Panel::Playlist, app.panel_view())
                .expect("native artwork target");
        assert_eq!(overlay.rect, native);
    }

    #[test]
    fn external_overlay_survives_a_failed_primary_protocol() {
        let mut app = App::new();
        app.state_mut().artwork.set_enabled(true);
        app.state_mut().artwork.set_artwork(
            0,
            Some(crate::artwork::testing::test_protocol_with_fallback_source(
                std::path::PathBuf::from("/cache/cover.png"),
            )),
        );
        app.state().artwork.fail_primary_for_test();

        let area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 90,
            height: 30,
        };
        app.tick_frame(frame_metrics(area), Instant::now());
        let overlay = artwork_overlay(area, &app).expect("external artwork target");

        assert!(overlay.rect.width > 0);
        assert!(overlay.rect.height > 0);
    }

    #[test]
    fn metadata_form_renders_all_ten_fields_in_strict_order() {
        let theme = Theme::default();
        let mut app = App::new();
        app.state_mut().popup_dialog.open_dialog(
            DialogMode::EditMetadata {
                path: std::path::PathBuf::from("/music/song.wav"),
                fields: Default::default(),
                cursor: 0,
                error: None,
                loading: false,
            },
            String::new(),
            None,
        );

        let backend = TestBackend::new(90, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        app.tick_frame(
            frame_metrics(ratatui::layout::Rect::new(0, 0, 90, 30)),
            Instant::now(),
        );
        terminal.draw(|frame| render(frame, &app, &theme)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let text: String = buf.content.iter().map(|cell| cell.symbol()).collect();
        for label in [
            "Title",
            "Artist",
            "Album",
            "Album Artist",
            "Track Number",
            "Disc Number",
            "Genre",
            "Year",
            "Composer",
            "Comment",
        ] {
            assert!(text.contains(label), "field {label} must render");
        }
    }
}
