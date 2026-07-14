//! Issues page: detected inefficiencies, colour-coded by severity.

use crate::analysis::fmt_int;
use crate::tui::app::App;
use crate::tui::theme;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let m = app.metrics();
    if m.findings.is_empty() {
        f.render_widget(
            Paragraph::new("✓ No notable inefficiencies detected.")
                .style(Style::default().fg(theme::GOOD))
                .block(Block::default().borders(Borders::ALL).title(" Issues ")),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = m
        .findings
        .iter()
        .map(|fd| {
            let color = theme::severity_color(fd.severity);
            let waste = if fd.wasted_tokens_est > 0 { format!("  (~{} tok)", fmt_int(fd.wasted_tokens_est)) } else { String::new() };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("[{}] ", fd.severity.label()), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(fd.kind.clone(), Style::default().add_modifier(Modifier::BOLD)),
                    Span::styled(waste, Style::default().fg(theme::MUTED)),
                ]),
                Line::from(Span::styled(format!("    {}", fd.detail), Style::default().fg(Color::Gray))),
            ])
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.sel[8].min(m.findings.len().saturating_sub(1))));
    f.render_stateful_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(" Inefficiencies ")).highlight_style(Style::default().bg(Color::DarkGray)),
        area,
        &mut state,
    );
}
