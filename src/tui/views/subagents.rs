//! Sub-agents page: per sub-agent tokens, tool mix, reads/edits and lines changed.

use crate::analysis::{fmt_int, short_model};
use crate::query;
use crate::tui::app::App;
use crate::tui::widgets::table;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Row};

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let mut list = app.metrics().subagents.clone();
    if list.is_empty() {
        f.render_widget(
            Paragraph::new("No sub-agent invocations in scope.").block(Block::default().borders(Borders::ALL).title(" Sub-agents ")),
            area,
        );
        return;
    }
    let col = query::SUBAGENT_COLS[app.sort_col[7]];
    let _ = query::sort_subagents(&mut list, col, app.sort_desc[7]);
    let headers = ["TYPE", "MODEL", "TOKENS", "COST", "TOOLS", "READ", "SRCH", "BASH", "EDIT", "±LINES", "SECS"];
    let widths = [12u16, 12, 10, 8, 6, 5, 5, 5, 5, 10, 0];
    table::capture_headers(app, area, &widths);
    let n = list.len();
    table::render(f, app, area, &headers, &widths, 7, n, |i| {
        let s = &list[i];
        Row::new(vec![
            s.agent_type.clone(),
            short_model(&s.model),
            fmt_int(s.total_tokens),
            format!("${:.2}", crate::pricing::price_for(&s.model).cost(&s.usage)),
            s.tool_use_count.to_string(),
            s.read_count.to_string(),
            s.search_count.to_string(),
            s.bash_count.to_string(),
            s.edit_count.to_string(),
            format!("+{}/-{}", s.lines_added, s.lines_removed),
            format!("{:.0}", s.duration_ms as f64 / 1000.0),
        ])
    }, " Sub-agents (Enter for detail) ");
}
