//! Interactive Ratatui dashboard.
//!
//! Architecture (frontend-style): `app` holds state + input handling (the controller),
//! `theme` and `format` are design tokens / helpers, `widgets` are reusable components
//! (tabs, table, chat bubble, popup, bars) and `views` are the per-tab pages. This module
//! is the composition root: it owns the terminal, the event loop, and the frame layout
//! that wires views + widgets together.

mod app;
mod format;
mod theme;
mod views;
mod widgets;

use crate::analysis::Analysis;
use crate::loader::LoadInfo;
use anyhow::Result;
use app::{App, Popup};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::prelude::*;
use std::io::stdout;
use std::time::Duration;

/// Launch the dashboard, restoring the terminal on exit.
pub fn run(a: &Analysis, info: &LoadInfo) -> Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let mut app = App::new(a, info);
    let res = event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    res
}

fn event_loop<B: Backend>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    // Redraw only when something changed — no idle CPU, and huge views only re-render on
    // actual input.
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|f| ui(f, app))?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                dirty = true;
                if app.handle_key(key.code, key.modifiers) {
                    return Ok(());
                }
            }
            Event::Mouse(m) => {
                dirty = true;
                app.handle_mouse(m);
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
}

/// Top-level frame layout: tab bar, active view, footer, and any modal popup.
fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    widgets::tabs::draw_tabs(f, app, chunks[0]);

    // Reset per-frame hit-test state; views populate header/table geometry as they draw.
    app.header_hits.clear();
    app.row0 = chunks[1].y + 2; // block border + header row
    app.rows_visible = chunks[1].height.saturating_sub(3);

    let area = chunks[1];
    match app.tab {
        0 => views::overview::draw(f, app, area),
        1 => views::sessions::draw(f, app, area),
        2 => views::transcript::draw(f, app, area),
        3 => views::timeline::draw(f, app, area),
        4 => views::tools::draw(f, app, area),
        5 => views::sinks::draw(f, app, area),
        6 => views::cache_attr::draw(f, app, area),
        7 => views::subagents::draw(f, app, area),
        8 => views::issues::draw(f, app, area),
        9 => views::rate::draw(f, app, area),
        _ => {}
    }
    widgets::tabs::draw_footer(f, app, chunks[2]);

    if let Some(popup) = app.popup {
        widgets::popup::dim_area(f, f.area());
        match popup {
            Popup::Transcript(idx) => widgets::popup::draw_transcript_popup(f, app, idx),
            Popup::Turn(turn) => widgets::popup::draw_turn_popup(f, app, turn),
            Popup::Detail { tab, idx } => widgets::popup::draw_detail_popup(f, app, tab, idx),
        }
    }
}
