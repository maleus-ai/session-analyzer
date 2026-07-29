//! Claude Code session-log provider (`.jsonl`).
//!
//! Each line is a JSON object. The format is heterogeneous and evolves, so we parse into
//! a `serde_json::Value` and defensively extract the fields we need.

use crate::model::*;
use crate::provider::{Capture, Provider};
use serde_json::Value;

pub struct ClaudeCodeProvider;

impl Provider for ClaudeCodeProvider {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn display_name(&self) -> &'static str {
        "Claude Code"
    }

    fn detect(&self, sample: &[String]) -> f32 {
        let mut score = 0.0f32;
        let mut seen = 0;
        for line in sample.iter().take(50) {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            seen += 1;
            let t = v.get("type").and_then(Value::as_str).unwrap_or("");
            // Markers specific to Claude Code session logs.
            if matches!(t, "assistant" | "user" | "ai-title" | "last-prompt" | "queue-operation" | "attachment") {
                score += 1.0;
            }
            if v.get("sessionId").is_some() {
                score += 0.5;
            }
            if v.get("requestId").is_some() || v.get("toolUseResult").is_some() {
                score += 0.5;
            }
        }
        if seen == 0 {
            return 0.0;
        }
        (score / seen as f32).min(1.0)
    }

    /// Coalesce assistant records that share a `requestId`. Claude Code logs ONE API response
    /// as several records — one per content block (thinking / text / each tool_use) — with the
    /// SAME usage repeated on every record. Left as-is, usage/cost/turns are counted 2-4×.
    /// Merge each group into a single turn: usage counted once (the copy with the most output,
    /// to survive a streamed partial), all tool/text/thinking blocks preserved. Per-session
    /// (requestIds are unique within a session). Order-preserving: the merged turn keeps the
    /// position of the group's first record. (Tool *results* are logged once each, so this
    /// does not touch Cache-attr / Sinks.)
    fn finalize(&self, out: &mut Dataset) {
        use std::collections::HashMap;
        let old = std::mem::take(&mut out.items);
        let mut merged: Vec<Item> = Vec::with_capacity(old.len());
        // key by (session_id, request_id) so distinct sessions never collide.
        let mut pos: HashMap<(String, String), usize> = HashMap::new();
        for it in old {
            if let ItemKind::Assistant(a) = &it.kind {
                if !a.request_id.is_empty() {
                    let key = (it.session_id.clone(), a.request_id.clone());
                    if let Some(&p) = pos.get(&key) {
                        if let ItemKind::Assistant(ex) = &mut merged[p].kind {
                            let ItemKind::Assistant(a) = &it.kind else { unreachable!() };
                            ex.tools.extend(a.tools.iter().cloned());
                            if !a.text.is_empty() {
                                if !ex.text.is_empty() {
                                    ex.text.push('\n');
                                }
                                ex.text.push_str(&a.text);
                            }
                            ex.thinking.push_str(&a.thinking);
                            ex.is_error |= a.is_error;
                            if a.usage.output_tokens > ex.usage.output_tokens {
                                ex.usage = a.usage.clone(); // streamed partial → keep complete usage
                            }
                            // Only the final block of a streamed response carries it.
                            if !a.stop_reason.is_empty() {
                                ex.stop_reason = a.stop_reason.clone();
                            }
                        }
                        continue;
                    }
                    pos.insert(key, merged.len());
                }
            }
            merged.push(it);
        }
        out.items = merged;
    }

    fn default_home_dir(&self) -> &'static str {
        ".claude"
    }

    fn config_dir_env(&self) -> Option<&'static str> {
        Some("CLAUDE_CONFIG_DIR")
    }

    /// A `.claude` tree holds live OAuth credentials and full config dumps beside the
    /// session logs. Package only what `ssa` actually reads — `projects/**` logs and the
    /// sub-agent sidecars — and name the secrets explicitly so their exclusion is visible.
    fn classify(&self, rel: &str) -> Capture {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let seg = |s: &str| rel.split('/').any(|c| c == s);

        // Credential stores a naive `tar czf ~/.claude` would ship. Named exactly, so the
        // "excluded" list stays meaningful rather than flagging every file called
        // `token.rs` that the user happened to edit.
        let secret_store = name.starts_with(".credentials.json")
            || name.starts_with(".claude.json")
            || seg("backups")
            || seg("statsig")
            // Shell snapshots capture the environment, which routinely holds API keys.
            || seg("shell-snapshots");
        if secret_store {
            return Capture::Sensitive;
        }

        // Everything outside `projects/` is not session data. `history.jsonl` at the root
        // is deliberately excluded too: it records everything the user ever typed across
        // every project, far beyond the session being shared.
        if !seg("projects") {
            return Capture::Skip;
        }
        // Inside the capture scope, a secret-shaped filename is worth naming rather than
        // silently packing.
        let looks_secret = name.contains("credential")
            || name.contains("secret")
            || name.starts_with(".env")
            || name.ends_with(".pem")
            || name.ends_with(".key")
            || name.starts_with("id_rsa");
        if looks_secret {
            return Capture::Sensitive;
        }
        if name.ends_with(".jsonl") || name.ends_with(".meta.json") {
            return Capture::Include;
        }
        Capture::Skip
    }

    fn parse_line(&self, line: &str, source: &str, out: &mut Dataset) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                out.note_parse_error();
                return;
            }
        };
        let session_id = v.get("sessionId").and_then(Value::as_str).unwrap_or("unknown").to_string();
        out.note_source(&session_id, source);
        if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
            out.note_cwd(&session_id, cwd);
        }
        if let Some(ep) = v.get("entrypoint").and_then(Value::as_str) {
            out.note_entrypoint(&session_id, ep);
        }
        let ts_ms = v.get("timestamp").and_then(Value::as_str).map(parse_ts_ms).unwrap_or(0);
        // Sub-agent records live in their own file but carry the parent's sessionId, so the
        // thread a record belongs to is read per line and attached to every event it emits.
        let origin = Origin {
            is_sidechain: v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false),
            agent_id: v.get("agentId").and_then(Value::as_str).unwrap_or("").to_string(),
        };

        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => self.parse_assistant(&v, session_id, ts_ms, &origin, out),
            Some("user") => self.parse_user(&v, session_id, ts_ms, &origin, out),
            Some("system") => self.parse_system(&v, session_id, ts_ms, &origin, out),
            Some("attachment") => self.parse_attachment(&v, session_id, ts_ms, &origin, out),
            // The harness logs one enqueue/dequeue pair per run; `dequeue` is the moment a
            // prompt actually starts executing, so it is the authoritative run boundary.
            Some("queue-operation") => {
                if v.get("operation").and_then(Value::as_str) == Some("dequeue") {
                    out.push(session_id, ts_ms, &origin, ItemKind::RunStart);
                }
            }
            // Sidecar synthesized by the loader from `subagents/agent-*.meta.json`.
            Some("subagent-meta") => {
                let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                out.note_agent_meta(
                    &origin.agent_id,
                    AgentMeta { agent_type: s("agentType"), description: s("description"), tool_use_id: s("toolUseId") },
                );
            }
            Some("ai-title") => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    out.set_title(session_id, t);
                }
            }
            _ => {}
        }
    }
}

impl ClaudeCodeProvider {
    fn parse_assistant(&self, v: &Value, session_id: String, ts_ms: i64, origin: &Origin, out: &mut Dataset) {
        let msg = match v.get("message") {
            Some(m) => m,
            None => return,
        };
        let usage = msg.get("usage").map(parse_usage).unwrap_or_default();
        let model = msg.get("model").and_then(Value::as_str).unwrap_or("unknown").to_string();
        if let Some(tier) = msg.get("usage").and_then(|u| u.get("service_tier")).and_then(Value::as_str) {
            out.note_service_tier(&session_id, tier);
        }

        let mut tools = Vec::new();
        let mut text = String::new();
        let mut thinking = String::new();
        if let Some(content) = msg.get("content").and_then(Value::as_array) {
            for c in content {
                match c.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = c.get("name").and_then(Value::as_str).unwrap_or("?");
                        let input = c.get("input").cloned().unwrap_or(Value::Null);
                        tools.push(ToolUse {
                            id: c.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
                            name: name.to_string(),
                            target: summarize_input(name, &input),
                            input_full: serde_json::to_string_pretty(&input).unwrap_or_default(),
                        });
                    }
                    Some("text") => {
                        if let Some(t) = c.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(t);
                        }
                    }
                    Some("thinking") => {
                        if let Some(t) = c.get("thinking").and_then(Value::as_str) {
                            thinking.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
        }

        out.push(
            session_id,
            ts_ms,
            origin,
            ItemKind::Assistant(AssistantMsg {
                model,
                usage,
                thinking,
                text,
                tools,
                is_error: v.get("isApiErrorMessage").and_then(Value::as_bool).unwrap_or(false),
                stop_reason: msg.get("stop_reason").and_then(Value::as_str).unwrap_or("").to_string(),
                // requestId identifies one billed API call; the SDK re-logs the same
                // response (identical usage) on resume/compaction, so we dedup on it.
                request_id: v.get("requestId").and_then(Value::as_str)
                    .or_else(|| msg.get("id").and_then(Value::as_str))
                    .unwrap_or("").to_string(),
            }),
        );
    }

    fn parse_user(&self, v: &Value, session_id: String, ts_ms: i64, origin: &Origin, out: &mut Dataset) {
        let is_meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false);

        if let Some(c) = v.get("message").and_then(|m| m.get("content")) {
            match c {
                Value::String(s) => {
                    out.push(
                        session_id.clone(),
                        ts_ms,
                        origin,
                        ItemKind::User(UserMsg { text: s.clone(), is_prompt: !is_meta }),
                    );
                }
                Value::Array(arr) => {
                    for b in arr {
                        match b.get("type").and_then(Value::as_str) {
                            Some("tool_result") => {
                                out.push(
                                    session_id.clone(),
                                    ts_ms,
                                    origin,
                                    ItemKind::ToolResult(ToolResultRec {
                                        tool_use_id: b
                                            .get("tool_use_id")
                                            .and_then(Value::as_str)
                                            .unwrap_or("")
                                            .to_string(),
                                        content: tool_result_text(b.get("content")),
                                        is_error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                                    }),
                                );
                            }
                            Some("text") => {
                                // Rare: a prompt delivered as a text block.
                                if let Some(t) = b.get("text").and_then(Value::as_str) {
                                    out.push(
                                        session_id.clone(),
                                        ts_ms,
                                        origin,
                                        ItemKind::User(UserMsg { text: t.to_string(), is_prompt: !is_meta }),
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        // Rich sub-agent result attached to an Agent tool call.
        if let Some(tur) = v.get("toolUseResult") {
            if tur.get("agentType").is_some() || tur.get("totalTokens").is_some() {
                let usage = tur.get("usage").map(parse_usage).unwrap_or_default();
                let stats = tur.get("toolStats");
                let sg = |k: &str| stats.and_then(|s| s.get(k)).and_then(Value::as_u64).unwrap_or(0);
                out.push(
                    session_id,
                    ts_ms,
                    origin,
                    ItemKind::Subagent(SubagentResult {
                        agent_type: tur.get("agentType").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                        agent_id: tur.get("agentId").and_then(Value::as_str).unwrap_or("").to_string(),
                        // The Agent `tool_use` this result answers — the sub-agent's anchor
                        // in the parent transcript.
                        tool_use_id: first_tool_use_id(v),
                        model: String::new(), // inferred from the invoking turn during analysis

                        total_tokens: tur.get("totalTokens").and_then(Value::as_u64).unwrap_or(0),
                        tool_use_count: tur.get("totalToolUseCount").and_then(Value::as_u64).unwrap_or(0),
                        duration_ms: tur.get("totalDurationMs").and_then(Value::as_u64).unwrap_or(0),
                        end_ms: 0, // set in analysis from the item timestamp
                        usage,
                        read_count: sg("readCount"),
                        search_count: sg("searchCount"),
                        bash_count: sg("bashCount"),
                        edit_count: sg("editFileCount"),
                        lines_added: sg("linesAdded"),
                        lines_removed: sg("linesRemoved"),
                    }),
                );
            }
        }
    }

    /// Harness `attachment` records. Pure context-plumbing deltas are skipped (they carry
    /// no conversational signal and would flood the transcript); everything else — known or
    /// not — is kept, so a new attachment type surfaces instead of vanishing.
    fn parse_attachment(&self, v: &Value, session_id: String, ts_ms: i64, origin: &Origin, out: &mut Dataset) {
        const PLUMBING: [&str; 3] = ["mcp_instructions_delta", "skill_listing", "agent_listing_delta"];
        let Some(att) = v.get("attachment") else { return };
        let subtype = att.get("type").and_then(Value::as_str).unwrap_or("unknown");
        // Not a message, but the only record of what the agent could reach — kept out of
        // the transcript and folded into a per-thread tool roster instead.
        if subtype == "deferred_tools_delta" {
            let names = |k: &str| {
                att.get(k)
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default()
            };
            out.push(
                session_id,
                ts_ms,
                origin,
                ItemKind::ToolRoster(ToolRosterDelta { added: names("addedNames"), removed: names("removedNames") }),
            );
            return;
        }
        if PLUMBING.contains(&subtype) {
            return;
        }
        out.push(
            session_id,
            ts_ms,
            origin,
            ItemKind::Event(HarnessEvent {
                subtype: subtype.to_string(),
                detail: attachment_detail(subtype, att),
                content: attachment_content(att),
                is_terminal: is_terminal_event(subtype),
                limit: attachment_limit(att),
            }),
        );
    }

    fn parse_system(&self, v: &Value, session_id: String, ts_ms: i64, origin: &Origin, out: &mut Dataset) {
        if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            if let Some(cm) = v.get("compactMetadata") {
                out.push(
                    session_id,
                    ts_ms,
                    origin,
                    ItemKind::Compact(CompactEvent {
                        trigger: cm.get("trigger").and_then(Value::as_str).unwrap_or("").to_string(),
                        pre_tokens: cm.get("preTokens").and_then(Value::as_u64).unwrap_or(0),
                        post_tokens: cm.get("postTokens").and_then(Value::as_u64).unwrap_or(0),
                        duration_ms: cm.get("durationMs").and_then(Value::as_u64).unwrap_or(0),
                    }),
                );
            }
        }
    }
}

/// Events that end a run rather than just annotating it. Matched by substring so a new
/// limit/abort variant is still recognised.
fn is_terminal_event(subtype: &str) -> bool {
    const TERMINAL: [&str; 5] = ["max_turns", "max_tokens", "abort", "interrupt", "cancel"];
    TERMINAL.iter().any(|t| subtype.contains(t))
}

/// One-line summary of an attachment payload, chosen per known subtype.
fn attachment_detail(subtype: &str, att: &Value) -> String {
    let n = |k: &str| att.get(k).and_then(Value::as_u64);
    let s = |k: &str| att.get(k).and_then(Value::as_str).unwrap_or("");
    match subtype {
        "max_turns_reached" => match (n("turnCount"), n("maxTurns")) {
            (Some(c), Some(m)) => format!("hit the turn limit: {c} of {m} turns"),
            _ => "hit the turn limit".into(),
        },
        "nested_memory" => format!("memory loaded: {}", s("path")),
        "hook_success" => format!("hook {} ok", s("hookName")),
        "hook_error" => format!("hook {} FAILED", s("hookName")),
        "task_reminder" => "task reminder injected".into(),
        _ => {
            // Unknown type: show whichever scalar fields it carries so it is still readable.
            match att.as_object() {
                Some(o) => o
                    .iter()
                    .filter(|(k, val)| k.as_str() != "type" && (val.is_number() || val.is_string() || val.is_boolean()))
                    .map(|(k, val)| format!("{k}={}", val.to_string().trim_matches('"').chars().take(40).collect::<String>()))
                    .take(4)
                    .collect::<Vec<_>>()
                    .join(" "),
                None => String::new(),
            }
        }
    }
}

/// `(value reached, configured cap)` for a limit attachment, whatever the field naming.
fn attachment_limit(att: &Value) -> Option<(u64, u64)> {
    let n = |k: &str| att.get(k).and_then(Value::as_u64);
    let used = n("turnCount").or_else(|| n("tokenCount")).or_else(|| n("count"))?;
    let cap = n("maxTurns").or_else(|| n("maxTokens")).or_else(|| n("max"))?;
    Some((used, cap))
}

/// Text an attachment injected into the context (hook output, memory file body).
fn attachment_content(att: &Value) -> String {
    match att.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(o @ Value::Object(_)) => o.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
        _ => String::new(),
    }
}

/// `tool_use_id` of the first `tool_result` block in a user record — the tool call this
/// record answers. Empty when the record carries no tool result.
fn first_tool_use_id(v: &Value) -> String {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .and_then(|arr| arr.iter().find_map(|b| b.get("tool_use_id").and_then(Value::as_str)))
        .unwrap_or("")
        .to_string()
}

fn parse_usage(v: &Value) -> Usage {
    let g = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cc = v.get("cache_creation");
    let ce = |k: &str| cc.and_then(|c| c.get(k)).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: g("input_tokens"),
        cache_creation_input_tokens: g("cache_creation_input_tokens"),
        cache_read_input_tokens: g("cache_read_input_tokens"),
        output_tokens: g("output_tokens"),
        ephemeral_5m: ce("ephemeral_5m_input_tokens"),
        ephemeral_1h: ce("ephemeral_1h_input_tokens"),
    }
}

/// Readable text of a tool_result's content.
///
/// Blocks are not always `text` — ToolSearch answers with `tool_reference`, other tools
/// return images or structured payloads. Rendering only `text` made those results print as
/// *nothing*, which reads as "the call failed silently" when in fact it succeeded. Every
/// block type now renders something.
fn tool_result_text(content: Option<&Value>) -> String {
    fn block(b: &Value) -> Option<String> {
        if let Some(t) = b.get("text").and_then(Value::as_str) {
            return Some(t.to_string());
        }
        match b.get("type").and_then(Value::as_str) {
            Some("tool_reference") => {
                Some(format!("→ tool made available: {}", b.get("tool_name").and_then(Value::as_str).unwrap_or("?")))
            }
            Some("image") => Some("(image)".into()),
            Some(other) => {
                // Unknown block: name it and show its scalar fields rather than nothing.
                let fields = b
                    .as_object()
                    .map(|o| {
                        o.iter()
                            .filter(|(k, v)| k.as_str() != "type" && !v.is_object() && !v.is_array())
                            .map(|(k, v)| format!("{k}={}", v.to_string().trim_matches('"').chars().take(60).collect::<String>()))
                            .take(4)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                Some(if fields.is_empty() { format!("({other})") } else { format!("({other}) {fields}") })
            }
            None => None,
        }
    }
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr.iter().filter_map(block).collect::<Vec<_>>().join("\n"),
        Some(v @ Value::Object(_)) => serde_json::to_string(v).unwrap_or_default(),
        _ => String::new(),
    }
}

/// One-line summary of a tool's principal argument.
fn summarize_input(name: &str, input: &Value) -> Option<String> {
    let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    match name {
        "Read" | "Write" | "Edit" | "NotebookEdit" => s("file_path"),
        "Bash" => s("command").map(|c| c.replace('\n', " ").chars().take(80).collect()),
        "Grep" => s("pattern"),
        "Glob" => s("pattern"),
        "Agent" | "Task" => input
            .get("subagent_type")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| s("description")),
        "Skill" => s("skill"),
        _ => s("file_path").or_else(|| s("path")).or_else(|| s("query")),
    }
}
