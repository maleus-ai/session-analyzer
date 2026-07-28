//! ssa — deep token-consumption analysis for agent session logs.
//!
//! With no subcommand it launches the interactive TUI. Every TUI view is also a headless
//! subcommand emitting text / json / csv, so an LLM agent can use the tool without a TTY.

mod analysis;
mod capture;
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
use std::fmt::Write as _;
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
    /// --sort: result | calls | input | errors | name.  Pass --available for the tool
    /// *roster* instead: what the agent could reach, and what it asked for and could not.
    Tools(ToolsArgs),

    /// Token sinks ranked by amplified cost (payload size × turns resident in context)
    ///
    /// --sort: amplified | size | calls | residency | contribution
    Sinks(ListArgs),

    /// Decompose cache-read by the content that stayed resident in context
    ///
    /// --sort: contribution | share | entries | tokens
    #[command(name = "cache-attr")]
    CacheAttr(ListArgs),

    /// Per sub-agent tokens, tool mix, reads/edits and lines changed (agents that finished)
    ///
    /// --sort: tokens | tools | reads | edits | duration
    Subagents(ListArgs),

    /// Every sub-agent conversation in the log, including ones that never returned
    ///
    /// Lists agent id, type, nesting depth, turns/tokens, timing and how each one ended, so
    /// you can then read one with `ssa transcript --session <id> --agent <agent-id>`.
    Agents(AgentsArgs),

    /// The session's runs (one SDK query / prompt cycle each) and how each one ended
    ///
    /// Turn limits apply per run, so this is what a `maxTurns` setting must be compared
    /// against — a session's total turn count exceeds it whenever there was >1 run.
    Runs(ScopeArgs),

    /// One line per turn: when, which thread, what it called — the "what did it do" view
    ///
    /// Accepts the same filters as `transcript`. A loop shows up as the same tool and
    /// target repeating down the WHAT column.
    Trace(TraceArgs),

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
    ///
    /// Sub-agent conversations are nested under the turn that spawned them and tagged with
    /// the agent. Narrow with --thread / --agent / --tool / --kind / --grep, page with
    /// --from / --limit, and widen around hits with --context.
    Transcript(TranscriptArgs),

    /// Find messages across sessions and sub-agents; prints locators to feed `show`
    ///
    /// One row per hit (session, item index, thread, snippet). Then read it in full with
    /// `ssa show --session <id> --item <n>` or in place with
    /// `ssa transcript --session <id> --from <n> --limit 5 --full`.
    Search(SearchArgs),

    /// Print the full text of a single transcript item (message / tool input / result)
    Show(ShowArgs),

    /// Detected inefficiencies: low cache hit, churn, spikes, repeated reads, errors, …
    Issues(ScopeArgs),

    /// Inspect a capture you were handed: does it carry credentials it shouldn't?
    ///
    /// The mirror of `ssa tar`. `ssa` only ever reads `projects/**`, so an archive full of
    /// credentials looks clean from inside every other command. Works on an archive or a
    /// directory, without extracting.
    Audit(AuditArgs),

    /// Package a harness's session logs into a shareable .tar.gz, excluding credentials
    ///
    /// `ssa tar claude` packages ~/.claude. Only files the provider recognises as session
    /// data are included; credentials, config backups and shell snapshots are excluded by
    /// construction and listed so you can see they were left out. Use this instead of
    /// `tar czf ~/.claude` — that ships the user's OAuth tokens.
    Tar(TarArgs),
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
    /// Harness events: turn/token limits, reminders, hook results, memory loads
    Event,
}
impl Kind {
    fn as_str(&self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Assistant => "assistant",
            Kind::Tool => "tool",
            Kind::Compact => "compact",
            Kind::Event => "event",
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

/// Which conversation thread(s) to include.
#[derive(ValueEnum, Clone, Copy, Debug)]
enum ThreadArg {
    /// Main conversation and every sub-agent
    All,
    /// Main conversation only
    Main,
    /// Sub-agent (sidechain) conversations only
    Sub,
}
impl From<ThreadArg> for report::Thread {
    fn from(t: ThreadArg) -> Self {
        match t {
            ThreadArg::All => report::Thread::All,
            ThreadArg::Main => report::Thread::Main,
            ThreadArg::Sub => report::Thread::Sub,
        }
    }
}

/// Message selection shared by `transcript` and `search`.
#[derive(Args, Debug, Clone)]
struct FilterArgs {
    /// Only messages of this role: user | assistant | tool | compact | event
    /// (`event` = harness events such as turn limits, reminders, hook results)
    #[arg(long, value_enum)]
    kind: Option<Kind>,
    /// Only messages whose text contains this substring (case-insensitive)
    #[arg(long)]
    grep: Option<String>,
    /// Which thread to read: all (default), main (hide sub-agents), sub (only sub-agents)
    #[arg(long, value_enum)]
    thread: Option<ThreadArg>,
    /// Only messages from sub-agents matching this id / type / description substring
    /// (e.g. --agent Explore). Implies --thread sub.
    #[arg(long)]
    agent: Option<String>,
    /// Only messages from sub-agents nested exactly this deep (1 = spawned by the main
    /// thread). Implies --thread sub.
    #[arg(long)]
    depth: Option<usize>,
    /// Only messages from sub-agents nested at least this deep. Implies --thread sub.
    #[arg(long)]
    min_depth: Option<usize>,
    /// Only messages involving this tool: calls made or results returned (e.g. --tool Bash)
    #[arg(long)]
    tool: Option<String>,
    /// Only messages whose tool *input* contains this substring (e.g. the prompt handed to
    /// an Agent call, or a command passed to Bash)
    #[arg(long)]
    input_grep: Option<String>,
    /// Only messages in this run (1-based; see `ssa runs`)
    #[arg(long)]
    run: Option<usize>,
    /// Only messages at/after this time — `HH:MM[:SS]` or `YYYY-MM-DD HH:MM[:SS]` (UTC)
    #[arg(long)]
    since: Option<String>,
    /// Only messages at/before this time (same formats as --since)
    #[arg(long)]
    until: Option<String>,
    /// Treat --grep / --input-grep as regular expressions (case-insensitive)
    #[arg(long)]
    regex: bool,
    /// Only failures: tool results that errored, and assistant turns that hit an API error
    #[arg(long)]
    errors_only: bool,
    /// Start at this transcript item index (pagination; see [#N] in the output)
    #[arg(long, default_value_t = 0)]
    from: usize,
    /// Maximum messages to return (0 = all)
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// Also include N messages either side of each --grep hit
    #[arg(long, default_value_t = 0)]
    context: usize,
}

impl FilterArgs {
    fn to_filter(&self, day_ms: i64) -> Result<report::ItemFilter> {
        Ok(report::ItemFilter {
            kind: self.kind.map(|k| k.as_str().to_string()),
            grep: self.grep.clone(),
            thread: self.thread.map(Into::into),
            agent: self.agent.clone(),
            depth: self.depth,
            min_depth: self.min_depth,
            tool: self.tool.clone(),
            input_grep: self.input_grep.clone(),
            run: self.run,
            since_ms: self.since.as_deref().map(|s| parse_when(s, day_ms)).transpose()?,
            until_ms: self.until.as_deref().map(|s| parse_when(s, day_ms)).transpose()?,
            regex: self.regex,
            errors_only: self.errors_only,
        from: self.from,
            limit: self.limit,
            context: self.context,
        })
    }
}

/// Parse `--since` / `--until`: a full `YYYY-MM-DD HH:MM[:SS]`, or a bare `HH:MM[:SS]`
/// resolved against `day_ms` (the day the session in scope started), so you can say
/// `--since 07:55` while reading a transcript without restating the date.
fn parse_when(s: &str, day_ms: i64) -> Result<i64> {
    const DAY: i64 = 86_400_000;
    let s = s.trim();
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (Some(d), t),
        None if s.contains(':') => (None, s),
        None => bail!("cannot parse time '{}': expected HH:MM[:SS] or YYYY-MM-DD HH:MM[:SS]", s),
    };
    let mut parts = time.split(':');
    let mut num = |what: &str| -> Result<i64> {
        match parts.next() {
            Some(p) => p.trim_end_matches('Z').parse::<i64>().map_err(|_| anyhow::anyhow!("bad {what} in '{s}'")),
            None => Ok(0),
        }
    };
    let (h, mi, se) = (num("hour")?, num("minute")?, num("second")?);
    let tod = (h * 3600 + mi * 60 + se) * 1000;
    match date {
        // `parse_ts_ms` needs a full timestamp; give it midnight and add the time of day.
        Some(d) => {
            let base = crate::model::parse_ts_ms(&format!("{d}T00:00:00.000Z"));
            if base == 0 {
                bail!("cannot parse date in '{}': expected YYYY-MM-DD", s);
            }
            Ok(base + tod)
        }
        None => Ok(day_ms.div_euclid(DAY) * DAY + tod),
    }
}

#[derive(Args, Debug)]
struct TranscriptArgs {
    /// Session to inspect (id prefix match)
    #[arg(long)]
    session: String,
    #[command(flatten)]
    filter: FilterArgs,
    /// Include full message text instead of a preview
    #[arg(long)]
    full: bool,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct AgentsArgs {
    /// Restrict to a single session (id prefix match)
    #[arg(long)]
    session: Option<String>,
    /// Only agents whose work falls in this run (1-based; see `ssa runs`)
    #[arg(long)]
    run: Option<usize>,
    /// Only agents matching this id / type / description substring
    #[arg(long)]
    agent: Option<String>,
    /// Only agents nested at least this deep
    #[arg(long)]
    min_depth: Option<usize>,
    /// Only agents whose every tool call spawned another agent — the exact set counted by
    /// the `Delegation loop` finding in `ssa issues`
    #[arg(long)]
    spinning: bool,
    /// Sort column: depth | tokens | turns | cost | tools | start | duration
    #[arg(long)]
    sort: Option<String>,
    /// Sort ascending (default is descending / largest-first)
    #[arg(long)]
    asc: bool,
    /// Maximum rows (0 = all)
    #[arg(long, default_value_t = 0)]
    top: usize,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct AuditArgs {
    /// Archive or directory to inspect (defaults to the same -p PATH other commands use)
    #[arg(value_name = "PATH")]
    target: Option<PathBuf>,
    /// Harness whose layout defines what counts as session data vs a secret
    #[arg(long, value_name = "PROVIDER")]
    provider_name: Option<String>,
    /// Also list the non-log files that are neither session data nor secrets
    #[arg(long)]
    verbose: bool,
}

#[derive(Args, Debug)]
struct ToolsArgs {
    #[command(flatten)]
    list: ListArgs,
    /// Show tool *availability* instead of usage: the deferred registry, what was called,
    /// and every tool an agent searched for that the harness could not provide
    #[arg(long)]
    available: bool,
}

#[derive(Args, Debug)]
struct TarArgs {
    /// Harness whose logs to package (e.g. `claude`). Defaults to the only known provider.
    #[arg(value_name = "PROVIDER")]
    provider: Option<String>,
    /// Directory to package; defaults to the provider's location under $HOME (~/.claude)
    #[arg(long, value_name = "PATH")]
    source: Option<PathBuf>,
    /// Package only this session (id prefix match) and its sub-agent logs
    #[arg(long)]
    session: Option<String>,
    /// Output archive path (default: ./<dir>-sessions.tar.gz)
    #[arg(short = 'o', long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Show exactly what would be included and excluded, and write nothing
    #[arg(long)]
    dry_run: bool,
    /// Overwrite the output file if it already exists
    #[arg(long)]
    force: bool,
    /// Skip the credential-shape scan of the payload (it reads every file)
    #[arg(long)]
    no_scan: bool,
}

#[derive(Args, Debug)]
struct TraceArgs {
    /// Session to inspect (id prefix match)
    #[arg(long)]
    session: String,
    #[command(flatten)]
    filter: FilterArgs,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Args, Debug)]
struct SearchArgs {
    /// Restrict to one session (id prefix match); default is every session in scope
    #[arg(long)]
    session: Option<String>,
    #[command(flatten)]
    filter: FilterArgs,
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

    // `tar` packages files rather than analysing them — it must not need a parseable
    // dataset (the point is to build one for someone else).
    if let Some(Command::Tar(o)) = &cli.command {
        return run_tar(o);
    }

    let mut paths: Vec<PathBuf> = cli.path.clone();
    if let Some(pp) = cli.path_pos.clone() {
        paths.push(pp);
    }
    if let Some(Command::Audit(o)) = &cli.command {
        return run_audit(o, &paths);
    }
    if paths.is_empty() {
        match loader::default_path() {
            Some(p) => paths.push(p),
            None => bail!("no input path; pass a PATH or -p, or run from a dir with ./data or ~/.claude"),
        }
    }

    // A capture carrying credentials looks clean from inside every analysis view, because
    // `ssa` only reads `projects/**`. Say so once, on stderr, before doing anything else.
    if let Some(w) = capture::load_warning(&paths) {
        eprint!("{w}");
    }

    let (dataset, info) = loader::load_all(&paths, cli.provider.as_deref())?;
    let analysis = analysis::analyze(&dataset);

    let output = match cli.command {
        None | Some(Command::Tui) => return tui::run(&analysis, &info),
        Some(cmd) => dispatch(&analysis, &info, cmd)?,
    };
    emit(if output.ends_with('\n') { output } else { format!("{output}\n") }.as_str());
    Ok(())
}

/// Run one headless command and return its rendered output.
fn dispatch(a: &Analysis, info: &loader::LoadInfo, cmd: Command) -> Result<String> {
    let capture_ms = info.capture_written_ms;
    match cmd {
        Command::Tui => unreachable!("handled in run()"),
        Command::Tar(_) | Command::Audit(_) => unreachable!("handled in run(); needs no dataset"),
        Command::Overview(o) => Ok(report::overview(&ctx(a, o.session.as_deref(), None, false, 0, o.format, capture_ms)?)),
        Command::Issues(o) => Ok(report::issues(&ctx(a, o.session.as_deref(), None, false, 0, o.format, capture_ms)?)),
        Command::Rate(o) => {
            let mut c = ctx(a, o.session.as_deref(), None, false, 0, o.format, capture_ms)?;
            c.window_hours = o.window;
            Ok(report::rate(&c))
        }
        Command::Sessions(o) => {
            let mut c = ctx(a, None, o.sort, !o.asc, o.top, o.format, capture_ms)?;
            c.grep = o.grep;
            report::sessions(&c)
        }
        Command::Projects(o) => Ok(report::projects(&ctx(a, None, None, false, o.top, o.format, capture_ms)?)),
        Command::Models(o) => Ok(report::models(&ctx(a, o.session.as_deref(), None, false, 0, o.format, capture_ms)?)),
        Command::Compare(o) => report::compare(a, &o.sessions, o.format.into()),
        Command::Pressure(o) => Ok(report::pressure(&ctx(a, None, None, false, o.top, o.format, capture_ms)?)),
        Command::Tools(o) => {
            let available = o.available;
            let c = list_ctx(a, o.list, capture_ms)?;
            if available { report::tools_available(&c) } else { report::tools(&c) }
        }
        Command::Sinks(o) => report::sinks(&list_ctx(a, o, capture_ms)?),
        Command::CacheAttr(o) => report::cache_attr(&list_ctx(a, o, capture_ms)?),
        Command::Subagents(o) => report::subagents(&list_ctx(a, o, capture_ms)?),
        Command::Agents(o) => {
            let filter = report::AgentFilter {
                run: o.run,
                agent: o.agent.clone(),
                min_depth: o.min_depth,
                spinning: o.spinning,
            };
            let c = ctx(a, o.session.as_deref(), o.sort.clone(), !o.asc, o.top, o.format, capture_ms)?;
            report::agents(&c, &filter)
        }
        Command::Spikes(o) => report::spikes(&list_ctx(a, o, capture_ms)?),
        Command::Timeline(o) => {
            let mut c = ctx(a, Some(&o.session), o.sort, !o.asc, 0, o.format, capture_ms)?;
            c.model = o.model;
            report::timeline(&c)
        }
        Command::Growth(o) => report::growth(&ctx(a, Some(&o.session), None, false, 0, o.format, capture_ms)?),
        Command::Transcript(o) => {
            let c = ctx(a, Some(&o.session), None, false, 0, o.format, capture_ms)?;
            let f = o.filter.to_filter(scope_day_ms(&c))?;
            report::transcript(&c, &f, o.full)
        }
        Command::Search(o) => {
            let c = ctx(a, o.session.as_deref(), None, false, 0, o.format, capture_ms)?;
            let f = o.filter.to_filter(scope_day_ms(&c))?;
            report::search(&c, &f)
        }
        Command::Trace(o) => {
            let c = ctx(a, Some(&o.session), None, false, 0, o.format, capture_ms)?;
            let f = o.filter.to_filter(scope_day_ms(&c))?;
            report::trace(&c, &f)
        }
        Command::Runs(o) => report::runs(&ctx(a, o.session.as_deref(), None, false, 0, o.format, capture_ms)?),
        Command::Show(o) => report::show(&ctx(a, Some(&o.session), None, false, 0, o.format, capture_ms)?, o.item),
    }
}

/// Report what a capture contains beyond session logs.
fn run_audit(o: &AuditArgs, fallback: &[PathBuf]) -> Result<()> {
    let prov = capture::resolve(o.provider_name.as_deref())?;
    let target = match (&o.target, fallback.first()) {
        (Some(t), _) => t.clone(),
        (None, Some(p)) => p.clone(),
        (None, None) => bail!("pass a path to audit, e.g. `ssa audit capture.tar.gz`"),
    };
    let a = capture::audit(prov.as_ref(), &target)?;

    let mut s = String::new();
    let _ = writeln!(s, "Capture    : {}", a.source.display());
    let _ = writeln!(
        s,
        "Log files   : {} ({} session transcript(s), {} sub-agent log(s), {} sidecar(s))",
        a.logs, a.session_logs, a.subagent_logs, a.sidecars
    );
    if a.sensitive.is_empty() {
        let _ = writeln!(s, "Credentials : none found ✓");
    } else {
        let _ = writeln!(s, "\n⚠ CREDENTIALS PRESENT — {} file(s) that should never have been shared:", a.sensitive.len());
        for f in &a.sensitive {
            let _ = writeln!(s, "  ! {f}");
        }
        let _ = writeln!(s, "\nIf this archive left the machine, rotate those credentials.");
        let _ = writeln!(s, "Repackage with: ssa tar {} --source <tree>", prov.id().split('-').next().unwrap_or("claude"));
    }
    let state = a.state_files();
    if !state.is_empty() {
        let _ = writeln!(s, "\nHarness state files ({}):", state.len());
        for f in state.iter().take(10) {
            let _ = writeln!(s, "  · {f}");
        }
        let _ = writeln!(s, "These record a running or most-recent session. If a run's outcome is `truncated`,");
        let _ = writeln!(s, "they are the best available evidence for whether it was still executing when the");
        let _ = writeln!(s, "capture was taken — read them yourself; `ssa` does not interpret them.");
    }
    let _ = writeln!(s, "\nOther non-log files: {}", a.other.len());
    if o.verbose {
        for f in a.other.iter().take(200) {
            let _ = writeln!(s, "  · {f}");
        }
        if a.other.len() > 200 {
            let _ = writeln!(s, "  … and {} more", a.other.len() - 200);
        }
    }
    let _ = writeln!(s, "\nNote: this checks the file *list*. Transcripts themselves can still embed");
    let _ = writeln!(s, "secrets the agent read — `ssa tar` scans payloads for that.");
    emit(&s);
    if !a.sensitive.is_empty() {
        std::process::exit(2);
    }
    Ok(())
}

/// Package a harness's session tree for sharing. Prints exactly what is included and what
/// was excluded as sensitive, so the archive's contents are never a surprise.
fn run_tar(o: &TarArgs) -> Result<()> {
    let prov = capture::resolve(o.provider.as_deref())?;
    let source = match &o.source {
        Some(p) => p.clone(),
        None => capture::default_source(prov.as_ref())
            .ok_or_else(|| anyhow::anyhow!("no {} directory found under $HOME; pass --source", prov.display_name()))?,
    };
    let plan = capture::plan(prov.as_ref(), &source, o.session.as_deref())?;

    let mut s = String::new();
    let _ = writeln!(s, "Source     : {}", plan.root.display());
    let _ = writeln!(s, "Provider   : {}", prov.display_name());
    let _ = writeln!(s, "Including  : {} file(s), {} session log(s), {}", plan.include.len(), plan.sessions(), human_bytes(plan.total_bytes));
    if !plan.sensitive.is_empty() {
        let _ = writeln!(s, "EXCLUDED (sensitive): {}", plan.sensitive.len());
        for f in plan.sensitive.iter().take(12) {
            let _ = writeln!(s, "  ✗ {f}");
        }
        if plan.sensitive.len() > 12 {
            let _ = writeln!(s, "  … and {} more", plan.sensitive.len() - 12);
        }
    }
    if !plan.skipped.is_empty() {
        let dirs: Vec<String> = plan.skipped.iter().map(|(d, n)| format!("{d} ({n})")).collect();
        let _ = writeln!(s, "Not session data, skipped: {}", dirs.join(", "));
    }
    if !o.no_scan {
        let hits = capture::scan_secrets(&plan);
        if hits.is_empty() {
            let _ = writeln!(s, "Payload scan: no credential-shaped strings found (heuristic — not a guarantee).");
        } else {
            let _ = writeln!(s, "⚠ Payload scan found credential-shaped strings INSIDE the logs:");
            for (label, n) in &hits {
                let _ = writeln!(s, "  ! {label} ×{n}");
            }
            let _ = writeln!(s, "  Transcripts contain whatever the agent read. Review before sharing.");
        }
    }
    if o.dry_run {
        let _ = writeln!(s, "\n(dry run — nothing written)");
        emit(&s);
        return Ok(());
    }

    let out = o.out.clone().unwrap_or_else(|| {
        let base = plan.root.file_name().and_then(|n| n.to_str()).unwrap_or("capture");
        PathBuf::from(format!("{}-sessions.tar.gz", base.trim_start_matches('.')))
    });
    let bytes = capture::write(&plan, &out, o.force)?;
    let _ = writeln!(s, "\nWrote {} ({})", out.display(), human_bytes(bytes));
    emit(&s);
    Ok(())
}

fn human_bytes(b: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Epoch millis of the first turn in scope — the day a bare `--since 07:55` resolves
/// against. Falls back to the whole dataset when no single session is focused.
fn scope_day_ms(c: &Ctx) -> i64 {
    let first = |s: &SessionReport| s.timeline.iter().map(|t| t.ts_ms).filter(|t| *t > 0).min();
    match c.session {
        Some(sr) => first(sr).unwrap_or(0),
        None => c.a.sessions.iter().filter_map(first).min().unwrap_or(0),
    }
}

/// Build a context for the shared list commands, applying `--min-tokens`/`--model`.
fn list_ctx<'a>(a: &'a Analysis, o: ListArgs, capture_ms: i64) -> Result<Ctx<'a>> {
    let mut c = ctx(a, o.session.as_deref(), o.sort, !o.asc, o.top, o.format, capture_ms)?;
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
    capture_written_ms: i64,
) -> Result<Ctx<'a>> {
    let session = match session {
        None => None,
        Some(sel) => {
            let matches: Vec<&SessionReport> = a.sessions.iter().filter(|s| s.session_id.starts_with(sel)).collect();
            match matches.as_slice() {
                [one] => Some(*one),
                // Reporting an id in `skipped_sessions` and then denying it exists is worse
                // than either behaviour alone — say why it has no views.
                [] if a.skipped_sessions.iter().any(|s| s.starts_with(sel)) => bail!(
                    "session '{}' has no assistant turns (it appears in `skipped_sessions`), so there is nothing to show",
                    sel
                ),
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
        capture_written_ms,
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
