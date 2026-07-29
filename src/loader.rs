//! Input loading: a single `.jsonl`, a folder of them, an archive
//! (`.zip` / `.tar` / `.tar.gz` / `.tgz`), or a `.claude` project tree.
//! Format-agnostic — enumeration and I/O only. Which harness produced the logs is
//! decided by the [`Provider`](crate::provider) layer.

use crate::model::Dataset;
use crate::provider::{self, Provider};
use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// How the input was interpreted — surfaced to the user.
#[derive(Debug, Default)]
pub struct LoadInfo {
    pub files_scanned: usize,
    pub kind: String,
    pub is_claude_tree: bool,
    pub provider_id: String,
    pub provider_name: String,
    /// Newest modification time across everything read, in epoch millis (0 = unknown).
    ///
    /// This is when the capture was last *written*, which is a different fact from when the
    /// last conversation record was *logged*. Comparing the two is how you tell a session
    /// that was captured mid-flight from one that ended and was archived later — but that
    /// is the reader's inference to draw, so this is reported as an observation and nothing
    /// more.
    pub capture_written_ms: i64,
}

impl LoadInfo {
    fn note_mtime(&mut self, ms: i64) {
        self.capture_written_ms = self.capture_written_ms.max(ms);
    }
}

/// Epoch millis of a filesystem mtime, or 0 when unavailable.
fn mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A raw line plus its source label.
type Source = String;

/// Load and merge multiple input paths into one `Dataset` (e.g. an archive plus a
/// `.claude` tree), so cross-source comparisons work in a single run.
pub fn load_all(paths: &[PathBuf], provider_id: Option<&str>) -> Result<(Dataset, LoadInfo)> {
    let mut ds = Dataset::default();
    let mut info = LoadInfo::default();
    for (i, p) in paths.iter().enumerate() {
        let (d, li) = load(p, provider_id)?;
        info.files_scanned += li.files_scanned;
        info.capture_written_ms = info.capture_written_ms.max(li.capture_written_ms);
        if i == 0 {
            info.kind = li.kind;
            info.is_claude_tree = li.is_claude_tree;
            info.provider_id = li.provider_id;
            info.provider_name = li.provider_name;
        } else {
            info.kind = format!("{} sources", i + 1);
        }
        ds.merge(d);
    }
    if ds.items.is_empty() {
        bail!("no session data found in the given path(s)");
    }
    Ok((ds, info))
}

/// Load any supported input path into a `Dataset`, using the given (or auto-detected)
/// provider.
pub fn load(path: &Path, provider_id: Option<&str>) -> Result<(Dataset, LoadInfo)> {
    if !path.exists() {
        bail!("path does not exist: {}", path.display());
    }

    let mut info = LoadInfo::default();
    let mut lines: Vec<(String, Source)> = Vec::new();

    if path.is_file() {
        match archive_kind(path) {
            Some(kind) => {
                info.kind = format!("{kind} archive");
                read_archive(path, kind, &mut lines, &mut info)?;
            }
            None => {
                info.kind = "single file".into();
                read_file(path, &source_name(path), &mut lines)?;
                info.note_mtime(mtime_ms(path));
                info.files_scanned = 1;
            }
        }
    } else {
        info.is_claude_tree = is_claude_tree(path);
        info.kind = if info.is_claude_tree { "claude project tree".into() } else { "directory".into() };
        for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            // In a `.claude` tree only the session logs live under `projects/`; skip
            // history.jsonl, tasks/, shell-snapshots and other non-session files.
            if info.is_claude_tree && p.extension().and_then(|e| e.to_str()) == Some("jsonl") && !in_projects(p) {
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let label = if info.is_claude_tree { project_label(p) } else { source_name(p) };
                if read_file(p, &label, &mut lines).is_ok() {
                    info.note_mtime(mtime_ms(p));
                    info.files_scanned += 1;
                }
            } else if is_subagent_meta(&p.to_string_lossy()) {
                let label = if info.is_claude_tree { project_label(p) } else { source_name(p) };
                if let Ok(body) = std::fs::read_to_string(p)
                    && let Some(l) = subagent_meta_line(&p.to_string_lossy(), &body)
                {
                    lines.push((l, label));
                }
            } else if let Some(kind) = archive_kind(p) {
                let _ = read_archive(p, kind, &mut lines, &mut info);
            }
        }
    }

    if lines.is_empty() {
        bail!("no session data found under {}", path.display());
    }

    // Pick a provider from a sample of the lines, then parse everything through it.
    let sample: Vec<String> = lines.iter().take(50).map(|(l, _)| l.clone()).collect();
    let prov: Box<dyn Provider> = provider::select(provider_id, &sample)?;
    info.provider_id = prov.id().to_string();
    info.provider_name = prov.display_name().to_string();

    let mut ds = Dataset::default();
    ds.provider = prov.id().to_string();
    for (line, src) in &lines {
        prov.parse_line(line, src, &mut ds);
    }
    prov.finalize(&mut ds); // normalize once (e.g. merge Claude Code block-records per turn)

    if ds.items.is_empty() {
        bail!("provider '{}' produced no events from {}", prov.id(), path.display());
    }
    Ok((ds, info))
}

/// Zip stores a DOS timestamp; convert to epoch millis via the shared civil-date parser.
fn zip_ms(d: zip::DateTime) -> Option<i64> {
    let s = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z", d.year(), d.month(), d.day(), d.hour(), d.minute(), d.second());
    let ms = crate::model::parse_ts_ms(&s);
    (ms > 0).then_some(ms)
}

/// Recognize archive containers by (possibly double) extension.
fn archive_kind(path: &Path) -> Option<&'static str> {
    let n = path.file_name()?.to_str()?.to_ascii_lowercase();
    if n.ends_with(".zip") {
        Some("zip")
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some("tar.gz")
    } else if n.ends_with(".tar") {
        Some("tar")
    } else {
        None
    }
}

fn read_archive(path: &Path, kind: &str, out: &mut Vec<(String, Source)>, info: &mut LoadInfo) -> Result<()> {
    match kind {
        "zip" => read_zip(path, out, info),
        "tar" => {
            let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            read_tar(file, &arch_label(path), out, info)
        }
        "tar.gz" => {
            let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
            read_tar(GzDecoder::new(file), &arch_label(path), out, info)
        }
        _ => Ok(()),
    }
}

fn is_claude_tree(path: &Path) -> bool {
    if path.file_name().and_then(|n| n.to_str()) == Some(".claude") {
        return true;
    }
    path.join("projects").is_dir() || path.file_name().and_then(|n| n.to_str()) == Some("projects")
}

/// True if some ancestor component is named `projects` (Claude Code session location).
fn in_projects(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == "projects")
}

/// Label a session log by its project (the dir under `projects/`), falling back to file.
fn project_label(path: &Path) -> String {
    let comps: Vec<_> = path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    if let Some(i) = comps.iter().position(|c| c == "projects") {
        if let Some(proj) = comps.get(i + 1) {
            return proj.clone();
        }
    }
    source_name(path)
}

fn read_file(path: &Path, label: &str, out: &mut Vec<(String, Source)>) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        out.push((line, label.to_string()));
    }
    Ok(())
}

fn read_zip(path: &Path, out: &mut Vec<(String, Source)>, info: &mut LoadInfo) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("opening zip {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file).with_context(|| format!("reading zip {}", path.display()))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let is_meta = is_subagent_meta(entry.name());
        if !entry.is_file() || !(entry.name().ends_with(".jsonl") || is_meta) {
            continue;
        }
        let entry_name = entry.name().to_string();
        let mut buf = String::new();
        if entry.read_to_string(&mut buf).is_err() {
            continue;
        }
        info.note_mtime(entry.last_modified().and_then(zip_ms).unwrap_or(0));
        let src = format!("{}:{}", arch_label(path), entry_name);
        if is_meta {
            if let Some(l) = subagent_meta_line(&entry_name, &buf) {
                out.push((l, src));
            }
            continue;
        }
        for line in buf.lines() {
            out.push((line.to_string(), src.clone()));
        }
        info.files_scanned += 1;
    }
    Ok(())
}

fn read_tar<R: Read>(reader: R, label_prefix: &str, out: &mut Vec<(String, Source)>, info: &mut LoadInfo) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    for entry in archive.entries().context("reading tar")? {
        let mut e = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let entry_name = e.path().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let is_meta = is_subagent_meta(&entry_name);
        if !entry_name.ends_with(".jsonl") && !is_meta {
            continue;
        }
        let mut buf = String::new();
        if e.read_to_string(&mut buf).is_err() {
            continue;
        }
        info.note_mtime(e.header().mtime().map(|s| s as i64 * 1000).unwrap_or(0));
        let src = format!("{}:{}", label_prefix, entry_name);
        if is_meta {
            if let Some(l) = subagent_meta_line(&entry_name, &buf) {
                out.push((l, src));
            }
            continue;
        }
        for line in buf.lines() {
            out.push((line.to_string(), src.clone()));
        }
        info.files_scanned += 1;
    }
    Ok(())
}

/// `.claude` writes each sub-agent's sidecar as
/// `<session-id>/subagents/agent-<agent-id>.meta.json`.
fn is_subagent_meta(path: &str) -> bool {
    let p = Path::new(path);
    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("agent-") && n.ends_with(".meta.json"))
        && p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()) == Some("subagents")
}

/// Turn a sub-agent sidecar into a one-line record the provider can parse alongside the
/// session logs. The session and agent ids live in the path, not the file, so they are
/// folded into the JSON here; `type` marks it for the provider layer.
///
/// This is the only source of a sub-agent's *type* when it never returned a result (still
/// running, stopped early, truncated export) — without it those agents are unattributable.
fn subagent_meta_line(path: &str, body: &str) -> Option<String> {
    let p = Path::new(path);
    let file = p.file_name()?.to_str()?;
    let agent_id = file.strip_prefix("agent-")?.strip_suffix(".meta.json")?;
    let session_id = p.parent()?.parent()?.file_name()?.to_str()?;
    let mut v: serde_json::Value = serde_json::from_str(body).ok()?;
    if !v.is_object() {
        return None;
    }
    v["type"] = serde_json::Value::String("subagent-meta".into());
    v["sessionId"] = serde_json::Value::String(session_id.to_string());
    v["agentId"] = serde_json::Value::String(agent_id.to_string());
    Some(v.to_string())
}

fn arch_label(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("archive").to_string()
}

fn source_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string())
}

/// Best-effort default input when none is supplied: `./data`, else the first registered
/// provider's config directory (`$CLAUDE_CONFIG_DIR`, else `~/.claude`).
///
/// This runs before provider auto-detection, so it asks every provider where its data
/// lives rather than hardcoding one harness's layout.
pub fn default_path() -> Option<PathBuf> {
    let data = PathBuf::from("data");
    if data.is_dir() {
        return Some(data);
    }
    provider::registry().iter().find_map(|p| p.config_dir())
}
