//! Overview page: headline stats, token composition bars and cache-read attribution.

use crate::analysis::{Outcome, RATE_WINDOW_HOURS, TurnPoint, fmt_int, rate_report, short_path};
use crate::tui::app::App;
use crate::tui::theme;
use crate::tui::format::{kv, kv_bold, kv_val, section_line, truncate};
use crate::tui::widgets::bars::comp_line;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

fn per_h(v: f64) -> String {
    if v >= 1e6 { format!("{:.2}M", v / 1e6) } else if v >= 1e3 { format!("{:.0}k", v / 1e3) } else { format!("{v:.0}") }
}

fn human(ms: i64) -> String {
    let mut s = ms / 1000;
    if s <= 0 { return "0s".into(); }
    let h = s / 3600; s %= 3600; let m = s / 60; s %= 60;
    if h > 0 { format!("{h}h {m}m") } else if m > 0 { format!("{m}m") } else { format!("{s}s") }
}

fn harness(app: &App) -> String {
    match app.focus {
        Some(i) => {
            let e = &app.a.sessions[i].entrypoint;
            if e.is_empty() { "unknown".into() } else { e.clone() }
        }
        None => {
            let mut by: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
            for s in &app.a.sessions {
                *by.entry(if s.entrypoint.is_empty() { "unknown" } else { s.entrypoint.as_str() }).or_default() += 1;
            }
            by.iter().map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join(", ")
        }
    }
}

pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let m = app.metrics();
    let turns: Vec<&TurnPoint> = match app.focus {
        Some(i) => app.a.sessions[i].timeline.iter().collect(),
        None => app.a.all_turns(),
    };
    let rr = rate_report(&turns, RATE_WINDOW_HOURS);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let mut lines = vec![
        kv("Provider", &app.info.provider_name),
        kv("Harness (entry)", &harness(app)),
        kv("Wall-clock span", &m.duration_human()),
        // Same honesty as the CLI: a pause under the 15-minute rate-window threshold must
        // not let a session read as continuous.
        kv("Active time", &format!("{}  ({} paused, max {})", human(rr.active_ms), human(rr.idle_ms), human(rr.longest_gap_ms))),
        kv("Peak turns/min", &format!("{}  (max concurrent subagents {})", fmt_int(rr.peak_turns_per_min), m.max_concurrent_subagents)),
        kv("Assistant turns", &fmt_int(m.assistant_turns)),
        kv("User prompts", &if m.subagent_prompts > 0 {
            format!("{}  (+{} sub-agent tasks)", fmt_int(m.user_prompts), fmt_int(m.subagent_prompts))
        } else {
            fmt_int(m.user_prompts)
        }),
        kv("Tool calls", &fmt_int(m.tool_calls)),
        kv("Context peak", &fmt_int(m.context_peak)),
        Line::from(""),
        section_line("TOKENS (limit-drivers first)"),
        kv_val("Cache write", &format!("{}  ({}/h)", fmt_int(m.usage.cache_creation_input_tokens), per_h(rr.cache_write_per_h)), Color::Magenta),
        kv_val("Fresh (in+wr+out)", &format!("{}  ({}/h)", fmt_int(m.usage.fresh()), per_h(rr.fresh_per_h)), Color::Yellow),
        kv("Output", &fmt_int(m.usage.output_tokens)),
        kv("Cache read (cheap)", &fmt_int(m.usage.cache_read_input_tokens)),
        kv_bold("TOTAL processed", &fmt_int(m.usage.total())),
        Line::from(""),
        section_line("COST & EFFICIENCY"),
        kv_val("Est. cost", &format!("${:.2}  (${:.0}/h)", m.cost_usd, rr.cost_per_h), Color::Green),
        kv("Cache hit rate", &format!("{:.1}%", m.cache_hit_rate() * 100.0)),
        kv("Cache churn", &{
            let c = m.cache_churn();
            if c.is_finite() { format!("{c:.2}x") } else { "∞".into() }
        }),
        kv("Peak 5h fresh", &fmt_int(rr.peak_window_fresh)),
    ];
    // How the work ended, and what it could reach — the two questions asked first when a
    // session goes wrong, and the ones the CLI already answers.
    let runs: Vec<&crate::analysis::RunSegment> = match app.focus {
        Some(i) => app.a.sessions[i].runs.iter().collect(),
        None => app.a.sessions.iter().flat_map(|s| s.runs.iter()).collect(),
    };
    if !runs.is_empty() {
        lines.push(Line::from(""));
        lines.push(section_line("RUNS  (turn limits apply per run)"));
        for r in runs.iter().take(6) {
            let color = match r.outcome {
                Outcome::LimitHit => theme::BAD,
                Outcome::Truncated | Outcome::Errored => theme::WARN,
                Outcome::Completed => theme::GOOD,
            };
            lines.push(kv_val(
                &format!("Run {}", r.index),
                &format!("{} turns · ${:.2} · {}", r.turns_main, r.cost_usd, r.outcome.label()),
                color,
            ));
            if !r.outcome_detail.is_empty() {
                lines.push(kv("", &format!("  {}", truncate(&r.outcome_detail, 52))));
            }
        }
    }
    if !m.tools_unavailable.is_empty() {
        lines.push(Line::from(""));
        lines.push(section_line("TOOLS REQUESTED BUT UNAVAILABLE"));
        for (name, n) in m.tools_unavailable.iter().take(4) {
            lines.push(kv_val(name, &format!("searched {n}×, never provided"), theme::BAD));
        }
    }
    if m.has_sidechain_detail {
        lines.push(Line::from(""));
        lines.push(section_line("MAIN vs SUB-AGENT"));
        lines.push(kv("Main-thread turns", &format!("{} ({} tok)", fmt_int(m.turns_main), fmt_int(m.usage_main.total()))));
        lines.push(kv("Sub-agent turns", &format!("{} ({} tok)", fmt_int(m.turns_sidechain), fmt_int(m.usage_sidechain.total()))));
        // "1" alone reads as "delegation was minimal" beside a deep chain.
        let threads: Vec<&crate::analysis::AgentThread> = match app.focus {
            Some(i) => app.a.sessions[i].threads.iter().collect(),
            None => app.a.sessions.iter().flat_map(|s| s.threads.iter()).collect(),
        };
        if !threads.is_empty() {
            let depth = threads.iter().map(|t| t.agent.depth).max().unwrap_or(0);
            lines.push(kv_val(
                "Sub-agents",
                &format!("{} finished / {} total · max depth {}", m.subagents.len(), threads.len(), depth),
                if depth >= 4 { theme::BAD } else { Color::Gray },
            ));
        }
    } else if !m.subagents.is_empty() {
        lines.push(Line::from(""));
        lines.push(section_line("SUB-AGENTS"));
        lines.push(kv("Count", &m.subagents.len().to_string()));
        lines.push(kv("Tokens (reported)", &fmt_int(m.subagent_tokens())));
    }
    if !m.compactions.is_empty() {
        let price = crate::pricing::price_for(&m.dominant_model());
        let pre: u64 = m.compactions.iter().map(|c| c.pre_tokens).sum();
        let cost: f64 = m.compactions.iter()
            .map(|c| (c.pre_tokens as f64 * price.cache_read + c.post_tokens as f64 * price.output) / 1_000_000.0)
            .sum();
        lines.push(kv("Compactions", &m.compactions.len().to_string()));
        lines.push(kv("  summarized", &format!("{} tok", fmt_int(pre))));
        lines.push(kv_val("  est. overhead", &format!("${:.2}  (not in totals)", cost), Color::Yellow));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Summary ")).scroll((app.scroll, 0)),
        cols[0],
    );

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)])
        .split(cols[1]);

    let u = &m.usage;
    let mx = u.total().max(1);
    let bw = right[0].width.saturating_sub(24) as usize;
    let comp = vec![
        comp_line("input", u.input_tokens, mx, bw, Color::Blue),
        comp_line("cache write", u.cache_creation_input_tokens, mx, bw, Color::Magenta),
        comp_line("cache read", u.cache_read_input_tokens, mx, bw, Color::Cyan),
        comp_line("output", u.output_tokens, mx, bw, Color::Green),
    ];
    f.render_widget(
        Paragraph::new(comp).block(Block::default().borders(Borders::ALL).title(" Token composition ")),
        right[0],
    );

    // Cache-read attribution (top sources) as share bars.
    let attr = app.cache_attr();
    let read = m.usage.cache_read_input_tokens.max(1);
    let aw = right[1].width.saturating_sub(30) as usize;
    let mut al: Vec<Line> = Vec::new();
    for c in attr.iter().take(right[1].height.saturating_sub(2) as usize) {
        let share = c.contribution as f64 / read as f64;
        let filled = (share * aw as f64) as usize;
        let bar = format!("{}{}", "█".repeat(filled.min(aw)), " ".repeat(aw.saturating_sub(filled)));
        let label = if c.is_baseline { c.tool.clone() } else { format!("{} {}", c.tool, short_path(&c.target)) };
        al.push(Line::from(vec![
            Span::styled(format!("{:>5.0}% ", share * 100.0), Style::default().fg(Color::Yellow)),
            Span::styled(bar, Style::default().fg(if c.is_baseline { Color::DarkGray } else { Color::Cyan })),
            Span::raw(" "),
            Span::raw(truncate(&label, 26)),
        ]));
    }
    f.render_widget(
        Paragraph::new(al).block(Block::default().borders(Borders::ALL).title(" Where cache-read came from ")),
        right[1],
    );
}
