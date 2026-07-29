//! Packaging a harness's session tree for sharing (`ssa tar`).
//!
//! A session tree is not safe to `tar czf` wholesale: `~/.claude` keeps live OAuth
//! credentials, config backups and shell snapshots (which capture the environment,
//! including API keys) in the same directory as the logs. Anyone handed such an archive
//! holds the user's credentials.
//!
//! So packaging works from an **allowlist**, defined per provider: a file is included only
//! if the provider recognises it as session data. Known secrets are reported by name so the
//! user can see they were excluded on purpose rather than missed, and everything else is
//! quietly skipped. A new file appearing in a future version of the harness is left out by
//! default rather than leaked.
//!
//! The log *contents* are a separate matter: tool output inside a transcript can contain
//! anything the agent read. [`scan_secrets`] does a cheap pass over the payload and warns
//! about the obvious shapes (API keys, bearer tokens, private keys) so the user knows
//! before sending. It is a warning, not a guarantee.

use crate::provider::{self, Capture, Provider};
use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// What a capture would contain, decided before anything is written.
pub struct Plan {
    pub root: PathBuf,
    /// Files to package, as (absolute path, path inside the archive).
    pub include: Vec<(PathBuf, String)>,
    /// Known-sensitive files, excluded. Reported so the exclusion is visible.
    pub sensitive: Vec<String>,
    /// Everything else that was left out, counted by top-level directory.
    pub skipped: BTreeMap<String, usize>,
    pub total_bytes: u64,
}

impl Plan {
    pub fn sessions(&self) -> usize {
        self.include
            .iter()
            .filter(|(_, rel)| rel.ends_with(".jsonl") && !rel.contains("/subagents/"))
            .count()
    }
}

/// Resolve the directory to package: an explicit path, else `$HOME/<provider default>`.
pub fn default_source(prov: &dyn Provider) -> Option<PathBuf> {
    prov.config_dir()
}

/// Decide what a capture of `root` would contain. Nothing is written.
///
/// `session` optionally narrows to one session id (prefix match): its log, its
/// `subagents/` sidecars, and nothing else.
pub fn plan(prov: &dyn Provider, root: &Path, session: Option<&str>) -> Result<Plan> {
    if !root.is_dir() {
        bail!("not a directory: {}", root.display());
    }
    let mut p = Plan {
        root: root.to_path_buf(),
        include: Vec::new(),
        sensitive: Vec::new(),
        skipped: BTreeMap::new(),
        total_bytes: 0,
    };
    // The archive root is named after the directory, so it unpacks into its own folder.
    let base = root.file_name().and_then(|n| n.to_str()).unwrap_or("capture").to_string();

    for entry in WalkDir::new(root).follow_links(false).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else { continue };
        let rel = rel.to_string_lossy().replace('\\', "/");
        match prov.classify(&rel) {
            Capture::Sensitive => p.sensitive.push(rel),
            Capture::Skip => {
                let top = rel.split('/').next().unwrap_or("(root)").to_string();
                *p.skipped.entry(top).or_default() += 1;
            }
            Capture::Include => {
                if let Some(sel) = session {
                    // Keep the session's own log plus its `<id>/subagents/*` sidecars.
                    if !rel.split('/').any(|c| c.starts_with(sel)) {
                        continue;
                    }
                }
                p.total_bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                p.include.push((path.to_path_buf(), format!("{base}/{rel}")));
            }
        }
    }
    p.include.sort_by(|a, b| a.1.cmp(&b.1));
    p.sensitive.sort();
    if p.include.is_empty() {
        match session {
            Some(s) => bail!("no session log under {} matched '{}'", root.display(), s),
            None => bail!("no session logs found under {}", root.display()),
        }
    }
    Ok(p)
}

/// Write the planned files to a `.tar.gz`. Refuses to overwrite unless `force`.
pub fn write(plan: &Plan, out: &Path, force: bool) -> Result<u64> {
    if out.exists() && !force {
        bail!("{} already exists (pass --force to overwrite)", out.display());
    }
    let file = std::fs::File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut tar = tar::Builder::new(GzEncoder::new(file, Compression::default()));
    for (abs, rel) in &plan.include {
        tar.append_path_with_name(abs, rel).with_context(|| format!("adding {}", abs.display()))?;
    }
    let enc = tar.into_inner().context("finishing archive")?;
    enc.finish().context("flushing archive")?;
    Ok(std::fs::metadata(out).map(|m| m.len()).unwrap_or(0))
}

/// Cheap scan of the payload for credential-shaped strings.
///
/// Transcripts embed whatever the agent read, so a clean file list does not mean a clean
/// archive. Returns `(label, occurrences)` per pattern. Heuristic and deliberately
/// conservative — a warning to the user, never a claim that the capture is safe.
pub fn scan_secrets(plan: &Plan) -> Vec<(&'static str, usize)> {
    // (label, needle). Substring matching only: cheap, and these shapes are distinctive.
    const NEEDLES: [(&str, &str); 8] = [
        ("Anthropic API key", "sk-ant-"),
        ("OpenAI API key", "sk-proj-"),
        ("AWS access key id", "AKIA"),
        ("GitHub token", "ghp_"),
        ("GitHub PAT (fine-grained)", "github_pat_"),
        ("Slack token", "xoxb-"),
        ("private key block", "-----BEGIN"),
        ("bearer token", "Authorization: Bearer "),
    ];
    let mut hits: BTreeMap<&'static str, usize> = BTreeMap::new();
    for (abs, _) in &plan.include {
        let Ok(body) = std::fs::read_to_string(abs) else { continue };
        for (label, needle) in NEEDLES {
            let n = body.matches(needle).count();
            if n > 0 {
                *hits.entry(label).or_default() += n;
            }
        }
    }
    hits.into_iter().collect()
}

/// What an archive or tree you were *handed* contains beyond session logs.
///
/// `ssa tar` prevents you shipping credentials; this is the other direction — noticing that
/// someone already did. `ssa` only ever reads `projects/**`, so without this a capture looks
/// clean from inside the tool no matter what else is in it.
pub struct Audit {
    pub source: PathBuf,
    /// Every file classified as session data. Not the number of *sessions* — see the
    /// breakdown fields, which is what a reader actually wants to compare against
    /// `overview`'s session count.
    pub logs: usize,
    pub sensitive: Vec<String>,
    pub other: Vec<String>,
    /// Top-level session transcripts (`projects/<proj>/<id>.jsonl`).
    pub session_logs: usize,
    /// Sub-agent transcripts (`.../subagents/agent-*.jsonl`).
    pub subagent_logs: usize,
    /// Sub-agent metadata sidecars (`.../subagents/agent-*.meta.json`).
    pub sidecars: usize,
}

impl Audit {
    /// Harness state files in the capture — things like `sessions/<pid>.json` that record a
    /// running or most-recent session. Their presence is evidence about *when* the capture
    /// was taken relative to the work, which the logs alone cannot settle. Reported as
    /// files found; what they imply is the reader's call.
    pub fn state_files(&self) -> Vec<&str> {
        self.other
            .iter()
            .map(String::as_str)
            .filter(|f| f.contains("/sessions/") || f.ends_with("/history.jsonl") || f.contains("shell-snapshots"))
            .collect()
    }
}

/// Inspect a capture (directory, `.tar`, `.tar.gz`, `.tgz` or `.zip`) without extracting it.
pub fn audit(prov: &dyn Provider, path: &Path) -> Result<Audit> {
    let mut a = Audit {
        source: path.to_path_buf(),
        logs: 0,
        sensitive: Vec::new(),
        other: Vec::new(),
        session_logs: 0,
        subagent_logs: 0,
        sidecars: 0,
    };
    fn tally(a: &mut Audit, rel: &str) {
        a.logs += 1;
        if rel.ends_with(".meta.json") {
            a.sidecars += 1;
        } else if rel.contains("/subagents/") {
            a.subagent_logs += 1;
        } else {
            a.session_logs += 1;
        }
    }
    let note = |member: &str, a: &mut Audit| {
        // Archive members are prefixed with the capture root; classify on the tail so the
        // same rules apply to a directory and to an archive of it.
        let rel = member.split_once('/').map(|(_, r)| r).unwrap_or(member);
        match prov.classify(rel) {
            Capture::Include => tally(a, rel),
            Capture::Sensitive => a.sensitive.push(member.to_string()),
            Capture::Skip => {
                if !member.ends_with('/') {
                    a.other.push(member.to_string())
                }
            }
        }
    };

    if path.is_dir() {
        for e in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
            if e.file_type().is_file()
                && let Ok(rel) = e.path().strip_prefix(path)
            {
                let rel = rel.to_string_lossy().replace('\\', "/");
                match prov.classify(&rel) {
                    Capture::Include => tally(&mut a, &rel),
                    Capture::Sensitive => a.sensitive.push(rel),
                    Capture::Skip => a.other.push(rel),
                }
            }
        }
    } else {
        for m in archive_members(path)? {
            note(&m, &mut a);
        }
    }
    a.sensitive.sort();
    a.other.sort();
    Ok(a)
}

/// Member names of a supported archive, without extracting it.
fn archive_members(path: &Path) -> Result<Vec<String>> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_ascii_lowercase();
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    if name.ends_with(".zip") {
        let mut z = zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;
        for i in 0..z.len() {
            out.push(z.by_index(i)?.name().to_string());
        }
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        collect_tar(flate2::read::GzDecoder::new(file), &mut out)?;
    } else if name.ends_with(".tar") {
        collect_tar(file, &mut out)?;
    } else {
        bail!("not an archive or directory: {}", path.display());
    }
    Ok(out)
}

fn collect_tar<R: std::io::Read>(r: R, out: &mut Vec<String>) -> Result<()> {
    for e in tar::Archive::new(r).entries().context("reading tar")? {
        let e = e.context("reading tar entry")?;
        if e.header().entry_type().is_file() {
            out.push(e.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
        }
    }
    Ok(())
}

/// One-line warning for a capture that carries credentials, or `None` when it is clean.
/// Printed to stderr on load so it cannot be missed, and cannot corrupt piped output.
pub fn load_warning(paths: &[PathBuf]) -> Option<String> {
    let prov = resolve(None).ok()?;
    let mut hits: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for p in paths {
        if let Ok(a) = audit(prov.as_ref(), p)
            && !a.sensitive.is_empty()
        {
            hits.push((p.clone(), a.sensitive));
        }
    }
    if hits.is_empty() {
        return None;
    }
    let mut s = String::new();
    for (p, files) in hits {
        let names: Vec<String> = files.iter().take(3).map(|f| f.rsplit('/').next().unwrap_or(f).to_string()).collect();
        s.push_str(&format!(
            "warning: {} contains {} credential file(s) ({}{}). Repackage with `ssa tar` before sharing; rotate anything already sent.\n",
            p.display(),
            files.len(),
            names.join(", "),
            if files.len() > 3 { ", …" } else { "" }
        ));
    }
    Some(s)
}

/// Resolve a provider by id/alias for `ssa tar` (`claude` → `claude-code`).
pub fn resolve(name: Option<&str>) -> Result<Box<dyn Provider>> {
    let Some(name) = name else {
        // Single provider registered → unambiguous default.
        let mut all = provider::registry();
        if all.len() == 1 {
            return Ok(all.remove(0));
        }
        bail!("specify a provider, e.g. `ssa tar claude`");
    };
    let want = name.to_ascii_lowercase();
    for p in provider::registry() {
        if p.id() == want || p.id().starts_with(&want) || p.display_name().to_ascii_lowercase().contains(&want) {
            return Ok(p);
        }
    }
    let known: Vec<&str> = provider::registry().iter().map(|p| p.id()).collect();
    bail!("unknown provider '{}'; known: {}", name, known.join(", "))
}
