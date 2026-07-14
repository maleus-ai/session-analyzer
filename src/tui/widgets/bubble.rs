//! Chat-bubble component: renders a transcript item as a bordered box — user messages
//! right-aligned, assistant/tool left-aligned — with a title in the top border, wrapped
//! body rows and per-message token stats.

use crate::analysis::{TItem, TKind, est_tokens, fmt_int, short_path};
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

/// Build one chat bubble for a transcript item. `w` is the available inner width.
pub fn bubble(it: &TItem, sel: bool, w: usize) -> Vec<Line<'static>> {
    match &it.kind {
        TKind::Compact { pre, post, trigger } => {
            let label = format!(" ✂ COMPACTED ({}) · {} → {} tokens ", trigger, fmt_int(*pre), fmt_int(*post));
            let side = w.saturating_sub(dw(&label)) / 2;
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    format!("{}{}{}", "─".repeat(side), label, "─".repeat(side)),
                    Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ]
        }
        TKind::User { text, is_prompt } => {
            let total = bubble_width(w);
            let inner = total.saturating_sub(4);
            let pad = w.saturating_sub(total); // right-align
            let bs = theme::border_style(sel, theme::USER);
            let title = vec![Span::styled(
                if *is_prompt { "👤 You" } else { "👤 You · meta" },
                Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD),
            )];
            let mut body = text_rows(text, inner, 6, Style::default().fg(Color::Gray));
            if body.is_empty() {
                body.push(vec![Span::styled("(empty)", Style::default().fg(theme::MUTED))]);
            }
            box_lines(pad, inner, &title, body, bs)
        }
        TKind::Assistant { turn, model, usage, thinking, text, tools, is_error } => {
            let total = bubble_width(w);
            let inner = total.saturating_sub(4);
            let bs = theme::border_style(sel, theme::ASSISTANT);
            let mut title = vec![
                Span::styled(format!("🤖 turn {turn}"), Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" · {model}"), Style::default().fg(theme::MUTED)),
            ];
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
                let mut tr = text_rows(thinking, inner, 3, Style::default().fg(theme::MUTED));
                if let Some(first) = tr.first_mut() {
                    first.insert(0, Span::styled("💭 ", Style::default().fg(theme::MUTED)));
                }
                body.extend(tr);
            }
            for t in tools {
                let line = format!("→ {} {}", t.name, short_path(&t.target));
                body.push(vec![Span::styled(truncate(&line, inner), Style::default().fg(theme::ACCENT))]);
            }
            box_lines(0, inner, &title, body, bs)
        }
        TKind::Tool { tool, target, tokens, content, is_error } => {
            let total = bubble_width(w).saturating_sub(8).max(24);
            let inner = total.saturating_sub(4);
            let pad = 4usize;
            let accent = if *tokens > 8_000 { theme::WARN } else { theme::MUTED };
            let bs = theme::border_style(sel, if *is_error { theme::BAD } else { theme::MUTED });
            let mut title = vec![
                Span::styled(format!("⚙ {tool} result"), Style::default().fg(theme::ACCENT).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" · ~{} tok", fmt_int(*tokens)), Style::default().fg(accent)),
            ];
            if *is_error {
                title.push(Span::styled(" · ERROR", Style::default().fg(theme::BAD)));
            }
            let mut body = vec![vec![Span::styled(truncate(&short_path(target), inner), Style::default().fg(Color::Gray))]];
            body.extend(text_rows(content, inner, 2, Style::default().fg(theme::MUTED)));
            box_lines(pad, inner, &title, body, bs)
        }
    }
}

/// Assemble a bordered box: title in the top border, `body` rows, then a spacer line.
fn box_lines(pad: usize, inner: usize, title: &[Span<'static>], body: Vec<Vec<Span<'static>>>, bs: Style) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(body.len() + 3);
    out.push(box_top(pad, inner, title, bs));
    for row in body {
        out.push(box_row(pad, inner, row, bs));
    }
    out.push(box_bottom(pad, inner, bs));
    out.push(Line::from("")); // air between bubbles
    out
}

fn box_top(pad: usize, inner: usize, title: &[Span<'static>], bs: Style) -> Line<'static> {
    // Clip the title so the top border stays the same width as the body rows.
    let title = clip_spans(title.to_vec(), inner.saturating_sub(2));
    let title_len: usize = title.iter().map(|s| dw(&s.content)).sum();
    let dashes = inner.saturating_sub(1 + title_len);
    let mut spans = vec![Span::raw(" ".repeat(pad)), Span::styled("╭─ ", bs)];
    spans.extend(title);
    spans.push(Span::styled(format!(" {}╮", "─".repeat(dashes)), bs));
    Line::from(spans)
}

fn box_row(pad: usize, inner: usize, content: Vec<Span<'static>>, bs: Style) -> Line<'static> {
    let used: usize = content.iter().map(|s| dw(&s.content)).sum();
    let fill = inner.saturating_sub(used);
    let mut spans = vec![Span::raw(" ".repeat(pad)), Span::styled("│ ", bs)];
    spans.extend(content);
    spans.push(Span::raw(" ".repeat(fill)));
    spans.push(Span::styled(" │", bs));
    Line::from(spans)
}

fn box_bottom(pad: usize, inner: usize, bs: Style) -> Line<'static> {
    Line::from(vec![Span::raw(" ".repeat(pad)), Span::styled(format!("╰{}╯", "─".repeat(inner + 2)), bs)])
}

/// Wrap `text` to `inner` columns, capped at `max` rows (last row gets an ellipsis when
/// truncated). Returns styled span rows.
fn text_rows(text: &str, inner: usize, max: usize, style: Style) -> Vec<Vec<Span<'static>>> {
    let mut rows = wrap_str(text, inner);
    if rows.len() > max {
        rows.truncate(max);
        if let Some(last) = rows.last_mut() {
            *last = format!("{}…", truncate(last, inner.saturating_sub(1)));
        }
    }
    rows.into_iter().map(|r| vec![Span::styled(r, style)]).collect()
}
