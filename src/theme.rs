use ratatui::style::{Color, Modifier, Style};

pub const PRODUCTIVE: Color = Color::Green;
pub const UNPRODUCTIVE: Color = Color::Red;
pub const NEUTRAL: Color = Color::Rgb(255, 200, 50);
pub const ACCENT: Color = Color::Cyan;
pub const MUTED: Color = Color::DarkGray;

pub const TEXT: Color = Color::White;
pub const TEXT_DIM: Color = Color::Gray;

#[allow(dead_code)]
pub struct Theme {
    pub title: Style,
    pub tab_active: Style,
    pub tab_inactive: Style,
    pub header: Style,
    pub border: Style,
    pub key_hint: Style,
    pub key_label: Style,
    pub productive: Style,
    pub unproductive: Style,
    pub neutral: Style,
    pub value: Style,
    pub value_dim: Style,
    pub accent: Style,
    pub bar_productive: Style,
    pub bar_unproductive: Style,
    pub bar_neutral: Style,
    pub bar_bg: Style,
    pub table_header: Style,
    pub table_row: Style,
    pub table_row_alt: Style,
    pub table_selected: Style,
    pub gauge_productive: Style,
    pub gauge_unproductive: Style,
    pub gauge_neutral: Style,
    pub warning: Style,
}

pub const THEME: Theme = Theme {
    title: Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
    tab_active: Style::new()
        .fg(TEXT)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD),
    tab_inactive: Style::new().fg(TEXT_DIM),
    header: Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
    border: Style::new().fg(Color::Rgb(60, 60, 80)),
    key_hint: Style::new().fg(Color::Black).bg(Color::DarkGray),
    key_label: Style::new().fg(Color::DarkGray).bg(Color::Black),
    productive: Style::new().fg(PRODUCTIVE),
    unproductive: Style::new().fg(UNPRODUCTIVE),
    neutral: Style::new().fg(NEUTRAL),
    value: Style::new().fg(TEXT).add_modifier(Modifier::BOLD),
    value_dim: Style::new().fg(MUTED),
    accent: Style::new().fg(ACCENT),
    bar_productive: Style::new().fg(PRODUCTIVE),
    bar_unproductive: Style::new().fg(UNPRODUCTIVE),
    bar_neutral: Style::new().fg(NEUTRAL),
    bar_bg: Style::new().fg(Color::Rgb(40, 40, 50)),
    table_header: Style::new()
        .fg(TEXT)
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::UNDERLINED),
    table_row: Style::new().fg(TEXT_DIM),
    table_row_alt: Style::new().fg(TEXT_DIM).bg(Color::Rgb(20, 20, 30)),
    table_selected: Style::new()
        .fg(TEXT)
        .bg(Color::Rgb(40, 50, 70))
        .add_modifier(Modifier::BOLD),
    gauge_productive: Style::new().fg(PRODUCTIVE).bg(Color::Rgb(20, 50, 20)),
    gauge_unproductive: Style::new().fg(UNPRODUCTIVE).bg(Color::Rgb(50, 20, 20)),
    gauge_neutral: Style::new().fg(NEUTRAL).bg(Color::Rgb(50, 50, 20)),
    warning: Style::new().fg(UNPRODUCTIVE).add_modifier(Modifier::BOLD),
};

use crate::config::Category;

/// Get the theme style for a category.
pub fn category_style(cat: Category) -> Style {
    match cat {
        Category::Productive => THEME.productive,
        Category::Unproductive => THEME.unproductive,
        Category::Neutral => THEME.neutral,
    }
}
