//! Design tokens: the semantic colour palette and shared style helpers, so appearance is
//! defined in one place instead of scattered literals.

use crate::analysis::Severity;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};

// Roles / accents.
pub const USER: Color = Color::Blue;
pub const ASSISTANT: Color = Color::Green;
pub const ACCENT: Color = Color::Cyan;
pub const MUTED: Color = Color::DarkGray;
pub const GOOD: Color = Color::Green;
pub const WARN: Color = Color::Yellow;
pub const BAD: Color = Color::Red;

/// Border colour for a bubble/box — bright cyan when selected, else its role colour.
pub fn border_style(selected: bool, base: Color) -> Style {
    if selected {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(base)
    }
}

/// Colour for a finding by severity.
pub fn severity_color(sev: Severity) -> Color {
    match sev {
        Severity::High => BAD,
        Severity::Warn => WARN,
        Severity::Info => Color::Gray,
    }
}

/// The bordered block used by modal popups (cyan frame + scroll/close hint).
pub fn popup_block(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(title.to_string(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)))
        .title(Line::from(" ↑↓ scroll · Esc close ").right_aligned())
}

/// Style for the active sort-column header cell.
pub fn active_header() -> Style {
    Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Style for an inactive header cell.
pub fn header() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}
