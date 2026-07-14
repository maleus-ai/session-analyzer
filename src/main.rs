//! ssa — deep token-consumption analysis for agent session logs.
//!
//! With no subcommand it launches the interactive TUI. Every TUI view is also a headless
//! subcommand emitting text / json / csv, so an LLM agent can use the tool without a TTY.

mod analysis;
mod loader;
mod model;
mod pricing;
mod provider;
mod providers;
mod query;
mod report;
mod tui;

use analysis::{Analysis, SessionReport};
use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use report::{Ctx, Fmt};
use std::io::Write;
use std::path::PathBuf;

// ------------------------------------------------------------------------- CLI

#[derive(Parser, Debug)]
#[command(
    name = "ssa",
    version,
    about = "Analyze token consumption, sinks, cache efficiency and context growth in agent session logs.",
    propagate_version = true,
    after_help = "EXAMPLES:\n  \
        ssa                         open the TUI on ./data or ~/.claude\n  \
        ssa ~/.claude               open the TUI on a path\n  \
        ssa overview -p data        headless summary\n  \
        ssa sinks -p data --top 10  top token sinks\n  \
        ssa timeline -p data --session 7ce6 --format json\n\n\
        Run `ssa <command> --help` for a command's options."
)]
struct Cli {
    /// Session logs to analyze: a folder, a .jsonl, an archive (.zip/.tar/.tar.gz/.tgz),
    /// or a .claude project tree. Repeatable (`-p A -p B`) to merge multiple sources.
    /// Defaults to ./data then ~/.claude.
    #[arg(short = 'p', long, global = true, value_name = "PATH")]
    path: Vec<PathBuf>,

    /// Force a provider instead of auto-detecting (e.g. claude-code).
    #[arg(long, global = true, value_name = "ID")]
    provider: Option<String>,

    /// PATH for the default TUI (equivalent to --path; lets you write `ssa <PATH>`).
    #[arg(value_name = "PATH")]
    path_pos: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Launch the interactive TUI dashboard (this is the default with no command)
    Tui,

    /// Totals: tokens, cost, cache hit/churn, context peak and per-model split
    Overview(ScopeArgs),

    /// Throughput & subscription pressure: fresh-tokens/hour, cache-write/hour, cost/hour
    /// and the peak rolling window (what actually exhausts a Max subscription)
    Rate(RateArgs),

    /// Rank every session by cost, tokens, cache-hit or turns
    ///
    /// --sort: cost | tokens | hit | turns | tools | subag | duration.  --grep filters by title/cwd.
    Sessions(SessionsArgs),

    /// Aggregate cost & tokens per project (working directory) across a .claude tree
    Projects(ProjectsArgs),

    /// Per-model breakdown: turns, tokens, cost (and fresh-token share)
    Models(ScopeArgs),

    /// Compare two or more sessions side by side (repeat --session)
    Compare(CompareArgs),

    /// Rank sessions by subscription pressure (sustained cache-write/hour) and flag
    /// unattended sdk-ts bursts that concentrate load into one rolling window
    Pressure(ProjectsArgs),

    /// Per-tool call counts, result-token footprint and errors
    ///
    /// --sort: result | calls | input | errors | name
    Tools(ListArgs),

    /// Token sinks ranked by amplified cost (payload size × turns resident in context)
    ///
    /// --sort: amplified | size | calls | residency | contribution
    Sinks(ListArgs),

    /// Decompose cache-read by the content that stayed resident in context
    ///
    /// --sort: contribution | share | entries | tokens
    #[command(name = "cache-attr")]
    CacheAttr(ListArgs),

    /// Per sub-agent tokens, tool mix, reads/edits and lines changed
    ///
    /// --sort: tokens | tools | reads | edits | duration
    Subagents(ListArgs),

    /// Per-turn context size, deltas, write/read/out and cost for one session
    ///
    /// --sort: turn | context | write | delta | cost
    Timeline(TimelineArgs),

    /// Context-growth sparkline plus detected spikes for one session
    Growth(SessionOnlyArgs),

    /// Anomalous context-growth turns and their cause
    ///
    /// --sort: delta | turn
    Spikes(ListArgs),

    /// Ordered messages (chat bubbles) with per-message token stats for one session
    Transcript(TranscriptArgs),

    /// Print the full text of a single transcript item (message / tool input / result)
    Show(ShowArgs),

    /// Detected inefficiencies: low cache hit, churn, spikes, repeated reads, errors, …
    Issues(ScopeArgs),
}

/// Output format shared by every command.
#[derive(ValueEnum, Clone, Copy, Debug)]
enum Format {
    Text,
    Json,
    Csv,
}
impl From<Format> for Fmt {
    fn from(f: Format) -> Fmt {
        match f {
            Format::Text => Fmt::Text,
            Format::Json => Fmt::Json,
            Format::Csv => Fmt::Csv,
        }
    }
}

/// Transcript role filter.
#[derive(ValueEnum, Clone, Copy, Debug)]
enum Kind {
    User,
    Assistant,
    Tool,
    Compact,
}
impl Kind {
    fn as_str(&self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Assistant => "assistant",
            Kind::Tool => "tool",
            Kind::Compact => "compact",
        }
    }
}

// Shared option groups — each subcommand only shows the options relevant to it.

#[derive(Args, Debug)]
struct ScopeArgs {
    /// Restrict to a single session (id prefix match)
    #[arg(long)]
    session: Option<String>,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct RateArgs {
    /// Restrict to a single session (id prefix match); default is all sessions combined
    #[arg(long)]
    session: Option<String>,
    /// Rolling-window size in hours (Claude Max ≈ 5)
    #[arg(long, default_value_t = 5.0)]
    window: f64,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct CompareArgs {
    /// Session id prefixes to compare (repeat: --session A --session B)
    #[arg(long = "session", required = true, num_args = 1..)]
    sessions: Vec<String>,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct SessionsArgs {
    /// Sort column (see command description for valid values)
    #[arg(long)]
    sort: Option<String>,
    /// Sort ascending (default is descending / largest-first)
    #[arg(long)]
    asc: bool,
    /// Filter sessions whose title or working directory contains this text (case-insensitive)
    #[arg(long)]
    grep: Option<String>,
    /// Maximum rows
    #[arg(long, default_value_t = 0)]  // 0 = all (no cap by default)
    top: usize,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct ProjectsArgs {
    /// Maximum rows
    #[arg(long, default_value_t = 0)]  // 0 = all (no cap by default)
    top: usize,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct ListArgs {
    /// Restrict to a single session (id prefix match)
    #[arg(long)]
    session: Option<String>,
    /// Sort column (see command description for valid values)
    #[arg(long)]
    sort: Option<String>,
    /// Sort ascending (default is descending / largest-first)
    #[arg(long)]
    asc: bool,
    /// Only rows with at least this many tokens (sinks/cache-attr/tools)
    #[arg(long, default_value_t = 0)]
    min_tokens: u64,
    /// Filter by model substring (subagents)
    #[arg(long)]
    model: Option<String>,
    /// Maximum rows
    #[arg(long, default_value_t = 0)]  // 0 = all (no cap by default)
    top: usize,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct TimelineArgs {
    /// Session to inspect (id prefix match)
    #[arg(long)]
    session: String,
    /// Sort column (see command description for valid values)
    #[arg(long)]
    sort: Option<String>,
    /// Sort ascending (default is descending)
    #[arg(long)]
    asc: bool,
    /// Only turns on this model (substring match)
    #[arg(long)]
    model: Option<String>,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct SessionOnlyArgs {
    /// Session to inspect (id prefix match)
    #[arg(long)]
    session: String,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct TranscriptArgs {
    /// Session to inspect (id prefix match)
    #[arg(long)]
    session: String,
    /// Only show messages of this role
    #[arg(long, value_enum)]
    kind: Option<Kind>,
    /// Only show messages whose text contains this substring (case-insensitive)
    #[arg(long)]
    grep: Option<String>,
    /// Include full message text instead of a preview
    #[arg(long)]
    full: bool,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct ShowArgs {
    /// Session to inspect (id prefix match)
    #[arg(long)]
    session: String,
    /// Transcript item index (see `transcript` output, e.g. [#42])
    #[arg(long)]
    item: usize,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

// -------------------------------------------------------------------- runtime

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    let mut paths: Vec<PathBuf> = cli.path.clone();
    if let Some(pp) = cli.path_pos.clone() {
        paths.push(pp);
    }
    if paths.is_empty() {
        match loader::default_path() {
            Some(p) => paths.push(p),
            None => bail!("no input path; pass a PATH or -p, or run from a dir with ./data or ~/.claude"),
        }
    }

    let (dataset, info) = loader::load_all(&paths, cli.provider.as_deref())?;
    let analysis = analysis::analyze(&dataset);

    let output = match cli.command {
        None | Some(Command::Tui) => return tui::run(&analysis, &info),
        Some(cmd) => dispatch(&analysis, cmd)?,
    };
    emit(if output.ends_with('\n') { output } else { format!("{output}\n") }.as_str());
    Ok(())
}

/// Run one headless command and return its rendered output.
fn dispatch(a: &Analysis, cmd: Command) -> Result<String> {
    match cmd {
        Command::Tui => unreachable!("handled in run()"),
        Command::Overview(o) => Ok(report::overview(&ctx(a, o.session.as_deref(), None, false, 0, o.format)?)),
        Command::Issues(o) => Ok(report::issues(&ctx(a, o.session.as_deref(), None, false, 0, o.format)?)),
        Command::Rate(o) => {
            let mut c = ctx(a, o.session.as_deref(), None, false, 0, o.format)?;
            c.window_hours = o.window;
            Ok(report::rate(&c))
        }
        Command::Sessions(o) => {
            let mut c = ctx(a, None, o.sort, !o.asc, o.top, o.format)?;
            c.grep = o.grep;
            report::sessions(&c)
        }
        Command::Projects(o) => Ok(report::projects(&ctx(a, None, None, false, o.top, o.format)?)),
        Command::Models(o) => Ok(report::models(&ctx(a, o.session.as_deref(), None, false, 0, o.format)?)),
        Command::Compare(o) => report::compare(a, &o.sessions, o.format.into()),
        Command::Pressure(o) => Ok(report::pressure(&ctx(a, None, None, false, o.top, o.format)?)),
        Command::Tools(o) => report::tools(&list_ctx(a, o)?),
        Command::Sinks(o) => report::sinks(&list_ctx(a, o)?),
        Command::CacheAttr(o) => report::cache_attr(&list_ctx(a, o)?),
        Command::Subagents(o) => report::subagents(&list_ctx(a, o)?),
        Command::Spikes(o) => report::spikes(&list_ctx(a, o)?),
        Command::Timeline(o) => {
            let mut c = ctx(a, Some(&o.session), o.sort, !o.asc, 0, o.format)?;
            c.model = o.model;
            report::timeline(&c)
        }
        Command::Growth(o) => report::growth(&ctx(a, Some(&o.session), None, false, 0, o.format)?),
        Command::Transcript(o) => {
            let mut c = ctx(a, Some(&o.session), None, false, 0, o.format)?;
            c.grep = o.grep;
            report::transcript(&c, o.kind.map(|k| k.as_str()), o.full)
        }
        Command::Show(o) => report::show(&ctx(a, Some(&o.session), None, false, 0, o.format)?, o.item),
    }
}

/// Build a context for the shared list commands, applying `--min-tokens`/`--model`.
fn list_ctx<'a>(a: &'a Analysis, o: ListArgs) -> Result<Ctx<'a>> {
    let mut c = ctx(a, o.session.as_deref(), o.sort, !o.asc, o.top, o.format)?;
    c.min_tokens = o.min_tokens;
    c.model = o.model;
    Ok(c)
}

/// Build a reporting context, resolving a session id prefix to a concrete report.
fn ctx<'a>(
    a: &'a Analysis,
    session: Option<&str>,
    sort: Option<String>,
    desc: bool,
    top: usize,
    format: Format,
) -> Result<Ctx<'a>> {
    let session = match session {
        None => None,
        Some(sel) => {
            let matches: Vec<&SessionReport> = a.sessions.iter().filter(|s| s.session_id.starts_with(sel)).collect();
            match matches.as_slice() {
                [one] => Some(*one),
                [] => bail!("no session matched '{}'", sel),
                many => bail!("'{}' matched {} sessions; use a longer prefix", sel, many.len()),
            }
        }
    };
    Ok(Ctx {
        a,
        session,
        sort,
        desc,
        top: if top == 0 { usize::MAX } else { top },
        fmt: format.into(),
        window_hours: analysis::RATE_WINDOW_HOURS,
        grep: None,
        min_tokens: 0,
        model: None,
    })
}

/// Write to stdout, exiting cleanly if the reader closed the pipe (e.g. `| head`).
fn emit(s: &str) {
    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    match h.write_all(s.as_bytes()).and_then(|_| h.flush()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
