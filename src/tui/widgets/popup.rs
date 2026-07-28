//! Modal popups: the transcript-item expander and the per-turn detail view, plus the
//! background-dimming helper that makes them stand out.

use crate::analysis::{SessionReport, TItem, TKind, est_tokens, fmt_int, short_model, short_path};
use crate::query;
use crate::tui::app::App;
use crate::tui::format::{centered, fmt_delta};
use crate::tui::theme;
use crate::tui::widgets::bubble::tok_span;
use ratatui::prelude::*;
use ratatui::widgets::{Clear, Paragraph, Wrap};

/// Recede everything currently in the buffer so a popup stands out.
pub fn dim_area(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_style(Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM));
        }
    }
}

/// Full text of one transcript item (message / tool input / tool result).
pub fn draw_transcript_popup(f: &mut Frame, app: &App, idx: usize) {
    let Some(sr) = app.focus_report() else { return };
    let Some(it) = sr.transcript.get(idx) else { return };
    let area = centered(f.area(), 82, 82);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(full_text(it))
            .wrap(Wrap { trim: false })
            .scroll((app.popup_scroll, 0))
            .block(theme::popup_block(&format!(" Item #{idx} "))),
        area,
    );
}

/// Rich detail for a single timeline turn: tokens, reasoning, response, tool calls and
/// the results that came back.
pub fn draw_turn_popup(f: &mut Frame, app: &App, turn: usize) {
    let Some(sr) = app.focus_report() else { return };
    let area = centered(f.area(), 84, 84);
    // `turn` is the session-wide sequence number; the title shows the per-thread one.
    let title = sr.timeline.iter().find(|t| t.turn == turn).map(|t| t.display_turn()).unwrap_or_else(|| turn.to_string());
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(turn_detail_lines(sr, turn))
            .wrap(Wrap { trim: false })
            .scroll((app.popup_scroll, 0))
            .block(theme::popup_block(&format!(" Turn {title} — detail "))),
        area,
    );
}

/// Detail popup for a selected table row (Tools/Sinks/Cache-attr/Sub-agents/Issues).
pub fn draw_detail_popup(f: &mut Frame, app: &App, tab: usize, idx: usize) {
    let (title, lines) = detail_lines(app, tab, idx);
    let area = centered(f.area(), 82, 78);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((app.popup_scroll, 0)).block(theme::popup_block(&title)),
        area,
    );
}

fn kvl(k: &str, v: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<16}"), Style::default().fg(Color::Gray)),
        Span::raw(v),
    ])
}

fn detail_lines(app: &App, tab: usize, idx: usize) -> (String, Vec<Line<'static>>) {
    let m = app.metrics();
    match tab {
        4 => {
            // Tools
            let mut list = m.tools.clone();
            let _ = query::sort_tools(&mut list, query::TOOL_COLS[app.sort_col[4]], app.sort_desc[4]);
            let Some(t) = list.get(idx) else { return ("Tool".into(), vec![]) };
            let mut out = vec![
                kvl("Tool", t.name.clone()),
                kvl("Calls", fmt_int(t.calls)),
                kvl("Result tokens", format!("~{} (est)", fmt_int(t.result_tokens_est))),
                kvl("Input chars", fmt_int(t.input_chars)),
                kvl("Errors", t.errors.to_string()),
            ];
            // If a session is focused, list this tool's calls (targets) from the transcript.
            if let Some(sr) = app.focus_report() {
                let calls: Vec<String> = sr.transcript.iter().filter_map(|it| match &it.kind {
                    TKind::Assistant { label, tools, .. } => {
                        let hits: Vec<String> = tools.iter().filter(|x| x.name == t.name).map(|x| format!("  turn {label}: {}", short_path(&x.target))).collect();
                        if hits.is_empty() { None } else { Some(hits.join("\n")) }
                    }
                    _ => None,
                }).collect();
                if !calls.is_empty() {
                    out.push(Line::from(""));
                    out.push(section(&format!("CALLS ({})", calls.len())));
                    push_block(&mut out, &calls.join("\n"), Color::Gray);
                }
            }
            (format!(" Tool — {} ", t.name), out)
        }
        5 | 6 => {
            // Sinks / Cache-attr (both are CacheContrib)
            let mut list = if tab == 5 {
                let mut v = query::sinks_only(app.cache_attr());
                let _ = query::sort_sinks(&mut v, query::SINK_COLS[app.sort_col[5]], app.sort_desc[5]);
                v
            } else {
                let mut v = app.cache_attr().to_vec();
                let _ = query::sort_cacheattr(&mut v, query::CACHEATTR_COLS[app.sort_col[6]], app.sort_desc[6]);
                v
            };
            let Some(c) = list.get_mut(idx).map(|c| c.clone()) else { return ("Sink".into(), vec![]) };
            let mut out = vec![
                kvl("Source", format!("{} {}", c.tool, if c.is_baseline { String::new() } else { short_path(&c.target) })),
                kvl("Share of read", format!("{:.1}%", c.share * 100.0)),
                kvl("Replayed tok", format!("~{} (est)", fmt_int(c.contribution))),
                kvl("One-time size", format!("~{} tok", fmt_int(c.tokens))),
                kvl("Entries", c.entries.to_string()),
                kvl("Resident turns", c.residency_turns.to_string()),
                kvl("Amplified cost", format!("${:.2}", c.amplified_cost)),
            ];
            if c.is_baseline {
                out.push(Line::from(""));
                out.push(Line::from(Span::styled("System prompt + prior conversation always resident (not a fixable sink).", Style::default().fg(theme::MUTED))));
            } else if let Some(sr) = app.focus_report() {
                // Where this content entered context.
                let where_: Vec<String> = sr.transcript.iter().filter_map(|it| match &it.kind {
                    TKind::Tool { tool, target, tokens, .. } if *tool == c.tool && (short_path(target) == short_path(&c.target) || *target == c.target) =>
                        Some(format!("  ~{} tok  {}", fmt_int(*tokens), short_path(target))),
                    _ => None,
                }).collect();
                if !where_.is_empty() {
                    out.push(Line::from(""));
                    out.push(section(&format!("ENTERED CONTEXT ({}×)", where_.len())));
                    push_block(&mut out, &where_.join("\n"), Color::Gray);
                }
            }
            (" Sink / cache-read source ".into(), out)
        }
        7 => {
            // One sub-agent thread. Reconstructed from its messages, so an agent that never
            // returned still has a detail page; the harness's own tool statistics are joined
            // in only when a result record exists.
            let mut list = crate::tui::views::subagents::threads(app);
            let _ = query::sort_agent_threads(&mut list, query::AGENT_COLS[app.sort_col[7]], app.sort_desc[7]);
            let Some((sr, t)) = list.get(idx).copied() else { return ("Sub-agent".into(), vec![]) };
            let mut out = vec![
                kvl("Agent", t.agent.label()),
                kvl("Agent id", t.agent.id.clone()),
                kvl("Type", t.agent.agent_type.clone()),
                kvl("Depth", format!("{} (1 = spawned by the main thread)", t.agent.depth)),
                kvl("Model", short_model(&t.model)),
                kvl("Outcome", t.outcome.label().to_string()),
            ];
            if t.is_spinning(&sr.transcript) {
                out.push(Line::from(Span::styled(
                    "Every tool call this agent made spawned another agent — it did no work itself.",
                    Style::default().fg(theme::BAD),
                )));
            }
            if !t.agent.description.is_empty() {
                out.push(Line::from(""));
                out.push(section("TASK"));
                push_block(&mut out, &t.agent.description, Color::White);
            }
            out.extend([
                Line::from(""),
                section("WORK"),
                kvl("Turns", t.turns.to_string()),
                kvl("Tool calls", t.tool_calls.to_string()),
                kvl("Tokens", fmt_int(t.usage.total())),
                kvl("Cost", format!("${:.2}", t.cost_usd)),
                kvl("Duration", crate::analysis::fmt_dur_ms(t.duration_ms())),
                kvl("Transcript items", format!("{}–{}  (Transcript tab)", t.first_item, t.last_item)),
                Line::from(""),
                section("USAGE"),
                kvl("cache read", fmt_int(t.usage.cache_read_input_tokens)),
                kvl("cache write", fmt_int(t.usage.cache_creation_input_tokens)),
                kvl("output", fmt_int(t.usage.output_tokens)),
            ]);
            // Only present for an agent that returned a result to its parent.
            if let Some(s) = sr.metrics.subagents.iter().find(|s| s.agent_id == t.agent.id) {
                out.push(Line::from(""));
                out.push(section("HARNESS-REPORTED TOOL STATS"));
                out.push(kvl("Reads", s.read_count.to_string()));
                out.push(kvl("Searches", s.search_count.to_string()));
                out.push(kvl("Bash", s.bash_count.to_string()));
                out.push(kvl("Edits", s.edit_count.to_string()));
                out.push(kvl("Lines", format!("+{} / -{}", s.lines_added, s.lines_removed)));
                out.push(kvl("Reported tokens", format!("{} (a different measure — see SKILL.md)", fmt_int(s.total_tokens))));
            } else {
                out.push(Line::from(""));
                out.push(Line::from(Span::styled(
                    "No result record: this agent never returned to its parent, so the harness's",
                    Style::default().fg(theme::MUTED),
                )));
                out.push(Line::from(Span::styled(
                    "own tool statistics are unknown for it.",
                    Style::default().fg(theme::MUTED),
                )));
            }
            (format!(" Sub-agent — {} ", t.agent.label()), out)
        }
        8 => {
            // Issues
            let Some(fd) = m.findings.get(idx) else { return ("Issue".into(), vec![]) };
            let color = theme::severity_color(fd.severity);
            let mut out = vec![
                Line::from(vec![
                    Span::styled(format!("[{}] ", fd.severity.label()), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    Span::styled(fd.kind.clone(), Style::default().add_modifier(Modifier::BOLD)),
                ]),
                Line::from(""),
            ];
            push_block(&mut out, &fd.detail, Color::White);
            if fd.wasted_tokens_est > 0 {
                out.push(Line::from(""));
                out.push(kvl("Est. wasted", format!("~{} tokens", fmt_int(fd.wasted_tokens_est))));
            }
            (format!(" Issue — {} ", fd.kind), out)
        }
        _ => ("Detail".into(), vec![]),
    }
}

fn turn_detail_lines(sr: &SessionReport, turn: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    let tp = sr.timeline.iter().find(|t| t.turn == turn);

    if let Some(t) = tp {
        out.push(Line::from(Span::styled(
            format!("Turn {} · {}", t.display_turn(), t.model),
            Style::default()
                .fg(if t.is_sidechain { theme::SIDECHAIN } else { Color::LightGreen })
                .add_modifier(Modifier::BOLD),
        )));
        if let Some(a) = &t.agent {
            out.push(Line::from(Span::styled(
                format!("sub-agent {} (depth {}) — not the main thread", a.label(), a.depth),
                Style::default().fg(theme::SIDECHAIN),
            )));
        }
        let reason = est_tokens(assistant_thinking_len(sr, turn));
        let mut stats = Vec::new();
        stats.extend(tok_span("read", t.usage.cache_read_input_tokens, Color::Cyan));
        stats.extend(tok_span("write", t.usage.cache_creation_input_tokens, Color::Magenta));
        if reason > 0 {
            stats.extend(tok_span("reason~", reason, Color::Blue));
        }
        stats.extend(tok_span("out", t.usage.output_tokens, Color::Green));
        stats.extend(tok_span("input", t.usage.input_tokens, Color::Yellow));
        out.push(Line::from(stats));
        out.push(Line::from(vec![
            Span::styled("context ", Style::default().fg(Color::Gray)),
            Span::raw(fmt_int(t.context_size)),
            Span::styled("   Δctx ", Style::default().fg(Color::Gray)),
            Span::styled(fmt_delta(t.delta), Style::default().fg(if t.is_spike { theme::BAD } else { Color::Gray })),
            Span::styled("   cost ", Style::default().fg(Color::Gray)),
            Span::styled(format!("${:.3}", t.cost), Style::default().fg(theme::GOOD)),
        ]));
        let mut flags = Vec::new();
        if t.is_spike {
            flags.push("▲ context spike");
        }
        if t.compaction_after {
            flags.push("✂ compaction after this turn");
        }
        if t.is_error {
            flags.push("! API error");
        }
        if !flags.is_empty() {
            out.push(Line::from(Span::styled(flags.join("   "), Style::default().fg(theme::WARN))));
        }
        if !t.cause.is_empty() {
            out.push(Line::from(Span::styled(format!("grew context via: {}", t.cause), Style::default().fg(theme::MUTED))));
        }
    } else {
        out.push(Line::from(Span::styled(format!("Turn {turn}"), Style::default().add_modifier(Modifier::BOLD))));
    }

    if let Some(pos) = sr.transcript.iter().position(|it| matches!(&it.kind, TKind::Assistant { turn: t, .. } if *t == turn)) {
        if let TKind::Assistant { thinking, text, tools, .. } = &sr.transcript[pos].kind {
            if !thinking.is_empty() {
                out.push(Line::from(""));
                out.push(section("💭 REASONING"));
                push_block(&mut out, thinking, Color::Gray);
            }
            if !text.is_empty() {
                out.push(Line::from(""));
                out.push(section("RESPONSE"));
                push_block(&mut out, text, Color::White);
            }
            if !tools.is_empty() {
                out.push(Line::from(""));
                out.push(section("TOOL CALLS"));
                for t in tools {
                    out.push(Line::from(Span::styled(
                        format!("  ⚙ {} {}", t.name, short_path(&t.target)),
                        Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
                    )));
                    push_block(&mut out, &clip_chars(&t.input_full, 800), theme::MUTED);
                }
            }
        }
        // Stay on this turn's own thread: a turn that spawned a sub-agent has that agent's
        // whole conversation spliced in before its own tool result.
        let owner = sr.transcript[pos].agent.as_ref().map(|a| a.id.as_str());
        let results: Vec<&TItem> = sr.transcript[pos + 1..]
            .iter()
            .filter(|it| it.agent.as_ref().map(|a| a.id.as_str()) == owner)
            .take_while(|it| !matches!(it.kind, TKind::Assistant { .. }))
            .filter(|it| matches!(it.kind, TKind::Tool { .. }))
            .collect();
        if !results.is_empty() {
            out.push(Line::from(""));
            out.push(section("RESULTS THIS TURN"));
            for r in results {
                if let TKind::Tool { tool, target, tokens, content, is_error } = &r.kind {
                    out.push(Line::from(vec![
                        Span::styled(format!("  ⚙→ {tool} "), Style::default().fg(theme::ACCENT)),
                        Span::raw(short_path(target)),
                        Span::styled(
                            format!("  ~{} tok", fmt_int(*tokens)),
                            Style::default().fg(if *tokens > 8_000 { theme::WARN } else { theme::MUTED }),
                        ),
                        Span::styled(if *is_error { "  ERROR" } else { "" }, Style::default().fg(theme::BAD)),
                    ]));
                    push_block(&mut out, &clip_chars(content, 1200), theme::MUTED);
                }
            }
        }
    }
    out
}

fn assistant_thinking_len(sr: &SessionReport, turn: usize) -> usize {
    sr.transcript
        .iter()
        .find_map(|it| match &it.kind {
            TKind::Assistant { turn: t, thinking, .. } if *t == turn => Some(thinking.len()),
            _ => None,
        })
        .unwrap_or(0)
}

fn section<'a>(t: &str) -> Line<'a> {
    Line::from(Span::styled(t.to_string(), Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)))
}

fn push_block(out: &mut Vec<Line<'static>>, text: &str, color: Color) {
    for l in text.lines() {
        out.push(Line::from(Span::styled(format!("  {l}"), Style::default().fg(color))));
    }
}

fn clip_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}\n… (truncated, open the transcript item to read all)")
    }
}

fn full_text(it: &TItem) -> String {
    let who = match &it.agent {
        Some(a) => format!("sub-agent {} · depth {}\n\n", a.label(), a.depth),
        None => String::new(),
    };
    let body = match &it.kind {
        TKind::User { text, .. } => {
            let tag = if it.agent.is_some() { "TASK FROM PARENT" } else { "USER" };
            format!("{tag}\n\n{text}")
        }
        TKind::Assistant { model, thinking, text, tools, .. } => {
            let mut s = format!("ASSISTANT ({model})\n");
            if !thinking.is_empty() {
                s.push_str(&format!("\n[thinking]\n{thinking}\n"));
            }
            if !text.is_empty() {
                s.push_str(&format!("\n{text}\n"));
            }
            for t in tools {
                s.push_str(&format!("\n[tool: {} → {}]\n{}\n", t.name, t.target, t.input_full));
            }
            s
        }
        TKind::Tool { tool, target, content, .. } => format!("TOOL RESULT · {tool} {target}\n\n{content}"),
        TKind::Compact { pre, post, trigger } => format!("COMPACTION ({trigger})\n{pre} → {post} tokens"),
        TKind::Event { subtype, detail, content, is_terminal } => {
            let mark = if *is_terminal { "RUN ENDED" } else { "HARNESS EVENT" };
            format!("{mark} · {subtype}\n\n{detail}\n\n{content}")
        }
    };
    format!("{who}{body}")
}
