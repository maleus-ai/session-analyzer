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

    /// Where this harness keeps its session logs, relative to `$HOME`. Used as the default
    /// source for `ssa tar`.
    fn default_home_dir(&self) -> &'static str {
        ""
    }

    /// Environment variable that relocates this harness's config directory, if it has one.
    /// Checked before `$HOME/<default_home_dir>`, so a relocated install is still found.
    fn config_dir_env(&self) -> Option<&'static str> {
        None
    }

    /// Where this harness's data actually lives: the relocation env var if set, else
    /// `$HOME/<default_home_dir>`. `None` when the provider declares no location or the
    /// directory does not exist.
    fn config_dir(&self) -> Option<std::path::PathBuf> {
        if let Some(var) = self.config_dir_env()
            && let Some(v) = std::env::var_os(var)
        {
            let p = std::path::PathBuf::from(v);
            if p.is_dir() {
                return Some(p);
            }
        }
        let dir = self.default_home_dir();
        if dir.is_empty() {
            return None;
        }
        let p = std::path::PathBuf::from(std::env::var_os("HOME")?).join(dir);
        p.is_dir().then_some(p)
    }

    /// Classify one file (path relative to the capture root) for packaging. See
    /// [`Capture`]. The default keeps nothing, so a provider must opt in explicitly —
    /// a capture must never include a file just because nobody thought about it.
    fn classify(&self, _rel_path: &str) -> Capture {
        Capture::Skip
    }
}

/// What `ssa tar` should do with a file it finds in a harness's data directory.
///
/// The default is to leave a file out. Session trees sit next to credentials, OAuth
/// tokens and full config dumps, so packaging works from an allowlist: anything not
/// recognised as session data is skipped, and anything recognised as a secret is reported
/// so the user can see it was deliberately excluded rather than missed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Session data — include it.
    Include,
    /// Known-sensitive (credentials, tokens, config that embeds them). Never included;
    /// listed in the summary so the exclusion is visible.
    Sensitive,
    /// Not session data and not sensitive — quietly left out.
    Skip,
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
