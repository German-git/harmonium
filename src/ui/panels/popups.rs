//! Modal popups and dialogs: confirmations, help, playlist manager, naming.
//!
//! Centered modal widgets resting over the panels. Each one is a self
//! contained renderer fed by the open popup state; commands and focus
//! handling live in the command layer.

use super::widgets::{
    DIALOG_HEIGHT, DIALOG_WIDTH, FORM_HEIGHT, FORM_WIDTH, HELP_HEIGHT_PERCENT, HELP_WIDTH_PERCENT,
    MANAGER_HEIGHT, MANAGER_WIDTH, POPUP_HEIGHT, POPUP_WIDTH, RENAME_FILE_WIDTH,
    SEARCH_LOADING_HEIGHT, SEARCH_QUERY_HEIGHT, SEARCH_QUERY_WIDTH, SEARCH_RESULTS_HEIGHT,
    SEARCH_RESULTS_WIDTH, build_help_lines, centered_percent_rect, centered_rect, sanitize_text,
};
use crate::ui::theme::Theme;
use crate::ui::view::{DialogMode, PanelViewModel, Popup, SearchScope};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};
use std::path::Path;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn draw_confirm_quit_popup(frame: &mut Frame, area: Rect, theme: &Theme) {
    let popup_area = centered_rect(POPUP_WIDTH, POPUP_HEIGHT, area);

    frame.render_widget(Clear, popup_area);

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            "Confirm Quit",
            Style::new().fg(theme.popup_border).bold(),
        ))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let lines = vec![
        Line::from(Span::styled(
            "Do you really want to quit?",
            Style::new().fg(theme.warning),
        )),
        Line::from(vec![
            Span::styled("[y]", Style::new().fg(theme.success)),
            Span::styled(" yes   ", Style::new().fg(theme.text_muted)),
            Span::styled("[n]", Style::new().fg(theme.error)),
            Span::styled(" no   ", Style::new().fg(theme.text_muted)),
            Span::styled("[Esc]", Style::new().fg(theme.text)),
            Span::styled(" cancel", Style::new().fg(theme.text_muted)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw the confirmation for the explicit playlist reorder action.
pub fn draw_confirm_sort_tracks_popup(frame: &mut Frame, area: Rect, theme: &Theme) {
    let popup_area = centered_rect(POPUP_WIDTH, POPUP_HEIGHT, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Sort tracks ",
            Style::new().fg(theme.popup_border).bold(),
        ))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let lines = vec![
        Line::from(Span::styled(
            "The tracks will be reordered",
            Style::new().fg(theme.warning),
        )),
        Line::from(vec![
            Span::styled("[Enter]", Style::new().fg(theme.success)),
            Span::styled(" confirm   ", Style::new().fg(theme.text_muted)),
            Span::styled("[Esc]", Style::new().fg(theme.text)),
            Span::styled(" cancel", Style::new().fg(theme.text_muted)),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw the overwrite confirmation shown when "Save as" targets an existing
/// name that differs from the active playlist.
///
/// Mirrors the layout and box style of `draw_confirm_quit_popup` so the two
/// warnings read as the same family of modal. A second enter confirms the
/// overwrite, while esc cancels back into the still-open naming dialog.
pub fn draw_confirm_overwrite_popup(frame: &mut Frame, area: Rect, name: &str, theme: &Theme) {
    let popup_area = centered_rect(POPUP_WIDTH, POPUP_HEIGHT, area);

    frame.render_widget(Clear, popup_area);

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Confirm overwrite ",
            Style::new().fg(theme.popup_border).bold(),
        ))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let lines = vec![
        Line::from(Span::styled(
            format!("Playlist '{name}' already exists. Overwrite it?"),
            Style::new().fg(theme.warning),
        )),
        Line::from(vec![
            Span::styled("[enter]", Style::new().fg(theme.success)),
            Span::styled(" overwrite   ", Style::new().fg(theme.text_muted)),
            Span::styled("[esc]", Style::new().fg(theme.error)),
            Span::styled(" cancel", Style::new().fg(theme.text_muted)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw the delete confirmation shown when the user presses `d` to delete a
/// playlist. Mirrors the layout and box style of `draw_confirm_overwrite_popup`
/// so the two warnings read as the same family of modal. A second enter confirms
/// the deletion, while esc cancels back into the previous state.
pub fn draw_confirm_delete_popup(frame: &mut Frame, area: Rect, name: &str, theme: &Theme) {
    let popup_area = centered_rect(POPUP_WIDTH, POPUP_HEIGHT, area);

    frame.render_widget(Clear, popup_area);

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Delete playlist ",
            Style::new().fg(theme.popup_border).bold(),
        ))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let lines = vec![
        Line::from(Span::styled(
            format!("Delete playlist '{name}'?"),
            Style::new().fg(theme.warning),
        )),
        Line::from(vec![
            Span::styled("[enter]", Style::new().fg(theme.success)),
            Span::styled(" delete   ", Style::new().fg(theme.text_muted)),
            Span::styled("[esc]", Style::new().fg(theme.error)),
            Span::styled(" cancel", Style::new().fg(theme.text_muted)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw the rename collision alert shown when the target name already exists.
///
/// Mirrors the overwrite/delete warning family. Enter or Esc both dismiss the
/// alert and the rename dialog underneath stays open and editable; there is
/// no overwrite option, matching the spec's no-overwrite collision rule.
pub fn draw_rename_collision_popup(
    frame: &mut Frame,
    area: Rect,
    existing: &Path,
    attempted: &str,
    theme: &Theme,
) {
    let popup_area = centered_rect(POPUP_WIDTH, POPUP_HEIGHT, area);

    frame.render_widget(Clear, popup_area);

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Rename collision ",
            Style::new().fg(theme.popup_border).bold(),
        ))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let lines = vec![
        Line::from(Span::styled(
            format!("'{attempted}' already exists in this directory."),
            Style::new().fg(theme.warning),
        )),
        Line::from(Span::styled(
            format!(
                "The existing file will not be overwritten ({}).",
                existing.display()
            ),
            Style::new().fg(theme.text_muted),
        )),
        Line::from(vec![
            Span::styled("[enter]", Style::new().fg(theme.text)),
            Span::styled(" or ", Style::new().fg(theme.text_muted)),
            Span::styled("[esc]", Style::new().fg(theme.text)),
            Span::styled(" to dismiss", Style::new().fg(theme.text_muted)),
        ]),
    ];

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw the query, loading or results phase of contextual search.
pub fn draw_search_popup(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let Some(popup) = view.popup.as_ref() else {
        return;
    };
    let (scope, phase) = match popup {
        Popup::SearchQuery { scope } => (*scope, SearchPopupPhase::Query),
        Popup::SearchLoading { scope, .. } => (*scope, SearchPopupPhase::Loading),
        Popup::SearchResults { scope, .. } => (*scope, SearchPopupPhase::Results),
        _ => return,
    };
    let (width, height) = match phase {
        SearchPopupPhase::Query => (SEARCH_QUERY_WIDTH, SEARCH_QUERY_HEIGHT),
        SearchPopupPhase::Loading => (SEARCH_RESULTS_WIDTH, SEARCH_LOADING_HEIGHT),
        SearchPopupPhase::Results => (SEARCH_RESULTS_WIDTH, SEARCH_RESULTS_HEIGHT),
    };
    let popup_area = centered_rect(width, height, area);
    frame.render_widget(Clear, popup_area);

    let title = match scope {
        SearchScope::Browser => format!(
            " Search in {} ",
            sanitize_text(&view.browser.title_location)
        ),
        SearchScope::Playlist => " Search track in playlist ".to_string(),
    };
    let hint = match phase {
        SearchPopupPhase::Query => " enter search · esc cancel ",
        SearchPopupPhase::Loading => " esc cancel ",
        SearchPopupPhase::Results => " h/j or up/down move · enter reveal · esc close ",
    };
    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            title,
            Style::new().fg(theme.popup_border).bold(),
        ))
        .title_bottom(Line::from(Span::styled(
            hint,
            Style::new().fg(theme.text_muted),
        )))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    match popup {
        Popup::SearchQuery { .. } => {
            let (line, cursor) = render_focused_input(
                view.search_query.as_deref().unwrap_or_default(),
                view.search_cursor,
                inner.width as usize,
                None,
                theme,
                Style::new().fg(theme.text_muted),
            );
            frame.render_widget(Paragraph::new(line), inner);
            let cursor_x = inner
                .x
                .saturating_add(u16::try_from(cursor).unwrap_or(u16::MAX))
                .min(inner.x.saturating_add(inner.width).saturating_sub(1));
            frame.set_cursor_position((cursor_x, inner.y));
        }
        Popup::SearchLoading { .. } => {
            let loading = loading_line(&view.footer.spinner_frame)
                .into_iter()
                .map(|span| span.style(Style::new().fg(theme.text_muted)))
                .collect::<Vec<_>>();
            frame.render_widget(Paragraph::new(Line::from(loading)), inner);
        }
        Popup::SearchResults {
            results, cursor, ..
        } => {
            if results.is_empty() {
                let message = match scope {
                    SearchScope::Browser => "File not found",
                    SearchScope::Playlist => "The track is not in this playlist",
                };
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        message,
                        Style::new().fg(theme.text_muted),
                    ))),
                    inner,
                );
            } else {
                let rows = results
                    .iter()
                    .map(|result| ListItem::new(sanitize_text(&result.label)))
                    .collect::<Vec<_>>();
                let mut list_state = ListState::default();
                list_state.select(Some(*cursor));
                frame.render_stateful_widget(
                    List::new(rows)
                        .highlight_style(Style::new().fg(theme.highlight).bold())
                        .style(Style::new().fg(theme.text).bg(theme.background)),
                    inner,
                    &mut list_state,
                );
            }
        }
        _ => unreachable!("search popup phase and variant must agree"),
    }
}

#[derive(Clone, Copy)]
enum SearchPopupPhase {
    Query,
    Loading,
    Results,
}

fn loading_line(frame: &str) -> Vec<Span<'static>> {
    vec![
        Span::styled(frame.to_string(), Style::new()),
        Span::styled(" ", Style::new()),
        Span::styled("Loading", Style::new()),
    ]
}

/// Draw the ten field metadata editor form.
///
/// The fields render in the strict spec order (Title through Comment), the
/// focused row is highlighted and carries the live edit buffer, and while the
/// prefill is still in flight a loading hint replaces the editable values.
/// The focused row also splits the buffer at the cursor so the caret cell is
/// drawn with an inverted style and the terminal caret is positioned on the
/// same cell, matching the rename-file popup behaviour.
pub fn draw_metadata_form(frame: &mut Frame, area: Rect, view: &PanelViewModel, theme: &Theme) {
    let Some(DialogMode::EditMetadata {
        fields,
        cursor,
        error,
        loading,
        ..
    }) = view.dialog.as_ref().map(|dialog| &dialog.mode)
    else {
        return;
    };

    let form_area = centered_rect(FORM_WIDTH, FORM_HEIGHT, area);
    frame.render_widget(Clear, form_area);

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Edit metadata ",
            Style::new().fg(theme.popup_border).bold(),
        ))
        .title_bottom(Line::from(Span::styled(
            " enter save · ↑/↓ field · ←/→ move cursor ",
            Style::new().fg(theme.text_muted),
        )))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));

    let inner = block.inner(form_area);
    frame.render_widget(block, form_area);

    let buffer = view
        .dialog
        .as_ref()
        .map(|dialog| dialog.input.as_str())
        .unwrap_or_default();
    let buffer_chars = buffer.chars().count();
    let text_cursor = view
        .dialog
        .as_ref()
        .map(|dialog| dialog.cursor)
        .unwrap_or(0)
        .min(buffer_chars);
    let cursor_style = Style::new()
        .fg(theme.background)
        .bg(theme.highlight)
        .add_modifier(Modifier::BOLD);
    let focused_text_style = Style::new().fg(theme.highlight).bold();
    let inactive_style = Style::new().fg(theme.text);

    let rows: Vec<Line> = view
        .metadata_labels
        .iter()
        .enumerate()
        .map(|(index, field)| {
            let is_cursor = index == *cursor;
            let label_prefix = format!(" {:>12}  ", field);
            if is_cursor {
                // Split the live buffer at the cursor so the caret cell can
                // carry the inverted style without disturbing the label.
                let (before, after) = split_at_char_index(buffer, text_cursor);
                let cursor_char = after.chars().next().unwrap_or(' ');
                let rest = after.chars().skip(1).collect::<String>();
                let mut spans = vec![
                    Span::styled(label_prefix.clone(), focused_text_style),
                    Span::styled(before.to_string(), focused_text_style),
                    Span::styled(cursor_char.to_string(), cursor_style),
                ];
                if !rest.is_empty() {
                    spans.push(Span::styled(rest, focused_text_style));
                }
                Line::from(spans)
            } else {
                // Other rows show the committed value, no cursor rendering.
                let label = format!("{}{}", label_prefix, fields[index]);
                Line::from(Span::styled(label, inactive_style))
            }
        })
        .collect();

    frame.render_widget(
        Paragraph::new(rows).style(Style::new().bg(theme.background)),
        inner,
    );

    let hint = if *loading {
        " Reading tags…".to_string()
    } else {
        error
            .clone()
            .map(|message| format!(" {message}"))
            .unwrap_or_else(|| " ".to_string())
    };
    // The hint line renders over the last content row so the loading state
    // and the validation errors stay visible without resizing the block
    let hint_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        ..inner
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::new().fg(theme.error)))),
        hint_area,
    );

    // Position the terminal caret on the same cell as the inverted caret
    // drawn above so the OS cursor matches the visual caret on the focused
    // row. Skip positioning while the prefill is loading because the buffer
    // is empty and there is nothing meaningful for the user to type yet.
    if !loading {
        let label_width = u16::try_from(label_width_for_field()).unwrap_or(u16::MAX);
        let cursor_x = inner
            .x
            .saturating_add(label_width)
            .saturating_add(u16::try_from(text_cursor).unwrap_or(u16::MAX))
            .min(inner.x.saturating_add(inner.width).saturating_sub(1));
        let cursor_y = inner
            .y
            .saturating_add(u16::try_from(*cursor).unwrap_or(u16::MAX));
        frame.set_cursor_position((cursor_x, cursor_y));
    }
}

/// One-cell-inset, right-aligned label column width used by the metadata form
/// (`" {:>12}  "` prefix). Lifted out of the renderer so the cursor-x math
/// uses the exact same value as the label cells.
fn label_width_for_field() -> usize {
    // " {label:>12}  " => 1 leading space + 12 right-aligned + 2 separators
    1 + 12 + 2
}

/// Draw the centered help popup listing every default binding.
///
/// The content comes straight from `input::default_keymap_summary` so the
/// reference can never drift away from the real keymap. Scrolling is a
/// plain paragraph offset: the command layer already clamps it against
/// the content length, and the fixed hint line lives on the bottom border
/// so it never scrolls out of view.
pub fn draw_help_popup(
    frame: &mut Frame,
    area: Rect,
    scroll: u16,
    theme: &Theme,
    view: &PanelViewModel,
) {
    let popup_area = centered_percent_rect(HELP_WIDTH_PERCENT, HELP_HEIGHT_PERCENT, area);

    frame.render_widget(Clear, popup_area);

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Help ",
            Style::new().fg(theme.popup_border).bold(),
        ))
        .title_bottom(Line::from(Span::styled(
            " esc/enter/q close · j/k scroll ",
            Style::new().fg(theme.text_muted),
        )))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    frame.render_widget(
        Paragraph::new(build_help_lines(theme, view.help.lines())).scroll((scroll, 0)),
        inner,
    );
}

/// Draw the named-playlist manager popup listing every saved playlist.
///
/// The active playlist (the one currently loaded into the queue) wears a
/// marker so the user can tell what they are editing at a glance. When the
/// naming dialog is open it overlays a small input box on top of the list.
pub fn draw_playlist_manager_popup(
    frame: &mut Frame,
    area: Rect,
    view: &PanelViewModel,
    theme: &Theme,
) {
    let (cursor, names) = match view.popup.as_ref() {
        Some(Popup::PlaylistManager { cursor, names }) => (cursor, names),
        _ => return,
    };

    let popup_area = centered_rect(MANAGER_WIDTH, MANAGER_HEIGHT, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            " Playlist manager ",
            Style::new().fg(theme.popup_border).bold(),
        ))
        .title_bottom(Line::from(Span::styled(
            " p close · r rename · d delete · enter load ",
            Style::new().fg(theme.text_muted),
        )))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let active_name = view.active_playlist_name.as_deref();
    let rows: Vec<ListItem> = if names.is_empty() {
        vec![ListItem::new(Line::from(Span::styled(
            "No saved playlists yet - focus the queue and press a to save",
            Style::new().fg(theme.text_muted),
        )))]
    } else {
        // Derive the visible slice from the authoritative cursor and the
        // measured popup viewport. Rendering stays pure: no scroll state is
        // invented or written back while drawing the frame.
        let visible = usize::from(inner.height);
        let selected = (*cursor).min(names.len().saturating_sub(1));
        let start = crate::browser_state::clamp_scroll_offset(
            view.playlist_manager_scroll_offset,
            selected,
            names.len(),
            visible,
        );
        names
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, name)| {
                let is_cursor = index == selected;
                let marker = if active_name == Some(name.as_str()) {
                    "◉"
                } else {
                    "○"
                };
                let style = if is_cursor {
                    Style::new().fg(theme.highlight)
                } else {
                    Style::new().fg(theme.text)
                };
                ListItem::new(Line::from(Span::styled(
                    format!("{marker} {}", sanitize_text(name)),
                    style,
                )))
            })
            .collect()
    };

    frame.render_widget(
        List::new(rows).style(Style::new().bg(theme.background)),
        inner,
    );

    // The naming dialog overlays the list while a name is being typed
    if view.dialog.is_some() {
        draw_naming_dialog(frame, popup_area, view, theme);
    }
}

/// Draw the in-popup naming dialog over the playlist manager list.
///
/// The typed name fills the input line and any validation error replaces the
/// hint so the user gets immediate feedback without leaving the dialog. The
/// cursor is rendered as an inverted cell on the input line and the
/// terminal caret is positioned at that same spot so it stays visible across
/// every terminal.
///
/// The rename-file popup widens to [`RENAME_FILE_WIDTH`] cells and locks the
/// file extension as a non-editable suffix. When the editable base name
/// overflows the inner area, the renderer scrolls a window that always
/// shows the cursor with a two-cell margin on each side (or as much margin
/// as the buffer allows at the ends).
pub fn draw_naming_dialog(frame: &mut Frame, parent: Rect, view: &PanelViewModel, theme: &Theme) {
    let mode = match view.dialog.as_ref().map(|dialog| &dialog.mode) {
        Some(mode) => mode,
        None => return,
    };
    let width = match mode {
        DialogMode::RenameFile { .. } | DialogMode::AddStream { .. } => RENAME_FILE_WIDTH,
        _ => DIALOG_WIDTH,
    };
    let dialog_area = centered_rect(width, DIALOG_HEIGHT, parent);
    frame.render_widget(Clear, dialog_area);

    let label = match mode {
        DialogMode::RenameSaved | DialogMode::RenamePlaying => "Rename playlist",
        DialogMode::SaveAs => "Save playlist as",
        DialogMode::NewPlaylist => "New playlist",
        DialogMode::RenameFile { .. } => "Rename file",
        DialogMode::RenameStream { .. } => "Rename stream",
        DialogMode::AddStream { loading, .. } => {
            if *loading {
                "Add Stream (resolving…)"
            } else {
                "Add Stream"
            }
        }
        // Settings and metadata fields draw their own editors
        DialogMode::SettingsEdit | DialogMode::EditMetadata { .. } => return,
    };

    let block = Block::bordered()
        .border_type(theme.border_type)
        .title(Span::styled(
            format!(" {label} "),
            Style::new().fg(theme.popup_border).bold(),
        ))
        .border_style(Style::new().fg(theme.popup_border))
        .style(Style::new().bg(theme.background));

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    // The file rename dialog carries its error in the mode, the playlist
    // naming dialogs use the shared state field
    let error = match mode {
        DialogMode::RenameFile { error, .. } => error.clone(),
        DialogMode::AddStream { error, .. } => error.clone(),
        _ => view.dialog.as_ref().and_then(|dialog| dialog.error.clone()),
    };

    // The input line is split at the cursor so the caret cell can be drawn
    // with an inverted style; if the cursor sits past the end of the buffer
    // (clamped to length above) the inverted cell acts as an end-of-line
    // caret right after the last character. When the buffer overflows the
    // popup width the renderer scrolls a window that keeps the cursor with
    // a two-cell margin on each side.
    let buffer = view
        .dialog
        .as_ref()
        .map(|dialog| dialog.input.as_str())
        .unwrap_or_default();
    let locked_extension = match mode {
        DialogMode::RenameFile { .. } => view
            .dialog
            .as_ref()
            .and_then(|dialog| dialog.extension.clone()),
        _ => None,
    };
    let cursor = view
        .dialog
        .as_ref()
        .map(|dialog| dialog.cursor)
        .unwrap_or(0)
        .min(buffer.chars().count());
    let locked_style = Style::new().fg(theme.text_muted);
    let (input_line, visual_cursor) = render_focused_input(
        buffer,
        cursor,
        inner.width as usize,
        locked_extension.as_deref(),
        theme,
        locked_style,
    );

    let hint_line = if let DialogMode::AddStream { loading: true, .. } = mode {
        // The resolver is running off-thread; show the in-flight spinner so
        // the user gets the same animation the Now Playing band uses. The
        // dialog title already says "(resolving…)" so the label stays just
        // "Loading" — the title carries the context.
        Line::from(
            loading_line(&view.footer.spinner_frame)
                .into_iter()
                .map(|span| span.style(Style::new().fg(theme.text_muted)))
                .collect::<Vec<_>>(),
        )
    } else {
        Line::from(Span::styled(
            error.unwrap_or_else(|| {
                if matches!(mode, DialogMode::AddStream { .. }) {
                    "https://… or youtube.com · enter to add · esc to cancel".to_string()
                } else {
                    "enter to confirm · esc to cancel".to_string()
                }
            }),
            Style::new().fg(theme.error),
        ))
    };
    let lines = vec![input_line, hint_line];
    frame.render_widget(Paragraph::new(lines), inner);

    // Place the terminal caret on the same cell as the inverted block above
    // so the OS cursor matches the visual caret even on soft cursors.
    let cursor_x = inner
        .x
        .saturating_add(u16::try_from(visual_cursor).unwrap_or(u16::MAX))
        .min(inner.x.saturating_add(inner.width).saturating_sub(1));
    let cursor_y = inner.y;
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Build the editable input line with the caret cell and an optional locked
/// suffix, returning the line plus the column at which the terminal caret
/// should sit.
///
/// `buffer` is the editable portion (without the locked suffix), `cursor` is
/// the character offset inside `buffer`, `visible_width` is the number of
/// cells available for the editable window, and `locked_suffix` (when
/// `Some`) is rendered after the editable window in a muted style. The
/// window always keeps the cursor with a two-cell margin on each side, or
/// as much margin as the buffer allows when the cursor sits near the
/// start or end of the editable text.
fn render_focused_input(
    buffer: &str,
    cursor: usize,
    visible_width: usize,
    locked_suffix: Option<&str>,
    theme: &Theme,
    locked_style: Style,
) -> (Line<'static>, usize) {
    let text_style = Style::new().fg(theme.text);
    let cursor_style = Style::new()
        .fg(theme.background)
        .bg(theme.highlight)
        .add_modifier(Modifier::BOLD);

    let chars: Vec<char> = buffer.chars().collect();
    let cursor = cursor.min(chars.len());
    let char_widths: Vec<usize> = chars.iter().map(|ch| ch.width().unwrap_or(0)).collect();
    let columns = cumulative_widths(&char_widths);
    let buffer_width = buffer.width();
    let visible_width = visible_width.max(1);

    // When the locked suffix exists, the suffix still consumes cells on the
    // line even though it is never editable. Reserve space for it inside the
    // window so the cursor can never land past the suffix boundary; the
    // suffix itself is appended verbatim after the editable window.
    let suffix_width = locked_suffix.map(str::width).unwrap_or(0);
    let editable_width = visible_width.saturating_sub(suffix_width).max(1);

    // No scrolling when the editable portion fits inside its reserved area.
    if buffer_width <= editable_width {
        let (before, after) = split_chars_at(&chars, cursor);
        let mut line = render_caret_line(&before, &after, text_style, cursor_style);
        if let Some(suffix) = locked_suffix {
            line.spans
                .push(Span::styled(suffix.to_string(), locked_style));
        }
        return (line, columns[cursor].min(editable_width.saturating_sub(1)));
    }

    let scroll_offset = compute_scroll_offset(columns[cursor], buffer_width, editable_width, 2);
    let window_start = choose_window_start(&char_widths, cursor, editable_width, scroll_offset);
    let window_end = window_end(&char_widths, window_start, editable_width);
    let window: Vec<char> = chars[window_start..window_end].to_vec();
    let visual_cursor = cursor.saturating_sub(window_start).min(window.len());
    let (before, after) = split_chars_at(&window, visual_cursor);
    let mut line = render_caret_line(&before, &after, text_style, cursor_style);
    if let Some(suffix) = locked_suffix {
        line.spans
            .push(Span::styled(suffix.to_string(), locked_style));
    }
    (
        line,
        display_width(&before).min(editable_width.saturating_sub(1)),
    )
}

/// Render the text-before-cursor, the caret cell, and the text-after-cursor
/// in the styles expected by the popup, with no locked suffix.
fn render_caret_line(
    before: &[char],
    after: &[char],
    text_style: Style,
    cursor_style: Style,
) -> Line<'static> {
    let cursor_char = after.first().copied().unwrap_or(' ');
    let rest: String = after.iter().skip(1).collect();
    let mut spans = vec![
        Span::styled(before.iter().collect::<String>(), text_style),
        Span::styled(cursor_char.to_string(), cursor_style),
    ];
    if !rest.is_empty() {
        spans.push(Span::styled(rest, text_style));
    }
    Line::from(spans)
}

/// Compute the scroll offset that keeps `cursor` in focus with at least
/// `margin` cells of editable text on each side whenever the buffer is
/// long enough to allow it.
///
/// When the buffer fits inside `visible_width` the offset is 0 (no
/// scrolling). Otherwise the offset places the cursor with `margin` cells
/// of context on the left and as much context as fits on the right; if
/// the cursor is too close to the end for both margins to fit, the
/// window slides right so the cursor stays visible (a graceful fallback
/// instead of letting `clamp` panic when the requested margins collide).
fn compute_scroll_offset(
    cursor_column: usize,
    buffer_width: usize,
    visible_width: usize,
    margin: usize,
) -> usize {
    if buffer_width <= visible_width {
        return 0;
    }
    let max_start = buffer_width - visible_width;
    // Ideal: place the cursor `margin` cells from the left edge of the
    // window so the user sees the requested context on both sides.
    let ideal_start = cursor_column.saturating_sub(margin);
    // Want `margin` cells past the cursor to stay inside the window too.
    let min_start = cursor_column
        .saturating_add(margin)
        .saturating_sub(visible_width);
    if min_start > max_start {
        // The two margin constraints collide (the cursor is too close to
        // either end for both sides to keep `margin` cells). Slide the
        // window right so the cursor stays visible at the right edge.
        return max_start.min(cursor_column);
    }
    ideal_start.clamp(min_start, max_start)
}

/// Return cumulative display columns at every character boundary.
fn cumulative_widths(widths: &[usize]) -> Vec<usize> {
    let mut columns: Vec<usize> = Vec::with_capacity(widths.len() + 1);
    columns.push(0);
    for width in widths {
        columns.push(columns.last().copied().unwrap_or(0).saturating_add(*width));
    }
    columns
}

/// Select the character boundary nearest to the requested display-column
/// scroll position while keeping the logical cursor inside the window.
fn choose_window_start(
    widths: &[usize],
    cursor: usize,
    visible_width: usize,
    requested_column: usize,
) -> usize {
    let cursor = cursor.min(widths.len());
    let columns = cumulative_widths(widths);
    let mut best_start = 0;
    let mut best_column = 0;
    let mut best_distance = usize::MAX;

    for start in 0..=cursor {
        let end = window_end(widths, start, visible_width);
        if end < cursor {
            continue;
        }

        let column = columns[start];
        let distance = column.abs_diff(requested_column);
        // Prefer the later boundary when a wide glyph puts two boundaries at
        // the same distance. This keeps end-of-input windows anchored to the
        // final visible glyph instead of leaving an avoidable blank tail.
        if distance < best_distance || (distance == best_distance && column > best_column) {
            best_start = start;
            best_column = column;
            best_distance = distance;
        }
    }

    best_start
}

/// Return the first character boundary after the display-column window.
/// Zero-width combining characters remain attached to the visible window.
fn window_end(widths: &[usize], start: usize, visible_width: usize) -> usize {
    let mut used: usize = 0;
    let mut end = start;
    while end < widths.len() {
        let width = widths[end];
        if width > 0 && used > 0 && used.saturating_add(width) > visible_width {
            break;
        }
        used = used.saturating_add(width);
        end += 1;
        if used >= visible_width {
            while end < widths.len() && widths[end] == 0 {
                end += 1;
            }
            break;
        }
    }
    end
}

fn display_width(chars: &[char]) -> usize {
    chars.iter().map(|ch| ch.width().unwrap_or(0)).sum()
}

/// Split a character slice at `index`, returning the two halves without
/// touching bytes (the caller already owns the chars).
fn split_chars_at(chars: &[char], index: usize) -> (Vec<char>, Vec<char>) {
    let at = index.min(chars.len());
    (chars[..at].to_vec(), chars[at..].to_vec())
}

/// Split `text` at the `index`-th character boundary, returning the prefix
/// up to (but not including) that character and the suffix from there on.
///
/// `index` is clamped to the character length of `text` so callers always
/// get a valid slice pair and never panic on out-of-range indices.
fn split_at_char_index(text: &str, index: usize) -> (&str, &str) {
    let len = text.chars().count();
    let at = index.min(len);
    let byte_index = text
        .char_indices()
        .nth(at)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len());
    text.split_at(byte_index)
}

#[cfg(test)]
mod tests {
    use super::render_focused_input;
    use crate::ui::theme::Theme;
    use ratatui::style::Style;
    use unicode_width::UnicodeWidthStr;

    fn render_input(
        buffer: &str,
        cursor: usize,
        visible_width: usize,
        locked_suffix: Option<&str>,
    ) -> (String, usize) {
        let (line, visual_cursor) = render_focused_input(
            buffer,
            cursor,
            visible_width,
            locked_suffix,
            &Theme::default(),
            Style::default(),
        );
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        (text, visual_cursor)
    }

    #[test]
    fn focused_input_uses_character_columns_for_ascii_windows() {
        let (text, cursor) = render_input("abcdef", 3, 5, None);

        assert_eq!(text, "bcdef");
        assert_eq!(cursor, 2);
        assert_eq!(text.width(), 5);
    }

    #[test]
    fn focused_input_uses_wide_columns_for_cjk_text() {
        let (text, cursor) = render_input("你好世界", 2, 5, None);

        assert_eq!(text, "好世");
        assert_eq!(cursor, 2);
        assert_eq!(text.width(), 4);
    }

    #[test]
    fn focused_input_uses_wide_columns_for_emoji() {
        let (text, cursor) = render_input("a🙂bc", 2, 4, None);

        assert_eq!(text, "🙂bc");
        assert_eq!(cursor, 2);
        assert_eq!(text.width(), 4);
    }

    #[test]
    fn focused_input_keeps_combining_characters_in_the_visible_window() {
        let (text, cursor) = render_input("e\u{301}clair", 1, 4, None);

        assert_eq!(text, "e\u{301}cla");
        assert_eq!(cursor, 1);
        assert_eq!(text.width(), 4);
    }

    #[test]
    fn focused_input_reserves_locked_suffix_by_display_width() {
        let (text, cursor) = render_input("track", 3, 8, Some(".wav"));

        assert_eq!(text, "rack.wav");
        assert_eq!(cursor, 2);
        assert_eq!(text.width(), 8);

        let (text, cursor) = render_input("track", 0, 8, Some("界.ogg"));
        assert_eq!(text, "tr界.ogg");
        assert_eq!(cursor, 0);
        assert_eq!(text.width(), 8);
    }

    #[test]
    fn focused_input_scrolls_long_input_by_display_columns() {
        let (text, cursor) = render_input("abcdefghijk", 8, 6, None);

        assert_eq!(text, "fghijk");
        assert_eq!(cursor, 3);
        assert_eq!(text.width(), 6);
    }

    #[test]
    fn focused_input_handles_narrow_and_degenerate_popups() {
        let (text, cursor) = render_input("🙂", 0, 0, Some(".wav"));
        assert_eq!(text, "🙂.wav");
        assert_eq!(cursor, 0);

        let (text, cursor) = render_input("", 99, 1, None);
        assert_eq!(text, " ");
        assert_eq!(cursor, 0);
    }

    #[test]
    fn focused_input_keeps_caret_at_character_boundaries() {
        let (_, start) = render_input("你好", 0, 4, None);
        let (_, after_first) = render_input("你好", 1, 4, None);
        let (_, end) = render_input("你好", 2, 4, None);

        assert_eq!(start, 0);
        assert_eq!(after_first, 2);
        assert_eq!(end, 3);
    }
}
