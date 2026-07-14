//! A sortable, mouse-aware data table. Views supply headers, fixed column widths (0 = the
//! flex column) and rows; this component draws the frame, sort arrows and selection, and
//! records column x-ranges so header clicks map to a sort column.

use crate::tui::app::{App, disp_map, tab_cols};
use crate::tui::theme;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Row, Table, TableState};

/// Keep the selected row visible by advancing a scroll offset minimally. Returns the new
/// first-visible absolute row index. Used both to window big tables and to map clicks.
pub fn window_offset(prev: usize, sel: usize, len: usize, visible: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    let mut off = prev;
    if sel < off {
        off = sel;
    } else if sel >= off + visible {
        off = sel + 1 - visible;
    }
    off.min(len - visible)
}

/// Rows that fit in a bordered table area (minus borders + header row).
pub fn visible_rows(area: Rect) -> usize {
    area.height.saturating_sub(3) as usize
}

/// Record column x-ranges for header-click sorting. A width of 0 is the flex column that
/// fills the remaining space.
pub fn capture_headers(app: &mut App, area: Rect, widths: &[u16]) {
    app.header_hits.clear();
    let inner_x = area.x + 1;
    let inner_w = area.width.saturating_sub(2);
    let fixed: u16 = widths.iter().filter(|w| **w > 0).sum::<u16>() + widths.len().saturating_sub(1) as u16;
    let flex = inner_w.saturating_sub(fixed);
    let mut x = inner_x;
    for (i, w) in widths.iter().enumerate() {
        let cw = if *w == 0 { flex } else { *w };
        app.header_hits.push((x, x + cw, i));
        x += cw + 1;
    }
}

/// Windowed table render. `build(i)` produces row `i`; only the visible window is built,
/// so huge tables (e.g. a 13k-turn timeline) stay O(viewport) per frame. The computed
/// scroll offset is stored on `app` so mouse row-clicks map to the correct absolute row.
#[allow(clippy::too_many_arguments)]
pub fn render<F: Fn(usize) -> Row<'static>>(
    f: &mut Frame,
    app: &mut App,
    area: Rect,
    headers: &[&str],
    widths: &[u16],
    tab: usize,
    len: usize,
    build: F,
    title: &str,
) {
    let active_name = tab_cols(tab).get(app.sort_col[tab]).copied();
    let arrow = if app.sort_desc[tab] { "▼" } else { "▲" };
    let map = disp_map(tab);
    let header_cells: Vec<_> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let is_active = active_name.is_some() && map.get(i).copied().flatten() == active_name;
            let label = if is_active { format!("{h}{arrow}") } else { h.to_string() };
            ratatui::widgets::Cell::from(label).style(if is_active { theme::active_header() } else { theme::header() })
        })
        .collect();
    let constraints: Vec<Constraint> =
        widths.iter().map(|w| if *w == 0 { Constraint::Min(10) } else { Constraint::Length(*w) }).collect();
    let visible = visible_rows(area);
    let sel = app.sel[tab].min(len.saturating_sub(1));
    let off = window_offset(app.row_offset[tab], sel, len, visible);
    app.row_offset[tab] = off;
    let end = (off + visible + 1).min(len);
    let rows: Vec<Row> = (off..end).map(&build).collect();
    // Rows are pre-windowed, so the state offset is 0 and selection is relative.
    let mut state = TableState::default().with_offset(0).with_selected(Some(sel - off));
    let table = Table::new(rows, constraints)
        .header(Row::new(header_cells))
        .column_spacing(1)
        .block(Block::default().borders(Borders::ALL).title(title.to_string()))
        .row_highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("");
    f.render_stateful_widget(table, area, &mut state);
}
