//! Chat-bubble component: renders a transcript item as a bordered box — user messages
//! right-aligned, assistant/tool left-aligned — with a title in the top border, wrapped
//! body rows and per-message token stats.

use crate::analysis::{AgentRef, AgentThread, Outcome, TItem, TKind, est_tokens, fmt_int, short_path};
use crate::tui::format::{clip_spans, dw, truncate, wrap_str};
use crate::tui::theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// A labelled token-stat span pair, e.g. `read 21,297`.
pub fn tok_span(label: &str, v: u64, color: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("{label} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}  ", fmt_int(v)), Style::default().fg(Color::Gray)),
    ]
}

/// Target bubble width for a given available inner width.
fn bubble_width(w: usize) -> usize {
    (w * 62 / 100).clamp(40.min(w.saturating_sub(2)), w.saturating_sub(2))
}

/// Prefix span marking a bubble as a sub-agent's, e.g. `▎Explore#a221ec `.
fn agent_tag(agent: &AgentRef) -> Span<'static> {
    Span::styled(
        format!("▎{} ", agent.label()),
        Style::default().fg(theme::SIDECHAIN).add_modifier(Modifier::BOLD),
    )
}

/// A whole sub-agent conversation, collapsed to one row.
///
/// Inlining a delegated thread stops working once nesting is deep: indentation caps out and
/// a 54-level chain buries the main conversation under 169 bubbles. Standing in for it with
/// a summary — what it was asked, what it cost, how it ended — keeps the parent readable and
/// makes the thread something you *open* rather than scroll past.
pub fn agent_row(t: &AgentThread, sel: bool, w: usize, depth: usize) -> Vec<Line<'static>> {
    let ind = (depth * 3).min(w / 5);
    let inner = bubble_width(w.saturating_sub(ind)).saturating_sub(4);
    let bs = theme::border_style(sel, theme::SIDECHAIN);
    let (oc, ocol) = match t.outcome {
        Outcome::Completed => ("completed", theme::GOOD),
        Outcome::LimitHit => ("limit-hit", theme::BAD),
        Outcome::Errored => ("errored", theme::BAD),
        Outcome::Truncated => ("truncated", theme::WARN),
    };
    let title = vec![
        Span::styled("▶ ", Style::default().fg(theme::SIDECHAIN).add_modifier(Modifier::BOLD)),
        Span::styled(t.agent.label(), Style::default().fg(theme::SIDECHAIN).add_modifier(Modifier::BOLD)),
        Span::styled("  sub-agent — Enter to open", Style::default().fg(theme::MUTED)),
    ];
    let mut body = vec![vec![
        Span::styled(format!("{} turns  ", t.turns), Style::default().fg(Color::Gray)),
        Span::styled(format!("{} tok  ", fmt_int(t.usage.total())), Style::default().fg(Color::Cyan)),
        Span::styled(format!("${:.2}  ", t.cost_usd), Style::default().fg(Color::Green)),
        Span::styled(format!("{}  ", crate::analysis::fmt_dur_ms(t.duration_ms())), Style::default().fg(Color::Gray)),
        Span::styled(oc, Style::default().fg(ocol).add_modifier(Modifier::BOLD)),
    ]];
    if !t.agent.description.is_empty() {
        body.push(vec![Span::styled(truncate(&t.agent.description, inner), Style::default().fg(Color::White))]);
    }
    box_lines(ind, 0, depth, inner, &title, body, bs)
}

/// Build one chat bubble for a transcript item. `w` is the available inner width.
///
/// Sub-agent items are indented by nesting depth and drawn in the sidechain colour with an
/// agent tag, so a delegated conversation can never be mistaken for the main thread's.
/// `depth` is nesting **relative to the thread being viewed**: 0 for the open thread's own
/// messages, 1 for something it spawned. Inside a sub-agent its own conversation should read
/// flush left, not permanently indented by how deep it happens to sit in the whole session.
pub fn bubble(it: &TItem, sel: bool, w: usize, depth: usize) -> Vec<Line<'static>> {
    let ind = (depth * 3).min(w / 5);
    let w = w.saturating_sub(ind);
    match &it.kind {
        TKind::Compact { pre, post, trigger } => {
            let who = it.agent.as_ref().map(|a| format!("{} ", a.label())).unwrap_or_default();
            let label = format!(" ✂ {who}COMPACTED ({}) · {} → {} tokens ", trigger, fmt_int(*pre), fmt_int(*post));
            let side = w.saturating_sub(dw(&label)) / 2;
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("{}{}{}", " ".repeat(ind) + &"─".repeat(side), label, "─".repeat(side)),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ]
        }
        // Harness events are centred dividers, like compaction — they interrupt the
        // conversation rather than being part of it. Terminal ones (a run hitting its turn
        // limit) are the loudest thing on the page, because they explain everything after.
        TKind::Event { subtype, detail, is_terminal, .. } => {
            let who = it.agent.as_ref().map(|a| format!("{} ", a.label())).unwrap_or_default();
            let label = if *is_terminal {
                format!(" ⛔ {who}RUN ENDED · {subtype} — {detail} ")
            } else {
                format!(" • {who}{subtype} — {detail} ")
            };
            let label = truncate(&label, w.saturating_sub(2));
            let side = w.saturating_sub(dw(&label)) / 2;
            let style = Style::default()
                .fg(if *is_terminal { theme::BAD } else { theme::MUTED })
                .add_modifier(if *is_terminal { Modifier::BOLD } else { Modifier::empty() });
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("{}{}{}", " ".repeat(ind) + &"─".repeat(side), label, "─".repeat(side)),
                    style,
                )),
                Line::from(""),
            ]
        }
        TKind::User { text, is_prompt } => {
            let total = bubble_width(w);
            let inner = total.saturating_sub(4);
            let extra = w.saturating_sub(total); // right-align
            let bs = theme::border_style(sel, if it.agent.is_some() { theme::SIDECHAIN } else { theme::USER });
            // Inside a sidechain this is not the human speaking — it is the task the parent
            // handed to the sub-agent.
            let mut title = Vec::new();
            match &it.agent {
                Some(a) => {
                    title.push(agent_tag(a));
                    // This bubble *is* the spawn point; say who spawned it, so the link
                    // survives however far the indentation has been capped.
                    title.push(Span::styled(
                        match a.parent_short() {
                            Some(p) => format!("◀ spawned by #{p}"),
                            None => "◀ spawned by the main thread".into(),
                        },
                        Style::default().fg(theme::SIDECHAIN).add_modifier(Modifier::BOLD),
                    ));
                }
                None => title.push(Span::styled(
                    if *is_prompt { "👤 You" } else { "👤 You · meta" },
                    Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD),
                )),
            }
            let mut body = text_rows(text, inner, 6, Style::default().fg(Color::Gray));
            if body.is_empty() {
                body.push(vec![Span::styled("(empty)", Style::default().fg(theme::MUTED))]);
            }
            box_lines(ind, extra, depth, inner, &title, body, bs)
        }
        TKind::Assistant { label, model, usage, thinking, text, tools, is_error, .. } => {
            let total = bubble_width(w);
            let inner = total.saturating_sub(4);
            let bs = theme::border_style(sel, if it.agent.is_some() { theme::SIDECHAIN } else { theme::ASSISTANT });
            let mut title = Vec::new();
            if let Some(a) = &it.agent {
                title.push(agent_tag(a));
            }
            title.push(Span::styled(
                format!("🤖 turn {label}"),
                Style::default()
                    .fg(if it.agent.is_some() { theme::SIDECHAIN } else { Color::LightGreen })
                    .add_modifier(Modifier::BOLD),
            ));
            title.push(Span::styled(format!(" · {model}"), Style::default().fg(theme::MUTED)));
            if *is_error {
                title.push(Span::styled(" · ERROR", Style::default().fg(theme::BAD)));
            }
            let mut body: Vec<Vec<Span>> = Vec::new();
            let reason = est_tokens(thinking.len());
            let mut stats: Vec<Span> = Vec::new();
            stats.extend(tok_span("read", usage.cache_read_input_tokens, Color::Cyan));
            stats.extend(tok_span("write", usage.cache_creation_input_tokens, Color::Magenta));
            if reason > 0 {
                stats.extend(tok_span("reason~", reason, Color::Blue));
            }
            stats.extend(tok_span("out", usage.output_tokens, Color::Green));
            body.push(clip_spans(stats, inner));
            if !text.is_empty() {
                body.push(vec![]);
                body.extend(text_rows(text, inner, 4, Style::default().fg(Color::White)));
            } else if !thinking.is_empty() {
                body.push(vec![]);
                // Wrap 2 columns narrower: the 💭 marker is prepended after wrapping and
                // would otherwise push the first row past the border.
                let mut tr = text_rows(thinking, inner.saturating_sub(2), 3, Style::default().fg(theme::MUTED));
                if let Some(first) = tr.first_mut() {
                    first.insert(0, Span::styled("💭 ", Style::default().fg(theme::MUTED)));
                }
                body.extend(tr);
            }
            for t in tools {
                let line = format!("→ {} {}", t.name, short_path(&t.target));
                let mut row = vec![Span::styled(truncate(&line, inner), Style::default().fg(theme::ACCENT))];
                // Name the agent this call created: the conversation spliced in below is
                // otherwise linked to it only by position, which deep nesting destroys.
                if let Some(child) = &t.spawned {
                    let used: usize = row.iter().map(|s| dw(&s.content)).sum();
                    let tail = format!("  ╰▶ spawns #{}", child.chars().take(6).collect::<String>());
                    if used + dw(&tail) <= inner {
                        row.push(Span::styled(tail, Style::default().fg(theme::SIDECHAIN).add_modifier(Modifier::BOLD)));
                    }
                }
                body.push(row);
            }
            box_lines(ind, 0, depth, inner, &title, body, bs)
        }
        TKind::Tool { tool, target, tokens, content, is_error } => {
            let total = bubble_width(w).saturating_sub(8).max(24);
            let inner = total.saturating_sub(4);
            let extra = 4;
            let accent = if *tokens > 8_000 { theme::WARN } else { theme::MUTED };
            let bs = theme::border_style(sel, if *is_error { theme::BAD } else { theme::MUTED });
            let mut title = Vec::new();
            if let Some(a) = &it.agent {
                title.push(agent_tag(a));
            }
            title.extend([
                Span::styled(format!("⚙ {tool} result"), Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" · ~{} tok", fmt_int(*tokens)), Style::default().fg(accent)),
            ]);
            if *is_error {
                title.push(Span::styled(" · ERROR", Style::default().fg(theme::BAD)));
            }
            let mut body = vec![vec![Span::styled(truncate(&short_path(target), inner), Style::default().fg(Color::Gray))]];
            body.extend(text_rows(content, inner, 2, Style::default().fg(theme::MUTED)));
            box_lines(ind, extra, depth, inner, &title, body, bs)
        }
    }
}

/// Left gutter for a nested bubble: one rail per open ancestor level.
///
/// Indentation alone stops conveying structure once it is capped — every deep bubble ends
/// up at the same offset and nothing shows what it hangs off. Rails keep the ancestry
/// visible, and the count of them *is* the depth.
fn gutter(depth: usize, indent: usize, extra_pad: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    if depth > 0 && indent > 0 {
        let mut s = String::new();
        while s.chars().count() + 3 <= indent {
            s.push_str("│  ");
        }
        while s.chars().count() < indent {
            s.push(' ');
        }
        out.push(Span::styled(s, Style::default().fg(theme::SIDECHAIN).add_modifier(Modifier::DIM)));
    } else if indent > 0 {
        out.push(Span::raw(" ".repeat(indent)));
    }
    if extra_pad > 0 {
        out.push(Span::raw(" ".repeat(extra_pad)));
    }
    out
}

/// Assemble a bordered box: title in the top border, `body` rows, then a spacer line.
fn box_lines(
    indent: usize,
    extra_pad: usize,
    depth: usize,
    inner: usize,
    title: &[Span<'static>],
    body: Vec<Vec<Span<'static>>>,
    bs: Style,
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(body.len() + 3);
    out.push(box_top(indent, extra_pad, depth, inner, title, bs));
    for row in body {
        out.push(box_row(indent, extra_pad, depth, inner, row, bs));
    }
    out.push(box_bottom(indent, extra_pad, depth, inner, bs));
    out.push(Line::from("")); // air between bubbles
    out
}

fn box_top(indent: usize, extra_pad: usize, depth: usize, inner: usize, title: &[Span<'static>], bs: Style) -> Line<'static> {
    // Clip the title so the top border stays the same width as the body rows.
    let title = clip_spans(title.to_vec(), inner.saturating_sub(2));
    let title_len: usize = title.iter().map(|s| dw(&s.content)).sum();
    let dashes = inner.saturating_sub(1 + title_len);
    let mut spans = gutter(depth, indent, extra_pad);
    spans.push(Span::styled("╭─ ", bs));
    spans.extend(title);
    spans.push(Span::styled(format!(" {}╮", "─".repeat(dashes)), bs));
    Line::from(spans)
}

fn box_row(indent: usize, extra_pad: usize, depth: usize, inner: usize, content: Vec<Span<'static>>, bs: Style) -> Line<'static> {
    let used: usize = content.iter().map(|s| dw(&s.content)).sum();
    let fill = inner.saturating_sub(used);
    let mut spans = gutter(depth, indent, extra_pad);
    spans.push(Span::styled("│ ", bs));
    spans.extend(content);
    spans.push(Span::raw(" ".repeat(fill)));
    spans.push(Span::styled(" │", bs));
    Line::from(spans)
}

fn box_bottom(indent: usize, extra_pad: usize, depth: usize, inner: usize, bs: Style) -> Line<'static> {
    {
        let mut spans = gutter(depth, indent, extra_pad);
        spans.push(Span::styled(format!("╰{}╯", "─".repeat(inner + 2)), bs));
        Line::from(spans)
    }
}

/// Wrap `text` to `inner` columns, capped at `max` rows (last row gets an ellipsis when
/// truncated). Returns styled span rows.
fn text_rows(text: &str, inner: usize, max: usize, style: Style) -> Vec<Vec<Span<'static>>> {
    let mut rows = wrap_str(text, inner);
    if rows.len() > max {
        rows.truncate(max);
        if let Some(last) = rows.last_mut() {
            // `truncate` already appends its own ellipsis when it shortens.
            let clipped: String = last.chars().take(inner.saturating_sub(1)).collect();
            *last = format!("{clipped}…");
        }
    }
    rows.into_iter().map(|r| vec![Span::styled(r, style)]).collect()
}
