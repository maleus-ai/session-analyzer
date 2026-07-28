//! Sub-agents page: every sub-agent conversation in scope.
//!
//! Driven by the reconstructed thread roster, not by the harness's result records — an
//! agent that never returned has no result record, so a view built on those alone showed
//! "1" for a session containing a 54-deep runaway chain. Threads that *did* return are
//! joined to their result record for the harness's own tool statistics.

use crate::analysis::{AgentThread, Outcome, SessionReport, fmt_dur_ms, fmt_int, short_model};
use crate::tui::app::App;
use crate::tui::theme;
use crate::tui::widgets::table;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row};

/// Every sub-agent thread in scope, in first-appearance order — the order a delegation
/// chain actually happened in.
pub fn threads<'a>(app: &App<'a>) -> Vec<(&'a SessionReport, &'a AgentThread)> {
    match app.focus {
        Some(i) => {
            let sr = &app.a.sessions[i];
            sr.threads.iter().map(|t| (sr, t)).collect()
        }
        None => app.a.sessions.iter().flat_map(|s| s.threads.iter().map(move |t| (s, t))).collect(),
    }
}

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let mut list = threads(app);
    // `[`/`]` cycle AGENT_COLS; no sort column selected keeps first-appearance order.
    let _ = crate::query::sort_agent_threads(&mut list, crate::query::AGENT_COLS[app.sort_col[7]], app.sort_desc[7]);
    if list.is_empty() {
        f.render_widget(
            Paragraph::new("No sub-agent conversations in scope.")
                .block(Block::default().borders(Borders::ALL).title(" Sub-agents ")),
            area,
        );
        return;
    }
    let finished = list.iter().filter(|(_, t)| t.completed).count();
    let max_depth = list.iter().map(|(_, t)| t.agent.depth).max().unwrap_or(0);
    let headers = ["AGENT", "TYPE", "D", "MODEL", "TURNS", "TOKENS", "COST", "TOOLS", "DUR", "OUTCOME", "DESCRIPTION"];
    let widths = [10u16, 16, 4, 12, 6, 10, 8, 6, 8, 11, 0];
    table::capture_headers(app, area, &widths);
    let n = list.len();
    table::render(
        f,
        app,
        area,
        &headers,
        &widths,
        7,
        n,
        |i| {
            let (sr, t) = list[i];
            // Same criterion as the `Delegation loop` finding and `agents --spinning`: this
            // agent's every tool call spawned another agent, so it did no work of its own.
            let spinning = t.is_spinning(&sr.transcript);
            let (oc, ocol) = match t.outcome {
                Outcome::Completed => ("completed", theme::GOOD),
                Outcome::LimitHit => ("limit-hit", theme::BAD),
                Outcome::Errored => ("errored", theme::BAD),
                Outcome::Truncated => ("truncated", theme::WARN),
            };
            Row::new(vec![
                Cell::from(Span::styled(
                    format!("{}{}", if spinning { "⟳" } else { " " }, t.agent.id.chars().take(8).collect::<String>()),
                    Style::default().fg(theme::SIDECHAIN),
                )),
                Cell::from(t.agent.agent_type.clone()),
                // Depth is the tell for a runaway chain — colour it once it stops being normal.
                Cell::from(Span::styled(
                    t.agent.depth.to_string(),
                    Style::default().fg(if t.agent.depth >= 4 { theme::BAD } else { Color::Gray }),
                )),
                Cell::from(short_model(&t.model)),
                Cell::from(t.turns.to_string()),
                Cell::from(fmt_int(t.usage.total())),
                Cell::from(format!("${:.2}", t.cost_usd)),
                Cell::from(t.tool_calls.to_string()),
                Cell::from(fmt_dur_ms(t.duration_ms())),
                Cell::from(Span::styled(oc, Style::default().fg(ocol))),
                Cell::from(t.agent.description.clone()),
            ])
        },
        &format!(
            " Sub-agents — {} total, {} finished, max depth {} · ⟳ = only spawned another agent (Enter for detail) ",
            n, finished, max_depth
        ),
    );
}
