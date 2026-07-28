//! CLI (non-interactive) reporting. Every TUI view has a matching function here in
//! text / json / csv, so an agent can obtain exactly what the TUI shows without a TTY.

use crate::analysis::*;
use crate::model::{SubagentResult, Usage};
use crate::query;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fmt {
    Text,
    Json,
    Csv,
}


/// Resolved scope + options for one CLI invocation.
pub struct Ctx<'a> {
    pub a: &'a Analysis,
    pub session: Option<&'a SessionReport>,
    pub sort: Option<String>,
    pub desc: bool,
    pub top: usize,
    pub fmt: Fmt,
    /// Rolling-window size (hours) for the `rate` command.
    pub window_hours: f64,
    /// Case-insensitive substring filter (`--grep`).
    pub grep: Option<String>,
    /// Minimum-tokens filter (`--min-tokens`).
    pub min_tokens: u64,
    /// Model substring filter (`--model`).
    pub model: Option<String>,
    /// When the capture was last written (epoch millis, 0 = unknown). Reported as an
    /// observation next to the last logged record; `ssa` draws no conclusion from it.
    pub capture_written_ms: i64,
}

impl<'a> Ctx<'a> {
    fn metrics(&self) -> &Metrics {
        self.session.map(|s| &s.metrics).unwrap_or(&self.a.global)
    }
    fn cache_attr(&self) -> &[CacheContrib] {
        self.session.map(|s| s.cache_attr.as_slice()).unwrap_or(&self.a.global_cache_attr)
    }
    fn require_session(&self, cmd: &str) -> Result<&SessionReport> {
        self.session.ok_or_else(|| anyhow::anyhow!("`{}` requires --session <id>", cmd))
    }
}

fn out_json(v: Value) -> String {
    serde_json::to_string_pretty(&v).unwrap_or_default()
}

// ------------------------------------------------------------------ table utils

fn render_table(headers: &[&str], rows: &[Vec<String>], fmt: Fmt) -> String {
    match fmt {
        Fmt::Csv => {
            let mut s = String::new();
            let _ = writeln!(s, "{}", headers.join(","));
            for r in rows {
                let cells: Vec<String> = r.iter().map(|c| csv_escape(c)).collect();
                let _ = writeln!(s, "{}", cells.join(","));
            }
            s
        }
        _ => {
            let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
            for r in rows {
                for (i, c) in r.iter().enumerate() {
                    if i < widths.len() {
                        widths[i] = widths[i].max(c.chars().count());
                    }
                }
            }
            let mut s = String::new();
            let hdr: Vec<String> = headers.iter().enumerate().map(|(i, h)| pad(h, widths[i])).collect();
            let _ = writeln!(s, "{}", hdr.join("  "));
            for r in rows {
                let cells: Vec<String> = r.iter().enumerate().map(|(i, c)| pad(c, widths.get(i).copied().unwrap_or(0))).collect();
                let _ = writeln!(s, "{}", cells.join("  "));
            }
            s
        }
    }
}

fn pad(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len >= w { s.to_string() } else { format!("{}{}", s, " ".repeat(w - len)) }
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn hr(title: &str) -> String {
    format!("== {} {}\n", title, "=".repeat(60usize.saturating_sub(title.len())))
}

/// Prepend a section header for human formats; omit it for machine-readable CSV.
fn titled(fmt: Fmt, title: &str, body: String) -> String {
    if fmt == Fmt::Csv { body } else { format!("{}{}", hr(title), body) }
}

// ---------------------------------------------------------------------- overview

/// Distinct entrypoints in scope, with session counts.
fn entrypoint_summary(ctx: &Ctx) -> Value {
    let mut by: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    match ctx.session {
        Some(sr) => {
            *by.entry(if sr.entrypoint.is_empty() { "unknown".into() } else { sr.entrypoint.clone() }).or_default() += 1;
        }
        None => {
            for s in &ctx.a.sessions {
                *by.entry(if s.entrypoint.is_empty() { "unknown".into() } else { s.entrypoint.clone() }).or_default() += 1;
            }
        }
    }
    Value::Object(by.into_iter().map(|(k, v)| (k, json!(v))).collect())
}

fn entrypoint_summary_text(ctx: &Ctx) -> String {
    match ctx.session {
        Some(sr) => if sr.entrypoint.is_empty() { "unknown".into() } else { sr.entrypoint.clone() },
        None => {
            let mut by: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
            for s in &ctx.a.sessions {
                *by.entry(if s.entrypoint.is_empty() { "unknown" } else { s.entrypoint.as_str() }).or_default() += 1;
            }
            by.iter().map(|(k, v)| format!("{k}×{v}")).collect::<Vec<_>>().join(", ")
        }
    }
}

pub fn overview(ctx: &Ctx) -> String {
    let a = ctx.a;
    let g = ctx.metrics();
    let turns: Vec<&TurnPoint> = match ctx.session {
        Some(sr) => sr.timeline.iter().collect(),
        None => a.all_turns(),
    };
    let rr = rate_report(&turns, RATE_WINDOW_HOURS);
    if ctx.fmt == Fmt::Json {
        let models: serde_json::Map<String, Value> = g.models.iter().map(|(k, v)| (k.clone(), usage_json(v))).collect();
        let mut v = json!({
            "provider": a.provider,
            "sessions": a.sessions.len(),
            "assistant_turns": g.assistant_turns,
            "turns_main": g.turns_main,
            "turns_sidechain": g.turns_sidechain,
            "has_sidechain_detail": g.has_sidechain_detail,
            "fresh_tokens": g.usage.fresh(),
            "fresh_per_hour": rr.fresh_per_h.round(),
            "cache_write_per_hour": rr.cache_write_per_h.round(),
            "peak_window_fresh": rr.peak_window_fresh,
            "usage_main": usage_json(&g.usage_main),
            "usage_sidechain": usage_json(&g.usage_sidechain),
            "user_prompts": g.user_prompts,
            "subagent_prompts": g.subagent_prompts,
            "skipped_sessions": a.skipped_sessions,
            "capture_written_ms": ctx.capture_written_ms,
            "last_record_ms": match ctx.session {
                Some(sr) => sr.runs.iter().map(|r| r.end_ms).max().unwrap_or(0),
                None => ctx.a.sessions.iter().flat_map(|s| s.runs.iter()).map(|r| r.end_ms).max().unwrap_or(0),
            },
            "runs": match ctx.session {
                Some(sr) => sr.runs.len(),
                None => ctx.a.sessions.iter().map(|s| s.runs.len()).sum(),
            },
            "terminal_events": g.terminal_events.iter()
                .map(|(k, d)| json!({ "subtype": k, "detail": d })).collect::<Vec<_>>(),
            "tool_calls": g.tool_calls,
            "duration_ms": g.duration_ms,
            "active_ms": rr.active_ms,
            "active_hours": round2(rr.active_ms as f64 / 3.6e6),
            "idle_gaps": rr.idle_gaps,
            "idle_ms": rr.idle_ms,
            "longest_gap_ms": rr.longest_gap_ms,
            "entrypoints": entrypoint_summary(ctx),
            "api_errors": g.api_errors,
            "context_peak": g.context_peak,
            "usage": usage_json(&g.usage),
            "estimated_cost_usd": round2(g.cost_usd),
            "cache_hit_rate": round4(g.cache_hit_rate()),
            "cache_churn": finite_or_null(g.cache_churn()),
            "subagent_count_finished": g.subagents.len(),
            "subagent_tokens": g.subagent_tokens(),
            "compactions": g.compactions.len(),
            "compaction_summarized_tokens": g.compactions.iter().map(|c| c.pre_tokens).sum::<u64>(),
            "compaction_cost_usd": round2(
                g.compactions.iter().map(|c| compaction_cost(c, &crate::pricing::price_for(&g.dominant_model()))).sum()
            ),
            "parse_errors": a.parse_errors,
            "by_model": Value::Object(models),
        });
        // Set outside the literal: `json!` hits its expansion-depth limit on an object this
        // large, and these are derived values anyway.
        let threads: usize = match ctx.session {
            Some(sr) => sr.threads.len(),
            None => ctx.a.sessions.iter().map(|s| s.threads.len()).sum(),
        };
        let depth = match ctx.session {
            Some(sr) => sr.threads.iter().map(|t| t.agent.depth).max().unwrap_or(0),
            None => ctx.a.sessions.iter().flat_map(|s| s.threads.iter()).map(|t| t.agent.depth).max().unwrap_or(0),
        };
        // Named apart so "1" cannot read as "delegation was minimal" beside a deep chain.
        v["subagent_count_seen"] = json!(threads);
        v["max_subagent_depth"] = json!(depth);
        // File mtimes are whole seconds, so a sub-second ordering against a log record is noise.
        v["capture_written"] =
            if ctx.capture_written_ms > 0 { json!(fmt_epoch_secs(ctx.capture_written_ms)) } else { Value::Null };
        v["capture_time_resolution_ms"] = json!(1000);
        return out_json(v);
    }
    let mut s = hr("OVERVIEW");
    let _ = writeln!(s, "Provider          : {}", a.provider);
    let nproj = a.projects().len();
    if nproj > 1 {
        let _ = writeln!(s, "Projects          : {}", nproj);
    }
    let _ = writeln!(s, "Sessions analyzed : {}", a.sessions.len());
    let _ = writeln!(s, "Harness (entry)   : {}", entrypoint_summary_text(ctx));
    if let Some(sr) = ctx.session {
        let start = sr.timeline.iter().map(|t| t.ts_ms).filter(|t| *t > 0).min().unwrap_or(0);
        let end = sr.timeline.iter().map(|t| t.ts_ms).max().unwrap_or(0);
        let _ = writeln!(s, "Service tier      : {}", if sr.service_tier.is_empty() { "-" } else { &sr.service_tier });
        let _ = writeln!(s, "Model (dominant)  : {}", short_model(&sr.metrics.dominant_model()));
        let _ = writeln!(s, "Time (UTC)        : {} → {}", fmt_epoch(start), fmt_epoch(end));
    }
    let _ = writeln!(s, "Wall-clock span   : {}", g.duration_human());
    // Say how much of the span was actually worked, and name the longest pause outright —
    // "active ≈ span" must not be readable as "continuous" while a 12-minute pause hides
    // under a 15-minute threshold.
    let _ = writeln!(
        s,
        "Active time       : {}   ({} paused, longest pause {}{})",
        human_ms(rr.active_ms),
        human_ms(rr.idle_ms),
        human_ms(rr.longest_gap_ms),
        if rr.idle_gaps > 0 { format!("; {} gap(s) > 15m", rr.idle_gaps) } else { String::new() }
    );
    let _ = writeln!(s, "Peak turns/min    : {}   (max concurrent subagents: {})", fmt_int(rr.peak_turns_per_min), g.max_concurrent_subagents);
    let _ = writeln!(s, "Assistant turns   : {}", fmt_int(g.assistant_turns));
    // How the work ended, up front: a run that hit a limit or stopped early explains far more
    // about a session than any token aggregate does.
    let runs_in_scope: Vec<&RunSegment> = match ctx.session {
        Some(sr) => sr.runs.iter().collect(),
        None => ctx.a.sessions.iter().flat_map(|s| s.runs.iter()).collect(),
    };
    if !runs_in_scope.is_empty() {
        let mut by: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for r in &runs_in_scope {
            *by.entry(r.outcome.label()).or_default() += 1;
        }
        let summary = by.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", ");
        let _ = writeln!(s, "Runs              : {}   ({})   [ssa runs]", runs_in_scope.len(), summary);
        for r in runs_in_scope.iter().filter(|r| !r.outcome_detail.is_empty()) {
            let _ = writeln!(s, "                    run {} — {}", r.index, r.outcome_detail);
        }
    }
    let _ = writeln!(
        s,
        "User prompts      : {}{}",
        fmt_int(g.user_prompts),
        if g.subagent_prompts > 0 { format!("   (+{} sub-agent task prompts)", fmt_int(g.subagent_prompts)) } else { String::new() }
    );
    let _ = writeln!(s, "Tool calls        : {}", fmt_int(g.tool_calls));
    let _ = writeln!(s, "Context peak      : {}", fmt_int(g.context_peak));
    if a.parse_errors > 0 {
        let _ = writeln!(s, "Unparsed lines    : {}", a.parse_errors);
    }
    // Archives preserve file mtimes, so the newest mtime is normally just the last log
    // write — it does not independently date the capture. Only worth showing when it is
    // meaningfully later, which means files were touched after the session ended.
    if ctx.capture_written_ms > 0 {
        let last = runs_in_scope.iter().map(|r| r.end_ms).max().unwrap_or(0);
        if last > 0 && ctx.capture_written_ms - last > 60_000 {
            let _ = writeln!(
                s,
                "Files touched     : {}   ({} after the last log record)",
                fmt_epoch_secs(ctx.capture_written_ms),
                fmt_dur_ms(ctx.capture_written_ms - last)
            );
        }
    }
    if !a.skipped_sessions.is_empty() {
        let _ = writeln!(
            s,
            "Skipped sessions  : {}   (no assistant turns: {})",
            a.skipped_sessions.len(),
            a.skipped_sessions.iter().map(|i| i.chars().take(8).collect::<String>()).collect::<Vec<_>>().join(", ")
        );
    }
    let _ = writeln!(s);
    // Lead with the limit-driving figures.
    let _ = writeln!(s, "Cache write       : {:>14}   ({} /h — subscription-limit driver)", fmt_int(g.usage.cache_creation_input_tokens), per_h(rr.cache_write_per_h));
    let _ = writeln!(s, "Fresh tokens      : {:>14}   ({} /h; = input+write+output)", fmt_int(g.usage.fresh()), per_h(rr.fresh_per_h));
    let _ = writeln!(s, "Output            : {:>14}", fmt_int(g.usage.output_tokens));
    let _ = writeln!(s, "Input (fresh)     : {:>14}", fmt_int(g.usage.input_tokens));
    let _ = writeln!(s, "Cache read        : {:>14}   (replayed, cheap — bulk of total)", fmt_int(g.usage.cache_read_input_tokens));
    let _ = writeln!(s, "TOTAL processed   : {:>14}", fmt_int(g.usage.total()));
    let _ = writeln!(s, "Est. cost         : {:>13} USD  (${:.0}/h)", format!("${:.2}", g.cost_usd), rr.cost_per_h);
    let _ = writeln!(s, "\nCache hit rate    : {:.1}%", g.cache_hit_rate() * 100.0);
    let churn = g.cache_churn();
    let _ = writeln!(s, "Cache churn       : {}", if churn.is_finite() { format!("{:.2}x", churn) } else { "∞".into() });
    // Main vs sub-agent split.
    if g.has_sidechain_detail {
        let _ = writeln!(
            s,
            "Main / sub-agent  : {} main turns ({} tok) · {} sub-agent turns ({} tok)",
            fmt_int(g.turns_main), fmt_int(g.usage_main.total()),
            fmt_int(g.turns_sidechain), fmt_int(g.usage_sidechain.total())
        );
    } else if !g.subagents.is_empty() {
        let _ = writeln!(s, "Sub-agents        : {} (~{} tok reported; full sidechain detail not in this input)", g.subagents.len(), fmt_int(g.subagent_tokens()));
    }
    if !g.subagents.is_empty() && g.has_sidechain_detail {
        // `subagents` counts only agents that returned a result. Showing that alone next
        // to a 54-deep runaway chain is the wrong impression to leave on a summary screen.
        let threads: usize = match ctx.session {
            Some(sr) => sr.threads.len(),
            None => ctx.a.sessions.iter().map(|s| s.threads.len()).sum(),
        };
        if threads > g.subagents.len() {
            let _ = writeln!(
                s,
                "Sub-agents        : {} finished / {} total   [ssa agents]",
                g.subagents.len(),
                threads
            );
        } else {
            let _ = writeln!(s, "Sub-agents        : {}", g.subagents.len());
        }
    }
    if !g.compactions.is_empty() {
        let price = crate::pricing::price_for(&g.dominant_model());
        let cost: f64 = g.compactions.iter().map(|c| compaction_cost(c, &price)).sum();
        let pre: u64 = g.compactions.iter().map(|c| c.pre_tokens).sum();
        let _ = writeln!(
            s,
            "Compactions       : {}  ({} tok summarized → ~${:.2} est. overhead, NOT in totals above)",
            g.compactions.len(), fmt_int(pre), cost
        );
    }
    if g.models.len() > 1 {
        let _ = writeln!(s, "\nBy model:");
        let mut models: Vec<_> = g.models.iter().collect();
        models.sort_by(|a, b| b.1.total().cmp(&a.1.total()));
        for (name, u) in models {
            let _ = writeln!(s, "  {:22} {} tokens", name, fmt_int(u.total()));
        }
    }
    s
}

// -------------------------------------------------------------------------- rate

pub fn rate(ctx: &Ctx) -> String {
    let turns: Vec<&TurnPoint> = match ctx.session {
        Some(sr) => sr.timeline.iter().collect(),
        None => ctx.a.all_turns(),
    };
    let r = rate_report(&turns, ctx.window_hours);
    if ctx.fmt == Fmt::Json {
        return out_json(json!({
            "turns": r.turns,
            "span_ms": r.span_ms,
            "total_fresh_tokens": r.total_fresh,
            "total_cache_write": r.total_cache_write,
            "total_output": r.total_output,
            "total_cost_usd": round2(r.total_cost),
            "fresh_per_hour": r.fresh_per_h.round(),
            "cache_write_per_hour": r.cache_write_per_h.round(),
            "output_per_hour": r.output_per_h.round(),
            "cost_per_hour": round2(r.cost_per_h),
            "window_hours": r.window_hours,
            "active_ms": r.active_ms,
            "active_hours": round2(r.active_ms as f64 / 3.6e6),
            "idle_gaps": r.idle_gaps,
            "peak_window_fresh": r.peak_window_fresh,
            "peak_window_cost_usd": round2(r.peak_window_cost),
            "peak_window_start_ms": r.peak_window_start_ms,
            "peak_burst_fresh": r.peak_burst_fresh,
            "peak_burst_cost_usd": round2(r.peak_burst_cost),
            "peak_fresh_per_min": r.peak_fresh_per_min,
            "peak_turns_per_min": r.peak_turns_per_min,
            "peak_rpm": r.peak_rpm,
            "peak_itpm": r.peak_itpm,
            "peak_otpm": r.peak_otpm,
            "max_concurrent_subagents": ctx.metrics().max_concurrent_subagents,
            "bucket_ms": r.bucket_ms,
            "buckets": r.buckets.iter().map(|b| json!({
                "start_ms": b.start_ms, "fresh": b.fresh, "cache_write": b.cache_write,
                "output": b.output, "cost_usd": round2(b.cost), "turns": b.turns,
            })).collect::<Vec<_>>(),
        }));
    }
    if r.turns == 0 || r.span_ms == 0 {
        return hr("RATE") + "Not enough timestamped turns to compute rates.\n";
    }
    let mut s = hr("RATE — throughput, continuity & window pressure");
    let _ = writeln!(s, "Span (first→last) : {}", human_ms(r.span_ms));
    let _ = writeln!(
        s,
        "Active time       : {}   ({} paused, longest pause {})",
        human_ms(r.active_ms),
        human_ms(r.idle_ms),
        human_ms(r.longest_gap_ms)
    );
    let _ = writeln!(s, "Turns             : {}", fmt_int(r.turns));
    let _ = writeln!(s, "Fresh tokens      : {}  (input + cache-write + output)", fmt_int(r.total_fresh));
    let _ = writeln!(s, "  cache write     : {}", fmt_int(r.total_cache_write));
    let _ = writeln!(s, "  output          : {}", fmt_int(r.total_output));
    let _ = writeln!(s, "Cost (list price) : ${:.2}", r.total_cost);
    let _ = writeln!(s, "\n--- RATES (per ACTIVE hour) ---");
    let ah = (r.active_ms as f64 / 3.6e6).max(1.0 / 60.0);
    let _ = writeln!(s, "Fresh /active-h   : {}", per_h(r.total_fresh as f64 / ah));
    let _ = writeln!(s, "Cache-write /a-h  : {}", per_h(r.total_cache_write as f64 / ah));
    let _ = writeln!(s, "(over full span: fresh {}/h, cache-write {}/h)", per_h(r.fresh_per_h), per_h(r.cache_write_per_h));
    let _ = writeln!(s, "\n--- PEAK {}h WINDOW ---", fmt_num(r.window_hours));
    let _ = writeln!(
        s,
        "Continuous burst  : {}  (${:.2})   ← honest: no idle gap inside the window",
        fmt_int(r.peak_burst_fresh),
        r.peak_burst_cost
    );
    let _ = writeln!(
        s,
        "Naive rolling     : {}  (${:.2})   (may smear across idle gaps — compare to burst)",
        fmt_int(r.peak_window_fresh),
        r.peak_window_cost
    );
    let _ = writeln!(s, "\n--- INSTANTANEOUS BURST (concurrency) ---");
    let _ = writeln!(s, "Peak fresh / minute : {}", fmt_int(r.peak_fresh_per_min));
    let _ = writeln!(s, "Peak turns / minute : {}", fmt_int(r.peak_turns_per_min));
    let _ = writeln!(s, "Max concurrent subagents : {}", ctx.metrics().max_concurrent_subagents);
    let _ = writeln!(s, "\n--- PEAK RATE-LIMITER LOAD (busiest 60s; Anthropic's three limiters) ---");
    let _ = writeln!(s, "RPM  (requests / min)      : {}", fmt_int(r.peak_rpm));
    let _ = writeln!(s, "ITPM (input tok / min)     : {}   (uncached input + cache-write; cache-read excluded)", fmt_int(r.peak_itpm));
    let _ = writeln!(s, "OTPM (output tok / min)    : {}", fmt_int(r.peak_otpm));
    let series: Vec<u64> = r.buckets.iter().map(|b| b.fresh).collect();
    let _ = writeln!(s, "\nfresh tokens over time ({} per bucket):", human_ms(r.bucket_ms));
    let _ = writeln!(s, "{}", sparkline(&series));
    s
}

fn per_h(v: f64) -> String {
    if v >= 1_000_000.0 {
        format!("{:.2}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.0}k", v / 1_000.0)
    } else {
        format!("{v:.0}")
    }
}

fn fmt_num(f: f64) -> String {
    if (f.fract()).abs() < 1e-9 { format!("{}", f as i64) } else { format!("{f}") }
}

fn human_ms(ms: i64) -> String {
    let mut s = ms / 1000;
    if s <= 0 {
        return "0s".into();
    }
    let h = s / 3600;
    s %= 3600;
    let m = s / 60;
    s %= 60;
    if h > 0 { format!("{h}h {m}m") } else if m > 0 { format!("{m}m {s}s") } else { format!("{s}s") }
}

// ------------------------------------------------------------------------ models

pub fn models(ctx: &Ctx) -> String {
    let m = ctx.metrics();
    let mut rows: Vec<(&String, &Usage, u64, f64)> = m
        .models
        .iter()
        .map(|(name, u)| {
            let turns = m.model_turns.get(name).copied().unwrap_or(0);
            let cost = crate::pricing::price_for(name).cost(u);
            (name, u, turns, cost)
        })
        .collect();
    rows.sort_by(|a, b| b.3.total_cmp(&a.3));
    if ctx.fmt == Fmt::Json {
        return out_json(Value::Array(rows.iter().map(|(name, u, turns, cost)| json!({
            "model": name, "turns": turns, "usage": usage_json(u),
            "fresh_tokens": u.fresh(), "estimated_cost_usd": round2(*cost),
        })).collect()));
    }
    let headers = ["MODEL", "TURNS", "TOTAL_TOK", "FRESH_TOK", "OUTPUT", "COST"];
    let out_rows: Vec<Vec<String>> = rows.iter().map(|(name, u, turns, cost)| vec![
        (*name).clone(), turns.to_string(), fmt_int(u.total()), fmt_int(u.fresh()),
        fmt_int(u.output_tokens), format!("${:.2}", cost),
    ]).collect();
    titled(ctx.fmt, "MODELS", render_table(&headers, &out_rows, ctx.fmt))
}

// ----------------------------------------------------------------------- compare

pub fn compare(a: &Analysis, sels: &[String], fmt: Fmt) -> Result<String> {
    let mut reports: Vec<&SessionReport> = Vec::new();
    for sel in sels {
        let matches: Vec<&SessionReport> = a.sessions.iter().filter(|s| s.session_id.starts_with(sel.as_str())).collect();
        match matches.as_slice() {
            [one] => reports.push(one),
            [] => bail!("no session matched '{}'", sel),
            many => bail!("'{}' matched {} sessions; use a longer prefix", sel, many.len()),
        }
    }
    if fmt == Fmt::Json {
        // Enrich each session with the rate/continuity fields (SKILL promises them here).
        return Ok(out_json(Value::Array(reports.iter().map(|s| {
            let r = rate_report(&s.timeline.iter().collect::<Vec<_>>(), RATE_WINDOW_HOURS);
            let mut j = session_json(s);
            j["fresh_per_hour"] = json!(r.fresh_per_h.round());
            j["cache_write_per_hour"] = json!(r.cache_write_per_h.round());
            j["peak_window_fresh"] = json!(r.peak_window_fresh);
            j["peak_burst_fresh"] = json!(r.peak_burst_fresh);
            j["peak_turns_per_min"] = json!(r.peak_turns_per_min);
            j["max_concurrent_subagents"] = json!(s.metrics.max_concurrent_subagents);
            j["peak_rpm"] = json!(r.peak_rpm);
            j["peak_itpm"] = json!(r.peak_itpm);
            j["peak_otpm"] = json!(r.peak_otpm);
            j
        }).collect())));
    }
    // Metric rows × session columns.
    let mut headers = vec!["METRIC".to_string()];
    for s in &reports {
        headers.push(s.session_id.chars().take(8).collect());
    }
    let hrefs: Vec<&str> = headers.iter().map(|s| s.as_str()).collect();
    let metric = |name: &str, f: &dyn Fn(&SessionReport) -> String| -> Vec<String> {
        let mut row = vec![name.to_string()];
        for s in &reports {
            row.push(f(s));
        }
        row
    };
    let rr = |s: &SessionReport| rate_report(&s.timeline.iter().collect::<Vec<_>>(), RATE_WINDOW_HOURS);
    let rows = vec![
        metric("title", &|s| truncate(&s.title, 22)),
        metric("entrypoint", &|s| if s.entrypoint.is_empty() { "-".into() } else { s.entrypoint.clone() }),
        metric("model", &|s| short_model(&s.metrics.dominant_model())),
        metric("service tier", &|s| if s.service_tier.is_empty() { "-".into() } else { s.service_tier.clone() }),
        metric("cost", &|s| format!("${:.2}", s.metrics.cost_usd)),
        metric("fresh tok", &|s| fmt_int(s.metrics.usage.fresh())),
        metric("turns m/sub", &|s| format!("{}/{}", s.metrics.turns_main, s.metrics.turns_sidechain)),
        metric("span", &|s| s.metrics.duration_human()),
        metric("active h", &|s| format!("{:.1}", s.metrics.active_ms as f64 / 3.6e6)),
        metric("idle gaps", &|s| s.metrics.idle_gaps.to_string()),
        metric("fresh /h", &|s| freshph(&rr(s))),
        metric("burst 5h fresh", &|s| fmt_int(rr(s).peak_burst_fresh)),
        metric("peak turns/min", &|s| fmt_int(rr(s).peak_turns_per_min)),
        metric("max concurrent", &|s| s.metrics.max_concurrent_subagents.to_string()),
        metric("peak RPM", &|s| fmt_int(rr(s).peak_rpm)),
        metric("peak ITPM", &|s| fmt_int(rr(s).peak_itpm)),
        metric("peak OTPM", &|s| fmt_int(rr(s).peak_otpm)),
    ];
    Ok(hr("COMPARE") + &render_table(&hrefs, &rows, fmt))
}

fn freshph(r: &RateReport) -> String {
    per_h(r.fresh_per_h)
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

// ---------------------------------------------------------------------- projects

pub fn projects(ctx: &Ctx) -> String {
    let mut list = ctx.a.projects();
    list.truncate(ctx.top);
    if ctx.fmt == Fmt::Json {
        return out_json(Value::Array(list.iter().map(|p| json!({
            "project": p.project, "sessions": p.sessions, "estimated_cost_usd": round2(p.cost_usd),
            "usage": usage_json(&p.usage), "assistant_turns": p.assistant_turns,
            "tool_calls": p.tool_calls, "subagent_tokens": p.subagent_tokens, "compactions": p.compactions,
        })).collect()));
    }
    let headers = ["COST", "SESS", "TOTAL_TOK", "CACHE_READ", "TURNS", "TOOLS", "PROJECT"];
    let rows: Vec<Vec<String>> = list.iter().map(|p| vec![
        format!("${:.2}", p.cost_usd), p.sessions.to_string(), fmt_int(p.usage.total()),
        fmt_int(p.usage.cache_read_input_tokens), fmt_int(p.assistant_turns), fmt_int(p.tool_calls),
        p.project.clone(),
    ]).collect();
    titled(ctx.fmt, "PROJECTS (by cost)", render_table(&headers, &rows, ctx.fmt))
}

// ---------------------------------------------------------------------- sessions

pub fn sessions(ctx: &Ctx) -> Result<String> {
    let mut list: Vec<&SessionReport> = ctx.a.sessions.iter().collect();
    if let Some(g) = &ctx.grep {
        let g = g.to_lowercase();
        list.retain(|s| s.title.to_lowercase().contains(&g) || s.cwd.to_lowercase().contains(&g));
    }
    let col = ctx.sort.as_deref().unwrap_or("cost");
    query::sort_sessions(&mut list, col, ctx.desc)?;
    list.truncate(ctx.top);

    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(list.iter().map(|s| session_json(s)).collect())));
    }
    let headers = ["COST", "TOTAL_TOK", "CW/H", "ACTIVE_H", "IDLE", "ENTRY", "MODEL", "SESSION", "TITLE"];
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|s| {
            let m = &s.metrics;
            let r = rate_report(&s.timeline.iter().collect::<Vec<_>>(), RATE_WINDOW_HOURS);
            vec![
                format!("${:.2}", m.cost_usd),
                fmt_int(m.usage.total()),
                per_h(r.cache_write_per_h),
                format!("{:.1}", r.active_ms as f64 / 3.6e6),
                r.idle_gaps.to_string(),
                if s.entrypoint.is_empty() { "-".into() } else { s.entrypoint.clone() },
                short_model(&m.dominant_model()),
                s.session_id.chars().take(8).collect(),
                s.title.clone(),
            ]
        })
        .collect();
    Ok(titled(ctx.fmt, "SESSIONS", render_table(&headers, &rows, ctx.fmt)))
}

// ------------------------------------------------------------------------ pressure

/// Rank sessions by sustained cache-write throughput — the best single predictor of
/// subscription-window exhaustion — and flag the unattended-burst outliers
/// (sdk-ts harness with no idle gaps concentrating load into one rolling window).
pub fn pressure(ctx: &Ctx) -> String {
    let mut rows: Vec<(&SessionReport, RateReport)> = ctx
        .a
        .sessions
        .iter()
        .map(|s| (s, rate_report(&s.timeline.iter().collect::<Vec<_>>(), RATE_WINDOW_HOURS)))
        .collect();
    rows.sort_by(|a, b| b.1.cache_write_per_h.total_cmp(&a.1.cache_write_per_h));
    rows.truncate(ctx.top);

    let flag = |s: &SessionReport, r: &RateReport| -> &'static str {
        // Continuous, machine-driven burst: headless harness with no human pauses.
        // "Unattended" must mean no human pause at all — not merely none longer than the
        // 15-minute rate-window threshold. A session with a 12-minute think is attended.
        if s.entrypoint.contains("sdk") && r.idle_ms == 0 && r.active_ms > 300_000 { "BURST" } else { "" }
    };

    if ctx.fmt == Fmt::Json {
        return out_json(Value::Array(rows.iter().map(|(s, r)| json!({
            "session_id": s.session_id, "title": s.title, "entrypoint": s.entrypoint,
            "cache_write_per_hour": r.cache_write_per_h.round(), "fresh_per_hour": r.fresh_per_h.round(),
            "peak_burst_fresh": r.peak_burst_fresh, "active_hours": round2(r.active_ms as f64 / 3.6e6),
            "idle_gaps": r.idle_gaps, "peak_turns_per_min": r.peak_turns_per_min,
            "unattended_burst": !flag(s, r).is_empty(),
        })).collect()));
    }
    let headers = ["CW/H", "FRESH/H", "BURST5H", "ACTIVE_H", "IDLE", "T/MIN", "ENTRY", "FLAG", "SESSION", "TITLE"];
    let out: Vec<Vec<String>> = rows.iter().map(|(s, r)| vec![
        per_h(r.cache_write_per_h), per_h(r.fresh_per_h), fmt_int(r.peak_burst_fresh),
        format!("{:.1}", r.active_ms as f64 / 3.6e6), r.idle_gaps.to_string(), r.peak_turns_per_min.to_string(),
        if s.entrypoint.is_empty() { "-".into() } else { s.entrypoint.clone() },
        flag(s, r).to_string(), s.session_id.chars().take(8).collect(), truncate(&s.title, 36),
    ]).collect();
    titled(
        ctx.fmt,
        "SUBSCRIPTION PRESSURE (by sustained cache-write/hour; BURST = sdk run with no pause > 5m)",
        render_table(&headers, &out, ctx.fmt),
    )
}

// ------------------------------------------------------------------------- tools

/// What the agent *could* do, not just what it did.
///
/// The deferred registry (tools loadable on demand via `ToolSearch`) plus every tool
/// actually called, and — the point of the view — every tool an agent searched for and
/// could not get. "Was Bash available?" is otherwise answerable only by inference from an
/// absence of calls, which is not the same thing.
pub fn tools_available(ctx: &Ctx) -> Result<String> {
    let m = ctx.metrics();
    let called: std::collections::BTreeMap<&str, u64> = m.tools.iter().map(|t| (t.name.as_str(), t.calls)).collect();
    let mut names: std::collections::BTreeSet<&str> = m.deferred_tools.iter().map(String::as_str).collect();
    names.extend(called.keys().copied());

    if ctx.fmt == Fmt::Json {
        return Ok(out_json(json!({
            "deferred_registry_size": m.deferred_tools.len(),
            "tools": names.iter().map(|n| json!({
                "name": n,
                "deferred": m.deferred_tools.contains(*n),
                "calls": called.get(*n).copied().unwrap_or(0),
            })).collect::<Vec<_>>(),
            "unavailable": m.tools_unavailable.iter()
                .map(|(n, c)| json!({ "name": n, "searched": c })).collect::<Vec<_>>(),
        })));
    }
    let headers = ["TOOL", "DEFERRED", "CALLS"];
    let rows: Vec<Vec<String>> = names
        .iter()
        .map(|n| {
            vec![
                (*n).to_string(),
                if m.deferred_tools.contains(*n) { "yes".into() } else { "-".to_string() },
                called.get(*n).copied().unwrap_or(0).to_string(),
            ]
        })
        .collect();
    let mut s = titled(ctx.fmt, "TOOL AVAILABILITY", render_table(&headers, &rows, ctx.fmt));
    if ctx.fmt == Fmt::Csv {
        return Ok(s);
    }
    if m.deferred_tools.is_empty() {
        let _ = writeln!(s, "\nNo deferred-tool registry recorded in this log — an empty roster here means");
        let _ = writeln!(s, "'not logged', NOT 'no tools available'.");
    } else {
        let _ = writeln!(s, "\nDeferred registry: {} tool(s) loadable on demand via ToolSearch.", m.deferred_tools.len());
    }
    if !m.tools_unavailable.is_empty() {
        let _ = writeln!(s, "\nREQUESTED BUT UNAVAILABLE — the harness had no such tool:");
        for (name, n) in &m.tools_unavailable {
            let _ = writeln!(s, "  ✗ {name}  (searched {n}×, never provided)");
        }
        let _ = writeln!(s, "An agent that cannot reach a capability will improvise around it — this is");
        let _ = writeln!(s, "the usual root cause behind workaround loops and repeated delegation.");
    }
    Ok(s)
}

pub fn tools(ctx: &Ctx) -> Result<String> {
    let mut list = ctx.metrics().tools.clone();
    if ctx.min_tokens > 0 {
        list.retain(|t| t.result_tokens_est >= ctx.min_tokens);
    }
    let col = ctx.sort.as_deref().unwrap_or("result");
    query::sort_tools(&mut list, col, ctx.desc)?;
    list.truncate(ctx.top);
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(list.iter().map(|t| json!({
            "tool": t.name, "calls": t.calls, "result_tokens_est": t.result_tokens_est,
            "input_chars": t.input_chars, "errors": t.errors,
        })).collect())));
    }
    let headers = ["TOOL", "CALLS", "RESULT_TOK", "IN_CHARS", "ERRORS"];
    let rows: Vec<Vec<String>> = list.iter().map(|t| vec![
        t.name.clone(), t.calls.to_string(), fmt_int(t.result_tokens_est), fmt_int(t.input_chars), t.errors.to_string(),
    ]).collect();
    Ok(titled(ctx.fmt, "TOOLS", render_table(&headers, &rows, ctx.fmt)))
}

// ------------------------------------------------------------------------- sinks

pub fn sinks(ctx: &Ctx) -> Result<String> {
    let mut list = query::sinks_only(ctx.cache_attr());
    if ctx.min_tokens > 0 {
        list.retain(|c| c.tokens >= ctx.min_tokens);
    }
    let col = ctx.sort.as_deref().unwrap_or("amplified");
    query::sort_sinks(&mut list, col, ctx.desc)?;
    list.truncate(ctx.top);
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(list.iter().map(cacheattr_json).collect())));
    }
    let headers = ["AMPL_COST", "SIZE_TOK", "REPLAYED_TOK", "SHARE%", "ENTRIES", "TOOL", "TARGET"];
    let rows: Vec<Vec<String>> = list.iter().map(|c| vec![
        format!("${:.2}", c.amplified_cost), fmt_int(c.tokens), fmt_int(c.contribution),
        format!("{:.1}", c.share * 100.0), c.entries.to_string(), c.tool.clone(), short_path(&c.target),
    ]).collect();
    Ok(titled(ctx.fmt, "TOKEN SINKS — amplified cost = size x turns resident (token counts are est.)", render_table(&headers, &rows, ctx.fmt)))
}

// -------------------------------------------------------------------- cache-attr

pub fn cache_attr(ctx: &Ctx) -> Result<String> {
    let mut list = ctx.cache_attr().to_vec();
    if ctx.min_tokens > 0 {
        list.retain(|c| c.contribution >= ctx.min_tokens || c.is_baseline);
    }
    let col = ctx.sort.as_deref().unwrap_or("contribution");
    query::sort_cacheattr(&mut list, col, ctx.desc)?;
    list.truncate(ctx.top);
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(list.iter().map(cacheattr_json).collect())));
    }
    let total = ctx.metrics().usage.cache_read_input_tokens;
    let headers = ["SHARE%", "REPLAYED_TOK", "COST", "ENTRIES", "SOURCE", "DETAIL"];
    let rows: Vec<Vec<String>> = list.iter().map(|c| vec![
        format!("{:.1}", c.share * 100.0), fmt_int(c.contribution), format!("${:.2}", c.amplified_cost),
        c.entries.to_string(), c.tool.clone(), short_path(&c.target),
    ]).collect();
    let table = render_table(&headers, &rows, ctx.fmt);
    if ctx.fmt == Fmt::Csv {
        return Ok(table);
    }
    Ok(format!("{}Decomposes {} cache-read tokens by resident content (token counts are est., normalized to reconcile).\n{}",
        hr("CACHE-READ ATTRIBUTION"), fmt_int(total), table))
}

// --------------------------------------------------------------------- subagents

pub fn subagents(ctx: &Ctx) -> Result<String> {
    let mut list = ctx.metrics().subagents.clone();
    if let Some(m) = &ctx.model {
        let m = m.to_lowercase();
        list.retain(|s| s.model.to_lowercase().contains(&m));
    }
    let col = ctx.sort.as_deref().unwrap_or("tokens");
    query::sort_subagents(&mut list, col, ctx.desc)?;
    list.truncate(ctx.top);
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(list.iter().map(subagent_json).collect())));
    }
    if list.is_empty() {
        return Ok(hr("SUB-AGENTS") + "No sub-agent invocations found.\n");
    }
    let headers = ["TYPE", "MODEL", "TOKENS", "COST", "TOOLS", "READ", "SRCH", "BASH", "EDIT", "LINES", "SECS"];
    let rows: Vec<Vec<String>> = list.iter().map(|s| vec![
        s.agent_type.clone(), short_model(&s.model), fmt_int(s.total_tokens),
        format!("${:.2}", crate::pricing::price_for(&s.model).cost(&s.usage)),
        s.tool_use_count.to_string(),
        s.read_count.to_string(), s.search_count.to_string(), s.bash_count.to_string(),
        s.edit_count.to_string(), format!("+{}/-{}", s.lines_added, s.lines_removed),
        format!("{:.0}", s.duration_ms as f64 / 1000.0),
    ]).collect();
    Ok(titled(ctx.fmt, "SUB-AGENTS", render_table(&headers, &rows, ctx.fmt)))
}

// -------------------------------------------------------------------------- runs

/// The session's runs (one SDK `query()` / prompt cycle each) with how every one ended.
/// Turn limits apply **per run**, so this is what a `maxTurns` setting must be read against
/// — a session's total turn count will exceed it whenever there was more than one run.
pub fn runs(ctx: &Ctx) -> Result<String> {
    let scope: Vec<&SessionReport> = match ctx.session {
        Some(sr) => vec![sr],
        None => ctx.a.sessions.iter().collect(),
    };
    let rows_data: Vec<(&SessionReport, &RunSegment)> =
        scope.iter().flat_map(|sr| sr.runs.iter().map(move |r| (*sr, r))).take(ctx.top).collect();

    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(rows_data.iter().map(|(sr, r)| json!({
            "session": sr.session_id,
            "run": r.index,
            "prompt": r.prompt.chars().take(400).collect::<String>(),
            "start_ms": r.start_ms, "end_ms": r.end_ms,
            "started": if r.start_ms > 0 { json!(fmt_epoch(r.start_ms)) } else { Value::Null },
            "duration_ms": (r.end_ms - r.start_ms).max(0),
            "turns_main": r.turns_main,
            "turns_sidechain": r.turns_sidechain,
            "usage": usage_json(&r.usage),
            "cost_usd": round4(r.cost_usd),
            "outcome": r.outcome.label(),
            "outcome_detail": r.outcome_detail,
            "capture_written_ms": ctx.capture_written_ms,
            "limit": r.limit_hit.as_ref().map(|(n, used, cap)| json!({ "kind": n, "used": used, "cap": cap })),
            "first_item": r.first_item, "last_item": r.last_item,
        })).collect())));
    }
    if rows_data.is_empty() {
        return Ok(hr("RUNS") + "No runs found.\n");
    }
    let headers = ["RUN", "STARTED", "DUR", "TURNS", "SUB", "TOKENS", "COST", "OUTCOME", "ITEMS", "PROMPT"];
    let rows: Vec<Vec<String>> = rows_data.iter().map(|(_, r)| vec![
        r.index.to_string(),
        fmt_epoch(r.start_ms),
        fmt_dur_ms(r.end_ms - r.start_ms),
        r.turns_main.to_string(),
        r.turns_sidechain.to_string(),
        fmt_int(r.usage.total()),
        format!("${:.2}", r.cost_usd),
        r.outcome.label().to_string(),
        format!("{}-{}", r.first_item, r.last_item),
        r.prompt.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(46).collect(),
    ]).collect();
    let mut s = titled(ctx.fmt, "RUNS", render_table(&headers, &rows, ctx.fmt));
    if ctx.fmt != Fmt::Csv {
        for (_, r) in &rows_data {
            if !r.outcome_detail.is_empty() {
                let _ = writeln!(s, "run {}: {}", r.index, r.outcome_detail);
            }
        }
    }
    Ok(s)
}

// ------------------------------------------------------------------------- trace

/// One line per turn: when, which thread, what it called. The compact "what did this
/// actually do" view — a repeated pattern (the same tool, the same target, over and over)
/// is visible at a glance where a full transcript buries it.
pub fn trace(ctx: &Ctx, filter: &ItemFilter) -> Result<String> {
    let sr = ctx.require_session("trace")?;
    // `--limit` caps trace *rows*, so drop the non-row items (prompts, tool results) before
    // applying it — otherwise a low limit selects messages this view never prints.
    let unlimited = ItemFilter { limit: 0, ..filter.clone() };
    let mut rows_data: Vec<&TItem> = unlimited
        .select(&sr.transcript)?
        .iter()
        .map(|&i| &sr.transcript[i])
        .filter(|it| matches!(&it.kind, TKind::Assistant { .. } | TKind::Event { .. } | TKind::Compact { .. }))
        .collect();
    if filter.limit > 0 {
        rows_data.truncate(filter.limit);
    }

    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(rows_data.iter().map(|it| {
            let mut v = json!({ "index": it.index, "ts_ms": it.ts_ms, "run": it.run,
                                "thread": it.agent.as_ref().map(|a| a.label()).unwrap_or_else(|| "main".into()) });
            match &it.kind {
                TKind::Assistant { label, model, usage, tools, stop_reason, .. } => {
                    v["kind"] = json!("turn");
                    v["turn"] = json!(label);
                    v["model"] = json!(model);
                    v["output_tokens"] = json!(usage.output_tokens);
                    v["stop_reason"] = json!(stop_reason);
                    v["tools"] = json!(tools.iter().map(|t| format!("{} {}", t.name, t.target)).collect::<Vec<_>>());
                }
                TKind::Event { subtype, detail, is_terminal, .. } => {
                    v["kind"] = json!("event");
                    v["subtype"] = json!(subtype);
                    v["detail"] = json!(detail);
                    v["is_terminal"] = json!(is_terminal);
                }
                TKind::Compact { pre, post, trigger } => {
                    v["kind"] = json!("compact");
                    v["detail"] = json!(format!("{trigger}: {} → {}", fmt_int(*pre), fmt_int(*post)));
                }
                _ => {}
            }
            v
        }).collect())));
    }
    let headers = ["TIME", "#", "RUN", "THREAD", "TURN", "OUT", "WHAT"];
    let rows: Vec<Vec<String>> = rows_data.iter().map(|it| {
        let thread = it.agent.as_ref().map(|a| a.label()).unwrap_or_else(|| "main".into());
        let (turn, out_tok, what) = match &it.kind {
            TKind::Assistant { label, usage, tools, stop_reason, .. } => {
                // `short_path` trims the *front*, which is right for a path and wrong for a
                // description: across a loop's near-identical rows the wording drift is the
                // evidence, and it lives at the start.
                let mut w = tools
                    .iter()
                    .map(|t| format!("{} {}", t.name, describe_target(&t.target)))
                    .collect::<Vec<_>>()
                    .join(" · ");
                if w.is_empty() {
                    w = if stop_reason.is_empty() { "(no tools, response cut off)".into() } else { "(text only)".into() };
                }
                (label.clone(), fmt_int(usage.output_tokens), w)
            }
            TKind::Event { subtype, detail, is_terminal, .. } => (
                String::new(),
                String::new(),
                format!("{} {subtype} — {detail}", if *is_terminal { "⛔" } else { "•" }),
            ),
            TKind::Compact { pre, post, trigger } => {
                (String::new(), String::new(), format!("✂ compacted ({trigger}) {} → {}", fmt_int(*pre), fmt_int(*post)))
            }
            _ => (String::new(), String::new(), String::new()),
        };
        vec![fmt_clock(it.ts_ms).trim_end().to_string(), it.index.to_string(), it.run.to_string(), thread, turn, out_tok, what]
    }).collect();
    Ok(titled(ctx.fmt, &format!("TRACE — {}", sr.title), render_table(&headers, &rows, ctx.fmt)))
}

// ------------------------------------------------------------------------ agents

/// Every sub-agent conversation in a session — including the ones that never returned a
/// result, which `subagents` cannot show. Each row carries the id and item range needed to
/// read that agent: `transcript --agent <id>` or `transcript --from <first> --limit N`.
/// Narrowing for `ssa agents`, mirroring the transcript vocabulary.
#[derive(Debug, Default, Clone)]
pub struct AgentFilter {
    pub run: Option<usize>,
    pub agent: Option<String>,
    pub min_depth: Option<usize>,
    /// Only agents whose every tool call spawned another agent — the exact population the
    /// `Delegation loop` finding counts.
    pub spinning: bool,
}

pub fn agents(ctx: &Ctx, filter: &AgentFilter) -> Result<String> {
    let scope: Vec<&SessionReport> = match ctx.session {
        Some(sr) => vec![sr],
        None => ctx.a.sessions.iter().collect(),
    };
    let matches = |sr: &SessionReport, t: &AgentThread| {
        // An agent belongs to the run its first message falls in.
        let run_ok = filter.run.is_none_or(|r| {
            sr.transcript.get(t.first_item).map(|it| it.run) == Some(r)
        });
        let name_ok = filter.agent.as_ref().is_none_or(|f| {
            let f = f.to_lowercase();
            t.agent.id.to_lowercase().contains(&f)
                || t.agent.agent_type.to_lowercase().contains(&f)
                || t.agent.description.to_lowercase().contains(&f)
        });
        run_ok
            && name_ok
            && filter.min_depth.is_none_or(|d| t.agent.depth >= d)
            && (!filter.spinning || t.is_spinning(&sr.transcript))
    };
    let mut rows_data: Vec<(&SessionReport, &AgentThread)> = scope
        .iter()
        .flat_map(|sr| sr.threads.iter().filter(move |t| matches(sr, t)).map(move |t| (*sr, t)))
        .collect();
    // Default order is first-appearance, which is how a delegation chain reads.
    if let Some(col) = ctx.sort.as_deref() {
        query::sort_agent_threads(&mut rows_data, col, ctx.desc)?;
    }
    rows_data.truncate(ctx.top);

    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(rows_data.iter().map(|(sr, t)| json!({
            "session": sr.session_id,
            "agent_id": t.agent.id,
            "agent_type": t.agent.agent_type,
            "description": t.agent.description,
            "depth": t.agent.depth,
            "model": t.model,
            "turns": t.turns,
            "tool_calls": t.tool_calls,
            "usage": usage_json(&t.usage),
            "cost_usd": round4(t.cost_usd),
            "completed": t.completed,
            "outcome": t.outcome.label(),
            "start_ms": t.start_ms, "end_ms": t.end_ms, "duration_ms": t.duration_ms(),
            "started": if t.start_ms > 0 { json!(fmt_epoch(t.start_ms)) } else { Value::Null },
            "first_item": t.first_item,
            "last_item": t.last_item,
        })).collect())));
    }
    if rows_data.is_empty() {
        return Ok(hr("SUB-AGENT THREADS") + "No sub-agent conversations found.\n");
    }
    let headers = ["AGENT_ID", "TYPE", "D", "MODEL", "TURNS", "TOKENS", "COST", "TOOLS", "STARTED", "DUR", "OUTCOME", "ITEMS", "DESCRIPTION"];
    let rows: Vec<Vec<String>> = rows_data.iter().map(|(_, t)| vec![
        t.agent.id.clone(),
        t.agent.agent_type.clone(),
        t.agent.depth.to_string(),
        short_model(&t.model),
        t.turns.to_string(),
        fmt_int(t.usage.total()),
        format!("${:.2}", t.cost_usd),
        t.tool_calls.to_string(),
        fmt_clock(t.start_ms).trim_end().to_string(),
        fmt_dur_ms(t.duration_ms()),
        t.outcome.label().to_string(),
        format!("{}-{}", t.first_item, t.last_item),
        t.agent.description.chars().take(44).collect(),
    ]).collect();
    let mut s = titled(ctx.fmt, "SUB-AGENT THREADS", render_table(&headers, &rows, ctx.fmt));
    if ctx.fmt == Fmt::Csv {
        return Ok(s);
    }
    // Totals plus a depth histogram. Summing the rows by hand is the first thing anyone
    // does with this view, and the histogram answers what the row list cannot: is this a
    // serial chain (one agent per level) or a parallel fan-out? Different bugs entirely.
    let ts: Vec<&AgentThread> = rows_data.iter().map(|(_, t)| *t).collect();
    let tokens: u64 = ts.iter().map(|t| t.usage.total()).sum();
    let cost: f64 = ts.iter().map(|t| t.cost_usd).sum();
    let turns: usize = ts.iter().map(|t| t.turns).sum();
    let calls: u64 = ts.iter().map(|t| t.tool_calls).sum();
    let mut by_depth: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    let mut by_outcome: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for t in &ts {
        *by_depth.entry(t.agent.depth).or_default() += 1;
        *by_outcome.entry(t.outcome.label()).or_default() += 1;
    }
    let _ = writeln!(
        s,
        "\nTOTAL   {} agent(s) · {} turns · {} tokens · ${:.2} · {} tool calls",
        ts.len(), turns, fmt_int(tokens), cost, calls
    );
    let _ = writeln!(s, "Outcome {}", by_outcome.iter().map(|(k, v)| format!("{v} {k}")).collect::<Vec<_>>().join(", "));
    let branching: Vec<String> = by_depth.iter().filter(|(_, n)| **n > 1).map(|(d, n)| format!("d{d}×{n}")).collect();
    let _ = writeln!(
        s,
        "Depth   {} level(s), max {} — {}",
        by_depth.len(),
        by_depth.keys().max().copied().unwrap_or(0),
        if branching.is_empty() {
            "strictly serial: one agent per level, a chain not a fan-out".to_string()
        } else {
            format!("branching at {}", branching.join(", "))
        }
    );
    Ok(s)
}

// ---------------------------------------------------------------------- timeline

pub fn timeline(ctx: &Ctx) -> Result<String> {
    let sr = ctx.require_session("timeline")?;
    let mut tl: Vec<&TurnPoint> = sr.timeline.iter().collect();
    if let Some(m) = &ctx.model {
        let m = m.to_lowercase();
        tl.retain(|t| t.model.to_lowercase().contains(&m));
    }
    if let Some(col) = &ctx.sort {
        query::sort_timeline(&mut tl, col, ctx.desc)?;
    }
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(tl.iter().map(|t| turn_json(t)).collect())));
    }
    // THREAD is its own column: the main thread and each sub-agent have separate context
    // windows, so a reader must be able to tell which window a CONTEXT figure belongs to.
    let headers = ["TURN", "THREAD", "CONTEXT", "ΔCTX", "WRITE", "READ", "OUT", "COST", "FLAGS", "CAUSE"];
    let rows: Vec<Vec<String>> = tl.iter().map(|t| vec![
        t.label.clone(),
        t.agent.as_ref().map(|a| a.label()).unwrap_or_else(|| "main".into()),
        fmt_int(t.context_size), fmt_delta(t.delta),
        fmt_int(t.usage.cache_creation_input_tokens), fmt_int(t.usage.cache_read_input_tokens),
        fmt_int(t.usage.output_tokens), format!("${:.3}", t.cost),
        flags(t), if t.cause.is_empty() { String::new() } else { short_path(&t.cause) },
    ]).collect();
    Ok(titled(ctx.fmt, &format!("TIMELINE — {}", sr.title), render_table(&headers, &rows, ctx.fmt)))
}

fn flags(t: &TurnPoint) -> String {
    let mut f = String::new();
    if t.is_spike { f.push('▲'); }
    if t.compaction_after { f.push('✂'); }
    if t.is_error { f.push('!'); }
    f
}

// ------------------------------------------------------------------------ growth

pub fn growth(ctx: &Ctx) -> Result<String> {
    let sr = ctx.require_session("growth")?;
    let series: Vec<u64> = sr.timeline.iter().map(|t| t.context_size).collect();
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(json!({
            "session_id": sr.session_id,
            "context_peak": ctx.metrics_of(sr).context_peak,
            "series": series,
            "spikes": sr.spikes.iter().map(spike_json).collect::<Vec<_>>(),
        })));
    }
    let mut s = hr(&format!("CONTEXT GROWTH — {}", sr.title));
    let _ = writeln!(s, "Peak context: {} tokens · {} turns · {} spike(s)\n", fmt_int(sr.metrics.context_peak), series.len(), sr.spikes.len());
    let _ = writeln!(s, "{}", sparkline(&series));
    let _ = writeln!(s, "turn 1{}turn {}", " ".repeat(series.len().saturating_sub(12)), series.len());
    if !sr.spikes.is_empty() {
        let _ = writeln!(s, "\nSpikes:");
        for sp in &sr.spikes {
            // Same thread-qualified label `spikes` and `timeline` print — a bare index
            // means nothing once turns are numbered per thread.
            let _ = writeln!(s, "  turn {:>18}  +{:>10} → {:>10}   {}", sp.label, fmt_int(sp.delta as u64), fmt_int(sp.context_size), sp.cause);
        }
    }
    Ok(s)
}

// ------------------------------------------------------------------------ spikes

pub fn spikes(ctx: &Ctx) -> Result<String> {
    // Aggregate across sessions when no session is selected.
    let mut rows_data: Vec<(String, &Spike)> = Vec::new();
    match ctx.session {
        Some(sr) => rows_data.extend(sr.spikes.iter().map(|s| (sr.title.clone(), s))),
        None => {
            for sr in &ctx.a.sessions {
                rows_data.extend(sr.spikes.iter().map(|s| (sr.title.clone(), s)));
            }
        }
    }
    let desc = ctx.sort.as_deref() != Some("turn");
    rows_data.sort_by(|a, b| if desc { b.1.delta.cmp(&a.1.delta) } else { a.1.turn.cmp(&b.1.turn) });
    rows_data.truncate(ctx.top);
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(rows_data.iter().map(|(t, s)| {
            let mut j = spike_json(s);
            j["session"] = json!(t);
            j
        }).collect())));
    }
    let headers = ["TURN", "ΔCTX", "CONTEXT", "SESSION", "CAUSE"];
    let rows: Vec<Vec<String>> = rows_data.iter().map(|(t, s)| vec![
        s.label.clone(), format!("+{}", fmt_int(s.delta as u64)), fmt_int(s.context_size),
        t.chars().take(28).collect(), s.cause.clone(),
    ]).collect();
    Ok(titled(ctx.fmt, "CONTEXT-GROWTH SPIKES", render_table(&headers, &rows, ctx.fmt)))
}

// --------------------------------------------------------------------- transcript

/// Which thread(s) of a conversation to include.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thread {
    /// Everything: the main conversation and every sub-agent.
    All,
    /// Only the main conversation — sub-agent work folded away.
    Main,
    /// Only sub-agent (sidechain) conversations.
    Sub,
}

/// Filters shared by `transcript` and `search`, so both select messages the same way.
#[derive(Debug, Default, Clone)]
pub struct ItemFilter {
    /// Role: user / assistant / tool / compact.
    pub kind: Option<String>,
    /// Case-insensitive substring the item's text must contain.
    pub grep: Option<String>,
    /// Which thread(s) to include.
    pub thread: Option<Thread>,
    /// Sub-agent id / type / description substring (implies `thread = sub`).
    pub agent: Option<String>,
    /// Exact nesting depth (implies `thread = sub`).
    pub depth: Option<usize>,
    /// Minimum nesting depth (implies `thread = sub`).
    pub min_depth: Option<usize>,
    /// Tool name the item must involve (a call made, or a result returned).
    pub tool: Option<String>,
    /// Substring the serialized tool *input* must contain (e.g. the prompt given to Agent).
    pub input_grep: Option<String>,
    /// Only items in this 1-based run.
    pub run: Option<usize>,
    /// Wall-clock window, epoch millis.
    pub since_ms: Option<i64>,
    pub until_ms: Option<i64>,
    /// Skip items before this transcript index (pagination).
    pub from: usize,
    /// Maximum items to return (0 = no cap).
    pub limit: usize,
    /// Items of surrounding context to keep around every match.
    pub context: usize,
    /// Treat `grep` / `input_grep` as regular expressions instead of substrings.
    pub regex: bool,
    /// Only items that failed: a tool result flagged `is_error`, or an assistant turn that
    /// hit an API error. `tools` reports error *counts*; this is how you read them.
    pub errors_only: bool,
}

impl ItemFilter {
    /// Compile the text matchers once. Returns an error for an invalid pattern rather than
    /// silently matching nothing.
    pub fn compile(&self) -> Result<Matchers> {
        let build = |p: &Option<String>| -> Result<Option<Matcher>> {
            let Some(p) = p else { return Ok(None) };
            Ok(Some(if self.regex {
                Matcher::Re(Box::new(
                    regex::RegexBuilder::new(p)
                        .case_insensitive(true)
                        .build()
                        .map_err(|e| anyhow::anyhow!("invalid --regex pattern {p:?}: {e}"))?,
                ))
            } else {
                Matcher::Sub(p.to_lowercase())
            }))
        };
        Ok(Matchers { text: build(&self.grep)?, input: build(&self.input_grep)? })
    }
}

/// A compiled text matcher: plain case-insensitive substring, or a regex.
pub enum Matcher {
    Sub(String),
    Re(Box<regex::Regex>),
}

impl Matcher {
    /// Byte offset of the first match, for centring a snippet on it.
    fn find(&self, hay: &str) -> Option<usize> {
        match self {
            Matcher::Sub(s) if s.is_empty() => None,
            Matcher::Sub(s) => hay.to_lowercase().find(s.as_str()),
            Matcher::Re(r) => r.find(hay).map(|m| m.start()),
        }
    }

    fn is_match(&self, hay: &str) -> bool {
        match self {
            Matcher::Sub(s) => hay.to_lowercase().contains(s),
            Matcher::Re(r) => r.is_match(hay),
        }
    }
}

/// Compiled matchers for one selection pass.
#[derive(Default)]
pub struct Matchers {
    text: Option<Matcher>,
    input: Option<Matcher>,
}

impl ItemFilter {
    fn thread_ok(&self, it: &TItem) -> bool {
        let want = if self.agent.is_some() || self.depth.is_some() || self.min_depth.is_some() {
            Thread::Sub
        } else {
            self.thread.unwrap_or(Thread::All)
        };
        match want {
            Thread::All => true,
            Thread::Main => it.agent.is_none(),
            Thread::Sub => it.agent.is_some(),
        }
    }
    fn agent_ok(&self, it: &TItem) -> bool {
        let Some(f) = &self.agent else { return true };
        let f = f.to_lowercase();
        it.agent.as_ref().is_some_and(|a| {
            a.id.to_lowercase().contains(&f)
                || a.agent_type.to_lowercase().contains(&f)
                || a.description.to_lowercase().contains(&f)
        })
    }
    fn depth_ok(&self, it: &TItem) -> bool {
        let d = it.agent.as_ref().map(|a| a.depth);
        self.depth.is_none_or(|w| d == Some(w)) && self.min_depth.is_none_or(|w| d.is_some_and(|x| x >= w))
    }
    fn error_ok(&self, it: &TItem) -> bool {
        if !self.errors_only {
            return true;
        }
        matches!(&it.kind, TKind::Tool { is_error: true, .. } | TKind::Assistant { is_error: true, .. })
    }
    fn tool_ok(&self, it: &TItem) -> bool {
        let Some(f) = &self.tool else { return true };
        let f = f.to_lowercase();
        match &it.kind {
            TKind::Assistant { tools, .. } => tools.iter().any(|t| t.name.to_lowercase().contains(&f)),
            TKind::Tool { tool, .. } => tool.to_lowercase().contains(&f),
            _ => false,
        }
    }
    fn text_ok(&self, m: &Matchers, it: &TItem) -> bool {
        m.text.as_ref().is_none_or(|p| p.is_match(&titem_text(it)))
    }
    fn input_ok(&self, m: &Matchers, it: &TItem) -> bool {
        m.input.as_ref().is_none_or(|p| p.is_match(&titem_tool_input(it)))
    }
    fn window_ok(&self, it: &TItem) -> bool {
        self.run.is_none_or(|r| it.run == r)
            // ts 0 = no timestamp on the record; don't drop it for a time filter it can't answer.
            && (it.ts_ms == 0
                || (self.since_ms.is_none_or(|s| it.ts_ms >= s) && self.until_ms.is_none_or(|u| it.ts_ms <= u)))
    }

    /// Indices of the items a transcript should show, honouring `--context` around each
    /// match and then `--from` / `--limit`.
    pub fn select(&self, items: &[TItem]) -> Result<Vec<usize>> {
        self.select_with(&self.compile()?, items)
    }

    /// `select` with pre-compiled matchers, so a multi-session sweep compiles once.
    pub fn select_with(&self, m: &Matchers, items: &[TItem]) -> Result<Vec<usize>> {
        // Everything except --grep narrows *which* messages exist; --context then widens
        // back around each text hit so a match can be read in situ.
        let eligible = |it: &TItem| {
            self.thread_ok(it)
                && self.agent_ok(it)
                && self.depth_ok(it)
                && self.error_ok(it)
                && self.tool_ok(it)
                && self.window_ok(it)
                && match_kind(it, self.kind.as_deref())
        };
        let mut keep: Vec<bool> = items.iter().map(|it| eligible(it) && self.text_ok(m, it) && self.input_ok(m, it)).collect();
        if self.context > 0 && self.grep.is_some() {
            let hits: Vec<usize> = keep.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i).collect();
            for h in hits {
                let lo = h.saturating_sub(self.context);
                let hi = (h + self.context).min(items.len().saturating_sub(1));
                for (i, k) in keep.iter_mut().enumerate().take(hi + 1).skip(lo) {
                    *k = *k || eligible(&items[i]);
                }
            }
        }
        Ok(keep
            .iter()
            .enumerate()
            .filter(|(i, k)| **k && *i >= self.from)
            .map(|(i, _)| i)
            .take(if self.limit == 0 { usize::MAX } else { self.limit })
            .collect())
    }
}

pub fn transcript(ctx: &Ctx, filter: &ItemFilter, full: bool) -> Result<String> {
    let sr = ctx.require_session("transcript")?;
    let picked = filter.select(&sr.transcript)?;
    let items: Vec<&TItem> = picked.iter().map(|&i| &sr.transcript[i]).collect();
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(items.iter().map(|it| titem_json(it, full)).collect())));
    }
    let mut s = hr(&format!("TRANSCRIPT — {} ({}/{} items)", sr.title, items.len(), sr.transcript.len()));
    for it in items {
        let _ = writeln!(s, "{}", render_bubble(it, full));
    }
    Ok(s)
}

// ------------------------------------------------------------------------ search

/// Locate messages across every session in scope. Returns one compact row per hit
/// (session, item index, thread, snippet) so a follow-up `show`/`transcript --from` can
/// pull the full text of exactly the right message.
pub fn search(ctx: &Ctx, filter: &ItemFilter) -> Result<String> {
    if filter.grep.is_none()
        && filter.agent.is_none()
        && filter.tool.is_none()
        && filter.kind.is_none()
        && filter.input_grep.is_none()
        && !filter.errors_only
    {
        bail!("`search` needs at least one of --grep / --input-grep / --agent / --tool / --kind");
    }
    let scope: Vec<&SessionReport> = match ctx.session {
        Some(sr) => vec![sr],
        None => ctx.a.sessions.iter().collect(),
    };
    let mut hits: Vec<(&SessionReport, usize, &TItem)> = Vec::new();
    // `limit` caps the whole result set, not each session, so apply it after collecting.
    let f = ItemFilter { limit: 0, ..filter.clone() };
    let matchers = f.compile()?;
    for sr in scope {
        for i in f.select_with(&matchers, &sr.transcript)? {
            hits.push((sr, i, &sr.transcript[i]));
        }
    }
    let total = hits.len();
    if filter.limit > 0 {
        hits.truncate(filter.limit);
    }

    if ctx.fmt == Fmt::Json {
        return Ok(out_json(json!({
            "total_matches": total,
            "returned": hits.len(),
            "matches": hits.iter().map(|(sr, i, it)| {
                let mut v = json!({
                    "session": sr.session_id, "session_title": sr.title, "item": i,
                    "kind": kind_name(it), "snippet": snippet(&titem_text(it), matchers.text.as_ref()),
                });
                v["agent"] = match &it.agent {
                    Some(a) => json!({ "id": a.id, "type": a.agent_type, "depth": a.depth, "description": a.description }),
                    None => Value::Null,
                };
                v
            }).collect::<Vec<_>>(),
        })));
    }
    let headers = ["SESSION", "ITEM", "KIND", "THREAD", "MATCH"];
    let rows: Vec<Vec<String>> = hits
        .iter()
        .map(|(sr, i, it)| {
            vec![
                sr.session_id.chars().take(8).collect(),
                i.to_string(),
                kind_name(it).into(),
                it.agent.as_ref().map(|a| a.label()).unwrap_or_else(|| "main".into()),
                snippet(&titem_text(it), matchers.text.as_ref()),
            ]
        })
        .collect();
    let title = format!("SEARCH — {} match(es){}", total, if hits.len() < total { format!(", showing {}", hits.len()) } else { String::new() });
    Ok(titled(ctx.fmt, &title, render_table(&headers, &rows, ctx.fmt)))
}

/// A tool's target for a narrow column: paths keep their tail (the filename), anything else
/// keeps its head (where the meaning is).
fn describe_target(t: &str) -> String {
    if t.contains('/') {
        return short_path(t);
    }
    if t.chars().count() <= 52 {
        return t.to_string();
    }
    format!("{}…", t.chars().take(51).collect::<String>())
}

fn kind_name(it: &TItem) -> &'static str {
    match &it.kind {
        TKind::User { .. } => "user",
        TKind::Assistant { .. } => "assistant",
        TKind::Tool { .. } => "tool",
        TKind::Compact { .. } => "compact",
        TKind::Event { .. } => "event",
    }
}

/// A one-line excerpt centred on the first match (or the head of the text when there is
/// none). Works for both substring and regex matchers.
fn snippet(text: &str, needle: Option<&Matcher>) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let start = needle
        .and_then(|m| m.find(&flat))
        .map(|p| flat[..p].char_indices().rev().nth(30).map(|(i, _)| i).unwrap_or(0))
        .unwrap_or(0);
    let mut s: String = flat[start..].chars().take(110).collect();
    if start > 0 {
        s.insert(0, '…');
    }
    s
}

/// Searchable text of a transcript item (for `--grep`).
fn titem_text(it: &TItem) -> String {
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

/// Serialized tool *inputs* for `--input-grep` — the actual arguments, which `titem_text`
/// deliberately keeps out of the main grep corpus (they are large and mostly noise).
fn titem_tool_input(it: &TItem) -> String {
    match &it.kind {
        TKind::Assistant { tools, .. } => tools.iter().map(|t| t.input_full.as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

fn match_kind(it: &TItem, filter: Option<&str>) -> bool {
    let Some(f) = filter else { return true };
    matches!((&it.kind, f),
        (TKind::User { .. }, "user") | (TKind::Assistant { .. }, "assistant") |
        (TKind::Tool { .. }, "tool") | (TKind::Compact { .. }, "compact") |
        (TKind::Event { .. }, "event"))
}

fn render_bubble(it: &TItem, full: bool) -> String {
    // Sub-agent lines are indented and tagged with the agent they came from — without it
    // a delegated conversation reads as the main thread's.
    // Indent by nesting depth, capped — a runaway delegation chain can be 50 deep, and
    // the `·dN` suffix in the tag carries the exact level.
    let ind = it.agent.as_ref().map(|a| "  ".repeat(a.depth.min(8))).unwrap_or_default();
    let who = it.agent.as_ref().map(|a| format!("▎{} ", a.label())).unwrap_or_default();
    let at = fmt_clock(it.ts_ms);
    let out = match &it.kind {
        TKind::User { text, is_prompt } => {
            let tag = match &it.agent {
                // Inside a sidechain the "user" is the parent handing over a task.
                Some(_) => "TASK FROM PARENT",
                None if *is_prompt => "USER",
                None => "USER(meta)",
            };
            format!("[#{}] 👤 {}\n{}", it.index, tag, body(text, full))
        }
        TKind::Assistant { label, model, usage, thinking, text, tools, is_error, .. } => {
            let mut s = format!(
                "[#{}] 🤖 ASSISTANT turn {} ({}){}  ↑read {} ✎write {} ↓out {}",
                it.index, label, model, if *is_error { " ERROR" } else { "" },
                fmt_int(usage.cache_read_input_tokens), fmt_int(usage.cache_creation_input_tokens), fmt_int(usage.output_tokens)
            );
            if !thinking.is_empty() {
                let _ = write!(s, "\n  💭 {}", body(thinking, full).replace('\n', "\n  "));
            }
            if !text.is_empty() {
                let _ = write!(s, "\n  {}", body(text, full).replace('\n', "\n  "));
            }
            for t in tools {
                let _ = write!(s, "\n  ⚙ {} {}", t.name, short_path(&t.target));
                if full && !t.input_full.is_empty() {
                    let _ = write!(s, "\n    {}", t.input_full.replace('\n', "\n    "));
                }
            }
            s
        }
        TKind::Tool { tool, target, tokens, content, is_error } => {
            let head = format!("[#{}] ⚙→ {} {} ~{} tok{}", it.index, tool, short_path(target), fmt_int(*tokens), if *is_error { " ERROR" } else { "" });
            if full { format!("{}\n{}", head, indent(content, "    ")) } else { head }
        }
        TKind::Compact { pre, post, trigger } => {
            format!("[#{}] ✂ ── COMPACTED ({}) {} → {} tokens ──", it.index, trigger, fmt_int(*pre), fmt_int(*post))
        }
        TKind::Event { subtype, detail, content, is_terminal } => {
            let mark = if *is_terminal { "⛔" } else { "•" };
            let head = format!("[#{}] {} {} — {}", it.index, mark, subtype, detail);
            if full && !content.is_empty() { format!("{}\n{}", head, indent(content, "    ")) } else { head }
        }
    };
    // Clock time on the header line: the only way to tell a sequential chain from a
    // parallel fan-out, and to line a message up against the timeline.
    let out = match out.split_once('\n') {
        Some((h, rest)) => format!("{at}{h}\n{rest}"),
        None => format!("{at}{out}"),
    };
    if ind.is_empty() {
        return out;
    }
    // Tag the header line with the agent, indent the rest under it.
    out.lines()
        .enumerate()
        .map(|(i, l)| if i == 0 { format!("{ind}{who}{l}") } else { format!("{ind}{l}") })
        .collect::<Vec<_>>()
        .join("\n")
}

fn body(text: &str, full: bool) -> String {
    if full {
        text.to_string()
    } else {
        let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        one.chars().take(160).collect()
    }
}

fn indent(s: &str, pad: &str) -> String {
    s.lines().map(|l| format!("{}{}", pad, l)).collect::<Vec<_>>().join("\n")
}

// -------------------------------------------------------------------------- show

pub fn show(ctx: &Ctx, item: usize) -> Result<String> {
    let sr = ctx.require_session("show")?;
    let it = sr.transcript.get(item).ok_or_else(|| anyhow::anyhow!("no item #{} (session has {})", item, sr.transcript.len()))?;
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(titem_json(&it, true)));
    }
    Ok(render_bubble(it, true))
}

// ------------------------------------------------------------------------ issues

pub fn issues(ctx: &Ctx) -> String {
    let m = ctx.metrics();
    if ctx.fmt == Fmt::Json {
        return out_json(Value::Array(m.findings.iter().map(finding_json).collect()));
    }
    let mut s = hr("INEFFICIENCIES");
    if m.findings.is_empty() {
        s.push_str("No notable inefficiencies detected.\n");
        return s;
    }
    for f in &m.findings {
        let waste = if f.wasted_tokens_est > 0 { format!(" [~{} tok wasted]", fmt_int(f.wasted_tokens_est)) } else { String::new() };
        let _ = writeln!(s, "[{}] {}{}", f.severity.label(), f.kind, waste);
        let _ = writeln!(s, "       {}", f.detail);
    }
    s
}

// -------------------------------------------------------------------- json helpers

impl<'a> Ctx<'a> {
    fn metrics_of<'b>(&self, sr: &'b SessionReport) -> &'b Metrics {
        &sr.metrics
    }
}

fn usage_json(u: &Usage) -> Value {
    json!({
        "input_tokens": u.input_tokens,
        "cache_creation_input_tokens": u.cache_creation_input_tokens,
        "cache_read_input_tokens": u.cache_read_input_tokens,
        "output_tokens": u.output_tokens,
        "total_tokens": u.total(),
    })
}

fn session_json(sr: &SessionReport) -> Value {
    let m = &sr.metrics;
    let ts_start = sr.timeline.iter().map(|t| t.ts_ms).filter(|t| *t > 0).min().unwrap_or(0);
    let ts_end = sr.timeline.iter().map(|t| t.ts_ms).max().unwrap_or(0);
    let r = rate_report(&sr.timeline.iter().collect::<Vec<_>>(), RATE_WINDOW_HOURS);
    json!({
        "session_id": sr.session_id, "title": sr.title, "source": sr.source, "cwd": sr.cwd,
        "entrypoint": sr.entrypoint, "service_tier": sr.service_tier, "model": sr.metrics.dominant_model(),
        "ts_start_ms": ts_start, "ts_end_ms": ts_end,
        "ts_start": fmt_epoch(ts_start), "ts_end": fmt_epoch(ts_end),
        "active_hours": round2(r.active_ms as f64 / 3.6e6), "idle_gaps": r.idle_gaps,
        "fresh_per_hour": r.fresh_per_h.round(), "cache_write_per_hour": r.cache_write_per_h.round(),
        "peak_burst_fresh": r.peak_burst_fresh, "peak_turns_per_min": r.peak_turns_per_min,
        "max_concurrent_subagents": m.max_concurrent_subagents,
        "usage": usage_json(&m.usage), "estimated_cost_usd": round2(m.cost_usd),
        "cache_hit_rate": round4(m.cache_hit_rate()), "cache_churn": finite_or_null(m.cache_churn()),
        "assistant_turns": m.assistant_turns, "turns_main": m.turns_main, "turns_sidechain": m.turns_sidechain,
        "user_prompts": m.user_prompts, "tool_calls": m.tool_calls,
        "subagent_tokens": m.subagent_tokens(), "compactions": m.compactions.len(),
        "context_peak": m.context_peak, "duration_ms": m.duration_ms,
        "active_ms": m.active_ms, "active_hours": round2(m.active_ms as f64 / 3.6e6), "idle_gaps": m.idle_gaps,
        "longest_burst_fresh": m.longest_burst_fresh, "spikes": sr.spikes.len(),
        "top_findings": Value::Array(m.findings.iter().take(5).map(finding_json).collect()),
    })
}

fn cacheattr_json(c: &CacheContrib) -> Value {
    json!({
        "tool": c.tool, "target": c.target, "size_tokens": c.tokens, "entries": c.entries,
        "residency_turns": c.residency_turns, "replayed_tokens": c.contribution,
        "share": round4(c.share), "amplified_cost_usd": round2(c.amplified_cost), "is_baseline": c.is_baseline,
    })
}

/// Estimated **summarization** cost of a compaction: the model reads the full pre-context
/// (from warm cache) and generates the ~post-token summary. This call carries NO usage on
/// the `compact_boundary` record, so it is otherwise MISSING from the session's token totals
/// — the estimate makes that hidden overhead visible. (The post-compaction cache re-warm is
/// a normal turn and is already counted, so it is deliberately excluded here.)
fn compaction_cost(ev: &crate::model::CompactEvent, price: &crate::pricing::Price) -> f64 {
    (ev.pre_tokens as f64 * price.cache_read + ev.post_tokens as f64 * price.output) / 1_000_000.0
}

fn subagent_json(s: &SubagentResult) -> Value {
    json!({
        "agent_type": s.agent_type, "agent_id": s.agent_id, "model": s.model, "total_tokens": s.total_tokens,
        "cost_usd": round2(crate::pricing::price_for(&s.model).cost(&s.usage)),
        "tool_use_count": s.tool_use_count, "duration_ms": s.duration_ms, "usage": usage_json(&s.usage),
        "read_count": s.read_count, "search_count": s.search_count, "bash_count": s.bash_count,
        "edit_count": s.edit_count, "lines_added": s.lines_added, "lines_removed": s.lines_removed,
    })
}

fn turn_json(t: &TurnPoint) -> Value {
    json!({
        "turn": t.turn, "turn_label": t.label, "model": t.model, "usage": usage_json(&t.usage),
        "context_size": t.context_size,
        "delta": t.delta, "added_tokens": t.added_tokens, "cost_usd": round4(t.cost),
        "ts_ms": t.ts_ms, "is_sidechain": t.is_sidechain,
        "agent": match &t.agent {
            Some(a) => json!({ "id": a.id, "type": a.agent_type, "depth": a.depth }),
            None => Value::Null,
        },
        "is_spike": t.is_spike, "compaction_after": t.compaction_after, "is_error": t.is_error, "cause": t.cause,
    })
}

fn spike_json(s: &Spike) -> Value {
    json!({ "turn": s.turn, "turn_label": s.label, "delta": s.delta, "context_size": s.context_size, "cause": s.cause })
}

fn finding_json(f: &Finding) -> Value {
    json!({ "severity": f.severity.label(), "kind": f.kind, "detail": f.detail, "wasted_tokens_est": f.wasted_tokens_est })
}

fn titem_json(it: &TItem, full: bool) -> Value {
    let mut v = match &it.kind {
        TKind::User { text, is_prompt } => json!({
            "index": it.index, "kind": "user", "is_prompt": is_prompt,
            "chars": text.len(), "text": clip(text, full),
        }),
        TKind::Assistant { turn, label, model, usage, thinking, text, tools, is_error, stop_reason } => json!({
            "index": it.index, "kind": "assistant", "turn": turn, "turn_label": label,
            "model": model, "usage": usage_json(usage),
            "is_error": is_error, "stop_reason": stop_reason,
            "thinking": clip(thinking, full), "text": clip(text, full),
            // Tool inputs are always present (truncated unless --full), so "what was this
            // agent actually asked to do" is answerable without dumping the transcript.
            "tools": tools.iter().map(|t| json!({"name": t.name, "target": t.target,
                "input": if full { t.input_full.clone() } else { t.input_full.chars().take(400).collect::<String>() }})).collect::<Vec<_>>(),
        }),
        TKind::Tool { tool, target, tokens, content, is_error } => json!({
            "index": it.index, "kind": "tool", "tool": tool, "target": target, "tokens_est": tokens,
            "is_error": is_error, "chars": content.len(), "content": clip(content, full),
        }),
        TKind::Compact { pre, post, trigger } => json!({
            "index": it.index, "kind": "compact", "trigger": trigger, "pre_tokens": pre, "post_tokens": post,
        }),
        TKind::Event { subtype, detail, content, is_terminal } => json!({
            "index": it.index, "kind": "event", "subtype": subtype, "detail": detail,
            "is_terminal": is_terminal, "chars": content.len(), "content": clip(content, full),
        }),
    };
    // One flag per item: true when any text field on it was shortened.
    v["truncated"] = json!(match &it.kind {
        TKind::User { text, .. } => clipped(text, full),
        TKind::Assistant { thinking, text, .. } => clipped(thinking, full) || clipped(text, full),
        TKind::Tool { content, .. } => clipped(content, full),
        TKind::Event { content, .. } => clipped(content, full),
        TKind::Compact { .. } => false,
    });
    v["ts_ms"] = json!(it.ts_ms);
    v["timestamp"] = if it.ts_ms > 0 { json!(fmt_epoch(it.ts_ms)) } else { Value::Null };
    v["run"] = json!(it.run);
    // Which thread produced this item — `null` for the main conversation.
    v["agent"] = match &it.agent {
        Some(a) => json!({ "id": a.id, "type": a.agent_type, "depth": a.depth }),
        None => Value::Null,
    };
    v
}

const CLIP_CHARS: usize = 200;

fn clip(s: &str, full: bool) -> String {
    if full { s.to_string() } else { s.chars().take(CLIP_CHARS).collect() }
}

/// True when `clip` would drop characters. Emitted alongside every clipped field so a
/// parser is never silently handed a fragment — a filter over truncated text can otherwise
/// produce a confident false negative.
fn clipped(s: &str, full: bool) -> bool {
    !full && s.chars().count() > CLIP_CHARS
}

fn fmt_delta(d: i64) -> String {
    if d > 0 { format!("+{}", fmt_int(d as u64)) } else if d < 0 { format!("-{}", fmt_int((-d) as u64)) } else { "0".into() }
}

/// Unicode block sparkline over a series (auto-scaled).
fn sparkline(series: &[u64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = series.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return String::new();
    }
    series.iter().map(|&v| {
        let idx = ((v as f64 / max as f64) * (BARS.len() - 1) as f64).round() as usize;
        BARS[idx.min(BARS.len() - 1)]
    }).collect()
}

fn round2(f: f64) -> f64 { (f * 100.0).round() / 100.0 }
fn round4(f: f64) -> f64 { (f * 10000.0).round() / 10000.0 }
fn finite_or_null(f: f64) -> Value { if f.is_finite() { json!(round4(f)) } else { Value::Null } }
