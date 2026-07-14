//! Sinks page: token sinks ranked by amplified cost (size × turns resident).

use crate::analysis::{fmt_int, short_path};
use crate::query;
use crate::tui::app::App;
use crate::tui::format::truncate;
use crate::tui::widgets::table;
use ratatui::prelude::*;
use ratatui::widgets::Row;

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let mut list = query::sinks_only(app.cache_attr());
    let col = query::SINK_COLS[app.sort_col[5]];
    let _ = query::sort_sinks(&mut list, col, app.sort_desc[5]);
    let headers = ["AMPL $", "SIZE", "REPLAYED", "SHARE%", "N", "TOOL", "TARGET"];
    let widths = [8u16, 10, 12, 7, 4, 10, 0];
    table::capture_headers(app, area, &widths);
    let n = list.len();
    table::render(f, app, area, &headers, &widths, 5, n, |i| {
        let c = &list[i];
        Row::new(vec![
            format!("${:.2}", c.amplified_cost),
            fmt_int(c.tokens),
            fmt_int(c.contribution),
            format!("{:.1}", c.share * 100.0),
            c.entries.to_string(),
            truncate(&c.tool, 10),
            short_path(&c.target),
        ])
    }, " Token sinks (amplified = size × turns resident · Enter for detail) ");
}
