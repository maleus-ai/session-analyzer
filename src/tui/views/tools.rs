//! Tools page: per-tool call counts, result-token footprint and errors.

use crate::analysis::fmt_int;
use crate::query;
use crate::tui::app::App;
use crate::tui::widgets::table;
use ratatui::prelude::*;
use ratatui::widgets::Row;

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let mut list = app.metrics().tools.clone();
    let col = query::TOOL_COLS[app.sort_col[4]];
    let _ = query::sort_tools(&mut list, col, app.sort_desc[4]);
    let headers = ["TOOL", "CALLS", "RESULT TOK", "IN CHARS", "ERR"];
    let widths = [14u16, 8, 12, 12, 0];
    table::capture_headers(app, area, &widths);
    let n = list.len();
    table::render(f, app, area, &headers, &widths, 4, n, |i| {
        let t = &list[i];
        Row::new(vec![t.name.clone(), t.calls.to_string(), fmt_int(t.result_tokens_est), fmt_int(t.input_chars), t.errors.to_string()])
    }, " Tools (Enter for detail) ");
}
