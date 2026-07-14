//! Inline bar-chart helpers (horizontal proportion bars).

use crate::analysis::fmt_int;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// A labelled horizontal bar: `label ████ value`, scaled so `max` fills `width` columns.
pub fn comp_line<'a>(label: &str, v: u64, max: u64, width: usize, color: Color) -> Line<'a> {
    let filled = ((v as f64 / max.max(1) as f64) * width as f64) as usize;
    let bar = "█".repeat(filled.min(width));
    Line::from(vec![
        Span::styled(format!("{label:<12}"), Style::default().fg(Color::Gray)),
        Span::styled(bar, Style::default().fg(color)),
        Span::raw(format!(" {}", fmt_int(v))),
    ])
}
