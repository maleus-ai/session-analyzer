//! Timeline page: a context-growth chart above a sortable per-turn table.

use crate::analysis::fmt_int;
use crate::query;
use crate::tui::app::App;
use crate::tui::format::{fmt_delta, truncate};
use crate::tui::widgets::table;
use ratatui::prelude::*;
use ratatui::widgets::{Axis, Block, Borders, Cell, Chart, Dataset, GraphType, Paragraph, Row};

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(fi) = app.focus else {
        f.render_widget(
            Paragraph::new("Select a session first (Sessions tab → Enter) to see its context-growth timeline.")
                .block(Block::default().borders(Borders::ALL).title(" Timeline ")),
            area,
        );
        return;
    };
    let sr = &app.a.sessions[fi];
    let rows_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    // Sort the per-turn list once (used for both the table and the chart's selection marker).
    let mut tl: Vec<&crate::analysis::TurnPoint> = sr.timeline.iter().collect();
    let col = query::TIMELINE_COLS[app.sort_col[3]];
    let _ = query::sort_timeline(&mut tl, col, app.sort_desc[3]);

    // Growth chart. The main thread and every sub-agent have **separate context windows**,
    // so plotting them as one series manufactures cliffs: an agent starting and finishing
    // reads as a bump followed by a drop, which looks exactly like a compaction that never
    // happened. Split the series so the shape of each thread is its own.
    let budget = (rows_area[0].width as usize * 2).max(2);
    let stride = (sr.timeline.len() / budget).max(1);
    let main: Vec<(f64, f64)> = sr
        .timeline
        .iter()
        .filter(|t| !t.is_sidechain)
        .step_by(stride.min(4))
        .map(|t| (t.turn as f64, t.context_size as f64))
        .collect();
    let sub: Vec<(f64, f64)> = sr
        .timeline
        .iter()
        .filter(|t| t.is_sidechain)
        .step_by(stride)
        .map(|t| (t.turn as f64, t.context_size as f64))
        .collect();
    let spikes: Vec<(f64, f64)> = sr.timeline.iter().filter(|t| t.is_spike).map(|t| (t.turn as f64, t.context_size as f64)).collect();
    // Cursor: mark the currently-selected turn (links the table to the chart).
    let sel_turn = tl.get(app.sel[3].min(tl.len().saturating_sub(1)));
    let cursor: Vec<(f64, f64)> = sel_turn.map(|t| vec![(t.turn as f64, t.context_size as f64)]).unwrap_or_default();
    let peak = sr.metrics.context_peak.max(1) as f64;
    let n = sr.timeline.len().max(1) as f64;
    let sel_label = sel_turn.map(|t| format!("turn {} · {} tok", t.display_turn(), fmt_int(t.context_size))).unwrap_or_default();
    // Sub-agent points are scattered, not joined: they belong to many different windows and
    // a line between them would imply a continuity that does not exist.
    let datasets = vec![
        Dataset::default().name("main thread").marker(symbols::Marker::Braille).graph_type(GraphType::Line).style(Style::default().fg(Color::Cyan)).data(&main),
        Dataset::default().name("sub-agents").marker(symbols::Marker::Dot).graph_type(GraphType::Scatter).style(Style::default().fg(crate::tui::theme::SIDECHAIN)).data(&sub),
        Dataset::default().name("spike").marker(symbols::Marker::Dot).graph_type(GraphType::Scatter).style(Style::default().fg(Color::Red)).data(&spikes),
        Dataset::default().name("cursor").marker(symbols::Marker::Block).graph_type(GraphType::Scatter).style(Style::default().fg(Color::White)).data(&cursor),
    ];
    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " Context growth — peak {} tok · {} main turns, {} sub-agent turns (separate windows) ",
                    fmt_int(sr.metrics.context_peak),
                    sr.timeline.iter().filter(|t| !t.is_sidechain).count(),
                    sr.timeline.iter().filter(|t| t.is_sidechain).count()
                ))
                .title(Line::from(format!(" ◈ {sel_label} ")).right_aligned()),
        )
        .x_axis(Axis::default().bounds([1.0, n]).labels(vec!["1".into(), format!("{}", sr.timeline.len())]))
        .y_axis(Axis::default().bounds([0.0, peak]).labels(vec!["0".into(), fmt_int(sr.metrics.context_peak)]));
    f.render_widget(chart, rows_area[0]);

    // Per-turn table (this view owns a sub-region, so refresh the hit-test rows).
    app.row0 = rows_area[1].y + 2;
    app.rows_visible = rows_area[1].height.saturating_sub(3);
    let headers = ["TURN", "THREAD", "CONTEXT", "ΔCTX", "WRITE", "OUT", "COST", "F", "CAUSE"];
    // TURN is wide enough for a sub-agent label (`Explore#a221ec ▸ 3`), not just a number.
    let widths = [6u16, 22, 11, 10, 10, 8, 8, 3, 0];
    table::capture_headers(app, rows_area[1], &widths);
    // Windowed: only the visible turns are built each frame (13k-turn sessions stay fluid).
    let n = tl.len();
    table::render(f, app, rows_area[1], &headers, &widths, 3, n, |i| {
        let t = tl[i];
        let dcolor = if t.is_spike { Color::Red } else if t.delta < 0 { Color::Green } else { Color::Gray };
        Row::new(vec![
            // TURN is the per-thread number; THREAD says which thread it counts within.
            Cell::from(t.label.clone()),
            Cell::from(Span::styled(
                truncate(&t.agent.as_ref().map(|a| a.label()).unwrap_or_else(|| "main".into()), 21),
                Style::default().fg(if t.is_sidechain { crate::tui::theme::SIDECHAIN } else { Color::Gray }),
            )),
            Cell::from(fmt_int(t.context_size)),
            Cell::from(Span::styled(fmt_delta(t.delta), Style::default().fg(dcolor))),
            Cell::from(fmt_int(t.usage.cache_creation_input_tokens)),
            Cell::from(fmt_int(t.usage.output_tokens)),
            Cell::from(format!("${:.3}", t.cost)),
            Cell::from({
                let mut fl = String::new();
                if t.is_spike { fl.push('▲'); }
                if t.compaction_after { fl.push('✂'); }
                fl
            }),
            Cell::from(truncate(&t.cause, 40)),
        ])
    }, " Per-turn (▲ spike · ✂ compaction · Enter for detail) ");
}
