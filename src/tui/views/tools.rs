//! Tools page: per-tool call counts, result-token footprint and errors.

use crate::analysis::fmt_int;
use crate::query;
use crate::tui::app::App;
use crate::tui::theme;
use crate::tui::widgets::table;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Row};

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let m = app.metrics();
    // A capability the agent asked for and could not get is the usual root cause behind
    // workaround loops — it belongs above the usage table, not buried in a CLI subcommand.
    let unavailable: Vec<(String, u64)> = m.tools_unavailable.iter().map(|(k, v)| (k.clone(), *v)).collect();
    let deferred = m.deferred_tools.len();
    let area = if unavailable.is_empty() {
        area
    } else {
        let rows = (unavailable.len() as u16 + 2).min(6);
        let split = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(rows), Constraint::Min(3)]).split(area);
        let mut lines: Vec<Line> = Vec::new();
        for (name, n) in unavailable.iter().take(4) {
            lines.push(Line::from(vec![
                Span::styled(format!("  ✗ {name}"), Style::default().fg(theme::BAD).add_modifier(Modifier::BOLD)),
                Span::styled(format!("   searched {n}×, never provided by the harness"), Style::default().fg(Color::Gray)),
            ]));
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(theme::BAD))
                    .title(" ⚠ Requested but UNAVAILABLE "),
            ),
            split[0],
        );
        split[1]
    };
    let mut list = m.tools.clone();
    let col = query::TOOL_COLS[app.sort_col[4]];
    let _ = query::sort_tools(&mut list, col, app.sort_desc[4]);
    let headers = ["TOOL", "CALLS", "RESULT TOK", "IN CHARS", "ERR"];
    let widths = [14u16, 8, 12, 12, 0];
    table::capture_headers(app, area, &widths);
    let n = list.len();
    table::render(f, app, area, &headers, &widths, 4, n, |i| {
        let t = &list[i];
        Row::new(vec![t.name.clone(), t.calls.to_string(), fmt_int(t.result_tokens_est), fmt_int(t.input_chars), t.errors.to_string()])
    }, &if deferred > 0 {
        format!(" Tools used — {} more loadable on demand (`ssa tools --available`) · Enter for detail ", deferred)
    } else {
        " Tools (Enter for detail) ".to_string()
    });
}
