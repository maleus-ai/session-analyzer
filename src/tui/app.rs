//! Application state and input handling — the controller.

use crate::analysis::{Analysis, CacheContrib, Metrics, SessionReport, TItem, TKind};
use crate::loader::LoadInfo;
use crate::query;
use crossterm::event::{self, KeyCode, KeyModifiers, MouseButton, MouseEventKind};

pub(crate) const TABS: [&str; 10] =
    ["Overview", "Sessions", "Transcript", "Timeline", "Tools", "Sinks", "Cache-attr", "Sub-agents", "Issues", "Rate"];
pub(crate) const NTABS: usize = 10;

/// Query column list backing each sortable tab (drives `[`/`]` cycling and sorting).
pub(crate) fn tab_cols(tab: usize) -> &'static [&'static str] {
    match tab {
        1 => query::SESSION_COLS,
        3 => query::TIMELINE_COLS,
        4 => query::TOOL_COLS,
        5 => query::SINK_COLS,
        6 => query::CACHEATTR_COLS,
        7 => query::AGENT_COLS,
        _ => &[],
    }
}

/// Maps each *displayed* column (render order) to the sort-column name it triggers, or
/// `None` if not sortable. Aligned 1:1 with each view's `headers`, so a header click
/// resolves to a valid sort column (never a raw index).
pub(crate) fn disp_map(tab: usize) -> &'static [Option<&'static str>] {
    match tab {
        // COST TOTAL TURNS ACTIVE_H IDLE ENTRY MODEL TITLE
        1 => &[Some("cost"), Some("tokens"), Some("turns"), Some("duration"), None, None, None, None],
        3 => &[Some("turn"), None, Some("context"), Some("delta"), Some("write"), None, Some("cost"), None, None],
        4 => &[Some("name"), Some("calls"), Some("result"), Some("input"), Some("errors")],
        5 => &[Some("amplified"), Some("size"), Some("contribution"), None, Some("calls"), None, None],
        6 => &[Some("share"), None, Some("contribution"), Some("contribution"), Some("entries"), None],
        // AGENT TYPE D MODEL TURNS TOKENS COST TOOLS DUR OUTCOME DESCRIPTION
        7 => &[Some("seq"), None, Some("depth"), None, Some("turns"), Some("tokens"), Some("cost"), Some("tools"), Some("duration"), None, None],
        _ => &[],
    }
}

/// Searchable text of a transcript item (for `/` search).
fn titem_search_text(it: &TItem) -> String {
    match &it.kind {
        TKind::User { text, .. } => text.clone(),
        TKind::Assistant { thinking, text, tools, .. } => {
            let mut s = format!("{thinking}\n{text}");
            for t in tools {
                s.push_str(&format!(" {} {}", t.name, t.target));
            }
            s
        }
        TKind::Tool { tool, target, content, .. } => format!("{tool} {target}\n{content}"),
        TKind::Compact { trigger, .. } => trigger.clone(),
        TKind::Event { subtype, detail, content, .. } => format!("{subtype} {detail}\n{content}"),
    }
}

/// A modal overlay. `Transcript` holds a transcript item index; `Turn` holds a 1-based
/// timeline turn number; `Detail` holds the tab + selected (sorted) row index of a table
/// whose row was opened for details.
/// One row of the Transcript tab: a message, or a collapsed sub-agent conversation.
#[derive(Clone)]
pub(crate) enum TRow {
    Item(usize),
    /// A sub-agent, shown as a single summary row until opened.
    Agent(String),
}

#[derive(Clone, Copy)]
pub(crate) enum Popup {
    Transcript(usize),
    Turn(usize),
    Detail { tab: usize, idx: usize },
}

pub(crate) struct App<'a> {
    pub a: &'a Analysis,
    pub info: &'a LoadInfo,
    pub tab: usize,
    pub focus: Option<usize>,
    pub sel: [usize; NTABS],
    pub sort_col: [usize; NTABS],
    pub sort_desc: [bool; NTABS],
    pub scroll: u16,
    /// First visible transcript item (windowed rendering, so huge transcripts stay fast).
    pub t_offset: usize,
    /// First visible table row per tab (windowed rendering + correct mouse hit-testing).
    pub row_offset: [usize; NTABS],
    pub popup: Option<Popup>,
    pub popup_scroll: u16,
    /// Transcript search: the query, and whether we're currently typing it.
    pub search: String,
    pub search_active: bool,
    /// Transcript: which thread is open. Empty = the main conversation; each entry is a
    /// sub-agent id one level deeper. A delegated conversation is collapsed to a single row
    /// until you step into it, so a 54-deep chain reads as one line instead of 169 bubbles.
    pub t_scope: Vec<String>,
    /// Transcript: show every thread inline instead of collapsing sub-agents (`a`).
    pub flatten: bool,
    // Hit-test geometry captured each frame while views draw.
    pub tab_hits: Vec<(u16, u16, usize)>,
    pub header_hits: Vec<(u16, u16, usize)>,
    pub row0: u16,
    pub rows_visible: u16,
}

impl<'a> App<'a> {
    pub fn new(a: &'a Analysis, info: &'a LoadInfo) -> Self {
        App {
            a,
            info,
            tab: 0,
            focus: None,
            sel: [0; NTABS],
            sort_col: [0; NTABS],
            // Sub-agents defaults to ascending `seq`, i.e. the order the delegation chain
            // actually happened in — the same default the CLI uses.
            sort_desc: {
                let mut d = [true; NTABS];
                d[7] = false;
                d
            },
            scroll: 0,
            t_offset: 0,
            row_offset: [0; NTABS],
            popup: None,
            popup_scroll: 0,
            search: String::new(),
            search_active: false,
            t_scope: Vec::new(),
            flatten: false,
            tab_hits: Vec::new(),
            header_hits: Vec::new(),
            row0: 0,
            rows_visible: 0,
        }
    }

    // ---- scoped data accessors (respect the focused session) ----

    pub fn metrics(&self) -> &Metrics {
        self.focus.map(|i| &self.a.sessions[i].metrics).unwrap_or(&self.a.global)
    }
    pub fn cache_attr(&self) -> &[CacheContrib] {
        self.focus.map(|i| self.a.sessions[i].cache_attr.as_slice()).unwrap_or(&self.a.global_cache_attr)
    }
    pub fn focus_report(&self) -> Option<&SessionReport> {
        self.focus.map(|i| &self.a.sessions[i])
    }

    /// What the Transcript tab shows: the messages of the currently open thread, with each
    /// sub-agent it spawns collapsed to one row that can be stepped into.
    pub fn transcript_view(&self) -> Vec<TRow> {
        let Some(sr) = self.focus_report() else { return Vec::new() };
        if self.flatten {
            return (0..sr.transcript.len()).map(TRow::Item).collect();
        }
        let scope = self.t_scope.last().map(String::as_str);
        let mut out = Vec::new();
        for (i, it) in sr.transcript.iter().enumerate() {
            if it.agent.as_ref().map(|a| a.id.as_str()) != scope {
                continue;
            }
            out.push(TRow::Item(i));
            // A spawn is followed by the child's whole conversation; stand in for it with
            // one row rather than inlining a thread that may itself be 50 levels deep.
            if let TKind::Assistant { tools, .. } = &it.kind {
                for t in tools {
                    if let Some(child) = &t.spawned {
                        out.push(TRow::Agent(child.clone()));
                    }
                }
            }
        }
        out
    }

    /// Breadcrumb for the open thread, e.g. `main ▸ general-purpose#a9e9a6 ▸ …`.
    pub fn scope_path(&self) -> String {
        let Some(sr) = self.focus_report() else { return "main".into() };
        let mut parts = vec!["main".to_string()];
        for id in &self.t_scope {
            parts.push(sr.threads.iter().find(|t| &t.agent.id == id).map(|t| t.agent.label()).unwrap_or_else(|| id.clone()));
        }
        parts.join(" ▸ ")
    }

    pub fn list_len(&self) -> usize {
        match self.tab {
            1 => self.a.sessions.len(),
            2 => self.transcript_view().len(),
            3 => self.focus_report().map_or(0, |s| s.timeline.len()),
            4 => self.metrics().tools.len(),
            5 => query::sinks_only(self.cache_attr()).len(),
            6 => self.cache_attr().len(),
            // The thread roster, not the result records: an agent that never returned has
            // no result record but is still a row.
            7 => crate::tui::views::subagents::threads(self).len(),
            8 => self.metrics().findings.len(),
            _ => 0,
        }
    }
    fn sel_mut(&mut self) -> &mut usize {
        &mut self.sel[self.tab]
    }

    // ---- input handling ----

    /// Handle a key press. Returns `true` when the app should quit.
    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        // Transcript search: capture typing while the search box is active.
        if self.search_active {
            match code {
                KeyCode::Esc => {
                    self.search.clear();
                    self.search_active = false;
                }
                KeyCode::Enter => {
                    self.search_active = false;
                    self.jump_transcript(1);
                }
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => self.search.push(c),
                _ => {}
            }
            return false;
        }

        if self.popup.is_some() {
            match code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    self.popup = None;
                    self.popup_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => self.popup_scroll = self.popup_scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => self.popup_scroll = self.popup_scroll.saturating_sub(1),
                KeyCode::PageDown => self.popup_scroll = self.popup_scroll.saturating_add(20),
                KeyCode::PageUp => self.popup_scroll = self.popup_scroll.saturating_sub(20),
                _ => {}
            }
            return false;
        }

        let len = self.list_len();
        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Tab | KeyCode::Right => self.switch_tab((self.tab + 1) % NTABS),
            KeyCode::BackTab | KeyCode::Left => self.switch_tab((self.tab + NTABS - 1) % NTABS),
            KeyCode::Char(c @ '1'..='9') => self.switch_tab((c as usize - '1' as usize).min(NTABS - 1)),
            KeyCode::Char('0') => self.switch_tab(9), // Rate (10th tab)
            KeyCode::Down | KeyCode::Char('j') => {
                if len > 0 {
                    *self.sel_mut() = (self.sel[self.tab] + 1).min(len - 1);
                }
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *self.sel_mut() = self.sel[self.tab].saturating_sub(1);
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                if len > 0 {
                    *self.sel_mut() = (self.sel[self.tab] + 10).min(len - 1);
                }
                self.scroll = self.scroll.saturating_add(10);
            }
            KeyCode::PageUp => {
                *self.sel_mut() = self.sel[self.tab].saturating_sub(10);
                self.scroll = self.scroll.saturating_sub(10);
            }
            KeyCode::Home => {
                *self.sel_mut() = 0;
                self.scroll = 0;
            }
            KeyCode::Char('[') => self.cycle_sort(-1),
            KeyCode::Char(']') => self.cycle_sort(1),
            KeyCode::Char('r') => {
                if !tab_cols(self.tab).is_empty() {
                    self.sort_desc[self.tab] = !self.sort_desc[self.tab];
                }
            }
            KeyCode::Char('/') if self.tab == 2 => {
                self.search.clear();
                self.search_active = true;
            }
            KeyCode::Char('n') if self.tab == 2 && !self.search.is_empty() => self.jump_transcript(1),
            KeyCode::Char('N') if self.tab == 2 && !self.search.is_empty() => self.jump_transcript(-1),
            // Toggle between the collapsed (per-thread) transcript and one flat stream.
            KeyCode::Char('a') if self.tab == 2 => {
                self.flatten = !self.flatten;
                self.t_scope.clear();
                self.sel[2] = 0;
                self.t_offset = 0;
            }
            KeyCode::Enter => self.activate(),
            // Esc walks back out of a sub-agent before it leaves the session.
            KeyCode::Esc => {
                if !self.search.is_empty() {
                    self.search.clear();
                } else if self.tab == 2 && !self.t_scope.is_empty() {
                    self.t_scope.pop();
                    self.sel[2] = 0;
                    self.t_offset = 0;
                } else {
                    self.focus = None;
                }
            }
            _ => {}
        }
        false
    }

    /// Move the transcript selection to the next (`dir=1`) / previous (`dir=-1`) message
    /// whose text matches the current search.
    fn jump_transcript(&mut self, dir: i32) {
        let q = self.search.to_lowercase();
        if q.is_empty() {
            return;
        }
        let Some(sr) = self.focus_report() else { return };
        let view = self.transcript_view();
        let n = view.len();
        if n == 0 {
            return;
        }
        let matches = |i: usize| match &view[i] {
            TRow::Item(t) => titem_search_text(&sr.transcript[*t]).to_lowercase().contains(&q),
            TRow::Agent(_) => false,
        };
        let cur = self.sel[2];
        let hit = (1..=n).find_map(|step| {
            let i = if dir >= 0 {
                (cur + step) % n
            } else {
                (cur + n - (step % n)) % n
            };
            if matches(i) { Some(i) } else { None }
        });
        if let Some(i) = hit {
            self.sel[2] = i;
        }
    }

    pub fn handle_mouse(&mut self, m: event::MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollDown => {
                let len = self.list_len();
                if self.popup.is_some() {
                    self.popup_scroll = self.popup_scroll.saturating_add(3);
                } else if len > 0 {
                    *self.sel_mut() = (self.sel[self.tab] + 1).min(len - 1);
                    self.scroll = self.scroll.saturating_add(1);
                }
            }
            MouseEventKind::ScrollUp => {
                if self.popup.is_some() {
                    self.popup_scroll = self.popup_scroll.saturating_sub(3);
                } else {
                    *self.sel_mut() = self.sel[self.tab].saturating_sub(1);
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.popup.is_some() {
                    self.popup = None;
                    return;
                }
                let (x, y) = (m.column, m.row);
                if let Some(&(_, _, tab)) = self.tab_hits.iter().find(|(x0, x1, _)| x >= *x0 && x < *x1) {
                    if y <= 2 {
                        self.switch_tab(tab);
                        return;
                    }
                }
                // Column header → map the display column to a sortable column name.
                if y == self.row0.saturating_sub(1) {
                    if let Some(&(_, _, disp)) = self.header_hits.iter().find(|(x0, x1, _)| x >= *x0 && x < *x1) {
                        if let Some(name) = disp_map(self.tab).get(disp).copied().flatten() {
                            if let Some(pos) = tab_cols(self.tab).iter().position(|c| *c == name) {
                                if self.sort_col[self.tab] == pos {
                                    self.sort_desc[self.tab] = !self.sort_desc[self.tab];
                                } else {
                                    self.sort_col[self.tab] = pos;
                                }
                            }
                        }
                        return;
                    }
                }
                // Data row → select (accounting for the table's scroll offset), and open
                // its detail on click (single-click acts like Enter for tables).
                if y >= self.row0 && y < self.row0 + self.rows_visible {
                    let len = self.list_len();
                    let idx = self.row_offset[self.tab] + (y - self.row0) as usize;
                    if len > 0 && idx < len {
                        let already = self.sel[self.tab] == idx;
                        *self.sel_mut() = idx;
                        // Second click on the same row drills in / opens detail.
                        if already {
                            self.activate();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn switch_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.scroll = 0;
    }

    fn cycle_sort(&mut self, dir: i32) {
        let cols = tab_cols(self.tab);
        if cols.is_empty() {
            return;
        }
        let n = cols.len() as i32;
        let cur = self.sort_col[self.tab] as i32;
        self.sort_col[self.tab] = (((cur + dir) % n + n) % n) as usize;
    }

    fn activate(&mut self) {
        match self.tab {
            1 => {
                if !self.a.sessions.is_empty() {
                    self.focus = Some(self.sel[1].min(self.a.sessions.len() - 1));
                    // Reset transcript/timeline scroll for the newly focused session.
                    self.sel[2] = 0;
                    self.sel[3] = 0;
                    self.t_offset = 0;
                }
            }
            2 => match self.transcript_view().get(self.sel[2]).cloned() {
                // Enter on a message expands it; on a collapsed sub-agent it steps into that
                // conversation, which is what makes a deep chain navigable at all.
                Some(TRow::Item(i)) => {
                    self.popup = Some(Popup::Transcript(i));
                    self.popup_scroll = 0;
                }
                Some(TRow::Agent(id)) => {
                    self.t_scope.push(id);
                    self.sel[2] = 0;
                    self.t_offset = 0;
                }
                None => {}
            },
            3 => {
                // The per-turn list is sorted; resolve the selection to its turn number.
                let turn = self.focus.and_then(|fi| {
                    let sr = &self.a.sessions[fi];
                    let mut tl: Vec<&crate::analysis::TurnPoint> = sr.timeline.iter().collect();
                    let col = query::TIMELINE_COLS[self.sort_col[3]];
                    let _ = query::sort_timeline(&mut tl, col, self.sort_desc[3]);
                    tl.get(self.sel[3]).map(|t| t.turn)
                });
                if let Some(t) = turn {
                    self.popup = Some(Popup::Turn(t));
                    self.popup_scroll = 0;
                }
            }
            // Table tabs: open a detail popup for the selected (sorted) row.
            4 | 5 | 6 | 7 | 8 => {
                if self.list_len() > 0 {
                    self.popup = Some(Popup::Detail { tab: self.tab, idx: self.sel[self.tab] });
                    self.popup_scroll = 0;
                }
            }
            _ => {}
        }
    }
}
