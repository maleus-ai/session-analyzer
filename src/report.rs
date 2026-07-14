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
        return out_json(json!({
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
            "tool_calls": g.tool_calls,
            "duration_ms": g.duration_ms,
            "active_ms": rr.active_ms,
            "active_hours": round2(rr.active_ms as f64 / 3.6e6),
            "idle_gaps": rr.idle_gaps,
            "entrypoints": entrypoint_summary(ctx),
            "api_errors": g.api_errors,
            "context_peak": g.context_peak,
            "usage": usage_json(&g.usage),
            "estimated_cost_usd": round2(g.cost_usd),
            "cache_hit_rate": round4(g.cache_hit_rate()),
            "cache_churn": finite_or_null(g.cache_churn()),
            "subagent_count": g.subagents.len(),
            "subagent_tokens": g.subagent_tokens(),
            "compactions": g.compactions.len(),
            "compaction_summarized_tokens": g.compactions.iter().map(|c| c.pre_tokens).sum::<u64>(),
            "compaction_cost_usd": round2(
                g.compactions.iter().map(|c| compaction_cost(c, &crate::pricing::price_for(&g.dominant_model()))).sum()
            ),
            "parse_errors": a.parse_errors,
            "by_model": Value::Object(models),
        }));
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
    let _ = writeln!(s, "Active time       : {}   ({} idle gap(s) > 15m)", human_ms(rr.active_ms), rr.idle_gaps);
    let _ = writeln!(s, "Peak turns/min    : {}   (max concurrent subagents: {})", fmt_int(rr.peak_turns_per_min), g.max_concurrent_subagents);
    let _ = writeln!(s, "Assistant turns   : {}", fmt_int(g.assistant_turns));
    let _ = writeln!(s, "User prompts      : {}", fmt_int(g.user_prompts));
    let _ = writeln!(s, "Tool calls        : {}", fmt_int(g.tool_calls));
    let _ = writeln!(s, "Context peak      : {}", fmt_int(g.context_peak));
    if a.parse_errors > 0 {
        let _ = writeln!(s, "Unparsed lines    : {}", a.parse_errors);
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
        let _ = writeln!(s, "Sub-agents        : {}", g.subagents.len());
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
    let _ = writeln!(s, "Active time       : {}   ({} idle gap(s) > 15m)", human_ms(r.active_ms), r.idle_gaps);
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
        if s.entrypoint.contains("sdk") && r.idle_gaps == 0 && r.active_ms > 300_000 { "BURST" } else { "" }
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
    titled(ctx.fmt, "SUBSCRIPTION PRESSURE (by sustained cache-write/hour; BURST = unattended sdk run, 0 idle)", render_table(&headers, &out, ctx.fmt))
}

// ------------------------------------------------------------------------- tools

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
    let headers = ["TURN", "CONTEXT", "ΔCTX", "WRITE", "READ", "OUT", "COST", "FLAGS", "CAUSE"];
    let rows: Vec<Vec<String>> = tl.iter().map(|t| vec![
        t.turn.to_string(), fmt_int(t.context_size), fmt_delta(t.delta),
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
            let _ = writeln!(s, "  turn {:>4}  +{:>10} → {:>10}   {}", sp.turn, fmt_int(sp.delta as u64), fmt_int(sp.context_size), sp.cause);
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
        s.turn.to_string(), format!("+{}", fmt_int(s.delta as u64)), fmt_int(s.context_size),
        t.chars().take(28).collect(), s.cause.clone(),
    ]).collect();
    Ok(titled(ctx.fmt, "CONTEXT-GROWTH SPIKES", render_table(&headers, &rows, ctx.fmt)))
}

// --------------------------------------------------------------------- transcript

pub fn transcript(ctx: &Ctx, kind_filter: Option<&str>, full: bool) -> Result<String> {
    let sr = ctx.require_session("transcript")?;
    let grep = ctx.grep.as_ref().map(|g| g.to_lowercase());
    let items: Vec<&TItem> = sr
        .transcript
        .iter()
        .filter(|it| match_kind(it, kind_filter))
        .filter(|it| grep.as_ref().map_or(true, |g| titem_text(it).to_lowercase().contains(g)))
        .collect();
    if ctx.fmt == Fmt::Json {
        return Ok(out_json(Value::Array(items.iter().map(|it| titem_json(it, full)).collect())));
    }
    let mut s = hr(&format!("TRANSCRIPT — {}", sr.title));
    for it in items {
        let _ = writeln!(s, "{}", render_bubble(it, full));
    }
    Ok(s)
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
    }
}

fn match_kind(it: &TItem, filter: Option<&str>) -> bool {
    let Some(f) = filter else { return true };
    matches!((&it.kind, f),
        (TKind::User { .. }, "user") | (TKind::Assistant { .. }, "assistant") |
        (TKind::Tool { .. }, "tool") | (TKind::Compact { .. }, "compact"))
}

fn render_bubble(it: &TItem, full: bool) -> String {
    match &it.kind {
        TKind::User { text, is_prompt } => {
            let tag = if *is_prompt { "USER" } else { "USER(meta)" };
            format!("[#{}] 👤 {}\n{}", it.index, tag, body(text, full))
        }
        TKind::Assistant { turn, model, usage, thinking, text, tools, is_error } => {
            let mut s = format!(
                "[#{}] 🤖 ASSISTANT turn {} ({}){}  ↑read {} ✎write {} ↓out {}",
                it.index, turn, model, if *is_error { " ERROR" } else { "" },
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
    }
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
        "turn": t.turn, "model": t.model, "usage": usage_json(&t.usage), "context_size": t.context_size,
        "delta": t.delta, "added_tokens": t.added_tokens, "cost_usd": round4(t.cost),
        "ts_ms": t.ts_ms, "is_sidechain": t.is_sidechain,
        "is_spike": t.is_spike, "compaction_after": t.compaction_after, "is_error": t.is_error, "cause": t.cause,
    })
}

fn spike_json(s: &Spike) -> Value {
    json!({ "turn": s.turn, "delta": s.delta, "context_size": s.context_size, "cause": s.cause })
}

fn finding_json(f: &Finding) -> Value {
    json!({ "severity": f.severity.label(), "kind": f.kind, "detail": f.detail, "wasted_tokens_est": f.wasted_tokens_est })
}

fn titem_json(it: &TItem, full: bool) -> Value {
    match &it.kind {
        TKind::User { text, is_prompt } => json!({
            "index": it.index, "kind": "user", "is_prompt": is_prompt,
            "chars": text.len(), "text": clip(text, full),
        }),
        TKind::Assistant { turn, model, usage, thinking, text, tools, is_error } => json!({
            "index": it.index, "kind": "assistant", "turn": turn, "model": model, "usage": usage_json(usage),
            "is_error": is_error, "thinking": clip(thinking, full), "text": clip(text, full),
            "tools": tools.iter().map(|t| json!({"name": t.name, "target": t.target,
                "input": if full { Value::String(t.input_full.clone()) } else { Value::Null }})).collect::<Vec<_>>(),
        }),
        TKind::Tool { tool, target, tokens, content, is_error } => json!({
            "index": it.index, "kind": "tool", "tool": tool, "target": target, "tokens_est": tokens,
            "is_error": is_error, "chars": content.len(), "content": clip(content, full),
        }),
        TKind::Compact { pre, post, trigger } => json!({
            "index": it.index, "kind": "compact", "trigger": trigger, "pre_tokens": pre, "post_tokens": post,
        }),
    }
}

fn clip(s: &str, full: bool) -> String {
    if full { s.to_string() } else { s.chars().take(200).collect() }
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
