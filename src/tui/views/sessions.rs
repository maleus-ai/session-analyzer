//! Sessions page: every session ranked, sortable; Enter/click drills in.

use crate::analysis::{SessionReport, fmt_int, short_model};
use crate::query;
use crate::tui::app::App;
use crate::tui::format::truncate;
use crate::tui::widgets::table;
use ratatui::prelude::*;
use ratatui::widgets::Row;

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    // Sort references — never clone the (potentially huge) session reports per frame.
    let mut list: Vec<&SessionReport> = app.a.sessions.iter().collect();
    let col = query::SESSION_COLS[app.sort_col[1]];
    let _ = query::sort_sessions(&mut list, col, app.sort_desc[1]);
    let headers = ["COST", "TOTAL", "TURNS", "ACTIVE_H", "IDLE", "ENTRY", "MODEL", "TITLE"];
    let widths = [8u16, 11, 6, 8, 5, 8, 11, 0];
    table::capture_headers(app, area, &widths);
    let n = list.len();
    table::render(f, app, area, &headers, &widths, 1, n, |i| {
        let s = list[i];
        let m = &s.metrics;
        Row::new(vec![
            format!("${:.2}", m.cost_usd),
            fmt_int(m.usage.total()),
            m.assistant_turns.to_string(),
            format!("{:.1}", m.active_ms as f64 / 3.6e6),
            m.idle_gaps.to_string(),
            if s.entrypoint.is_empty() { "-".into() } else { s.entrypoint.clone() },
            short_model(&m.dominant_model()),
            truncate(&s.title, 34),
        ])
    }, " Sessions (Enter/click to drill in) ");
}
