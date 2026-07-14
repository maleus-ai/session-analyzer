//! Text and layout helpers shared across widgets and views (no rendering state).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::analysis::fmt_int;

/// Display (terminal-column) width — counts wide chars (emoji, CJK) as 2.
pub fn dw(s: &str) -> usize {
    s.width()
}

/// Truncate to `n` display-ish chars with an ellipsis.
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

/// Signed, thousands-separated delta (`+1,234` / `-56` / `0`).
pub fn fmt_delta(d: i64) -> String {
    if d > 0 {
        format!("+{}", fmt_int(d as u64))
    } else if d < 0 {
        format!("-{}", fmt_int((-d) as u64))
    } else {
        "0".into()
    }
}

/// Greedy word-wrap to `w` columns, hard-splitting over-long words.
pub fn wrap_str(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let wl = word.chars().count();
        if wl > w {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut rest = word;
            while rest.chars().count() > w {
                let idx = rest.char_indices().nth(w).map(|(i, _)| i).unwrap_or(rest.len());
                out.push(rest[..idx].to_string());
                rest = &rest[idx..];
            }
            cur = rest.to_string();
        } else if cur.is_empty() {
            cur = word.to_string();
        } else if cur.chars().count() + 1 + wl <= w {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Truncate a run of styled spans so their combined display width fits `inner`.
pub fn clip_spans(spans: Vec<Span<'static>>, inner: usize) -> Vec<Span<'static>> {
    let mut used = 0usize;
    let mut out = Vec::new();
    for s in spans {
        let l = dw(&s.content);
        if used + l <= inner {
            used += l;
            out.push(s);
        } else {
            let room = inner.saturating_sub(used);
            if room > 0 {
                let t: String = s.content.chars().take(room).collect();
                out.push(Span::styled(t, s.style));
            }
            break;
        }
    }
    out
}

/// A centered sub-rectangle sized as a percentage of `area`.
pub fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

// ---- key/value summary lines (Overview) ----

/// Pad a label to the key column, always leaving ≥1 space before the value even when the
/// label is longer than the column (fixes label/value collision).
fn label(k: &str) -> String {
    let w = 18usize.max(k.chars().count() + 1);
    format!("{k:<w$}")
}

pub fn kv<'a>(k: &str, v: &str) -> Line<'a> {
    Line::from(vec![Span::styled(label(k), Style::default().fg(Color::Gray)), Span::raw(v.to_string())])
}
pub fn kv_bold<'a>(k: &str, v: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(label(k), Style::default().fg(Color::Gray)),
        Span::styled(v.to_string(), Style::default().add_modifier(Modifier::BOLD)),
    ])
}
pub fn kv_val<'a>(k: &str, v: &str, c: Color) -> Line<'a> {
    Line::from(vec![
        Span::styled(label(k), Style::default().fg(Color::Gray)),
        Span::styled(v.to_string(), Style::default().fg(c).add_modifier(Modifier::BOLD)),
    ])
}
pub fn section_line<'a>(t: &str) -> Line<'a> {
    Line::from(Span::styled(t.to_string(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)))
}
