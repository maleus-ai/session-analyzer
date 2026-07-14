//! Cache-attribution page: decompose cache-read by resident content, with a share bar.

use crate::analysis::{fmt_int, short_path};
use crate::query;
use crate::tui::app::App;
use crate::tui::format::truncate;
use crate::tui::widgets::table;
use ratatui::prelude::*;
use ratatui::widgets::Row;

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let mut list = app.cache_attr().to_vec();
    let col = query::CACHEATTR_COLS[app.sort_col[6]];
    let _ = query::sort_cacheattr(&mut list, col, app.sort_desc[6]);
    let read = app.metrics().usage.cache_read_input_tokens.max(1);
    let headers = ["SHARE%", "BAR", "REPLAYED", "$", "N", "SOURCE"];
    let widths = [7u16, 16, 12, 8, 4, 0];
    table::capture_headers(app, area, &widths);
    let n = list.len();
    table::render(f, app, area, &headers, &widths, 6, n, |i| {
        let c = &list[i];
        let share = c.contribution as f64 / read as f64;
        let filled = (share * 14.0) as usize;
        let bar = format!("{}{}", "█".repeat(filled.min(14)), " ".repeat(14usize.saturating_sub(filled)));
        let label = if c.is_baseline { c.tool.clone() } else { format!("{} {}", c.tool, short_path(&c.target)) };
        Row::new(vec![
            format!("{:.1}", share * 100.0),
            bar,
            fmt_int(c.contribution),
            format!("${:.2}", c.amplified_cost),
            c.entries.to_string(),
            truncate(&label, 40),
        ])
    }, &format!(" Cache-read attribution ({} tok decomposed · Enter for detail) ", fmt_int(read)));
}
