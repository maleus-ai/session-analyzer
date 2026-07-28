//! Rate page: throughput over wall-clock time and the peak rolling window — the figures
//! that actually predict subscription-limit exhaustion.

use crate::analysis::{RATE_WINDOW_HOURS, TurnPoint, fmt_int, rate_report};
use crate::tui::app::App;
use crate::tui::format::{kv, kv_bold, kv_val, section_line};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Sparkline};

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let turns: Vec<&TurnPoint> = match app.focus {
        Some(i) => app.a.sessions[i].timeline.iter().collect(),
        None => app.a.all_turns(),
    };
    let r = rate_report(&turns, RATE_WINDOW_HOURS);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(24), Constraint::Length(5), Constraint::Min(0)])
        .split(area);

    // Summary panel.
    let per_h = |v: f64| -> String {
        if v >= 1e6 { format!("{:.2}M", v / 1e6) } else if v >= 1e3 { format!("{:.0}k", v / 1e3) } else { format!("{v:.0}") }
    };
    let ah = (r.active_ms as f64 / 3.6e6).max(1.0 / 60.0);
    let lines = vec![
        section_line("THROUGHPUT & CONTINUITY"),
        kv("Span (first→last)", &human(r.span_ms)),
        // Same wording as `overview` and the CLI: a pause shorter than the 15-minute
        // rate-window threshold must still be visible.
        kv("Active time", &format!("{}  ({} paused, max {})", human(r.active_ms), human(r.idle_ms), human(r.longest_gap_ms))),
        kv("Turns", &fmt_int(r.turns)),
        kv_val("Cache-write /active-h", &per_h(r.total_cache_write as f64 / ah), Color::Magenta),
        kv_val("Fresh /active-h", &per_h(r.total_fresh as f64 / ah), Color::Yellow),
        kv("Cost /active-h", &format!("${:.0}", r.total_cost / ah)),
        Line::from(""),
        section_line(&format!("PEAK {}h WINDOW", r.window_hours as i64)),
        kv_bold("Continuous burst", &fmt_int(r.peak_burst_fresh)),
        kv("Naive rolling", &fmt_int(r.peak_window_fresh)),
        Line::from(""),
        section_line("INSTANTANEOUS BURST (concurrency)"),
        kv("Peak fresh/min", &fmt_int(r.peak_fresh_per_min)),
        kv("Peak turns/min", &fmt_int(r.peak_turns_per_min)),
        kv("Max concurrent subagents", &app.metrics().max_concurrent_subagents.to_string()),
        Line::from(""),
        section_line("PEAK RATE-LIMITER LOAD (busiest 60s)"),
        kv_val("RPM  (requests/min)", &fmt_int(r.peak_rpm), Color::Cyan),
        kv_val("ITPM (input tok/min)", &fmt_int(r.peak_itpm), Color::Magenta),
        kv_val("OTPM (output tok/min)", &fmt_int(r.peak_otpm), Color::Green),
    ];
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Rate ")),
        rows[0],
    );

    if r.buckets.is_empty() {
        f.render_widget(
            Paragraph::new("No timestamped turns to chart.").block(Block::default().borders(Borders::ALL)),
            rows[1],
        );
        return;
    }

    // Time range for the x-axis (shown as right-aligned title).
    let t0 = r.buckets.first().map(|b| clock(b.start_ms)).unwrap_or_default();
    let t1 = r.buckets.last().map(|b| clock(b.start_ms + r.bucket_ms)).unwrap_or_default();
    let span = format!(" {t0} → {t1} (UTC) ");

    // Fresh-tokens-per-bucket sparkline, with peak + 0 markers and the time range.
    let fresh: Vec<u64> = r.buckets.iter().map(|b| b.fresh).collect();
    let fpeak = fresh.iter().copied().max().unwrap_or(0);
    spark(f, rows[1], &fresh,
        format!(" Fresh/{} — peak {} ", human(r.bucket_ms), fmt_int(fpeak)), &span, Color::Yellow);

    // Cache-write-per-bucket sparkline (the limit driver).
    let cw: Vec<u64> = r.buckets.iter().map(|b| b.cache_write).collect();
    let cpeak = cw.iter().copied().max().unwrap_or(0);
    spark(f, rows[2], &cw,
        format!(" Cache-write/{} — peak {} (limit driver) ", human(r.bucket_ms), fmt_int(cpeak)), &span, Color::Magenta);
}

/// A sparkline with a peak-labelled title, a right-aligned time-range, and a `peak`/`0`
/// scale gutter on the left so values are readable.
fn spark(f: &mut Frame, area: Rect, data: &[u64], title: String, span: &str, color: Color) {
    let block = Block::default().borders(Borders::ALL).title(title).title(Line::from(span.to_string()).right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Left scale gutter: peak at top, 0 at bottom.
    let peak = data.iter().copied().max().unwrap_or(0);
    let gutter = (fmt_int(peak).len() as u16).max(1) + 1;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(gutter), Constraint::Min(0)])
        .split(inner);
    let scale = vec![Line::from(Span::styled(fmt_int(peak), Style::default().fg(Color::DarkGray)))]
        .into_iter()
        .chain((1..inner.height.saturating_sub(1)).map(|_| Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled("0", Style::default().fg(Color::DarkGray)))))
        .collect::<Vec<_>>();
    f.render_widget(ratatui::widgets::Paragraph::new(scale), cols[0]);
    f.render_widget(Sparkline::default().data(data).style(Style::default().fg(color)), cols[1]);
}

/// Epoch millis → `HH:MM` (UTC).
fn clock(ms: i64) -> String {
    let s = ms.div_euclid(1000).rem_euclid(86400);
    format!("{:02}:{:02}", s / 3600, (s % 3600) / 60)
}

fn human(ms: i64) -> String {
    let mut s = ms / 1000;
    if s <= 0 {
        return "0s".into();
    }
    let h = s / 3600;
    s %= 3600;
    let m = s / 60;
    s %= 60;
    if h > 0 { format!("{h}h {m}m") } else if m > 0 { format!("{m}m {s}s") } else { format!("{s}s") }
}
