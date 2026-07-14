//! Shared sorting/filtering for every tabular view, so the CLI (`--sort`) and the TUI
//! (column-header click / `[` `]` keys) stay in perfect lockstep.
//!
//! Each view exposes an ordered list of sortable column names; sorting is by name so a
//! CLI flag and a TUI header click resolve identically.

use crate::analysis::*;
use crate::model::SubagentResult;
use anyhow::{Result, bail};

pub const SESSION_COLS: &[&str] =
    &["cost", "tokens", "hit", "turns", "tools", "subag", "duration", "active", "fresh_rate", "cwrate", "burst", "turnsmin"];
pub const TOOL_COLS: &[&str] = &["result", "calls", "input", "errors", "name"];
pub const SINK_COLS: &[&str] = &["amplified", "size", "calls", "residency", "contribution"];
pub const CACHEATTR_COLS: &[&str] = &["contribution", "share", "entries", "tokens"];
pub const SUBAGENT_COLS: &[&str] = &["tokens", "tools", "reads", "edits", "duration"];
pub const TIMELINE_COLS: &[&str] = &["turn", "context", "write", "delta", "cost"];

fn ord<T, F: FnMut(&T) -> f64>(v: &mut [T], desc: bool, mut key: F) {
    v.sort_by(|a, b| {
        let (ka, kb) = (key(a), key(b));
        if desc { kb.total_cmp(&ka) } else { ka.total_cmp(&kb) }
    });
}

fn check(view: &str, col: &str, cols: &[&str]) -> Result<()> {
    if cols.contains(&col) {
        Ok(())
    } else {
        bail!("unknown --sort column '{}' for {}. Options: {}", col, view, cols.join(", "))
    }
}

pub fn sort_sessions(v: &mut [&SessionReport], col: &str, desc: bool) -> Result<()> {
    check("sessions", col, SESSION_COLS)?;
    match col {
        "cost" => ord(v, desc, |s| s.metrics.cost_usd),
        "tokens" => ord(v, desc, |s| s.metrics.usage.total() as f64),
        "hit" => ord(v, desc, |s| s.metrics.cache_hit_rate()),
        "turns" => ord(v, desc, |s| s.metrics.assistant_turns as f64),
        "tools" => ord(v, desc, |s| s.metrics.tool_calls as f64),
        "subag" => ord(v, desc, |s| s.metrics.subagent_tokens() as f64),
        "duration" => ord(v, desc, |s| s.metrics.duration_ms as f64),
        "active" => ord(v, desc, |s| s.metrics.active_ms as f64),
        "fresh_rate" => ord(v, desc, |s| session_rate(s).fresh_per_h),
        "cwrate" => ord(v, desc, |s| session_rate(s).cache_write_per_h),
        "burst" => ord(v, desc, |s| session_rate(s).peak_burst_fresh as f64),
        "turnsmin" => ord(v, desc, |s| session_rate(s).peak_turns_per_min as f64),
        _ => unreachable!(),
    }
    Ok(())
}

/// Rate report for one session (used by throughput sort columns).
fn session_rate(s: &SessionReport) -> RateReport {
    rate_report(&s.timeline.iter().collect::<Vec<_>>(), RATE_WINDOW_HOURS)
}

pub fn sort_tools(v: &mut [ToolStat], col: &str, desc: bool) -> Result<()> {
    check("tools", col, TOOL_COLS)?;
    match col {
        "result" => ord(v, desc, |t| t.result_tokens_est as f64),
        "calls" => ord(v, desc, |t| t.calls as f64),
        "input" => ord(v, desc, |t| t.input_chars as f64),
        "errors" => ord(v, desc, |t| t.errors as f64),
        "name" => v.sort_by(|a, b| if desc { b.name.cmp(&a.name) } else { a.name.cmp(&b.name) }),
        _ => unreachable!(),
    }
    Ok(())
}

pub fn sort_sinks(v: &mut [CacheContrib], col: &str, desc: bool) -> Result<()> {
    check("sinks", col, SINK_COLS)?;
    match col {
        "amplified" => ord(v, desc, |c| c.amplified_cost),
        "size" => ord(v, desc, |c| c.tokens as f64),
        "calls" => ord(v, desc, |c| c.entries as f64),
        "residency" => ord(v, desc, |c| c.residency_turns as f64),
        "contribution" => ord(v, desc, |c| c.contribution as f64),
        _ => unreachable!(),
    }
    Ok(())
}

pub fn sort_cacheattr(v: &mut [CacheContrib], col: &str, desc: bool) -> Result<()> {
    check("cache-attr", col, CACHEATTR_COLS)?;
    match col {
        "contribution" => ord(v, desc, |c| c.contribution as f64),
        "share" => ord(v, desc, |c| c.share),
        "entries" => ord(v, desc, |c| c.entries as f64),
        "tokens" => ord(v, desc, |c| c.tokens as f64),
        _ => unreachable!(),
    }
    Ok(())
}

pub fn sort_subagents(v: &mut [SubagentResult], col: &str, desc: bool) -> Result<()> {
    check("subagents", col, SUBAGENT_COLS)?;
    match col {
        "tokens" => ord(v, desc, |s| s.total_tokens as f64),
        "tools" => ord(v, desc, |s| s.tool_use_count as f64),
        "reads" => ord(v, desc, |s| s.read_count as f64),
        "edits" => ord(v, desc, |s| s.edit_count as f64),
        "duration" => ord(v, desc, |s| s.duration_ms as f64),
        _ => unreachable!(),
    }
    Ok(())
}

pub fn sort_timeline(v: &mut [&TurnPoint], col: &str, desc: bool) -> Result<()> {
    check("timeline", col, TIMELINE_COLS)?;
    match col {
        "turn" => ord(v, desc, |t| t.turn as f64),
        "context" => ord(v, desc, |t| t.context_size as f64),
        "write" => ord(v, desc, |t| t.usage.cache_creation_input_tokens as f64),
        "delta" => ord(v, desc, |t| t.delta as f64),
        "cost" => ord(v, desc, |t| t.cost),
        _ => unreachable!(),
    }
    Ok(())
}

/// Real sinks only (drops the synthetic baseline row).
pub fn sinks_only(cache_attr: &[CacheContrib]) -> Vec<CacheContrib> {
    cache_attr.iter().filter(|c| !c.is_baseline).cloned().collect()
}
