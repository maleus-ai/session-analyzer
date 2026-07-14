//! Provider abstraction: the single extension point for supporting other harnesses.
//!
//! A [`Provider`] turns one harness's raw log lines into the normalized
//! [`Dataset`](crate::model::Dataset). Adding support for a new harness (a different
//! agent framework / CLI) is: implement this trait in a new file under `providers/`, then
//! add one line to [`registry`]. Nothing downstream changes.

use crate::model::Dataset;
use crate::providers::claude_code::ClaudeCodeProvider;
use anyhow::{Result, bail};

/// Parses a specific harness's session logs into the normalized model.
pub trait Provider {
    /// Stable machine id, e.g. `"claude-code"`.
    fn id(&self) -> &'static str;

    /// Human-facing name.
    fn display_name(&self) -> &'static str;

    /// Confidence in `[0.0, 1.0]` that this provider can parse the given sample lines.
    /// Used for auto-detection; the highest scorer wins.
    fn detect(&self, sample: &[String]) -> f32;

    /// Parse one raw log line, appending any resulting events to `out`.
    /// `source` identifies the originating file / archive entry.
    fn parse_line(&self, line: &str, source: &str, out: &mut Dataset);

    /// Normalize the fully-parsed dataset once, after every line is ingested. Providers use
    /// this for quirks that need whole-session context (e.g. Claude Code logs one API
    /// response as several block-records, repeating the usage on each — they must be merged
    /// into one turn). Default: no-op.
    fn finalize(&self, _out: &mut Dataset) {}
}

/// All known providers.
pub fn registry() -> Vec<Box<dyn Provider>> {
    vec![Box::new(ClaudeCodeProvider)]
    // Add other harnesses here, e.g. Box::new(CodexProvider),
}

/// Select a provider by explicit `id`, or auto-detect from `sample` lines.
pub fn select(id: Option<&str>, sample: &[String]) -> Result<Box<dyn Provider>> {
    let providers = registry();
    if let Some(id) = id {
        for p in providers {
            if p.id() == id {
                return Ok(p);
            }
        }
        bail!(
            "unknown provider '{}'. Available: {}",
            id,
            registry().iter().map(|p| p.id()).collect::<Vec<_>>().join(", ")
        );
    }
    // Auto-detect: highest confidence wins, ties broken by registry order.
    let mut best: Option<(f32, Box<dyn Provider>)> = None;
    for p in providers {
        let score = p.detect(sample);
        if best.as_ref().map_or(true, |(s, _)| score > *s) {
            best = Some((score, p));
        }
    }
    match best {
        Some((score, p)) if score > 0.0 => Ok(p),
        _ => bail!("could not detect a session-log provider for this input"),
    }
}
