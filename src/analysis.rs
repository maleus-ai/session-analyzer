//! Aggregation and derived analytical views over the normalized [`Dataset`].
//!
//! Everything is computed from the single ordered [`Item`] stream, per session:
//! - **Metrics**: token/cost aggregates, per-tool stats, sinks, sub-agents, compactions.
//! - **Transcript**: resolved, ordered, full-text messages for the bubble view.
//! - **Timeline**: per-assistant-turn token series → context-growth curve + write spikes.
//! - **Cache attribution**: decomposition of cache-read by the content that stayed
//!   resident in context (the "context tax" — a block of `T` tokens resident for `R`
//!   turns contributes `T×R` to cache-read).
//! - **Spikes**: anomalous context-growth turns with their cause.

use crate::model::*;
use crate::pricing::price_for;
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};

const CHARS_PER_TOKEN: f64 = 3.7;

/// Gap between turns above which we treat the session as having paused (idle). Used to
/// separate a continuous unattended burst from stop-and-go interactive work.
pub const IDLE_GAP_MS: i64 = 15 * 60 * 1000; // 15 minutes

/// Gap between turns above which the agent was clearly **not working** — someone was
/// reading, thinking, or away. Deliberately smaller than [`IDLE_GAP_MS`], because the two
/// answer different questions: 15 minutes is about whether load lands in one rate-limit
/// window, while "was this continuous unattended work?" is falsified by a pause far shorter
/// than that. A single turn plus its tool calls rarely exceeds this; when it does (a long
/// test run) active time is under-counted, which is the safe direction to err.
pub const WORK_GAP_MS: i64 = 5 * 60 * 1000; // 5 minutes

pub fn est_tokens(chars: usize) -> u64 {
    (chars as f64 / CHARS_PER_TOKEN) as u64
}

// ------------------------------------------------------------------- view types

/// Per-tool rollup.
#[derive(Debug, Clone, Default)]
pub struct ToolStat {
    pub name: String,
    pub calls: u64,
    pub input_chars: u64,
    pub result_tokens_est: u64,
    pub result_chars: u64,
    pub errors: u64,
}

/// Decomposition of cache-read (and one-time size) attributed to a content source.
#[derive(Debug, Clone)]
pub struct CacheContrib {
    pub tool: String,
    pub target: String,
    /// One-time size when this content entered context (Σ over entries).
    pub tokens: u64,
    pub entries: u64,
    /// Σ residency in turns across entries.
    pub residency_turns: u64,
    /// Replayed tokens = Σ tokens×residency (normalized to reconcile with cache-read).
    pub contribution: u64,
    /// Share of total cache-read [0,1].
    pub share: f64,
    /// Estimated USD cost of those replays.
    pub amplified_cost: f64,
    /// Synthetic baseline row (system prompt / prior conversation), not a real sink.
    pub is_baseline: bool,
}

/// Identifies the sub-agent a turn or transcript item belongs to. `None` on the main thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRef {
    /// Sub-agent id, as logged (`agentId`).
    pub id: String,
    /// Agent type from the spawning Agent call (`Explore`, `general-purpose`, …).
    pub agent_type: String,
    /// The one-line description the Agent call gave this sub-agent, when recorded.
    pub description: String,
    /// Nesting level: 1 for an agent spawned by the main thread, 2 for one spawned by an
    /// agent, and so on.
    pub depth: usize,
    /// The agent that spawned this one; `None` when the main thread did. Indentation alone
    /// cannot show what a deeply nested bubble hangs off — naming the parent can.
    pub parent_id: Option<String>,
}

impl AgentRef {
    /// Short id of the spawning agent, for "this hangs off that" in a header.
    pub fn parent_short(&self) -> Option<String> {
        self.parent_id.as_ref().map(|p| p.chars().take(6).collect())
    }

    /// Short display label, e.g. `Explore#a221ec`, suffixed with `·d3` when the agent is
    /// itself nested inside other agents (indentation alone can't show depth 50).
    pub fn label(&self) -> String {
        let short: String = self.id.chars().take(6).collect();
        let base = if short.is_empty() { self.agent_type.clone() } else { format!("{}#{}", self.agent_type, short) };
        if self.depth > 1 { format!("{base}·d{}", self.depth) } else { base }
    }
}

/// How a thread (a run or a sub-agent) ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Finished normally — the model stopped of its own accord, or a result came back.
    Completed,
    /// A harness limit ended it (turn/token cap, abort, interrupt).
    LimitHit,
    /// The log stops mid-generation: no closing `stop_reason` and no result. That is the
    /// observation — whether the process was stopped, crashed, or was simply still running
    /// when the capture was taken is *not* determinable from the log alone, so `ssa` does
    /// not claim one. Compare the last record time with the capture's write time to judge.
    Truncated,
    /// The last turn was an API error.
    Errored,
}

impl Outcome {
    /// Classify a thread from its last assistant turn. `tool_use` means the model asked for
    /// a tool and the log ends there — the conversation was left waiting, i.e. cut off, not
    /// finished. Only an explicit end-of-turn counts as completion.
    pub fn from_last_turn(stop_reason: &str, is_error: bool) -> Outcome {
        match () {
            _ if is_error => Outcome::Errored,
            _ if matches!(stop_reason, "end_turn" | "stop_sequence") => Outcome::Completed,
            _ => Outcome::Truncated,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Outcome::Completed => "completed",
            Outcome::LimitHit => "limit-hit",
            Outcome::Truncated => "truncated",
            Outcome::Errored => "errored",
        }
    }
}

/// One run: a single SDK `query()` or interactive prompt cycle. A session file can hold
/// several, and **turn limits apply per run, not per session** — so a session showing 23
/// turns against `maxTurns: 20` is not a contradiction, it is two runs.
#[derive(Debug, Clone)]
pub struct RunSegment {
    /// 1-based index within the session.
    pub index: usize,
    /// The prompt that started it (first main-thread user prompt in the segment).
    pub prompt: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Main-thread assistant turns — the count a `maxTurns` setting is compared against.
    pub turns_main: usize,
    pub turns_sidechain: usize,
    pub usage: Usage,
    pub cost_usd: f64,
    pub outcome: Outcome,
    /// Terminal event that ended it, e.g. `max_turns_reached: hit the turn limit: 21 of 20`.
    pub outcome_detail: String,
    /// The limit it hit, as numbers: `(event subtype, value reached, configured cap)`.
    /// Prose is for humans; this is what a script should read.
    pub limit_hit: Option<(String, u64, u64)>,
    /// Transcript item range.
    pub first_item: usize,
    pub last_item: usize,
}

/// One sub-agent conversation, reconstructed from the transcript rather than from the
/// result record — so agents that never returned (still running, stopped early, truncated)
/// are still listed and readable. Complements [`SubagentResult`], which only exists for
/// agents that finished but carries the harness's own tool statistics.
#[derive(Debug, Clone)]
pub struct AgentThread {
    pub agent: AgentRef,
    /// Model its turns actually ran on.
    pub model: String,
    pub turns: usize,
    pub usage: Usage,
    pub cost_usd: f64,
    pub tool_calls: u64,
    /// Transcript item range this agent occupies (`transcript --from`/`--limit` reads it).
    pub first_item: usize,
    pub last_item: usize,
    pub start_ms: i64,
    pub end_ms: i64,
    /// Whether a result came back to the parent.
    pub completed: bool,
    /// How it ended, as far as the log shows — separates "hit a limit" and "ended cleanly"
    /// from "the log just stops", which `completed` alone conflates.
    pub outcome: Outcome,
}

impl AgentThread {
    /// True when this agent's *every* tool call was spawning another agent — it did no work
    /// of its own. This is the exact criterion behind the `Delegation loop` finding, exposed
    /// so the finding's count is reproducible (`ssa agents --spinning`) rather than a number
    /// you have to take on trust.
    pub fn is_spinning(&self, transcript: &[TItem]) -> bool {
        if self.agent.depth < 2 {
            return false;
        }
        let (mut all, mut delegating) = (0usize, 0usize);
        for it in transcript.iter().filter(|it| it.agent.as_ref().is_some_and(|a| a.id == self.agent.id)) {
            if let TKind::Assistant { tools, .. } = &it.kind {
                all += tools.len();
                delegating += tools.iter().filter(|t| t.name == "Agent" || t.name == "Task").count();
            }
        }
        all > 0 && all == delegating
    }

    pub fn duration_ms(&self) -> i64 {
        (self.end_ms - self.start_ms).max(0)
    }
}

/// One assistant turn on the timeline.
#[derive(Debug, Clone)]
pub struct TurnPoint {
    /// Session-wide sequence number — unique, and the key used to link a timeline row to
    /// its transcript item. Not what's displayed; see `label`.
    pub turn: usize,
    /// Displayed turn number, counted **per thread**: the main conversation and each
    /// sub-agent each start at 1, so a delegated turn never borrows the main count. Pair it
    /// with `agent` (or use [`TurnPoint::display_turn`]) to say which thread it belongs to.
    pub label: String,
    /// The sub-agent this turn belongs to, or `None` for the main thread.
    pub agent: Option<AgentRef>,
    pub model: String,
    pub usage: Usage,
    /// Epoch millis of this turn (0 if unknown).
    pub ts_ms: i64,
    /// Whether this is a sub-agent (sidechain) turn.
    pub is_sidechain: bool,
    /// Context tokens processed this turn (cache_read + cache_write + fresh input).
    pub context_size: u64,
    /// Change in context size vs the previous turn.
    pub delta: i64,
    /// Estimated tokens added to context since the previous turn (tool results + prompts).
    pub added_tokens: u64,
    pub cost: f64,
    pub is_error: bool,
    pub is_spike: bool,
    pub compaction_after: bool,
    /// Largest single thing added before this turn (spike cause).
    pub cause: String,
}

impl TurnPoint {
    /// Turn number qualified by its thread, for flat lists that mix both: `7` on the main
    /// thread, `Explore#a221ec ▸ 3` inside a sub-agent.
    pub fn display_turn(&self) -> String {
        match &self.agent {
            Some(a) => format!("{} ▸ {}", a.label(), self.label),
            None => self.label.clone(),
        }
    }
}

impl Usage {
    /// Fresh (non-cache-read) tokens the model must actually compute: input + cache-write
    /// + output. This is the throughput that drives subscription-window limits.
    pub fn fresh(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.output_tokens
    }

    /// Input tokens that count toward Anthropic's **ITPM** (input-tokens-per-minute) limiter:
    /// uncached input + cache *write*. `cache_read` is explicitly excluded — it does not count
    /// toward ITPM for current models (per Anthropic's rate-limit docs).
    pub fn rate_limit_input(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens
    }
}

/// A detected context-growth anomaly.
#[derive(Debug, Clone)]
pub struct Spike {
    /// Session-wide sequence number (links to `TurnPoint::turn`).
    pub turn: usize,
    /// Thread-qualified turn label, matching what `timeline` prints. The bare sequence
    /// number is meaningless to a reader who was told turns are numbered per thread.
    pub label: String,
    pub delta: i64,
    pub context_size: u64,
    pub cause: String,
}

/// A resolved transcript item (full text) for the bubble view.
#[derive(Debug, Clone)]
pub struct TItem {
    pub index: usize,
    /// Sub-agent this item was logged by, or `None` when it is main-thread conversation.
    /// Sub-agent records share the parent's `sessionId`, so without this they render as
    /// though the main agent had produced them.
    pub agent: Option<AgentRef>,
    /// Epoch millis (0 when the record carried no timestamp). Needed to tell a sequential
    /// chain from a parallel fan-out, and to line a message up against the timeline.
    pub ts_ms: i64,
    /// 1-based run this item belongs to (see [`RunSegment`]).
    pub run: usize,
    pub kind: TKind,
}

#[derive(Debug, Clone)]
pub enum TKind {
    User { text: String, is_prompt: bool },
    Assistant {
        /// Session-wide sequence number (links to `TurnPoint::turn`).
        turn: usize,
        /// Per-thread turn number for display (see `TurnPoint::label`).
        label: String,
        model: String,
        usage: Usage,
        thinking: String,
        text: String,
        tools: Vec<TTool>,
        is_error: bool,
        /// Why the model stopped. Empty = the response was never closed out, i.e. the run
        /// never finished writing — the response was cut off mid-generation.
        stop_reason: String,
    },
    Tool { tool: String, target: String, tokens: u64, content: String, is_error: bool },
    Compact { pre: u64, post: u64, trigger: String },
    /// A harness event (turn-limit hit, reminder injected, hook result, memory loaded).
    Event { subtype: String, detail: String, content: String, is_terminal: bool },
}

#[derive(Debug, Clone)]
pub struct TTool {
    pub name: String,
    pub target: String,
    /// For an `Agent` call: the **id** of the sub-agent it spawned. Lets the transcript
    /// collapse that agent's whole conversation to one row and open it on demand.
    pub spawned: Option<String>,
    pub input_full: String,
}

/// A detected inefficiency.
#[derive(Debug, Clone)]
pub struct Finding {
    pub severity: Severity,
    pub kind: String,
    pub detail: String,
    pub wasted_tokens_est: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    High,
}
impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::High => "HIGH",
        }
    }
}

/// Aggregated metrics for a session (or the whole dataset).
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub usage: Usage,
    pub cost_usd: f64,
    pub assistant_turns: u64,
    pub user_prompts: u64,
    pub tool_calls: u64,
    pub thinking_chars: u64,
    pub text_chars: u64,
    pub api_errors: u64,
    pub duration_ms: i64,
    /// Wall-clock time spent actively working (span minus idle gaps > IDLE_GAP_MS).
    pub active_ms: i64,
    /// Number of idle gaps (pauses longer than IDLE_GAP_MS between turns).
    pub idle_gaps: u64,
    /// Time not spent working: the sum of every gap longer than [`WORK_GAP_MS`].
    pub idle_ms: i64,
    /// The single longest pause between turns. Reported unconditionally, so a session is
    /// never described as continuous while hiding a pause below whatever threshold applies.
    pub longest_gap_ms: i64,
    /// Longest single continuous burst (no idle gap), in ms, and its fresh tokens.
    pub longest_burst_ms: i64,
    pub longest_burst_fresh: u64,
    /// Max sub-agents running at the same instant (interval overlap of their runtimes).
    pub max_concurrent_subagents: u64,
    pub context_peak: u64,
    /// Usage split by main conversation thread vs sub-agent (sidechain) turns.
    pub usage_main: Usage,
    pub usage_sidechain: Usage,
    /// Assistant turns on the main thread vs sidechains.
    pub turns_main: u64,
    pub turns_sidechain: u64,
    /// Whether any sidechain (sub-agent) turns were present in the input.
    pub has_sidechain_detail: bool,
    pub models: HashMap<String, Usage>,
    /// Assistant turns per model (all threads).
    pub model_turns: HashMap<String, u64>,
    /// Assistant turns per model on the MAIN thread only (excludes sub-agents), so the
    /// session's headline model reflects the driving agent, not whichever model the
    /// sub-agents happened to use.
    pub model_turns_main: HashMap<String, u64>,
    pub tools: Vec<ToolStat>,
    pub subagents: Vec<SubagentResult>,
    pub compactions: Vec<CompactEvent>,
    /// Run-ending harness events seen in scope, as `(subtype, detail)` — e.g.
    /// `("max_turns_reached", "hit the turn limit: 21 of 20 turns")`.
    pub terminal_events: Vec<(String, String)>,
    /// Task prompts handed to sub-agents (counted apart from the human's `user_prompts`).
    pub subagent_prompts: u64,
    /// Tools the harness made reachable on demand (the deferred registry), union across
    /// threads. Empty when the log records no roster — which is not the same as "no tools".
    pub deferred_tools: std::collections::BTreeSet<String>,
    /// Tools an agent searched for and the harness could not provide, with the number of
    /// times it was asked for. This is the direct evidence for "the agent needed X and X
    /// did not exist" — otherwise only inferable from what happens not to be called.
    pub tools_unavailable: std::collections::BTreeMap<String, u64>,
    pub findings: Vec<Finding>,
}

impl Metrics {
    pub fn cache_hit_rate(&self) -> f64 {
        let denom = self.usage.billed_input() as f64;
        if denom == 0.0 { 0.0 } else { self.usage.cache_read_input_tokens as f64 / denom }
    }
    pub fn cache_churn(&self) -> f64 {
        let reads = self.usage.cache_read_input_tokens as f64;
        if reads == 0.0 {
            if self.usage.cache_creation_input_tokens > 0 { f64::INFINITY } else { 0.0 }
        } else {
            self.usage.cache_creation_input_tokens as f64 / reads
        }
    }
    pub fn subagent_tokens(&self) -> u64 {
        self.subagents.iter().map(|s| s.total_tokens).sum()
    }

    /// The session's headline model: the model that ran the most **main-thread** turns
    /// (falling back to all turns when there is no main thread, e.g. a pure sub-agent log).
    /// This avoids reporting a session as "sonnet" just because its sub-agents used sonnet
    /// while the driving agent was opus.
    pub fn dominant_model(&self) -> String {
        let src = if self.model_turns_main.is_empty() { &self.model_turns } else { &self.model_turns_main };
        src.iter()
            .max_by_key(|(_, n)| **n)
            .map(|(m, _)| m.clone())
            .unwrap_or_default()
    }
    pub fn duration_human(&self) -> String {
        let mut s = self.duration_ms / 1000;
        if s <= 0 {
            return "-".into();
        }
        let h = s / 3600;
        s %= 3600;
        let m = s / 60;
        s %= 60;
        if h > 0 { format!("{}h {}m", h, m) } else if m > 0 { format!("{}m {}s", m, s) } else { format!("{}s", s) }
    }
}

/// One session's identity plus all its computed views.
#[derive(Debug, Clone)]
pub struct SessionReport {
    pub session_id: String,
    pub title: String,
    pub source: String,
    pub cwd: String,
    pub entrypoint: String,
    pub service_tier: String,
    pub metrics: Metrics,
    pub transcript: Vec<TItem>,
    pub timeline: Vec<TurnPoint>,
    pub cache_attr: Vec<CacheContrib>,
    pub spikes: Vec<Spike>,
    /// Every sub-agent conversation in this session, finished or not, in the order they
    /// first appear.
    pub threads: Vec<AgentThread>,
    /// The session's runs (SDK queries / prompt cycles), in order.
    pub runs: Vec<RunSegment>,
    /// Findings that only make sense per session (run outcomes, loops). Also folded into
    /// the global metrics so the unscoped `issues` view is not missing them.
    pub control_findings: Vec<Finding>,
}

/// Full analysis result.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub provider: String,
    pub global: Metrics,
    /// Cache-read decomposition across all sessions.
    pub global_cache_attr: Vec<CacheContrib>,
    pub sessions: Vec<SessionReport>,
    /// Session ids present in the input but carrying no analysable turns (a bare slash
    /// command, a truncated file). Reported rather than dropped, so a session count can be
    /// read as a completeness statement.
    pub skipped_sessions: Vec<String>,
    pub parse_errors: u64,
}

/// Aggregate of all sessions sharing a working directory / project (surfaced when
/// analyzing a multi-project `.claude` tree).
#[derive(Debug, Clone)]
pub struct ProjectReport {
    pub project: String,
    pub sessions: usize,
    pub cost_usd: f64,
    pub usage: Usage,
    pub assistant_turns: u64,
    pub tool_calls: u64,
    pub subagent_tokens: u64,
    pub compactions: usize,
}

impl Analysis {
    /// Group sessions by project (cwd, else source label). Sorted by cost, high→low.
    pub fn projects(&self) -> Vec<ProjectReport> {
        let mut by: HashMap<String, ProjectReport> = HashMap::new();
        for s in &self.sessions {
            let key = if !s.cwd.is_empty() { s.cwd.clone() } else { s.source.clone() };
            let e = by.entry(key.clone()).or_insert_with(|| ProjectReport {
                project: key,
                sessions: 0,
                cost_usd: 0.0,
                usage: Usage::default(),
                assistant_turns: 0,
                tool_calls: 0,
                subagent_tokens: 0,
                compactions: 0,
            });
            e.sessions += 1;
            e.cost_usd += s.metrics.cost_usd;
            e.usage.add(&s.metrics.usage);
            e.assistant_turns += s.metrics.assistant_turns;
            e.tool_calls += s.metrics.tool_calls;
            e.subagent_tokens += s.metrics.subagent_tokens();
            e.compactions += s.metrics.compactions.len();
        }
        let mut v: Vec<ProjectReport> = by.into_values().collect();
        v.sort_by(|a, b| b.cost_usd.total_cmp(&a.cost_usd));
        v
    }

    /// All assistant turns across sessions, time-ordered — the basis for rate analysis.
    pub fn all_turns(&self) -> Vec<&TurnPoint> {
        let mut v: Vec<&TurnPoint> = self.sessions.iter().flat_map(|s| s.timeline.iter()).collect();
        v.sort_by_key(|t| t.ts_ms);
        v
    }
}

/// Default rolling window for subscription-pressure analysis (Claude Max ≈ 5h window).
pub const RATE_WINDOW_HOURS: f64 = 5.0;

/// One time bucket of consumption (for charting throughput over wall-clock time).
#[derive(Debug, Clone)]
pub struct RateBucket {
    pub start_ms: i64,
    pub fresh: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost: f64,
    pub turns: u64,
}

/// Throughput analysis: consumption *flows* (per-hour) and the peak rolling window — the
/// figures that actually predict subscription-limit exhaustion.
#[derive(Debug, Clone)]
pub struct RateReport {
    pub turns: u64,
    pub span_ms: i64,
    pub total_fresh: u64,
    pub total_cache_write: u64,
    pub total_output: u64,
    pub total_cost: f64,
    pub fresh_per_h: f64,
    pub cache_write_per_h: f64,
    pub output_per_h: f64,
    pub cost_per_h: f64,
    pub window_hours: f64,
    pub peak_window_fresh: u64,
    pub peak_window_cost: f64,
    pub peak_window_start_ms: i64,
    /// Continuity-aware peak: max fresh tokens in any window that does NOT span an idle
    /// gap. This is the honest "single continuous burst" figure (the naive peak_window can
    /// smear across multi-day gaps and over-count).
    pub peak_burst_fresh: u64,
    pub peak_burst_cost: f64,
    /// Active wall-clock (span minus idle gaps) and gap count.
    pub active_ms: i64,
    pub idle_gaps: u64,
    pub idle_ms: i64,
    pub longest_gap_ms: i64,
    /// Instantaneous burst: max fresh tokens and turns within any 60-second window.
    pub peak_fresh_per_min: u64,
    pub peak_turns_per_min: u64,
    /// Peak per-minute figures matching Anthropic's three limiters (busiest 60s window):
    /// RPM = API requests/min (every assistant turn incl. parallel sub-agents),
    /// ITPM = input tokens/min (uncached input + cache-write; cache-read excluded),
    /// OTPM = output tokens/min.
    pub peak_rpm: u64,
    pub peak_itpm: u64,
    pub peak_otpm: u64,
    pub bucket_ms: i64,
    pub buckets: Vec<RateBucket>,
}

/// Build a rate report from time-ordered turns. `window_hours` sizes the rolling window.
pub fn rate_report(turns: &[&TurnPoint], window_hours: f64) -> RateReport {
    let mut r = RateReport {
        turns: turns.len() as u64,
        window_hours,
        span_ms: 0,
        total_fresh: 0,
        total_cache_write: 0,
        total_output: 0,
        total_cost: 0.0,
        fresh_per_h: 0.0,
        cache_write_per_h: 0.0,
        output_per_h: 0.0,
        cost_per_h: 0.0,
        peak_window_fresh: 0,
        peak_window_cost: 0.0,
        peak_window_start_ms: 0,
        peak_burst_fresh: 0,
        peak_burst_cost: 0.0,
        active_ms: 0,
        idle_gaps: 0,
        idle_ms: 0,
        longest_gap_ms: 0,
        peak_fresh_per_min: 0,
        peak_turns_per_min: 0,
        peak_rpm: 0,
        peak_itpm: 0,
        peak_otpm: 0,
        bucket_ms: 0,
        buckets: Vec::new(),
    };
    // Only timestamped turns participate.
    let mut ts: Vec<&TurnPoint> = turns.iter().copied().filter(|t| t.ts_ms > 0).collect();
    if ts.is_empty() {
        return r;
    }
    ts.sort_by_key(|t| t.ts_ms);

    for t in &ts {
        r.total_fresh += t.usage.fresh();
        r.total_cache_write += t.usage.cache_creation_input_tokens;
        r.total_output += t.usage.output_tokens;
        r.total_cost += t.cost;
    }
    let first = ts.first().unwrap().ts_ms;
    let last = ts.last().unwrap().ts_ms;
    r.span_ms = (last - first).max(0);
    let span_h = (r.span_ms as f64 / 3_600_000.0).max(1.0 / 60.0); // ≥1 minute
    r.fresh_per_h = r.total_fresh as f64 / span_h;
    r.cache_write_per_h = r.total_cache_write as f64 / span_h;
    r.output_per_h = r.total_output as f64 / span_h;
    r.cost_per_h = r.total_cost / span_h;

    // Peak rolling window (max fresh tokens in any window_hours span).
    let win_ms = (window_hours * 3_600_000.0) as i64;
    let mut left = 0usize;
    let mut run_fresh = 0u64;
    let mut run_cost = 0.0f64;
    for right in 0..ts.len() {
        run_fresh += ts[right].usage.fresh();
        run_cost += ts[right].cost;
        while ts[right].ts_ms - ts[left].ts_ms > win_ms {
            run_fresh -= ts[left].usage.fresh();
            run_cost -= ts[left].cost;
            left += 1;
        }
        if run_fresh > r.peak_window_fresh {
            r.peak_window_fresh = run_fresh;
            r.peak_window_cost = run_cost;
            r.peak_window_start_ms = ts[left].ts_ms;
        }
    }

    // Continuity-aware peak: a window that resets whenever there is an idle gap, so it
    // reflects a single unbroken burst rather than a smear across multi-day pauses.
    let mut bl = 0usize;
    let mut b_fresh = 0u64;
    let mut b_cost = 0.0f64;
    for right in 0..ts.len() {
        if right > 0 && ts[right].ts_ms - ts[right - 1].ts_ms > IDLE_GAP_MS {
            bl = right; // idle gap → start a new burst
            b_fresh = 0;
            b_cost = 0.0;
        }
        b_fresh += ts[right].usage.fresh();
        b_cost += ts[right].cost;
        while ts[right].ts_ms - ts[bl].ts_ms > win_ms {
            b_fresh -= ts[bl].usage.fresh();
            b_cost -= ts[bl].cost;
            bl += 1;
        }
        if b_fresh > r.peak_burst_fresh {
            r.peak_burst_fresh = b_fresh;
            r.peak_burst_cost = b_cost;
        }
    }
    // Active time + idle gaps (from the sorted turns).
    for i in 1..ts.len() {
        let d = ts[i].ts_ms - ts[i - 1].ts_ms;
        r.longest_gap_ms = r.longest_gap_ms.max(d);
        if d > IDLE_GAP_MS {
            r.idle_gaps += 1;
        }
        // Active time uses the *work* threshold: a 12-minute pause is not the agent
        // working, even though it is too short to split a rate-limit window.
        if d > WORK_GAP_MS {
            r.idle_ms += d;
        } else {
            r.active_ms += d;
        }
    }

    // Instantaneous burst: 60-second sliding window. Peak fresh + turns/min, plus the three
    // figures matching Anthropic's per-minute limiters (RPM / ITPM / OTPM).
    let mut ml = 0usize;
    let mut min_fresh = 0u64;
    let mut min_in = 0u64; // ITPM-relevant input (excludes cache-read)
    let mut min_out = 0u64;
    for right in 0..ts.len() {
        min_fresh += ts[right].usage.fresh();
        min_in += ts[right].usage.rate_limit_input();
        min_out += ts[right].usage.output_tokens;
        while ts[right].ts_ms - ts[ml].ts_ms > 60_000 {
            min_fresh -= ts[ml].usage.fresh();
            min_in -= ts[ml].usage.rate_limit_input();
            min_out -= ts[ml].usage.output_tokens;
            ml += 1;
        }
        let reqs = (right - ml + 1) as u64;
        r.peak_fresh_per_min = r.peak_fresh_per_min.max(min_fresh);
        r.peak_turns_per_min = r.peak_turns_per_min.max(reqs);
        r.peak_rpm = r.peak_rpm.max(reqs);
        r.peak_itpm = r.peak_itpm.max(min_in);
        r.peak_otpm = r.peak_otpm.max(min_out);
    }

    // Time buckets for charting (aim for ~48 buckets, ≥1 minute each).
    let bucket_ms = ((r.span_ms / 48).max(60_000)) as i64;
    r.bucket_ms = bucket_ms;
    let nbuckets = ((r.span_ms / bucket_ms) as usize) + 1;
    let mut buckets: Vec<RateBucket> = (0..nbuckets)
        .map(|i| RateBucket { start_ms: first + i as i64 * bucket_ms, fresh: 0, cache_write: 0, output: 0, cost: 0.0, turns: 0 })
        .collect();
    for t in &ts {
        let idx = ((t.ts_ms - first) / bucket_ms) as usize;
        if let Some(b) = buckets.get_mut(idx.min(nbuckets - 1)) {
            b.fresh += t.usage.fresh();
            b.cache_write += t.usage.cache_creation_input_tokens;
            b.output += t.usage.output_tokens;
            b.cost += t.cost;
            b.turns += 1;
        }
    }
    r.buckets = buckets;
    r
}

// ----------------------------------------------------------------------- driver

pub fn analyze(ds: &Dataset) -> Analysis {
    // Group item indices by session, preserving first-seen order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, it) in ds.items.iter().enumerate() {
        let g = groups.entry(it.session_id.clone()).or_default();
        if g.is_empty() {
            order.push(it.session_id.clone());
        }
        g.push(i);
    }

    // Resolve tool_use ids to (name, target) globally, so tool results whose tool_use
    // lives in another file (sub-agents, resumed sessions) still attribute correctly.
    // Also map each sub-agent id to the ACTUAL model its sidechain turns ran on (Explore
    // often runs on a cheaper model than the parent), so we don't mis-report it.
    let mut gindex: HashMap<String, (String, String)> = HashMap::new();
    let mut agent_models: HashMap<String, String> = HashMap::new();
    for it in &ds.items {
        if let ItemKind::Assistant(a) = &it.kind {
            for t in &a.tools {
                gindex.insert(t.id.clone(), (t.name.clone(), t.target.clone().unwrap_or_default()));
            }
            if it.origin.is_sidechain && !it.origin.agent_id.is_empty() && a.model != "<synthetic>" {
                agent_models.entry(it.origin.agent_id.clone()).or_insert_with(|| a.model.clone());
            }
        }
    }
    let agents = agent_index(ds);
    // Agent `tool_use` id -> the sub-agent it launched. Sidecars cover agents that never
    // returned a result; the result records cover logs exported without sidecars.
    let mut spawned: HashMap<String, String> = ds
        .agent_meta
        .iter()
        .filter(|(_, m)| !m.tool_use_id.is_empty())
        .map(|(id, m)| (m.tool_use_id.clone(), id.clone()))
        .collect();
    for it in &ds.items {
        if let ItemKind::Subagent(s) = &it.kind
            && !s.tool_use_id.is_empty()
            && !s.agent_id.is_empty()
        {
            spawned.insert(s.tool_use_id.clone(), s.agent_id.clone());
        }
    }

    // Build each session's views in parallel — they are independent, and the transcript
    // construction (string clones, wrapping data) dominates load time for big trees.
    let sessions: Vec<Result<SessionReport, String>> = order
        .par_iter()
        .filter_map(|sid| {
            let items: Vec<&Item> = groups[sid].iter().map(|&i| &ds.items[i]).collect();
            // Sub-agent logs arrive as separate files appended after the main thread; put
            // each one back inside the turn that spawned it before building any view.
            let items = nest_sidechains(&items, &spawned);
            let built = build(&items, &gindex, &agent_models, &agents, &spawned, true);
            if built.metrics.assistant_turns == 0 && built.metrics.tool_calls == 0 && built.metrics.subagents.is_empty() {
                // Nothing to analyse (a bare `/login`, a truncated file). Don't drop it
                // silently — a vanished session makes every count look like a total.
                return Some(Err(sid.clone()));
            }
            Some(Ok(SessionReport {
                session_id: sid.clone(),
                title: ds.titles.get(sid).cloned().unwrap_or_else(|| "(untitled)".into()),
                source: ds.sources.get(sid).cloned().unwrap_or_default(),
                cwd: ds.cwds.get(sid).cloned().unwrap_or_default(),
                entrypoint: ds.entrypoints.get(sid).cloned().unwrap_or_default(),
                service_tier: ds.service_tiers.get(sid).cloned().unwrap_or_default(),
                metrics: built.metrics,
                transcript: built.transcript,
                timeline: built.timeline,
                cache_attr: built.cache_attr,
                spikes: built.spikes,
                threads: built.threads,
                runs: built.runs,
                control_findings: built.control,
            }))
        })
        .collect();
    let (mut sessions, skipped): (Vec<SessionReport>, Vec<String>) = {
        let mut ok = Vec::new();
        let mut skipped = Vec::new();
        for r in sessions {
            match r {
                Ok(s) => ok.push(s),
                Err(id) => skipped.push(id),
            }
        }
        (ok, skipped)
    };
    sessions.sort_by(|a, b| b.metrics.cost_usd.total_cmp(&a.metrics.cost_usd));

    // Global metrics only need aggregates + cache attribution, so skip the (unused) global
    // transcript/timeline construction — a big saving over the whole dataset.
    // Nest here too, so cache residency is measured in the same order the per-session
    // views use — otherwise the same file reports a different share scoped vs unscoped.
    // Only the sessions actually reported: otherwise "Sessions analyzed: 1" sits next to
    // totals that silently include a session excluded from every other view.
    let dropped: HashSet<&str> = skipped.iter().map(String::as_str).collect();
    let kept: Vec<&Item> = ds.items.iter().filter(|it| !dropped.contains(it.session_id.as_str())).collect();
    let all: Vec<&Item> = nest_sidechains(&kept, &spawned);
    let mut global_built = build(&all, &gindex, &agent_models, &agents, &spawned, false);
    // Control-flow findings are per-session by nature; fold them into the global view so
    // `issues` without --session still reports how runs ended and where work looped.
    let multi = sessions.len() > 1;
    for s in &sessions {
        global_built.metrics.findings.extend(s.control_findings.iter().cloned().map(|mut f| {
            if multi {
                f.detail = format!("[{}] {}", s.session_id.chars().take(8).collect::<String>(), f.detail);
            }
            f
        }));
    }
    sort_findings(&mut global_built.metrics.findings);

    Analysis {
        provider: ds.provider.clone(),
        global: global_built.metrics,
        global_cache_attr: global_built.cache_attr,
        sessions,
        skipped_sessions: skipped,
        parse_errors: ds.parse_errors,
    }
}

/// Describe every sub-agent seen in the dataset: its type and how deeply it is nested
/// (agents spawn agents). Keyed by `agentId`.
fn agent_index(ds: &Dataset) -> HashMap<String, AgentRef> {
    // agent id -> (type, id of the agent that spawned it; empty = main thread)
    let mut raw: HashMap<&str, (&str, &str)> = HashMap::new();
    for it in &ds.items {
        if let ItemKind::Subagent(s) = &it.kind
            && !s.agent_id.is_empty()
        {
            raw.insert(&s.agent_id, (&s.agent_type, &it.origin.agent_id));
        }
    }
    // Sidecars name the type of agents that never returned a result (still running, stopped
    // truncated export) — the only record of what they were.
    for (id, meta) in &ds.agent_meta {
        raw.entry(id.as_str()).or_insert((meta.agent_type.as_str(), ""));
    }
    // Last resort: an agent with sidechain records but no metadata at all still gets a
    // thread of its own rather than being folded into the main conversation.
    for it in &ds.items {
        if it.origin.is_sidechain && !it.origin.agent_id.is_empty() {
            raw.entry(&it.origin.agent_id).or_insert(("agent", ""));
        }
    }
    // A sidecar's parent is whichever thread issued its spawning tool call.
    let spawner: HashMap<&str, &str> = ds
        .items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::Assistant(a) => Some(a.tools.iter().map(move |t| (t.id.as_str(), it.origin.agent_id.as_str()))),
            _ => None,
        })
        .flatten()
        .collect();
    for (id, meta) in &ds.agent_meta {
        if let Some(&parent) = spawner.get(meta.tool_use_id.as_str())
            && let Some(e) = raw.get_mut(id.as_str())
            && e.1.is_empty()
        {
            e.1 = parent;
        }
    }

    raw.iter()
        .map(|(id, (ty, _))| {
            (
                id.to_string(),
                AgentRef {
                    id: id.to_string(),
                    agent_type: ty.to_string(),
                    description: ds.agent_meta.get(*id).map(|m| m.description.clone()).unwrap_or_default(),
                    depth: depth_of(&raw, id),
                    parent_id: raw.get(*id).map(|(_, p)| *p).filter(|p| !p.is_empty()).map(str::to_string),
                },
            )
        })
        .collect()
}

/// How deeply an agent is nested: 1 when the main thread spawned it, +1 per agent above it.
fn depth_of<'a>(raw: &HashMap<&'a str, (&'a str, &'a str)>, start: &'a str) -> usize {
    let mut id: &str = start;
    let mut d = 1usize;
    // Walk up to the main thread; the visit cap makes a malformed cycle terminate.
    for _ in 0..raw.len() {
        match raw.get(id) {
            Some((_, parent)) if !parent.is_empty() => {
                id = parent;
                d += 1;
            }
            _ => break,
        }
    }
    d
}

/// Reorder one session's events so each sub-agent's sidechain records sit **inside** the
/// parent turn that spawned them, immediately before the tool result they produced.
///
/// Claude Code stores every sub-agent in its own `.jsonl`, so a naive read appends whole
/// agent transcripts after the main thread — which is why they looked like a continuation
/// of the main conversation. Agents that spawned agents nest recursively. Records whose
/// spawning Agent call is missing from the log are appended at the end (first-seen order)
/// so nothing is dropped.
fn nest_sidechains<'a>(items: &[&'a Item], spawned: &'a HashMap<String, String>) -> Vec<&'a Item> {
    let mut main: Vec<&'a Item> = Vec::new();
    let mut by_agent: HashMap<&'a str, Vec<&'a Item>> = HashMap::new();
    let mut agent_order: Vec<&'a str> = Vec::new();
    for it in items {
        if it.origin.is_sidechain && !it.origin.agent_id.is_empty() {
            let chain = by_agent.entry(it.origin.agent_id.as_str()).or_insert_with(|| {
                agent_order.push(it.origin.agent_id.as_str());
                Vec::new()
            });
            chain.push(it);
        } else {
            main.push(it);
        }
    }
    if by_agent.is_empty() {
        return main;
    }

    // Tool calls that got a result back. An Agent call with a result is anchored on that
    // result; one without (still running, stopped early, log truncated) is anchored on the
    // turn that made the call, which is the only position left.
    let answered: HashSet<&'a str> = items
        .iter()
        .filter_map(|it| match &it.kind {
            ItemKind::ToolResult(r) => Some(r.tool_use_id.as_str()),
            _ => None,
        })
        .collect();

    let mut ctx = NestCtx { by_agent, spawned, answered, placed: HashSet::new() };
    let mut out: Vec<&'a Item> = Vec::with_capacity(items.len());
    emit_chain(&main, &mut ctx, &mut out);
    // Anything still unplaced (no spawning call anywhere in the log) goes at the end rather
    // than being dropped.
    for aid in agent_order {
        if ctx.placed.insert(aid)
            && let Some(chain) = ctx.by_agent.get(aid).cloned()
        {
            emit_chain(&chain, &mut ctx, &mut out);
        }
    }
    out
}

struct NestCtx<'a> {
    by_agent: HashMap<&'a str, Vec<&'a Item>>,
    spawned: &'a HashMap<String, String>,
    answered: HashSet<&'a str>,
    placed: HashSet<&'a str>,
}

impl<'a> NestCtx<'a> {
    /// Splice in the sub-agent launched by `tool_use_id`, if it hasn't been placed yet.
    /// `placed` also guards against a malformed log that loops back on itself.
    fn splice(&mut self, tool_use_id: &str, out: &mut Vec<&'a Item>) {
        let Some(aid) = self.spawned.get(tool_use_id) else { return };
        let Some((aid, chain)) = self.by_agent.get_key_value(aid.as_str()).map(|(k, v)| (*k, v.clone())) else {
            return;
        };
        if self.placed.insert(aid) {
            emit_chain(&chain, self, out);
        }
    }
}

/// Emit one thread, splicing each sub-agent it spawns into the parent turn that spawned it.
fn emit_chain<'a>(chain: &[&'a Item], ctx: &mut NestCtx<'a>, out: &mut Vec<&'a Item>) {
    for it in chain {
        match &it.kind {
            // A completed sub-agent belongs just before the result it produced.
            ItemKind::ToolResult(r) => ctx.splice(&r.tool_use_id, out),
            // One that never returned belongs right after the call that launched it.
            ItemKind::Assistant(a) => {
                out.push(it);
                for t in &a.tools {
                    if !ctx.answered.contains(t.id.as_str()) {
                        ctx.splice(&t.id, out);
                    }
                }
                continue;
            }
            _ => {}
        }
        out.push(it);
    }
}

struct Built {
    metrics: Metrics,
    /// Findings that can only be derived per session (see `detect_control_flow`).
    control: Vec<Finding>,
    transcript: Vec<TItem>,
    threads: Vec<AgentThread>,
    runs: Vec<RunSegment>,
    timeline: Vec<TurnPoint>,
    cache_attr: Vec<CacheContrib>,
    spikes: Vec<Spike>,
}

/// Per-thread (main conversation or one sub-agent) context-growth state. Each thread has
/// its own context window, so deltas and "what grew it" are only meaningful within one.
#[derive(Default)]
struct ThreadState {
    /// Context size at this thread's previous assistant turn.
    prev_ctx: Option<u64>,
    /// Tokens added to this thread since its last assistant turn.
    pending_added: u64,
    /// Largest single addition since then: (description, tokens).
    pending_cause: (String, u64),
}

/// A block of content currently resident in the cached context.
struct Resident {
    tokens: u64,
    key: (String, String), // (tool, target)
    entry_turn: usize,
}

/// Core builder: one ordered pass over `items` produces every view. Resident context is
/// evicted on compaction and on session change (so a global multi-session pass stays
/// correct); timeline/spikes are only meaningful for a single-session pass. When `full` is
/// false, the transcript/timeline (and their string clones) are skipped — used for the
/// global aggregate pass, which only needs metrics + cache attribution.
fn build(
    items: &[&Item],
    tool_index_seed: &HashMap<String, (String, String)>,
    agent_models: &HashMap<String, String>,
    agents: &HashMap<String, AgentRef>,
    spawned: &HashMap<String, String>,
    full: bool,
) -> Built {
    // tool_use id -> label of the agent that call spawned.
    let spawned_agents: HashMap<&str, String> =
        spawned.iter().map(|(tid, aid)| (tid.as_str(), aid.clone())).collect();
    let mut m = Metrics::default();
    let mut transcript: Vec<TItem> = Vec::new();
    let mut timeline: Vec<TurnPoint> = Vec::new();

    // Displayed turn numbers are counted per thread: the main conversation has its own
    // 1..N, and each sub-agent restarts at 1. (`turns_seen` stays session-wide — it keys
    // the timeline↔transcript link and measures cache residency.)
    let mut main_turns = 0usize;
    let mut agent_turns: HashMap<String, usize> = HashMap::new();

    let mut tstats: HashMap<String, ToolStat> = HashMap::new();
    // id -> (name, target); seeded globally so cross-file references resolve.
    let mut tool_index: HashMap<String, (String, String)> = tool_index_seed.clone();

    // Cache-attribution state.
    let mut resident: Vec<Resident> = Vec::new();
    let mut contribs: HashMap<(String, String), CacheContrib> = HashMap::new();
    let mut turns_seen = 0usize;
    let mut read_price_num = 0.0f64; // Σ price.cache_read × turn.cache_read
    let mut read_price_den = 0.0f64;

    // Timeline / spike state, tracked **per thread**: the main conversation and each
    // sub-agent run in their own context window, so comparing a sub-agent's context size
    // against the main thread's would invent growth that never happened.
    let mut threads: HashMap<String, ThreadState> = HashMap::new();
    let mut cur_session = String::new();
    let mut cur_model = String::new(); // model of the most recent assistant turn

    let mut ts_min = i64::MAX;
    let mut ts_max = i64::MIN;
    let mut read_repeat: HashMap<String, u64> = HashMap::new();

    // Run segmentation. A run starts on an explicit `queue-operation` boundary, or — when
    // the harness logs none — on a main-thread prompt that follows at least one assistant
    // turn (so a prompt's cwd preamble doesn't split it in two).
    let mut runs: Vec<RunSegment> = Vec::new();
    let mut saw_explicit_run = false;
    let mut turns_this_run = 0usize;

    let evict_all = |resident: &mut Vec<Resident>, contribs: &mut HashMap<(String, String), CacheContrib>, exit_turn: usize| {
        for b in resident.drain(..) {
            let residency = exit_turn.saturating_sub(b.entry_turn) as u64;
            let c = contribs.entry(b.key.clone()).or_insert_with(|| CacheContrib {
                tool: b.key.0.clone(),
                target: b.key.1.clone(),
                tokens: 0,
                entries: 0,
                residency_turns: 0,
                contribution: 0,
                share: 0.0,
                amplified_cost: 0.0,
                is_baseline: false,
            });
            c.tokens += b.tokens;
            c.entries += 1;
            c.residency_turns += residency;
            c.contribution += b.tokens * residency;
        }
    };

    // Assistant records are already coalesced by requestId at load time
    // (`ClaudeCodeProvider::finalize`), so each turn appears once here with all its blocks.
    for it in items {
        // Session boundary: residency does not cross sessions.
        if it.session_id != cur_session {
            if !cur_session.is_empty() {
                evict_all(&mut resident, &mut contribs, turns_seen);
            }
            cur_session = it.session_id.clone();
            turns_seen = 0;
            main_turns = 0;
            agent_turns.clear();
            threads.clear();
            runs.clear();
            saw_explicit_run = false;
            turns_this_run = 0;
        }
        if it.ts_ms > 0 {
            ts_min = ts_min.min(it.ts_ms);
            ts_max = ts_max.max(it.ts_ms);
        }
        let idx = transcript.len();

        // ---- run boundaries (main thread only; sub-agents live inside a run) ----
        if !it.origin.is_sidechain {
            let explicit = matches!(&it.kind, ItemKind::RunStart);
            if explicit {
                saw_explicit_run = true;
            }
            let inferred = !saw_explicit_run
                && turns_this_run > 0
                && matches!(&it.kind, ItemKind::User(u) if u.is_prompt);
            if explicit || inferred || runs.is_empty() {
                if let Some(prev) = runs.last_mut() {
                    prev.last_item = idx.saturating_sub(1);
                }
                runs.push(RunSegment {
                    index: runs.len() + 1,
                    prompt: String::new(),
                    start_ms: it.ts_ms,
                    end_ms: it.ts_ms,
                    turns_main: 0,
                    turns_sidechain: 0,
                    usage: Usage::default(),
                    cost_usd: 0.0,
                    // Assume the worst until a clean stop is seen: a run whose last turn
                    // never wrote a `stop_reason` really was cut off.
                    outcome: Outcome::Truncated,
                    outcome_detail: String::new(),
                    limit_hit: None,
                    first_item: idx,
                    last_item: idx,
                });
                turns_this_run = 0;
                // A new run restarts the context window. Carrying the previous run's size
                // forward manufactures a huge fake delta at the boundary, which then gets
                // reported as a "context spike" that never happened.
                if let Some(th) = threads.get_mut("") {
                    th.prev_ctx = None;
                    th.pending_added = 0;
                    th.pending_cause = (String::new(), 0);
                }
            }
        }
        let run_no = runs.len().max(1);
        if let Some(run) = runs.last_mut() {
            if it.ts_ms > 0 {
                if run.start_ms == 0 {
                    run.start_ms = it.ts_ms;
                }
                run.end_ms = run.end_ms.max(it.ts_ms);
            }
            run.last_item = idx;
            match &it.kind {
                // A prompt arrives as several blocks (a cwd preamble, a resume nudge, the
                // real request). Label the run with the most substantive one.
                ItemKind::User(u)
                    if u.is_prompt && !it.origin.is_sidechain && u.text.len() > run.prompt.len() =>
                {
                    run.prompt = u.text.clone();
                }
                ItemKind::Assistant(a) => {
                    run.usage.add(&a.usage);
                    run.cost_usd += price_for(&a.model).cost(&a.usage);
                    if it.origin.is_sidechain {
                        run.turns_sidechain += 1;
                    } else {
                        run.turns_main += 1;
                        turns_this_run += 1;
                        run.outcome = Outcome::from_last_turn(&a.stop_reason, a.is_error);
                    run.outcome_detail = match run.outcome {
                        // Say *which* turn: with sub-agents nested inline, the run's last
                        // main-thread turn is often far from the log's last record.
                        // `main_turns` is bumped by the Assistant arm *after* this block, so
                        // it still holds the previous turn here. Use the number this turn
                        // will be given, or the label disagrees with `trace`/`transcript`.
                        Outcome::Truncated if a.stop_reason.is_empty() => {
                            format!("main-thread turn {} has no stop_reason — it was still being generated", main_turns + 1)
                        }
                        Outcome::Truncated => {
                            format!("main-thread turn {} stopped on `{}` and no result followed", main_turns + 1, a.stop_reason)
                        }
                        Outcome::Errored => format!("main-thread turn {} returned an API error", main_turns + 1),
                        _ => String::new(),
                    };
                    }
                }
                // A limit event is the last word on how the run ended.
                ItemKind::Event(e) if e.is_terminal => {
                    run.outcome = Outcome::LimitHit;
                    run.outcome_detail = format!("{}: {}", e.subtype, e.detail);
                    run.limit_hit = e.limit.map(|(used, cap)| (e.subtype.clone(), used, cap));
                }
                _ => {}
            }
        }
        // Which thread logged this event — attached to every transcript item so a
        // sub-agent's messages are never rendered as the main agent's.
        let agent: Option<AgentRef> = if it.origin.is_sidechain {
            Some(agents.get(&it.origin.agent_id).cloned().unwrap_or_else(|| AgentRef {
                id: it.origin.agent_id.clone(),
                agent_type: "agent".into(),
                description: String::new(),
                depth: 1,
                parent_id: None,
            }))
        } else {
            None
        };

        match &it.kind {
            ItemKind::Assistant(a) => {
                turns_seen += 1;
                cur_model = a.model.clone();
                m.assistant_turns += 1;
                m.usage.add(&a.usage);
                m.thinking_chars += a.thinking.len() as u64;
                m.text_chars += a.text.len() as u64;
                m.models.entry(a.model.clone()).or_default().add(&a.usage);
                *m.model_turns.entry(a.model.clone()).or_default() += 1;
                let label = match &agent {
                    Some(ag) => {
                        let n = agent_turns.entry(ag.id.clone()).or_insert(0);
                        *n += 1;
                        n.to_string()
                    }
                    None => {
                        main_turns += 1;
                        main_turns.to_string()
                    }
                };
                if it.origin.is_sidechain {
                    m.usage_sidechain.add(&a.usage);
                    m.turns_sidechain += 1;
                    m.has_sidechain_detail = true;
                } else {
                    m.usage_main.add(&a.usage);
                    m.turns_main += 1;
                    *m.model_turns_main.entry(a.model.clone()).or_default() += 1;
                }
                let price = price_for(&a.model);
                m.cost_usd += price.cost(&a.usage);
                if a.is_error {
                    m.api_errors += 1;
                }
                read_price_num += price.cache_read * a.usage.cache_read_input_tokens as f64;
                read_price_den += a.usage.cache_read_input_tokens as f64;

                let mut ttools = Vec::new();
                for t in &a.tools {
                    m.tool_calls += 1;
                    let target = t.target.clone().unwrap_or_default();
                    tool_index.insert(t.id.clone(), (t.name.clone(), target.clone()));
                    let e = tstats.entry(t.name.clone()).or_default();
                    e.name = t.name.clone();
                    e.calls += 1;
                    e.input_chars += t.input_chars() as u64;
                    if (t.name == "Read" || t.name == "Grep") && !target.is_empty() {
                        *read_repeat.entry(target.clone()).or_insert(0) += 1;
                    }
                    if full {
                        ttools.push(TTool {
                            name: t.name.clone(),
                            target,
                            // Resolve the Agent call to the agent it created, so the tool
                            // line points at the conversation spliced in below it.
                            spawned: spawned_agents.get(t.id.as_str()).cloned(),
                            input_full: t.input_full.clone(),
                        });
                    }
                }

                let ctx = a.usage.billed_input();
                m.context_peak = m.context_peak.max(ctx);
                if full {
                    let th = threads.entry(it.origin.agent_id.clone()).or_default();
                    let delta = th.prev_ctx.map(|p| ctx as i64 - p as i64).unwrap_or(0);
                    timeline.push(TurnPoint {
                        turn: turns_seen,
                        label: label.clone(),
                        agent: agent.clone(),
                        model: a.model.clone(),
                        usage: a.usage.clone(),
                        ts_ms: it.ts_ms,
                        is_sidechain: it.origin.is_sidechain,
                        context_size: ctx,
                        delta,
                        added_tokens: th.pending_added,
                        cost: price.cost(&a.usage),
                        is_error: a.is_error,
                        is_spike: false,
                        compaction_after: false,
                        cause: th.pending_cause.0.clone(),
                    });
                    // A turn with no usage (a synthetic placeholder) says nothing about
                    // context size — leave the chain untouched rather than resetting it to 0.
                    if ctx > 0 {
                        th.prev_ctx = Some(ctx);
                    }
                    th.pending_added = 0;
                    th.pending_cause = (String::new(), 0);

                    transcript.push(TItem {
                        index: idx,
                        agent: agent.clone(),
                        ts_ms: it.ts_ms,
                        run: run_no,
                        kind: TKind::Assistant {
                            turn: turns_seen,
                            label,
                            model: a.model.clone(),
                            usage: a.usage.clone(),
                            thinking: a.thinking.clone(),
                            text: a.text.clone(),
                            tools: ttools,
                            is_error: a.is_error,
                            stop_reason: a.stop_reason.clone(),
                        },
                    });
                }
            }
            ItemKind::ToolResult(r) => {
                let (name, target) = tool_index
                    .get(&r.tool_use_id)
                    .cloned()
                    .unwrap_or_else(|| ("(unknown)".into(), String::new()));
                let toks = est_tokens(r.content_chars());
                let e = tstats.entry(name.clone()).or_default();
                e.name = name.clone();
                e.result_chars += r.content_chars() as u64;
                e.result_tokens_est += toks;
                if r.is_error {
                    e.errors += 1;
                }
                // A ToolSearch that found nothing names a capability the agent needed and
                // could not get — the single most useful fact when diagnosing a workaround
                // loop, and nowhere else in the log.
                if name == "ToolSearch" && r.content.contains("No matching deferred tools found") {
                    for want in target.trim_start_matches("select:").split(',') {
                        let want = want.trim();
                        if !want.is_empty() {
                            *m.tools_unavailable.entry(want.to_string()).or_default() += 1;
                        }
                    }
                }
                let key_target = if target.is_empty() { name.clone() } else { target.clone() };
                resident.push(Resident { tokens: toks, key: (name.clone(), key_target.clone()), entry_turn: turns_seen });
                let th = threads.entry(it.origin.agent_id.clone()).or_default();
                th.pending_added += toks;
                if toks > th.pending_cause.1 {
                    th.pending_cause = (format!("{} {}", name, short_path(&key_target)), toks);
                }
                if full {
                    transcript.push(TItem {
                        index: idx,
                        agent: agent.clone(),
                        ts_ms: it.ts_ms,
                        run: run_no,
                        kind: TKind::Tool {
                            tool: name,
                            target: key_target,
                            tokens: toks,
                            content: r.content.clone(),
                            is_error: r.is_error,
                        },
                    });
                }
            }
            ItemKind::User(u) => {
                // Only the human's prompts. A sidechain "user" message is the parent
                // handing a task to a sub-agent, not a person typing.
                if u.is_prompt && !it.origin.is_sidechain {
                    m.user_prompts += 1;
                } else if u.is_prompt {
                    m.subagent_prompts += 1;
                }
                let toks = est_tokens(u.text.len());
                if toks > 0 {
                    resident.push(Resident { tokens: toks, key: ("(user prompt)".into(), "(user prompt)".into()), entry_turn: turns_seen });
                    threads.entry(it.origin.agent_id.clone()).or_default().pending_added += toks;
                }
                if full {
                    transcript.push(TItem {
                        index: idx,
                        agent: agent.clone(),
                        ts_ms: it.ts_ms,
                        run: run_no,
                        kind: TKind::User { text: u.text.clone(), is_prompt: u.is_prompt },
                    });
                }
            }
            ItemKind::Subagent(s) => {
                let mut sg = s.clone();
                // Prefer the sub-agent's *actual* model from its sidechain turns (Explore
                // often runs on a cheaper/faster model — sonnet or haiku — than the parent).
                // Fall back to the invoking turn's model only when no sidechain was captured.
                if let Some(real) = agent_models.get(&sg.agent_id) {
                    sg.model = real.clone();
                } else if sg.model.is_empty() {
                    sg.model = cur_model.clone();
                }
                sg.end_ms = it.ts_ms;
                m.subagents.push(sg);
            }
            ItemKind::Compact(c) => {
                evict_all(&mut resident, &mut contribs, turns_seen);
                m.compactions.push(c.clone());
                // Mark the last turn *of the same thread* — with sub-agents nested inline,
                // `timeline.last()` is often some agent's turn, not the one that compacted.
                if let Some(last) = timeline.iter_mut().rev().find(|t| t.agent.as_ref().map(|a| a.id.as_str()).unwrap_or("") == it.origin.agent_id) {
                    last.compaction_after = true;
                }
                threads.entry(it.origin.agent_id.clone()).or_default().prev_ctx = None; // context resets
                if full {
                    transcript.push(TItem {
                        index: idx,
                        agent: agent.clone(),
                        ts_ms: it.ts_ms,
                        run: run_no,
                        kind: TKind::Compact { pre: c.pre_tokens, post: c.post_tokens, trigger: c.trigger.clone() },
                    });
                }
            }
            ItemKind::Event(e) => {
                if e.is_terminal {
                    m.terminal_events.push((e.subtype.clone(), e.detail.clone()));
                }
                // Injected content (hook output, memory files) really does enter the
                // context, so it counts toward growth like any tool result would.
                let toks = est_tokens(e.content.len());
                if toks > 0 {
                    // Attribute to the thing that entered context (a memory file's path),
                    // not to the sentence describing it — otherwise every row truncates to
                    // the same useless prefix in a narrow column.
                    let source = match e.detail.split_once(": ") {
                        Some((_, what)) if !what.is_empty() => what.to_string(),
                        _ if !e.detail.is_empty() => e.detail.clone(),
                        _ => format!("({})", e.subtype),
                    };
                    let key = (format!("(harness {})", e.subtype), source);
                    resident.push(Resident { tokens: toks, key, entry_turn: turns_seen });
                    threads.entry(it.origin.agent_id.clone()).or_default().pending_added += toks;
                }
                if full {
                    transcript.push(TItem {
                        index: idx,
                        agent: agent.clone(),
                        ts_ms: it.ts_ms,
                        run: run_no,
                        kind: TKind::Event {
                            subtype: e.subtype.clone(),
                            detail: e.detail.clone(),
                            content: e.content.clone(),
                            is_terminal: e.is_terminal,
                        },
                    });
                }
            }
            ItemKind::ToolRoster(d) => {
                for t in &d.added {
                    m.deferred_tools.insert(t.clone());
                }
                for t in &d.removed {
                    m.deferred_tools.remove(t);
                }
            }
            // Boundary marker only — the segment it opens was recorded above.
            ItemKind::RunStart => {}
        }
    }
    evict_all(&mut resident, &mut contribs, turns_seen);
    if let Some(run) = runs.last_mut() {
        run.last_item = transcript.len().saturating_sub(1);
    }

    if ts_max >= ts_min && ts_min != i64::MAX {
        m.duration_ms = ts_max - ts_min;
    }
    // Continuity: split the turn timeline on idle gaps to separate a genuine continuous
    // burst from stop-and-go work spread over days.
    {
        let mut pts: Vec<(i64, u64)> = timeline.iter().filter(|t| t.ts_ms > 0).map(|t| (t.ts_ms, t.usage.fresh())).collect();
        pts.sort_by_key(|p| p.0); // sidechain turns interleave in time, so sort by timestamp
        let mut cur_start = 0i64;
        let mut cur_fresh = 0u64;
        for i in 0..pts.len() {
            if i == 0 {
                cur_start = pts[0].0;
            } else {
                let d = pts[i].0 - pts[i - 1].0;
                m.longest_gap_ms = m.longest_gap_ms.max(d);
                if d > WORK_GAP_MS {
                    m.idle_ms += d;
                }
                if d > IDLE_GAP_MS {
                    m.idle_gaps += 1;
                    // finalize the burst that just ended.
                    let bms = pts[i - 1].0 - cur_start;
                    if cur_fresh > m.longest_burst_fresh {
                        m.longest_burst_fresh = cur_fresh;
                        m.longest_burst_ms = bms;
                    }
                    cur_start = pts[i].0;
                    cur_fresh = 0;
                }
                if d <= WORK_GAP_MS {
                    m.active_ms += d;
                }
            }
            cur_fresh += pts[i].1;
        }
        if let Some(last) = pts.last() {
            let bms = last.0 - cur_start;
            if cur_fresh > m.longest_burst_fresh {
                m.longest_burst_fresh = cur_fresh;
                m.longest_burst_ms = bms;
            }
        }
    }
    // Max concurrent sub-agents: sweep the [start, end] runtime intervals for overlap.
    {
        let mut events: Vec<(i64, i64)> = Vec::new(); // (time, +1 start / -1 end)
        for s in &m.subagents {
            if s.end_ms > 0 && s.duration_ms > 0 {
                events.push((s.end_ms - s.duration_ms as i64, 1));
                events.push((s.end_ms, -1));
            }
        }
        events.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1))); // starts before ends at same ts
        let mut cur = 0i64;
        for (_, d) in events {
            cur += d;
            m.max_concurrent_subagents = m.max_concurrent_subagents.max(cur.max(0) as u64);
        }
    }
    m.subagents.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens));

    m.tools = tstats.into_values().collect();
    m.tools.sort_by(|a, b| (b.result_tokens_est + b.input_chars).cmp(&(a.result_tokens_est + a.input_chars)));

    // ---- normalize cache attribution to reconcile with actual cache-read ----
    let actual_read = m.usage.cache_read_input_tokens;
    let explained: u64 = contribs.values().map(|c| c.contribution).sum();
    let eff_read_price = if read_price_den > 0.0 { read_price_num / read_price_den } else { 0.0 };
    let scale = if explained > actual_read && explained > 0 { actual_read as f64 / explained as f64 } else { 1.0 };
    let mut cache_attr: Vec<CacheContrib> = contribs.into_values().collect();
    for c in &mut cache_attr {
        c.contribution = (c.contribution as f64 * scale) as u64;
    }
    // Baseline row = the always-resident overhead not explained by tracked content.
    let scaled_explained: u64 = cache_attr.iter().map(|c| c.contribution).sum();
    let baseline = actual_read.saturating_sub(scaled_explained);
    if baseline > 0 {
        cache_attr.push(CacheContrib {
            tool: "(base context)".into(),
            target: "system prompt · prior conversation · thinking".into(),
            tokens: 0,
            entries: 0,
            residency_turns: turns_seen as u64,
            contribution: baseline,
            share: 0.0,
            amplified_cost: 0.0,
            is_baseline: true,
        });
    }
    for c in &mut cache_attr {
        c.share = if actual_read > 0 { c.contribution as f64 / actual_read as f64 } else { 0.0 };
        c.amplified_cost = c.contribution as f64 * eff_read_price / 1_000_000.0;
    }
    cache_attr.sort_by(|a, b| b.contribution.cmp(&a.contribution));

    // ---- spike detection over timeline deltas ----
    let spikes = detect_spikes(&mut timeline, !m.compactions.is_empty());

    // ---- findings ----
    let threads = collect_threads(&transcript, &m);
    // Control-flow findings need the transcript, which the aggregate pass does not build.
    // They are returned separately so `analyze` can fold each session's into the global
    // view — otherwise `issues` (unscoped) silently hides the loudest failures.
    // Only the per-session pass can derive these (it has the transcript and timeline);
    // `analyze` folds each session's set into the global view, so computing them here for
    // the aggregate pass too would double-report every one.
    let control =
        if full { detect_control_flow(&m, &runs, &threads, &transcript, &spikes) } else { Vec::new() };
    m.findings = detect_findings(&m, &cache_attr, &read_repeat);
    m.findings.extend(control.iter().cloned());
    sort_findings(&mut m.findings);

    Built { metrics: m, transcript, timeline, cache_attr, spikes, threads, runs, control }
}

/// Roll the transcript up into one entry per sub-agent conversation, in first-appearance
/// order. Driven by the transcript (not the result records) so agents that never returned
/// are listed too — those are exactly the ones with no `SubagentResult`.
fn collect_threads(transcript: &[TItem], m: &Metrics) -> Vec<AgentThread> {
    let completed: HashSet<&str> = m.subagents.iter().map(|s| s.agent_id.as_str()).collect();
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, AgentThread> = HashMap::new();
    for (i, it) in transcript.iter().enumerate() {
        let Some(ag) = &it.agent else { continue };
        let t = by_id.entry(ag.id.clone()).or_insert_with(|| {
            order.push(ag.id.clone());
            AgentThread {
                agent: ag.clone(),
                model: String::new(),
                turns: 0,
                usage: Usage::default(),
                cost_usd: 0.0,
                tool_calls: 0,
                first_item: i,
                last_item: i,
                start_ms: it.ts_ms,
                end_ms: it.ts_ms,
                completed: completed.contains(ag.id.as_str()),
                outcome: Outcome::Truncated,
            }
        });
        t.last_item = i;
        if it.ts_ms > 0 {
            if t.start_ms == 0 {
                t.start_ms = it.ts_ms;
            }
            t.end_ms = t.end_ms.max(it.ts_ms);
        }
        match &it.kind {
            TKind::Assistant { model, usage, tools, is_error, stop_reason, .. } => {
                t.turns += 1;
                t.usage.add(usage);
                t.cost_usd += price_for(model).cost(usage);
                t.tool_calls += tools.len() as u64;
                if t.model.is_empty() {
                    t.model = model.clone();
                }
                // Recomputed each turn so the *last* turn decides the outcome.
                t.outcome = Outcome::from_last_turn(stop_reason, *is_error);
            }
            TKind::Event { is_terminal: true, .. } => t.outcome = Outcome::LimitHit,
            _ => {}
        }
    }
    // A result came back to the parent ⇒ the agent finished, whatever its last turn looked
    // like (the closing turn is logged on the parent's side).
    let mut out: Vec<AgentThread> = order.into_iter().filter_map(|id| by_id.remove(&id)).collect();
    for t in &mut out {
        if t.completed && t.outcome == Outcome::Truncated {
            t.outcome = Outcome::Completed;
        }
    }
    out
}

/// Flag turns whose context jump is anomalous; returns them (also sets `is_spike`).
fn detect_spikes(timeline: &mut [TurnPoint], any_compaction: bool) -> Vec<Spike> {
    let deltas: Vec<f64> = timeline.iter().filter(|t| t.delta > 0).map(|t| t.delta as f64).collect();
    if deltas.len() < 3 {
        return Vec::new();
    }
    let mean = deltas.iter().sum::<f64>() / deltas.len() as f64;
    let var = deltas.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / deltas.len() as f64;
    let std = var.sqrt();
    let threshold = (mean + 2.0 * std).max(15_000.0);
    let mut spikes = Vec::new();
    for t in timeline.iter_mut() {
        if t.delta as f64 >= threshold {
            t.is_spike = true;
            spikes.push(Spike {
                turn: t.turn,
                label: t.display_turn(),
                delta: t.delta,
                context_size: t.context_size,
                // Only blame compaction when one actually happened; otherwise say plainly
                // that the growth is unattributed rather than inventing a cause.
                cause: if !t.cause.is_empty() {
                    t.cause.clone()
                } else if any_compaction {
                    "accumulated history / post-compaction re-expansion".into()
                } else {
                    "unattributed (no single tool result explains it)".into()
                },
            });
        }
    }
    spikes.sort_by(|a, b| b.delta.cmp(&a.delta));
    spikes
}

fn detect_findings(m: &Metrics, cache_attr: &[CacheContrib], read_repeat: &HashMap<String, u64>) -> Vec<Finding> {
    let mut out = Vec::new();

    let hit = m.cache_hit_rate();
    if m.usage.billed_input() > 50_000 && hit < 0.80 {
        out.push(Finding {
            severity: if hit < 0.5 { Severity::High } else { Severity::Warn },
            kind: "Low cache hit rate".into(),
            detail: format!(
                "{:.0}% of input served from cache (target >90%). {} written vs {} read.",
                hit * 100.0,
                fmt_int(m.usage.cache_creation_input_tokens),
                fmt_int(m.usage.cache_read_input_tokens)
            ),
            wasted_tokens_est: ((0.90 - hit).max(0.0) * m.usage.cache_creation_input_tokens as f64) as u64,
        });
    }

    let churn = m.cache_churn();
    if churn.is_finite() && churn > 0.5 && m.usage.cache_creation_input_tokens > 30_000 {
        out.push(Finding {
            severity: if churn > 1.0 { Severity::High } else { Severity::Warn },
            kind: "Cache churn".into(),
            detail: format!("Cache writes are {:.2}x reads — context keeps changing so caches expire before paying off.", churn),
            wasted_tokens_est: (m.usage.cache_creation_input_tokens as f64 * 0.25) as u64,
        });
    }

    // Top cache-read contributors (real sinks, excluding baseline).
    for c in cache_attr.iter().filter(|c| !c.is_baseline).take(3) {
        if c.share > 0.05 {
            out.push(Finding {
                severity: if c.share > 0.15 { Severity::Warn } else { Severity::Info },
                kind: "Cache-read sink".into(),
                detail: format!(
                    "{} {} accounts for {:.0}% of cache-read (~{} replayed tokens, ${:.2}) across {} entr(y/ies).",
                    c.tool, short_path(&c.target), c.share * 100.0, fmt_int(c.contribution), c.amplified_cost, c.entries
                ),
                wasted_tokens_est: 0,
            });
        }
    }

    // Repeated reads.
    let mut repeats: Vec<(&String, u64)> = read_repeat.iter().filter(|(_, c)| **c >= 3).map(|(k, v)| (k, *v)).collect();
    repeats.sort_by(|a, b| b.1.cmp(&a.1));
    for (path, count) in repeats.iter().take(4) {
        out.push(Finding {
            severity: if *count >= 5 { Severity::Warn } else { Severity::Info },
            kind: "Repeated read".into(),
            detail: format!("{} read {} times — consider reading once or narrowing.", short_path(path), count),
            wasted_tokens_est: 0,
        });
    }

    if m.api_errors >= 2 {
        out.push(Finding {
            severity: Severity::Warn,
            kind: "API errors".into(),
            detail: format!("{} assistant turn(s) failed with an API error and had to be retried.", m.api_errors),
            wasted_tokens_est: 0,
        });
    }

    let errs: u64 = m.tools.iter().map(|t| t.errors).sum();
    if errs >= 3 {
        out.push(Finding {
            severity: Severity::Info,
            kind: "Tool errors".into(),
            detail: format!("{} tool call(s) returned errors — each burns a full context pass.", errs),
            wasted_tokens_est: 0,
        });
    }

    let auto = m.compactions.iter().filter(|c| c.trigger == "auto").count();
    if auto > 0 {
        let saved: u64 = m.compactions.iter().map(|c| c.pre_tokens.saturating_sub(c.post_tokens)).sum();
        let secs: u64 = m.compactions.iter().map(|c| c.duration_ms).sum::<u64>() / 1000;
        out.push(Finding {
            severity: Severity::Warn,
            kind: "Auto-compaction".into(),
            detail: format!("{} automatic compaction(s) costing ~{}s; context hit the window (~{} tokens dropped).", auto, secs, fmt_int(saved)),
            wasted_tokens_est: 0,
        });
    }

    for s in m.subagents.iter().take(3) {
        if s.total_tokens > 60_000 {
            out.push(Finding {
                severity: Severity::Info,
                kind: "Heavy sub-agent".into(),
                detail: format!("{} sub-agent used {} tokens over {} tool calls.", s.agent_type, fmt_int(s.total_tokens), s.tool_use_count),
                wasted_tokens_est: 0,
            });
        }
    }

    sort_findings(&mut out);
    out
}

/// Worst first, then biggest waste.
fn sort_findings(v: &mut [Finding]) {
    v.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.wasted_tokens_est.cmp(&a.wasted_tokens_est)));
}

/// Findings about *control flow* rather than token efficiency: how the run ended, and
/// whether the agent got stuck repeating itself. These are the questions asked first when
/// a session goes wrong, and none of them are answerable from token aggregates.
fn detect_control_flow(
    m: &Metrics,
    runs: &[RunSegment],
    threads: &[AgentThread],
    transcript: &[TItem],
    spikes: &[Spike],
) -> Vec<Finding> {
    let mut out = Vec::new();

    // ---- how each run ended ----
    for r in runs {
        match r.outcome {
            Outcome::LimitHit => out.push(Finding {
                severity: Severity::High,
                kind: "Run hit a limit".into(),
                detail: format!(
                    "Run {} ({} main turns) ended on {}. Work stopped here — anything after this is a new run.",
                    r.index,
                    r.turns_main,
                    if r.outcome_detail.is_empty() { "a harness limit".into() } else { r.outcome_detail.clone() }
                ),
                wasted_tokens_est: 0,
            }),
            Outcome::Truncated if r.turns_main > 0 => out.push(Finding {
                severity: Severity::Warn,
                kind: "Run cut off".into(),
                detail: format!(
                    "Run {} ends mid-generation after {} main turns: the last response has no closing stop_reason and no records follow it. Compare the last record time with when the capture was written to judge whether it was still running.",
                    r.index, r.turns_main
                ),
                wasted_tokens_est: 0,
            }),
            _ => {}
        }
    }
    // Terminal events outside any run we segmented (defensive: never lose the signal).
    if runs.is_empty() {
        for (subtype, detail) in &m.terminal_events {
            out.push(Finding {
                severity: Severity::High,
                kind: "Run hit a limit".into(),
                detail: format!("{subtype}: {detail}"),
                wasted_tokens_est: 0,
            });
        }
    }

    // ---- delegation loop ----
    // Per agent: how many tool calls it made, and how many of those merely spawned another
    // agent. An agent whose *only* action is delegating did no work of its own.
    let mut tool_mix: HashMap<&str, (usize, usize)> = HashMap::new();
    for it in transcript {
        if let (Some(ag), TKind::Assistant { tools, .. }) = (&it.agent, &it.kind) {
            let e = tool_mix.entry(ag.id.as_str()).or_default();
            e.0 += tools.len();
            e.1 += tools.iter().filter(|t| t.name == "Agent" || t.name == "Task").count();
        }
    }
    let spinning: Vec<&AgentThread> = threads
        .iter()
        .filter(|t| {
            let (all, delegating) = tool_mix.get(t.agent.id.as_str()).copied().unwrap_or((0, 0));
            t.agent.depth >= 2 && all > 0 && all == delegating
        })
        .collect();
    debug_assert_eq!(
        spinning.len(),
        threads.iter().filter(|t| t.is_spinning(transcript)).count(),
        "`is_spinning` must match the finding's own criterion — `ssa agents --spinning` documents this number"
    );
    let max_depth = threads.iter().map(|t| t.agent.depth).max().unwrap_or(0);
    if spinning.len() >= 3 {
        let tokens: u64 = spinning.iter().map(|t| t.usage.total()).sum();
        let cost: f64 = spinning.iter().map(|t| t.cost_usd).sum();
        let deepest = spinning.iter().max_by_key(|t| t.agent.depth).unwrap();
        out.push(Finding {
            severity: Severity::High,
            kind: "Delegation loop".into(),
            detail: format!(
                "{} sub-agents did nothing but spawn another sub-agent, nesting {} levels deep — {} tokens, ${:.2}, no work done. Repeated task: \"{}\". {}",
                spinning.len(),
                max_depth,
                fmt_int(tokens),
                cost,
                deepest.agent.description.chars().take(60).collect::<String>(),
                if m.tools_unavailable.is_empty() {
                    "Root cause is usually a tool the agents cannot reach, so each level re-derives the same fallback.".to_string()
                } else {
                    // We know which capability was missing — say so instead of guessing.
                    format!(
                        "Root cause: {} was requested and unavailable, so every level re-derived the same fallback (see `ssa tools --available`; `ssa agents --spinning` lists the {} agents counted here).",
                        m.tools_unavailable.keys().cloned().collect::<Vec<_>>().join(", "),
                        spinning.len()
                    )
                }
            ),
            wasted_tokens_est: tokens,
        });
    } else if max_depth >= 4 {
        out.push(Finding {
            severity: Severity::Warn,
            kind: "Deep delegation".into(),
            detail: format!("Sub-agents nest {max_depth} levels deep; every level re-pays its whole prompt as cache-write."),
            wasted_tokens_est: 0,
        });
    }

    // ---- the same call issued over and over ----
    // `Repeated read` already covers Read/Grep; this catches everything else, including the
    // identical Agent/Bash call retried in a loop.
    let mut calls: HashMap<(&str, &str), u64> = HashMap::new();
    for it in transcript {
        if let TKind::Assistant { tools, .. } = &it.kind {
            for t in tools {
                if !matches!(t.name.as_str(), "Read" | "Grep") {
                    *calls.entry((t.name.as_str(), t.target.as_str())).or_default() += 1;
                }
            }
        }
    }
    let mut repeated: Vec<((&str, &str), u64)> = calls.into_iter().filter(|(_, c)| *c >= 4).collect();
    repeated.sort_by(|a, b| b.1.cmp(&a.1));
    for ((tool, target), count) in repeated.iter().take(3) {
        out.push(Finding {
            severity: if *count >= 8 { Severity::Warn } else { Severity::Info },
            kind: "Repeated tool call".into(),
            detail: format!("`{} {}` issued {} times — identical call, so identical result.", tool, short_path(target), count),
            wasted_tokens_est: 0,
        });
    }

    // ---- a capability the agent could not reach ----
    if !m.tools_unavailable.is_empty() {
        let list = m
            .tools_unavailable
            .iter()
            .map(|(n, c)| format!("{n} (×{c})"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(Finding {
            severity: Severity::High,
            kind: "Tool unavailable".into(),
            detail: format!(
                "The agent searched for {list} and the harness had no such tool. An agent that cannot reach a capability improvises around it — check for repeated calls or delegation below."
            ),
            wasted_tokens_est: 0,
        });
    }

    // ---- context-growth spikes ----
    // Per session, like everything else here: they need the timeline, which the aggregate
    // pass does not build.
    for s in spikes.iter().take(3) {
        out.push(Finding {
            severity: Severity::Warn,
            kind: "Context spike".into(),
            detail: format!(
                "Turn {}: context jumped +{} tokens (to {}). Cause: {}.",
                s.label,
                fmt_int(s.delta as u64),
                fmt_int(s.context_size),
                s.cause
            ),
            wasted_tokens_est: 0,
        });
    }

    // ---- a turn that produced nothing ----
    let empty = transcript
        .iter()
        .filter(|it| matches!(&it.kind, TKind::Assistant { usage, tools, .. } if usage.output_tokens == 0 && tools.is_empty()))
        .count();
    if empty > 0 {
        out.push(Finding {
            severity: Severity::Info,
            kind: "Empty turn".into(),
            detail: format!("{empty} assistant turn(s) produced no output and no tool call — usually the tail of a run that was stopped."),
            wasted_tokens_est: 0,
        });
    }

    out
}

// ---------------------------------------------------------------------- helpers

pub fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::new();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Compact a model id for display: `claude-opus-4-8` → `opus-4-8`, `<synthetic>` → `-`.
pub fn short_model(m: &str) -> String {
    if m.is_empty() || m == "unknown" || m == "<synthetic>" {
        "-".to_string()
    } else {
        m.strip_prefix("claude-").unwrap_or(m).to_string()
    }
}

/// Format epoch millis (UTC) as `YYYY-MM-DD HH:MM` — inverse of [`parse_ts_ms`].
pub fn fmt_epoch(ms: i64) -> String {
    if ms <= 0 {
        return "-".to_string();
    }
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (h, mi) = (tod / 3600, (tod % 3600) / 60);
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

/// `YYYY-MM-DD HH:MM:SS` — full precision, for comparing a log record against a file time.
pub fn fmt_epoch_secs(ms: i64) -> String {
    if ms <= 0 {
        return "-".into();
    }
    let secs = ms.div_euclid(1000).rem_euclid(60);
    format!("{}:{:02}", fmt_epoch(ms), secs)
}

/// `HH:MM:SS` wall-clock time, for lining transcript messages up against each other.
/// Blank (fixed-width) when the record carried no timestamp, so columns stay aligned.
pub fn fmt_clock(ms: i64) -> String {
    if ms <= 0 {
        return "         ".into();
    }
    let tod = ms.div_euclid(1000).rem_euclid(86400);
    format!("{:02}:{:02}:{:02} ", tod / 3600, (tod % 3600) / 60, tod % 60)
}

/// Human duration for a millisecond span: `4.2s`, `6m41s`, `1h03m`.
pub fn fmt_dur_ms(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

pub fn short_path(p: &str) -> String {
    if p.chars().count() <= 48 {
        return p.to_string();
    }
    let parts: Vec<&str> = p.split('/').collect();
    if parts.len() > 3 {
        format!(".../{}", parts[parts.len().saturating_sub(3)..].join("/"))
    } else {
        let tail: String = p.chars().rev().take(46).collect::<Vec<_>>().into_iter().rev().collect();
        format!("…{}", tail)
    }
}
