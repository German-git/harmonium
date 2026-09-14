//! Private widgets shared by the settings and popup renderers.
//!
//! Keeping these layout primitives here makes the dependency direction
//! explicit: panel renderers depend on this module, while this module does not
//! depend on either renderer.

use crate::ui::theme::Theme;
use crate::ui::view::HelpContentLine;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub(super) const SETTINGS_DISPLAY_COLUMN_FILL: u16 = 3;
pub(super) const SETTINGS_THEME_COLUMN_FILL: u16 = 3;
pub(super) const SETTINGS_COLORS_COLUMN_FILL: u16 = 4;

pub(super) const CHECKBOX_CHECKED: &str = "\u{2713}"; // ✓
pub(super) const CHECKBOX_EMPTY: &str = "\u{25a1}"; // □

pub(super) const POPUP_WIDTH: u16 = 46;
pub(super) const POPUP_HEIGHT: u16 = 6;
pub(super) const FORM_WIDTH: u16 = 52;
pub(super) const FORM_HEIGHT: u16 = 14;
pub(super) const SEARCH_QUERY_WIDTH: u16 = 48;
pub(super) const SEARCH_RESULTS_WIDTH: u16 = 64;
pub(super) const SEARCH_QUERY_HEIGHT: u16 = 6;
pub(super) const SEARCH_LOADING_HEIGHT: u16 = 5;
pub(super) const SEARCH_RESULTS_HEIGHT: u16 = 14;
pub(super) const HELP_WIDTH_PERCENT: u32 = 70;
pub(super) const HELP_HEIGHT_PERCENT: u32 = 75;
pub(super) const MANAGER_WIDTH: u16 = 50;
pub(super) const MANAGER_HEIGHT: u16 = 16;
pub(super) const DIALOG_WIDTH: u16 = 44;
pub(super) const DIALOG_HEIGHT: u16 = 6;
pub(super) const RENAME_FILE_WIDTH: u16 = 64;

pub(super) fn sanitize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' | '\r' | '\t' => out.push(' '),
            _ if ch.is_control() => {}
            _ => out.push(ch),
        }
    }
    out
}

pub(super) fn truncate_to_width(text: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;

    if max == 0 {
        return String::new();
    }
    let sanitized = sanitize_text(text);
    let total: usize = sanitized.chars().map(|ch| ch.width().unwrap_or(0)).sum();
    if total <= max {
        return sanitized;
    }
    let keep = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in sanitized.chars() {
        let width = ch.width().unwrap_or(0);
        if used + width > keep {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push('…');
    out
}

pub(super) fn settings_item_line(
    raw: String,
    selected: bool,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let body = truncate_to_width(&raw, width);
    let style = if selected {
        Style::new()
            .fg(theme.highlight)
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.text)
    };
    Line::from(Span::styled(body, style))
}

pub(super) fn color_item_line(
    selected: bool,
    label: &str,
    value: &str,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let color = value.parse::<ratatui::style::Color>().unwrap_or(theme.text);
    let base = if selected {
        Style::new()
            .fg(theme.highlight)
            .bg(theme.selection)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.text)
    };
    let body = truncate_to_width(&format!("\u{2588} {label}: {value}"), width);
    let mut chars = body.chars();
    let preview = chars.next().unwrap_or(' ');
    Line::from(vec![
        Span::styled(preview.to_string(), base.fg(color)),
        Span::styled(chars.collect::<String>(), base),
    ])
}

pub(super) fn gain_item_line(
    selected: bool,
    gain_db: crate::audio::GainDb,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    use crate::audio::playback::{MAX_GAIN_DB, MIN_GAIN_DB};

    let ratio = ((gain_db.as_f32() - MIN_GAIN_DB) / (MAX_GAIN_DB - MIN_GAIN_DB)).clamp(0.0, 1.0);
    let slots = 11usize;
    let thumb = (ratio * (slots - 1) as f32).round() as usize;
    let bar = format!(
        "{}{}{}",
        "\u{2501}".repeat(thumb),
        "\u{25CF}",
        "\u{2500}".repeat(slots - 1 - thumb),
    );
    let value = format!("{gain_db} dB");
    settings_item_line(format!("Gain  {bar}  {value}"), selected, width, theme)
}

pub(super) fn crossfade_item_line(
    selected: bool,
    seconds: crate::audio::CrossfadeSeconds,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    use crate::config::{CROSSFADE_MAX_SECONDS, CROSSFADE_STEP_SECONDS};

    let total = (CROSSFADE_MAX_SECONDS / CROSSFADE_STEP_SECONDS - 1) as usize;
    let filled = (seconds.as_u16() / CROSSFADE_STEP_SECONDS).min(total as u16) as usize;
    let bar = format!(
        "{}{}",
        "\u{25AE}".repeat(filled),
        "\u{25AF}".repeat(total.saturating_sub(filled)),
    );
    let value = if !seconds.is_enabled() {
        "Off".to_string()
    } else {
        format!("{seconds} s")
    };
    settings_item_line(format!("Crossfade  {bar}  {value}"), selected, width, theme)
}

pub(super) fn build_help_lines<'a>(
    theme: &Theme,
    cached_lines: &'a [HelpContentLine],
) -> Vec<Line<'a>> {
    let mut lines = Vec::new();
    for cached_line in cached_lines {
        match cached_line {
            HelpContentLine::Header(title) => lines.push(Line::from(Span::styled(
                *title,
                Style::new().fg(theme.highlight).bold(),
            ))),
            HelpContentLine::Blank => lines.push(Line::default()),
            HelpContentLine::Row { key, description } => lines.push(Line::from(vec![
                Span::styled(key.as_str(), Style::new().fg(theme.text)),
                Span::styled("  ", Style::new().fg(theme.text_muted)),
                Span::styled(*description, Style::new().fg(theme.text_muted)),
            ])),
        }
    }
    lines
}

pub(super) fn centered_percent_rect(percent_width: u32, percent_height: u32, outer: Rect) -> Rect {
    let width = (u32::from(outer.width) * percent_width / 100) as u16;
    let height = (u32::from(outer.height) * percent_height / 100) as u16;

    centered_rect(width.max(1), height.max(1), outer)
}

pub(super) fn centered_rect(width: u16, height: u16, outer: Rect) -> Rect {
    let width = width.min(outer.width);
    let height = height.min(outer.height);
    let x = outer
        .x
        .saturating_add(outer.width.saturating_sub(width) / 2);
    let y = outer
        .y
        .saturating_add(outer.height.saturating_sub(height) / 2);

    Rect {
        x,
        y,
        width,
        height,
    }
}
