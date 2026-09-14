//! Settings popup rendering: the five tabs and their rows.
//!
//! Draws the full-window settings editor: tab strip, per-tab body and the
//! live edit/alert sub-states. Everything reads the working SettingsDraft
//! from the open popup and stays free of command logic (that lives in
//! crate::app::settings).

use super::widgets::{
    CHECKBOX_CHECKED, CHECKBOX_EMPTY, SETTINGS_COLORS_COLUMN_FILL, SETTINGS_DISPLAY_COLUMN_FILL,
    SETTINGS_THEME_COLUMN_FILL, centered_rect, color_item_line, crossfade_item_line,
    gain_item_line, settings_item_line,
};
use crate::ui::theme::Theme;
use crate::ui::view::{
    AppearanceColumn, DialogMode, PanelViewModel, Popup, SettingsDraft, SettingsField, SettingsTab,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Tabs, Wrap};

/// Draw the full-window settings editor.
pub fn draw_settings(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let Some(Popup::Settings { tab, .. }) = view.popup.as_ref() else {
        return;
    };
    let is_editing = view
        .dialog
        .as_ref()
        .is_some_and(|dialog| dialog.mode == DialogMode::SettingsEdit);

    frame.render_widget(Clear, area);

    // Use the same bordered-block styling as the main panels (focused accent,
    // themed background, one-cell padding) so the settings window reads as part
    // of the same interface rather than a separate modal with its own border
    // colour. The bottom hint line stays because it documents the navigation.
    let border_color = theme.border_focused;
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Settings ".to_string(),
            Style::new().fg(border_color).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Line::from(Span::styled(
            " tab/shift+tab tabs · ←/→ or h/l columns · ↑/↓ move · enter edit/save · esc save+close ",
            Style::new().fg(theme.text_muted),
        )))
        .border_style(Style::new().fg(border_color))
        .style(Style::new().bg(theme.background));
    let outer = block.inner(area);
    frame.render_widget(block, area);
    if outer.height < 4 || outer.width < 20 {
        return;
    }

    // Tab bar + body split, both sized by layout so nothing overflows.
    let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).split(outer);
    let tab_index = SettingsTab::all()
        .iter()
        .position(|t| *t == *tab)
        .unwrap_or(0);
    let tab_titles: Vec<Line> = SettingsTab::all()
        .iter()
        .map(|t| Line::from(t.label()))
        .collect();
    let tabs = Tabs::new(tab_titles)
        .select(tab_index)
        .style(Style::new().fg(theme.text_muted))
        .highlight_style(
            Style::new()
                .fg(theme.highlight)
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        )
        .divider("  ");
    frame.render_widget(tabs, chunks[0]);

    let body = chunks[1];
    match tab {
        SettingsTab::General => draw_settings_general(frame, body, view, theme),
        SettingsTab::Appearance => draw_settings_appearance(frame, body, view, theme),
        SettingsTab::Keys => draw_settings_keys(frame, body, view, theme),
        SettingsTab::Playback => draw_settings_playback(frame, body, view, theme),
        SettingsTab::Sound => draw_settings_sound(frame, body, view, theme),
    }

    // Modal overlays always sit above the tab content.
    if is_editing {
        draw_settings_edit(frame, area, view, theme);
    }
    if let Some(message) = view.alert.as_deref() {
        draw_settings_alert(frame, area, message, theme);
    }
}

/// General tab: browser directory, runtime toggles, and the explicit reorder
/// action.
fn draw_settings_general(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let Some(Popup::Settings { draft, .. }) = view.popup.as_ref() else {
        return;
    };
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Options ",
            Style::new()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(theme.border_focused))
        .style(Style::new().bg(theme.background));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width.saturating_sub(1));

    let mut lines: Vec<Line> = Vec::new();
    let options = [
        (
            crate::state::SettingsField::GeneralBrowserDirectory,
            format!("Browser dir: {}", draft.browser_directory),
        ),
        (
            crate::state::SettingsField::GeneralConfirmQuit,
            format!(
                "{} Confirm quit",
                if draft.confirm_quit {
                    CHECKBOX_CHECKED
                } else {
                    CHECKBOX_EMPTY
                }
            ),
        ),
        (
            crate::state::SettingsField::GeneralResumePreviousTrack,
            format!(
                "{} Resume track",
                if draft.resume_previous_track {
                    CHECKBOX_CHECKED
                } else {
                    CHECKBOX_EMPTY
                }
            ),
        ),
        (
            crate::state::SettingsField::GeneralShowHidden,
            format!(
                "{} Show hidden files",
                if draft.show_hidden {
                    CHECKBOX_CHECKED
                } else {
                    CHECKBOX_EMPTY
                }
            ),
        ),
    ];
    for (field, row) in options {
        lines.push(settings_item_line(
            row,
            draft.general_field == field,
            width,
            theme,
        ));
    }

    lines.push(settings_item_line(
        "Sort tracks [Enter]".to_string(),
        draft.general_field == crate::state::SettingsField::GeneralSortTracks,
        width,
        theme,
    ));

    let selected = SettingsField::GENERAL_ALL
        .iter()
        .position(|field| *field == draft.general_field)
        .unwrap_or(0);
    let top = selected.saturating_sub(usize::from(inner.height.saturating_sub(1)));
    frame.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), inner);
}

/// Appearance tab: three proportionally-sized sibling columns. "Display" holds
/// the Now Playing and Playlist columns options, "Themes" the theme list and
/// "Colors" the editable palette.
fn draw_settings_appearance(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let Some(Popup::Settings { draft, .. }) = view.popup.as_ref() else {
        return;
    };
    let applied_theme = view.applied_theme.as_str();
    let cols = Layout::horizontal([
        Constraint::Fill(SETTINGS_DISPLAY_COLUMN_FILL),
        Constraint::Fill(SETTINGS_THEME_COLUMN_FILL),
        Constraint::Fill(SETTINGS_COLORS_COLUMN_FILL),
    ])
    .split(area);
    let display_col = cols[0];
    let themes_col = cols[1];
    let colors_col = cols[2];

    // Display column (fixed width, leftmost).
    let display_focused = draft.appearance_column == AppearanceColumn::Display;
    let display_border = if display_focused {
        theme.border_focused
    } else {
        theme.border
    };
    let display_block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Display ",
            Style::new().fg(display_border).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(display_border))
        .style(Style::new().bg(theme.background));
    let display_inner = display_block.inner(display_col);
    frame.render_widget(display_block, display_col);
    let display_width = usize::from(display_inner.width.saturating_sub(1));

    let now_playing_metadata = draft.now_playing_display.sort_by == crate::config::SortBy::Metadata;
    let playlist_columns_metadata =
        draft.playlist_columns.display_by == crate::config::SortBy::Metadata;
    let radio = |on: bool| if on { "\u{25c9}" } else { "\u{25cb}" }; // ◉ / ○
    let mut lines = vec![Line::from(Span::styled(
        "Border type",
        Style::new()
            .fg(theme.highlight)
            .add_modifier(Modifier::BOLD),
    ))];
    for border_type in crate::config::BorderType::ALL {
        let field = crate::state::AppearanceDisplayField::Border(border_type);
        lines.push(settings_item_line(
            format!(
                "{} {}",
                radio(draft.border_type == border_type),
                border_type.label()
            ),
            draft.appearance_display_field == field,
            display_width,
            theme,
        ));
    }
    lines.push(Line::from(Span::styled(
        "Now playing",
        Style::new()
            .fg(theme.highlight)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(settings_item_line(
        format!("{} Filename", radio(!now_playing_metadata)),
        draft.appearance_display_field
            == crate::state::AppearanceDisplayField::NowPlayingSort(
                crate::config::SortBy::Filename,
            ),
        display_width,
        theme,
    ));
    lines.push(settings_item_line(
        format!("{} Metadata", radio(now_playing_metadata)),
        draft.appearance_display_field
            == crate::state::AppearanceDisplayField::NowPlayingSort(
                crate::config::SortBy::Metadata,
            ),
        display_width,
        theme,
    ));
    let np_sub = [
        format!(
            " {} Track Number",
            if draft.now_playing_display.metadata_track_number {
                CHECKBOX_CHECKED
            } else {
                CHECKBOX_EMPTY
            }
        ),
        format!(
            " {} Artist",
            if draft.now_playing_display.metadata_artist {
                CHECKBOX_CHECKED
            } else {
                CHECKBOX_EMPTY
            }
        ),
        format!(
            " {} Album",
            if draft.now_playing_display.metadata_album {
                CHECKBOX_CHECKED
            } else {
                CHECKBOX_EMPTY
            }
        ),
        format!(
            " {} Title",
            if draft.now_playing_display.metadata_title {
                CHECKBOX_CHECKED
            } else {
                CHECKBOX_EMPTY
            }
        ),
    ];
    for (metadata, row) in [
        crate::config::SortMetadataField::TrackNumber,
        crate::config::SortMetadataField::Artist,
        crate::config::SortMetadataField::Album,
        crate::config::SortMetadataField::Title,
    ]
    .into_iter()
    .zip(np_sub)
    {
        let field = crate::state::AppearanceDisplayField::NowPlayingMetadata(metadata);
        let is_cursor = draft.appearance_display_field == field;
        let mut line = settings_item_line(row, is_cursor, display_width, theme);
        if !now_playing_metadata && !is_cursor {
            for span in line.spans.iter_mut() {
                span.style = span.style.fg(theme.text_muted);
            }
        }
        lines.push(line);
    }
    lines.push(Line::from(Span::styled(
        "Playlist columns",
        Style::new()
            .fg(theme.highlight)
            .add_modifier(Modifier::BOLD),
    )));
    let playlist_rows = [
        format!("{} Filename", radio(!playlist_columns_metadata)),
        format!("{} Metadata", radio(playlist_columns_metadata)),
        format!(
            " {} Artist",
            if draft.playlist_columns.metadata_artist {
                CHECKBOX_CHECKED
            } else {
                CHECKBOX_EMPTY
            }
        ),
        format!(
            " {} Album",
            if draft.playlist_columns.metadata_album {
                CHECKBOX_CHECKED
            } else {
                CHECKBOX_EMPTY
            }
        ),
        format!(
            " {} Track Number",
            if draft.playlist_columns.metadata_track_number {
                CHECKBOX_CHECKED
            } else {
                CHECKBOX_EMPTY
            }
        ),
        format!(" {} Title", CHECKBOX_CHECKED),
    ];
    for (field, row) in [
        Some(crate::state::AppearanceDisplayField::PlaylistSort(
            crate::config::SortBy::Filename,
        )),
        Some(crate::state::AppearanceDisplayField::PlaylistSort(
            crate::config::SortBy::Metadata,
        )),
        Some(crate::state::AppearanceDisplayField::PlaylistMetadata(
            crate::config::SortMetadataField::Artist,
        )),
        Some(crate::state::AppearanceDisplayField::PlaylistMetadata(
            crate::config::SortMetadataField::Album,
        )),
        Some(crate::state::AppearanceDisplayField::PlaylistMetadata(
            crate::config::SortMetadataField::TrackNumber,
        )),
        None,
    ]
    .into_iter()
    .zip(playlist_rows)
    {
        let is_cursor = field.is_some_and(|field| draft.appearance_display_field == field);
        let mut line = settings_item_line(row, is_cursor, display_width, theme);
        if field.is_none()
            || (field.is_some_and(|field| {
                matches!(
                    field,
                    crate::state::AppearanceDisplayField::PlaylistMetadata(_)
                )
            }) && !playlist_columns_metadata)
        {
            for span in line.spans.iter_mut() {
                span.style = span.style.fg(theme.text_muted);
            }
        }
        lines.push(line);
    }
    let selected_line = draft.appearance_display_field.row();
    let np_top = selected_line.saturating_sub(usize::from(display_inner.height.saturating_sub(1)));
    frame.render_widget(
        Paragraph::new(lines).scroll((np_top as u16, 0)),
        display_inner,
    );

    // Themes column (fixed width): alphabetical list, the active (applied)
    // theme marked with ◉ and the rest with ○, each row still highlightable.
    let themes_focused = draft.appearance_column == AppearanceColumn::Themes;
    let themes_border = if themes_focused {
        theme.border_focused
    } else {
        theme.border
    };
    let themes_block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Themes ",
            Style::new().fg(themes_border).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(themes_border))
        .style(Style::new().bg(theme.background));
    let themes_inner = themes_block.inner(themes_col);
    frame.render_widget(themes_block, themes_col);
    let theme_width = usize::from(themes_inner.width.saturating_sub(1));
    let theme_lines: Vec<Line> = draft
        .theme_names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let marker = if name.as_str() == applied_theme {
                "\u{25c9}" // ◉ active (applied) theme
            } else {
                "\u{25cb}" // ○ other themes
            };
            settings_item_line(
                format!("{marker} {name}"),
                draft.selected_theme() == Some(index),
                theme_width,
                theme,
            )
        })
        .collect();
    let theme_top = draft
        .selected_theme()
        .unwrap_or(0)
        .saturating_sub(usize::from(themes_inner.height.saturating_sub(1)));
    frame.render_widget(
        Paragraph::new(theme_lines).scroll((theme_top as u16, 0)),
        themes_inner,
    );

    // Colors column (takes the remaining width): each row leads with a color
    // swatch block (█) rendered in that color, then the label and value.
    let colors_focused = draft.appearance_column == AppearanceColumn::Colors;
    let colors_border = if colors_focused {
        theme.border_focused
    } else {
        theme.border
    };
    let colors_block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Theme editor ",
            Style::new().fg(colors_border).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(colors_border))
        .style(Style::new().bg(theme.background));
    let colors_inner = colors_block.inner(colors_col);
    frame.render_widget(colors_block, colors_col);
    let color_width = usize::from(colors_inner.width.saturating_sub(1));
    let color_lines: Vec<Line> = crate::ui::theme::ThemeColorField::ALL
        .into_iter()
        .map(|field| {
            let value = settings_color_value(&draft, field);
            let selected = draft.appearance_color_field == field;
            color_item_line(selected, field.label(), value, color_width, theme)
        })
        .collect();
    let color_top = draft
        .appearance_color_field
        .index()
        .saturating_sub(usize::from(colors_inner.height.saturating_sub(1)));
    frame.render_widget(
        Paragraph::new(color_lines).scroll((color_top as u16, 0)),
        colors_inner,
    );
}

/// Sound tab: audio backend list and device list.
fn draw_settings_sound(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let Some(Popup::Settings { draft, .. }) = view.popup.as_ref() else {
        return;
    };
    // A single, user-facing "Output device" list (PipeWire sinks). The
    // technical backend stays hidden behind the audio layer. The list gets its
    // own bordered block so the section reads as a discrete panel; its title
    // replaces the previous bare header.
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Output device ",
            Style::new()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(theme.border_focused))
        .style(Style::new().bg(theme.background));
    let list_inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(list_inner.width.saturating_sub(1));
    if draft.outputs.is_empty() {
        let hint = Line::from(Span::styled(
            "No output devices found",
            Style::new().fg(theme.text_muted),
        ));
        frame.render_widget(Paragraph::new(hint), list_inner);
        return;
    }
    let lines: Vec<Line> = draft
        .outputs
        .iter()
        .enumerate()
        .map(|(index, output)| {
            settings_item_line(
                output.name.clone(),
                draft.selected_output() == Some(index),
                width,
                theme,
            )
        })
        .collect();
    let top = draft
        .selected_output()
        .unwrap_or(0)
        .saturating_sub(usize::from(list_inner.height.saturating_sub(1)));
    frame.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), list_inner);
}

/// Playback tab: an Options column with the Remote lyrics toggle, a horizontal
/// preamp gain slider (dB) and a crossfade length slider (s).
fn draw_settings_playback(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let Some(Popup::Settings { draft, .. }) = view.popup.as_ref() else {
        return;
    };
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Options ",
            Style::new()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(theme.border_focused))
        .style(Style::new().bg(theme.background));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width.saturating_sub(1));

    let remote_row = format!(
        "{} Remote lyrics",
        if draft.remote_lyrics {
            CHECKBOX_CHECKED
        } else {
            CHECKBOX_EMPTY
        }
    );
    let lines = vec![
        settings_item_line(
            remote_row,
            draft.playback_field == SettingsField::PlaybackRemoteLyrics,
            width,
            theme,
        ),
        gain_item_line(
            draft.playback_field == SettingsField::PlaybackGain,
            draft.gain_db,
            width,
            theme,
        ),
        crossfade_item_line(
            draft.playback_field == SettingsField::PlaybackCrossfade,
            draft.crossfade_seconds,
            width,
            theme,
        ),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Keys tab: the editable global bindings, no sub-tabs.
fn draw_settings_keys(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let Some(Popup::Settings { draft, .. }) = view.popup.as_ref() else {
        return;
    };
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Global keys ",
            Style::new()
                .fg(theme.border_focused)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(theme.border_focused))
        .style(Style::new().bg(theme.background));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width.saturating_sub(1));
    let lines: Vec<Line> = crate::config::KeySettingsRow::ALL
        .iter()
        .map(|row| {
            let text = format!("{}: {}", row.label(), row.get(&draft.keys_draft));
            settings_item_line(text, draft.keys_field == *row, width, theme)
        })
        .collect();
    let top = draft
        .keys_field
        .index()
        .saturating_sub(usize::from(inner.height.saturating_sub(1)));
    frame.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), inner);
}

/// Text edit overlay for a settings field.
fn draw_settings_edit(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let title = settings_edit_title(view);
    let input = view
        .dialog
        .as_ref()
        .map(|dialog| dialog.input.clone())
        .unwrap_or_default();
    let rect = centered_rect(62, 6, area);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            format!(" {title} "),
            Style::new()
                .fg(theme.popup_border)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let input_line = if is_settings_color_edit(view) {
        // Lead the field with a live swatch block so the edited color is visible
        // to the left of the input, hinting the upcoming value.
        let color = input.parse::<ratatui::style::Color>().unwrap_or(theme.text);
        Line::from(vec![
            Span::styled("\u{2588} ", Style::new().fg(color)),
            Span::styled(input.clone(), Style::new().fg(theme.text)),
        ])
    } else {
        Line::from(Span::styled(input.clone(), Style::new().fg(theme.text)))
    };
    let hint = view
        .dialog
        .as_ref()
        .and_then(|dialog| dialog.error.clone())
        .unwrap_or_else(|| "enter save · esc cancel".to_string());
    let text = vec![
        input_line,
        Line::default(),
        Line::from(Span::styled(hint, Style::new().fg(theme.text_muted))),
    ];
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// Whether the settings edit dialog is editing an Appearance color slot (the
/// popup is on the Appearance tab with the Colors column focused).
fn is_settings_color_edit(view: &PanelViewModel) -> bool {
    matches!(
        view.popup.as_ref(),
        Some(Popup::Settings {
            tab: SettingsTab::Appearance,
            draft,
            ..
        }) if draft.appearance_column == AppearanceColumn::Colors
    )
}

/// Rejection alert shown inside the settings popup until any key is pressed.
fn draw_settings_alert(frame: &mut Frame, area: Rect, message: &str, theme: &Theme) {
    let rect = centered_rect(62, 6, area);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Warning ",
            Style::new().fg(theme.error).add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::new().fg(theme.error))
        .style(Style::new().bg(theme.background));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let text = vec![
        Line::from(Span::styled(message, Style::new().fg(theme.warning))),
        Line::default(),
        Line::from(Span::styled(
            "press any key",
            Style::new().fg(theme.text_muted),
        )),
    ];
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), inner);
}

/// Current color value for a typed Appearance field.
fn settings_color_value(draft: &SettingsDraft, field: crate::ui::theme::ThemeColorField) -> &str {
    field.get(&draft.colors)
}

pub(super) fn settings_edit_title(view: &PanelViewModel) -> String {
    let Some(Popup::Settings { tab, draft, .. }) = view.popup.as_ref() else {
        return "Edit value".to_string();
    };
    match tab {
        crate::state::SettingsTab::General
            if draft.general_field == crate::state::SettingsField::GeneralBrowserDirectory =>
        {
            "Edit browser directory".to_string()
        }
        crate::state::SettingsTab::Appearance => {
            let Some(field) = draft.appearance_color_field() else {
                return "Edit value".to_string();
            };
            format!("Edit {}", field.label())
        }
        crate::state::SettingsTab::Keys => {
            let Some(row) = draft.key_settings_row() else {
                return "Edit value".to_string();
            };
            format!("Edit {}", row.label())
        }
        _ => "Edit value".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{draw_settings_keys, settings_color_value};
    use crate::app::App;
    use crate::state::Popup;
    use crate::state::SettingsDraft;
    use crate::ui::theme::Theme;

    #[test]
    fn color_labels_end_with_the_lyrics_overrides() {
        assert_eq!(
            crate::ui::theme::ThemeColorField::ALL[18].label(),
            "time_text"
        );
        assert_eq!(
            crate::ui::theme::ThemeColorField::ALL[19].label(),
            "lyrics_text"
        );
        assert_eq!(
            crate::ui::theme::ThemeColorField::ALL[20].label(),
            "lyrics_highlight"
        );
        assert_eq!(
            crate::ui::theme::ThemeColorField::ALL[21].label(),
            "lyrics_background"
        );
        assert_eq!(
            crate::ui::theme::ThemeColorField::ALL[22].label(),
            "lyrics_border"
        );
        assert_eq!(
            crate::ui::theme::ThemeColorField::ALL[23].label(),
            "lyrics_border_focused"
        );
        assert_eq!(crate::ui::theme::ThemeColorField::ALL.len(), 24);
        assert_eq!(crate::ui::theme::ThemeColorField::from_index(24), None);
    }

    #[test]
    fn color_values_read_the_lyrics_draft_fields_at_their_indices() {
        let mut draft = SettingsDraft::default();
        draft.colors.lyrics_text = "white".to_string();
        draft.colors.lyrics_highlight = "#fabd2f".to_string();
        draft.colors.lyrics_background = String::new();
        draft.colors.lyrics_border = "200".to_string();
        draft.colors.lyrics_border_focused = "magenta".to_string();

        assert_eq!(
            settings_color_value(&draft, crate::ui::theme::ThemeColorField::LyricsText),
            "white"
        );
        assert_eq!(
            settings_color_value(&draft, crate::ui::theme::ThemeColorField::LyricsHighlight),
            "#fabd2f"
        );
        // An unset lyrics row renders as the empty string: the editor shows
        // the key as blank while the panel keeps inheriting its role.
        assert_eq!(
            settings_color_value(&draft, crate::ui::theme::ThemeColorField::LyricsBackground),
            ""
        );
        assert_eq!(
            settings_color_value(&draft, crate::ui::theme::ThemeColorField::LyricsBorder),
            "200"
        );
        assert_eq!(
            settings_color_value(
                &draft,
                crate::ui::theme::ThemeColorField::LyricsBorderFocused,
            ),
            "magenta"
        );
    }

    #[test]
    fn keys_renderer_preserves_the_existing_row_labels_and_order() {
        let mut app = App::new();
        app.state_mut().popup_dialog.open_popup(Popup::Settings {
            tab: crate::state::SettingsTab::Keys,
            focus: crate::state::SettingsFocus::Content,
            draft: SettingsDraft::default(),
        });
        let backend = ratatui::backend::TestBackend::new(64, 20);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        app.tick_frame(
            crate::ui::frame_metrics(ratatui::layout::Rect::new(0, 0, 64, 20)),
            std::time::Instant::now(),
        );
        terminal
            .draw(|frame| {
                draw_settings_keys(frame, frame.area(), app.panel_view(), &Theme::default())
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<Vec<_>>()
                    .concat()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let labels = crate::config::KeySettingsRow::ALL.map(|row| format!("{}:", row.label()));
        let positions: Vec<usize> = labels
            .iter()
            .map(|label| rendered.find(label).expect("settings label must render"))
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
