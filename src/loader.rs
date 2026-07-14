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
                    info.files_scanned += 1;
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
        if !entry.is_file() || !entry.name().ends_with(".jsonl") {
            continue;
        }
        let entry_name = entry.name().to_string();
        let mut buf = String::new();
        if entry.read_to_string(&mut buf).is_err() {
            continue;
        }
        let src = format!("{}:{}", arch_label(path), entry_name);
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
        if !entry_name.ends_with(".jsonl") {
            continue;
        }
        let mut buf = String::new();
        if e.read_to_string(&mut buf).is_err() {
            continue;
        }
        let src = format!("{}:{}", label_prefix, entry_name);
        for line in buf.lines() {
            out.push((line.to_string(), src.clone()));
        }
        info.files_scanned += 1;
    }
    Ok(())
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

/// Best-effort default input when none is supplied: `./data`, else `~/.claude`.
pub fn default_path() -> Option<PathBuf> {
    let data = PathBuf::from("data");
    if data.is_dir() {
        return Some(data);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let c = PathBuf::from(home).join(".claude");
        if c.is_dir() {
            return Some(c);
        }
    }
    None
}
