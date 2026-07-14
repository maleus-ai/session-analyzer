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
use std::collections::HashMap;

const CHARS_PER_TOKEN: f64 = 3.7;

/// Gap between turns above which we treat the session as having paused (idle). Used to
/// separate a continuous unattended burst from stop-and-go interactive work.
pub const IDLE_GAP_MS: i64 = 15 * 60 * 1000; // 15 minutes

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

/// One assistant turn on the timeline.
#[derive(Debug, Clone)]
pub struct TurnPoint {
    pub turn: usize,
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
    pub turn: usize,
    pub delta: i64,
    pub context_size: u64,
    pub cause: String,
}

/// A resolved transcript item (full text) for the bubble view.
#[derive(Debug, Clone)]
pub struct TItem {
    pub index: usize,
    pub kind: TKind,
}

#[derive(Debug, Clone)]
pub enum TKind {
    User { text: String, is_prompt: bool },
    Assistant {
        turn: usize,
        model: String,
        usage: Usage,
        thinking: String,
        text: String,
        tools: Vec<TTool>,
        is_error: bool,
    },
    Tool { tool: String, target: String, tokens: u64, content: String, is_error: bool },
    Compact { pre: u64, post: u64, trigger: String },
}

#[derive(Debug, Clone)]
pub struct TTool {
    pub name: String,
    pub target: String,
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
}

/// Full analysis result.
#[derive(Debug, Clone)]
pub struct Analysis {
    pub provider: String,
    pub global: Metrics,
    /// Cache-read decomposition across all sessions.
    pub global_cache_attr: Vec<CacheContrib>,
    pub sessions: Vec<SessionReport>,
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
        if d > IDLE_GAP_MS {
            r.idle_gaps += 1;
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
            if a.is_sidechain && !a.agent_id.is_empty() && a.model != "<synthetic>" {
                agent_models.entry(a.agent_id.clone()).or_insert_with(|| a.model.clone());
            }
        }
    }

    // Build each session's views in parallel — they are independent, and the transcript
    // construction (string clones, wrapping data) dominates load time for big trees.
    let mut sessions: Vec<SessionReport> = order
        .par_iter()
        .filter_map(|sid| {
            let items: Vec<&Item> = groups[sid].iter().map(|&i| &ds.items[i]).collect();
            let built = build(&items, &gindex, &agent_models, true);
            if built.metrics.assistant_turns == 0 && built.metrics.tool_calls == 0 && built.metrics.subagents.is_empty() {
                return None;
            }
            Some(SessionReport {
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
            })
        })
        .collect();
    sessions.sort_by(|a, b| b.metrics.cost_usd.total_cmp(&a.metrics.cost_usd));

    // Global metrics only need aggregates + cache attribution, so skip the (unused) global
    // transcript/timeline construction — a big saving over the whole dataset.
    let all: Vec<&Item> = ds.items.iter().collect();
    let global_built = build(&all, &gindex, &agent_models, false);

    Analysis {
        provider: ds.provider.clone(),
        global: global_built.metrics,
        global_cache_attr: global_built.cache_attr,
        sessions,
        parse_errors: ds.parse_errors,
    }
}

struct Built {
    metrics: Metrics,
    transcript: Vec<TItem>,
    timeline: Vec<TurnPoint>,
    cache_attr: Vec<CacheContrib>,
    spikes: Vec<Spike>,
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
    full: bool,
) -> Built {
    let mut m = Metrics::default();
    let mut transcript: Vec<TItem> = Vec::new();
    let mut timeline: Vec<TurnPoint> = Vec::new();

    let mut tstats: HashMap<String, ToolStat> = HashMap::new();
    // id -> (name, target); seeded globally so cross-file references resolve.
    let mut tool_index: HashMap<String, (String, String)> = tool_index_seed.clone();

    // Cache-attribution state.
    let mut resident: Vec<Resident> = Vec::new();
    let mut contribs: HashMap<(String, String), CacheContrib> = HashMap::new();
    let mut turns_seen = 0usize;
    let mut read_price_num = 0.0f64; // Σ price.cache_read × turn.cache_read
    let mut read_price_den = 0.0f64;

    // Timeline / spike state.
    let mut prev_ctx: Option<u64> = None;
    let mut pending_added: u64 = 0; // tokens added since last assistant turn
    let mut pending_cause: (String, u64) = (String::new(), 0);
    let mut cur_session = String::new();
    let mut cur_model = String::new(); // model of the most recent assistant turn

    let mut ts_min = i64::MAX;
    let mut ts_max = i64::MIN;
    let mut read_repeat: HashMap<String, u64> = HashMap::new();

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
            prev_ctx = None;
            pending_added = 0;
            pending_cause = (String::new(), 0);
        }
        if it.ts_ms > 0 {
            ts_min = ts_min.min(it.ts_ms);
            ts_max = ts_max.max(it.ts_ms);
        }
        let idx = transcript.len();

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
                if a.is_sidechain {
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
                        ttools.push(TTool { name: t.name.clone(), target, input_full: t.input_full.clone() });
                    }
                }

                let ctx = a.usage.billed_input();
                m.context_peak = m.context_peak.max(ctx);
                if full {
                    let delta = prev_ctx.map(|p| ctx as i64 - p as i64).unwrap_or(0);
                    timeline.push(TurnPoint {
                        turn: turns_seen,
                        model: a.model.clone(),
                        usage: a.usage.clone(),
                        ts_ms: it.ts_ms,
                        is_sidechain: a.is_sidechain,
                        context_size: ctx,
                        delta,
                        added_tokens: pending_added,
                        cost: price.cost(&a.usage),
                        is_error: a.is_error,
                        is_spike: false,
                        compaction_after: false,
                        cause: pending_cause.0.clone(),
                    });
                    prev_ctx = Some(ctx);
                    pending_added = 0;
                    pending_cause = (String::new(), 0);

                    transcript.push(TItem {
                        index: idx,
                        kind: TKind::Assistant {
                            turn: turns_seen,
                            model: a.model.clone(),
                            usage: a.usage.clone(),
                            thinking: a.thinking.clone(),
                            text: a.text.clone(),
                            tools: ttools,
                            is_error: a.is_error,
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
                let key_target = if target.is_empty() { name.clone() } else { target.clone() };
                resident.push(Resident { tokens: toks, key: (name.clone(), key_target.clone()), entry_turn: turns_seen });
                pending_added += toks;
                if toks > pending_cause.1 {
                    pending_cause = (format!("{} {}", name, short_path(&key_target)), toks);
                }
                if full {
                    transcript.push(TItem {
                        index: idx,
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
                if u.is_prompt {
                    m.user_prompts += 1;
                }
                let toks = est_tokens(u.text.len());
                if toks > 0 {
                    resident.push(Resident { tokens: toks, key: ("(user prompt)".into(), "(user prompt)".into()), entry_turn: turns_seen });
                    pending_added += toks;
                }
                if full {
                    transcript.push(TItem {
                        index: idx,
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
                if let Some(last) = timeline.last_mut() {
                    last.compaction_after = true;
                }
                prev_ctx = None; // context resets after compaction
                if full {
                    transcript.push(TItem {
                        index: idx,
                        kind: TKind::Compact { pre: c.pre_tokens, post: c.post_tokens, trigger: c.trigger.clone() },
                    });
                }
            }
        }
    }
    evict_all(&mut resident, &mut contribs, turns_seen);

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
                } else {
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
    let spikes = detect_spikes(&mut timeline);

    // ---- findings ----
    m.findings = detect_findings(&m, &cache_attr, &spikes, &read_repeat);

    Built { metrics: m, transcript, timeline, cache_attr, spikes }
}

/// Flag turns whose context jump is anomalous; returns them (also sets `is_spike`).
fn detect_spikes(timeline: &mut [TurnPoint]) -> Vec<Spike> {
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
                delta: t.delta,
                context_size: t.context_size,
                cause: if t.cause.is_empty() { "accumulated history / post-compaction re-expansion".into() } else { t.cause.clone() },
            });
        }
    }
    spikes.sort_by(|a, b| b.delta.cmp(&a.delta));
    spikes
}

fn detect_findings(m: &Metrics, cache_attr: &[CacheContrib], spikes: &[Spike], read_repeat: &HashMap<String, u64>) -> Vec<Finding> {
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

    // Context-growth spikes.
    for s in spikes.iter().take(3) {
        out.push(Finding {
            severity: Severity::Warn,
            kind: "Context spike".into(),
            detail: format!("Turn {}: context jumped +{} tokens (to {}). Cause: {}.", s.turn, fmt_int(s.delta as u64), fmt_int(s.context_size), s.cause),
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

    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.wasted_tokens_est.cmp(&a.wasted_tokens_est)));
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
