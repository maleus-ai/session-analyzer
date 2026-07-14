//! Transcript page: a chat-bubble view of the focused session's messages.
//!
//! Rendered **windowed** — only the bubbles that fit the viewport are built each frame, so
//! a transcript with thousands of turns stays responsive (no per-frame O(all) work).

use crate::tui::app::App;
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

    let len = sr.transcript.len();
    let title = format!(" Transcript — {}  ({} items) ", truncate(&sr.title, 40), len);
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
        for (i, it) in sr.transcript.iter().enumerate().skip(offset) {
            let b = bubble(it, i == sel, w);
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
