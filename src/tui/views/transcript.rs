//! Transcript page: a chat-bubble view of the focused session's messages.
//!
//! Rendered **windowed** — only the bubbles that fit the viewport are built each frame, so
//! a transcript with thousands of turns stays responsive (no per-frame O(all) work).

use crate::tui::app::{App, TRow};
use crate::tui::widgets::bubble::agent_row;
use crate::tui::format::truncate;
use crate::tui::widgets::bubble::bubble;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(sr) = app.focus_report() else {
        f.render_widget(
            Paragraph::new("Select a session first (Sessions tab → Enter), then view its transcript.")
                .block(Block::default().borders(Borders::ALL).title(" Transcript ")),
            area,
        );
        return;
    };

    let view = app.transcript_view();
    let len = view.len();
    // Title carries the breadcrumb: which thread is open, and how to get back out.
    let path = app.scope_path();
    let title = if app.flatten {
        format!(" Transcript — {}  ({} items, all threads inline) ", truncate(&sr.title, 34), len)
    } else if app.t_scope.is_empty() {
        let subs = sr.threads.len();
        format!(
            " Transcript — {}  ({} items{}) ",
            truncate(&sr.title, 34),
            len,
            if subs > 0 { format!(" · {subs} sub-agent(s) collapsed") } else { String::new() }
        )
    } else {
        format!(" {}  ({} items · Esc to go back) ", path, len)
    };
    let mut block = Block::default().borders(Borders::ALL).title(title);
    if !app.search.is_empty() {
        block = block.title(Line::from(format!(" /{} (n/N) ", app.search)).right_aligned());
    }
    let inner = block.inner(area);
    let w = inner.width as usize;
    let h = inner.height as usize;
    if len == 0 || h == 0 {
        f.render_widget(block, area);
        return;
    }

    let sel = app.sel[2].min(len - 1);
    // Keep the selection within the window: never start below it.
    let mut offset = app.t_offset.min(sel);

    // Build downward from `offset`; if the selection scrolls past the bottom, advance the
    // window and rebuild. Only bubbles that fit the viewport are ever constructed.
    let lines = loop {
        let mut lines: Vec<Line> = Vec::new();
        let mut reached_sel = false;
        for (i, row) in view.iter().enumerate().skip(offset) {
            // Depth is relative to the open thread, so its own messages sit flush and only
            // what it spawned is indented.
            let base = if app.flatten { 0 } else { app.t_scope.len() };
            let b = match row {
                TRow::Item(ti) => {
                    let it = &sr.transcript[*ti];
                    let d = it.agent.as_ref().map_or(0, |a| a.depth).saturating_sub(base);
                    bubble(it, i == sel, w, d)
                }
                // A whole delegated conversation, standing in as one row.
                TRow::Agent(id) => match sr.threads.iter().find(|t| &t.agent.id == id) {
                    Some(t) => agent_row(t, i == sel, w, t.agent.depth.saturating_sub(base)),
                    None => continue,
                },
            };
            if !lines.is_empty() && lines.len() + b.len() > h {
                break;
            }
            lines.extend(b);
            if i == sel {
                reached_sel = true;
            }
            if lines.len() >= h {
                break;
            }
        }
        if reached_sel || offset >= sel {
            break lines;
        }
        offset += 1;
    };

    app.t_offset = offset;
    f.render_widget(Paragraph::new(lines).block(block), area);
}
