//! The top tab bar and the bottom context-sensitive help footer.

use crate::tui::app::{App, TABS};
use crate::tui::format::truncate;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

/// Draw the tab bar. Rendered as manual spans so we can record each tab's x-range for
/// mouse hit-testing.
pub fn draw_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let scope = match app.focus {
        Some(i) => format!(" ▸ {} ", truncate(&app.a.sessions[i].title, 36)),
        None => format!(" ▸ all {} sessions · {} ", app.a.sessions.len(), app.info.provider_name),
    };
    let mut spans: Vec<Span> = Vec::new();
    app.tab_hits.clear();
    let mut x = area.x + 1;
    for (i, name) in TABS.iter().enumerate() {
        let label = format!(" {name} ");
        let w = label.chars().count() as u16;
        app.tab_hits.push((x, x + w, i));
        let style = if i == app.tab {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw("│"));
        x += w + 1;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" session-analyzer ")
        .title(Line::from(scope).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
}

pub fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    // Active search box takes over the footer.
    if app.search_active {
        let line = Line::from(vec![
            Span::styled(" /", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::styled(app.search.clone(), Style::default().fg(Color::White)),
            Span::styled("▌", Style::default().fg(Color::Cyan)),
            Span::styled("   (Enter jump · Esc cancel)", Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    if app.tab == 2 && !app.search.is_empty() {
        let line = Line::from(vec![
            Span::styled(format!(" search: {} ", app.search), Style::default().fg(Color::Black).bg(Color::Cyan)),
            Span::styled("  n/N next/prev · Esc clear · Enter/click expand ", Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let help = if app.popup.is_some() {
        " ↑/↓ scroll · Esc/Enter close "
    } else {
        match app.tab {
            1 => " click/Enter drill in · ↑↓ move · [ ] sort · r reverse · click header · q quit ",
            2 => " ↑↓ bubbles · Enter expand · / search · Esc back · 1-9/0 tabs · q quit ",
            3 => " ↑↓ turns · Enter detail · [ ] sort · r reverse · Esc back · q quit ",
            4 | 5 | 6 | 7 | 8 => " ↑↓ move · Enter/click detail · [ ] sort · r reverse · q quit ",
            _ => " ←→/1-9/0 tabs · ↑↓ move · [ ] sort · r reverse · click to sort/select · q quit ",
        }
    };
    f.render_widget(Paragraph::new(Line::from(help).style(Style::default().fg(Color::DarkGray))), area);
}
