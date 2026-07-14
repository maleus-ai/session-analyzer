//! Claude Code session-log provider (`.jsonl`).
//!
//! Each line is a JSON object. The format is heterogeneous and evolves, so we parse into
//! a `serde_json::Value` and defensively extract the fields we need.

use crate::model::*;
use crate::provider::Provider;
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
            if matches!(t, "assistant" | "user" | "ai-title" | "last-prompt" | "queue-operation") {
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

        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => self.parse_assistant(&v, session_id, ts_ms, out),
            Some("user") => self.parse_user(&v, session_id, ts_ms, out),
            Some("system") => self.parse_system(&v, session_id, ts_ms, out),
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
    fn parse_assistant(&self, v: &Value, session_id: String, ts_ms: i64, out: &mut Dataset) {
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
            ItemKind::Assistant(AssistantMsg {
                model,
                usage,
                thinking,
                text,
                tools,
                is_error: v.get("isApiErrorMessage").and_then(Value::as_bool).unwrap_or(false),
                is_sidechain: v.get("isSidechain").and_then(Value::as_bool).unwrap_or(false),
                agent_id: v.get("agentId").and_then(Value::as_str).unwrap_or("").to_string(),
                // requestId identifies one billed API call; the SDK re-logs the same
                // response (identical usage) on resume/compaction, so we dedup on it.
                request_id: v.get("requestId").and_then(Value::as_str)
                    .or_else(|| msg.get("id").and_then(Value::as_str))
                    .unwrap_or("").to_string(),
            }),
        );
    }

    fn parse_user(&self, v: &Value, session_id: String, ts_ms: i64, out: &mut Dataset) {
        let is_meta = v.get("isMeta").and_then(Value::as_bool).unwrap_or(false);

        if let Some(c) = v.get("message").and_then(|m| m.get("content")) {
            match c {
                Value::String(s) => {
                    out.push(
                        session_id.clone(),
                        ts_ms,
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
                    ItemKind::Subagent(SubagentResult {
                        agent_type: tur.get("agentType").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                        agent_id: tur.get("agentId").and_then(Value::as_str).unwrap_or("").to_string(),
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

    fn parse_system(&self, v: &Value, session_id: String, ts_ms: i64, out: &mut Dataset) {
        if v.get("subtype").and_then(Value::as_str) == Some("compact_boundary") {
            if let Some(cm) = v.get("compactMetadata") {
                out.push(
                    session_id,
                    ts_ms,
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

/// Characters of a tool_result's content (string or array of text blocks).
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
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
