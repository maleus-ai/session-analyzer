//! Issues page: detected inefficiencies, colour-coded by severity.

use crate::analysis::fmt_int;
use crate::tui::app::App;
use crate::tui::theme;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let m = app.metrics();
    // Findings carry the whole explanation; cutting them at the right edge throws away the
    // part that says what to do about it.
    let wrap_at = (area.width as usize).saturating_sub(8).max(20);
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
            ]
            .into_iter()
            .chain(
                crate::tui::format::wrap_str(&fd.detail, wrap_at)
                    .into_iter()
                    .map(|l| Line::from(Span::styled(format!("    {l}"), Style::default().fg(Color::Gray)))),
            )
            .collect::<Vec<_>>())
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
