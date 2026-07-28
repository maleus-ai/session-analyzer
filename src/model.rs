//! Normalized, **provider-agnostic** session model.
//!
//! A [`Provider`](crate::provider::Provider) parses a specific harness's logs into this
//! shared model; everything downstream (analysis, query, report, TUI) operates only on
//! these types and never sees a provider's raw format.
//!
//! The dataset is a single **ordered event log** ([`Item`]s). Each source file is one
//! session and its lines are chronological, so per-session order is preserved by simply
//! keeping insertion order. Aggregates *and* the transcript/timeline views are derived
//! from this one stream, so there is a single source of truth.

use std::collections::BTreeMap;

/// Token usage for a single assistant API call (Anthropic-style buckets; general
/// enough that other harnesses map their prompt/completion/cached counts onto it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub ephemeral_5m: u64,
    pub ephemeral_1h: u64,
}

impl Usage {
    /// Total tokens the model had to process for this call (context + output).
    pub fn total(&self) -> u64 {
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
    }

    /// Non-output context tokens this call processed (fresh + cache write + cache read).
    pub fn billed_input(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }

    pub fn add(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.cache_creation_input_tokens += o.cache_creation_input_tokens;
        self.cache_read_input_tokens += o.cache_read_input_tokens;
        self.output_tokens += o.output_tokens;
        self.ephemeral_5m += o.ephemeral_5m;
        self.ephemeral_1h += o.ephemeral_1h;
    }
}

/// A tool invocation emitted by the assistant.
#[derive(Debug, Clone)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    /// One-line summary of the primary input (path, command, pattern, …).
    pub target: Option<String>,
    /// Full serialized tool input, for the expand view.
    pub input_full: String,
}

impl ToolUse {
    pub fn input_chars(&self) -> usize {
        self.input_full.len()
    }
}

/// Which conversation thread a record belongs to.
///
/// Claude Code writes each sub-agent's turns to its own `.jsonl` (under
/// `<session>/subagents/`) but stamps them with the **parent** `sessionId`, so this flag is
/// the only thing separating a sub-agent's messages from the main thread's. Every event
/// carries it — an unattributed record would render as if the main agent had said it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Origin {
    /// True for sub-agent (sidechain) records rather than the main conversation thread.
    pub is_sidechain: bool,
    /// Sub-agent id for sidechain records (links to `SubagentResult.agent_id`); empty on
    /// the main thread.
    pub agent_id: String,
}

/// One assistant turn (a single API response).
#[derive(Debug, Clone)]
pub struct AssistantMsg {
    pub model: String,
    pub usage: Usage,
    pub thinking: String,
    pub text: String,
    pub tools: Vec<ToolUse>,
    pub is_error: bool,
    /// Why the model stopped: `tool_use`, `end_turn`, `max_tokens`, … Empty when the record
    /// predates the field or the response was cut off before it was written — the latter
    /// being the signal that a run ended mid-generation.
    pub stop_reason: String,
    /// API request id (or message id) for this assistant response. The `.jsonl` streams a
    /// response as several records that share this id; we keep only one per id so usage
    /// isn't multiply-counted. Empty when unknown (then no dedup).
    pub request_id: String,
}

/// A user event: a real prompt, or a batch of tool results (see [`ItemKind`]).
#[derive(Debug, Clone)]
pub struct UserMsg {
    pub text: String,
    /// True when this was an actual human prompt, not tool output or a meta message.
    pub is_prompt: bool,
}

/// A tool result returned to the model (folded into subsequent context).
#[derive(Debug, Clone)]
pub struct ToolResultRec {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

impl ToolResultRec {
    pub fn content_chars(&self) -> usize {
        self.content.len()
    }
}

/// Result of a completed sub-agent.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub agent_type: String,
    pub agent_id: String,
    /// `tool_use` id of the Agent call that spawned this sub-agent. Links the sub-agent's
    /// sidechain records back to the exact turn that launched them, so the transcript can
    /// nest them under it instead of dumping them at the end.
    pub tool_use_id: String,
    /// The sub-agent's **actual** model, read from its sidechain turns (matched by
    /// `agent_id`) when present; otherwise the invoking turn's model. Filled in analysis.
    pub model: String,
    pub total_tokens: u64,
    pub tool_use_count: u64,
    pub duration_ms: u64,
    /// Epoch millis when the sub-agent finished (filled in analysis from the item ts).
    pub end_ms: i64,
    pub usage: Usage,
    pub read_count: u64,
    pub search_count: u64,
    pub bash_count: u64,
    pub edit_count: u64,
    pub lines_added: u64,
    pub lines_removed: u64,
}

/// Sidecar metadata for one sub-agent, recorded when it is spawned rather than when it
/// finishes — so an agent that never returned is still identifiable.
#[derive(Debug, Clone, Default)]
pub struct AgentMeta {
    pub agent_type: String,
    /// Human description given to the Agent call.
    pub description: String,
    /// `tool_use` id of the Agent call that spawned it.
    pub tool_use_id: String,
}

/// A harness-emitted event attached to the conversation: a run-termination marker, an
/// injected reminder, a hook result, memory pulled into context, …
///
/// These are logged as `type: "attachment"` records. They are not messages, so they used to
/// be discarded — which made the most important question about a session ("why did it
/// stop?") unanswerable, since `max_turns_reached` lives here and nowhere else.
#[derive(Debug, Clone)]
pub struct HarnessEvent {
    /// Attachment subtype as logged, e.g. `max_turns_reached`, `task_reminder`.
    pub subtype: String,
    /// One-line human summary of the payload.
    pub detail: String,
    /// Content this event injected into the context, if any (hook output, memory file).
    pub content: String,
    /// True when this event marks the end of a run (turn/token limit, abort, error).
    pub is_terminal: bool,
    /// For a limit event, `(value reached, configured cap)` as numbers — so a caller never
    /// has to parse them back out of `detail`.
    pub limit: Option<(u64, u64)>,
}

/// A change to the set of tools the agent can reach on demand.
///
/// The harness logs these as `deferred_tools_delta` attachments. They are the **only**
/// record of what an agent could actually do — without them, "was Bash available?" can be
/// answered only by inference from what happened to be called.
#[derive(Debug, Clone)]
pub struct ToolRosterDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

/// A context-compaction event.
#[derive(Debug, Clone)]
pub struct CompactEvent {
    pub trigger: String,
    pub pre_tokens: u64,
    pub post_tokens: u64,
    pub duration_ms: u64,
}

/// One ordered event in a session.
#[derive(Debug, Clone)]
pub enum ItemKind {
    User(UserMsg),
    Assistant(AssistantMsg),
    ToolResult(ToolResultRec),
    Subagent(SubagentResult),
    Compact(CompactEvent),
    Event(HarnessEvent),
    /// Tools made available to (or withdrawn from) a thread.
    ToolRoster(ToolRosterDelta),
    /// Start of a new run (one SDK `query()` / one interactive prompt cycle) within a
    /// session, as logged by the harness. A session file can hold several; turn limits
    /// apply per run, not per session.
    RunStart,
}

/// A timestamped, session-tagged event.
#[derive(Debug, Clone)]
pub struct Item {
    pub session_id: String,
    pub ts_ms: i64,
    /// Main thread or sub-agent sidechain (see [`Origin`]).
    pub origin: Origin,
    pub kind: ItemKind,
}

/// Everything parsed from the input: one ordered event stream plus session-level side
/// tables. Providers fill this via the `push_*` helpers.
#[derive(Debug, Default)]
pub struct Dataset {
    /// Provider id that parsed this dataset (e.g. "claude-code").
    pub provider: String,
    /// Ordered event log (per-session order preserved).
    pub items: Vec<Item>,
    /// sessionId -> human title.
    pub titles: BTreeMap<String, String>,
    /// sessionId -> source file / zip-entry name.
    pub sources: BTreeMap<String, String>,
    /// sessionId -> working directory.
    pub cwds: BTreeMap<String, String>,
    /// sessionId -> harness entrypoint (e.g. "cli", "sdk-ts").
    pub entrypoints: BTreeMap<String, String>,
    /// sessionId -> service tier (e.g. "standard", "priority").
    pub service_tiers: BTreeMap<String, String>,
    /// agentId -> sub-agent metadata, from the `.meta.json` written next to each
    /// sub-agent's log. Covers agents that never returned a result.
    pub agent_meta: BTreeMap<String, AgentMeta>,
    /// Lines that failed to parse.
    pub parse_errors: u64,
}

impl Dataset {
    pub fn push(&mut self, session_id: impl Into<String>, ts_ms: i64, origin: &Origin, kind: ItemKind) {
        self.items.push(Item { session_id: session_id.into(), ts_ms, origin: origin.clone(), kind });
    }

    pub fn set_title(&mut self, session_id: impl Into<String>, title: impl Into<String>) {
        self.titles.insert(session_id.into(), title.into());
    }

    pub fn note_source(&mut self, session_id: &str, source: &str) {
        if session_id != "unknown" {
            self.sources
                .entry(session_id.to_string())
                .or_insert_with(|| source.to_string());
        }
    }

    pub fn note_cwd(&mut self, session_id: &str, cwd: &str) {
        if session_id != "unknown" {
            self.cwds
                .entry(session_id.to_string())
                .or_insert_with(|| cwd.to_string());
        }
    }

    pub fn note_entrypoint(&mut self, session_id: &str, entrypoint: &str) {
        if session_id != "unknown" && !entrypoint.is_empty() {
            self.entrypoints
                .entry(session_id.to_string())
                .or_insert_with(|| entrypoint.to_string());
        }
    }

    pub fn note_service_tier(&mut self, session_id: &str, tier: &str) {
        if session_id != "unknown" && !tier.is_empty() {
            self.service_tiers
                .entry(session_id.to_string())
                .or_insert_with(|| tier.to_string());
        }
    }

    pub fn note_agent_meta(&mut self, agent_id: &str, meta: AgentMeta) {
        if !agent_id.is_empty() {
            self.agent_meta.insert(agent_id.to_string(), meta);
        }
    }

    pub fn note_parse_error(&mut self) {
        self.parse_errors += 1;
    }

    /// Merge another dataset in (for analyzing multiple sources — e.g. an archive plus a
    /// `.claude` tree — in one run). Side tables keep the first value per session.
    pub fn merge(&mut self, other: Dataset) {
        if self.provider.is_empty() {
            self.provider = other.provider;
        }
        self.items.extend(other.items);
        for (k, v) in other.titles {
            self.titles.entry(k).or_insert(v);
        }
        for (k, v) in other.sources {
            self.sources.entry(k).or_insert(v);
        }
        for (k, v) in other.cwds {
            self.cwds.entry(k).or_insert(v);
        }
        for (k, v) in other.entrypoints {
            self.entrypoints.entry(k).or_insert(v);
        }
        for (k, v) in other.service_tiers {
            self.service_tiers.entry(k).or_insert(v);
        }
        for (k, v) in other.agent_meta {
            self.agent_meta.entry(k).or_insert(v);
        }
        self.parse_errors += other.parse_errors;
    }
}

/// Parse an ISO-8601 UTC timestamp (`2026-07-12T03:49:31.108Z`) to epoch millis.
/// Returns 0 on any parse failure — callers treat 0 as "unknown". Provider-agnostic util.
pub fn parse_ts_ms(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return 0;
    }
    let num = |lo: usize, hi: usize| -> Option<i64> { s.get(lo..hi).and_then(|x| x.parse::<i64>().ok()) };
    let (y, mo, d) = match (num(0, 4), num(5, 7), num(8, 10)) {
        (Some(y), Some(mo), Some(d)) => (y, mo, d),
        _ => return 0,
    };
    let (h, mi, se) = match (num(11, 13), num(14, 16), num(17, 19)) {
        (Some(h), Some(mi), Some(se)) => (h, mi, se),
        _ => return 0,
    };
    let ms = s.get(20..23).and_then(|x| x.parse::<i64>().ok()).unwrap_or(0);
    // days-from-civil (Howard Hinnant's algorithm)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = if y2 >= 0 { y2 } else { y2 - 399 } / 400;
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    (((days * 24 + h) * 60 + mi) * 60 + se) * 1000 + ms
}
