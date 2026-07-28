# session-analyzer

A Rust TUI and CLI for analysing agent session logs. It answers two kinds of question:

- **What the session did, and why it went wrong.** How each run ended, whether the agent
  ever answered, delegation loops, repeated tool calls, capabilities the agent needed and
  could not reach, and what each sub-agent did.
- **What it cost.** Tokens by bucket, cache efficiency, context growth, token sinks and
  subscription pressure.

Input is a folder, a single `.jsonl`, an archive (`.zip`/`.tar`/`.tar.gz`/`.tgz`), or a
`~/.claude` project tree. Every view is also a headless command emitting text, JSON or CSV,
so an LLM agent can drive the whole tool without a terminal.

## Installation

```sh
# Install the latest release (static musl on Linux, native on macOS) into ~/.local/bin
curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash

# A specific version
curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash -s v0.1.0

# glibc build instead of musl, or a custom install dir
curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash -s -- --gnu --bin-dir /usr/local/bin
```

If Claude Code is installed, the installer offers to register `SKILL.md` as a global skill
(`~/.claude/skills/analyze-claude-sessions/`). It only asks when a terminal is attached, so
piped and CI installs do not block. Decide up front with a flag:

```sh
… | bash -s -- --skill        # install the skill without asking
… | bash -s -- --no-skill     # never install it
SSA_INSTALL_SKILL=yes …       # same, via the environment
```

Prebuilt binaries (`x86_64`/`aarch64` Linux gnu+musl, and Apple Silicon macOS) are published
on every `v*` tag by [`.github/workflows/release.yml`](.github/workflows/release.yml).
Intel macOS is not built; build from source if you need it.

Or build from source (requires Rust 1.85+):

```sh
git clone https://github.com/maleus-ai/session-analyzer.git
cd session-analyzer
./install.sh            # cargo build --release, then install `ssa` into ~/.local/bin
```

## Build

```sh
cargo build --release
# binary: ./target/release/ssa
```

## Quick start

```sh
ssa                            # TUI on ./data or ~/.claude
ssa PATH                       # TUI on a path
ssa overview -p PATH           # headless summary
ssa sinks -p PATH              # biggest token sinks
ssa cache-attr -p PATH         # where cache-read came from
ssa timeline -p PATH --session ID
ssa overview -p PATH --format json
```

The path is a global option: pass it with `-p`/`--path`, or as the leading positional for
the TUI (`ssa PATH`). Omitted, it defaults to `./data` then `~/.claude`.

Run `ssa --help` for the command list, and `ssa <command> --help`
for a command's options (including its valid `--sort` columns).

`PATH` may be a **folder** of `.jsonl`, a single **`.jsonl`**, an **archive**
(`.zip` / `.tar` / `.tar.gz` / `.tgz`), or a **`.claude` / project tree**.

**`.claude` trees**: only session logs under `projects/**` are read (so `history.jsonl`,
`tasks/`, `shell-snapshots/` etc. are skipped), sessions are labelled by project, and the
`projects` command / Overview surface a **per-project** cost & token breakdown across
every working directory in the tree.

## Providers (multi-harness)

Parsing is behind a `Provider` trait, so other agent harnesses can be supported later.
The provider is auto-detected from the logs; force one with `--provider claude-code`.
Adding a harness means implementing `Provider` in `src/providers/` and registering it.
Everything downstream (analysis, TUI, CLI) is provider-agnostic.

## Commands (CLI = TUI parity)

| Command | Shows |
|---------|-------|
| `overview` | totals, cost, cache hit/churn, cache-write /h + fresh /h, main-vs-sub split |
| `rate` | throughput & **subscription pressure**: fresh/h, cache-write/h, peak burst, peak turns/min, concurrency, **peak RPM / ITPM / OTPM** |
| `pressure` | rank sessions by sustained cache-write/hour; auto-flags unattended `sdk-ts` `BURST` runs |
| `sessions` | every session ranked (`--grep` by title/cwd) |
| `projects` | per-project (working-directory) totals across a `.claude` tree |
| `models` | per-model turns / tokens / fresh tokens / cost |
| `compare` | 2+ sessions side by side (repeat `--session`) |
| `tools` | per-tool calls, result-token footprint, errors (`--available` for the tool roster) |
| `audit <capture>` | inspect an archive you were handed for leaked credentials |
| `sinks` | token sinks ranked by **amplified cost** (size × turns resident) |
| `cache-attr` | decomposition of cache-read by the content that stayed resident |
| `subagents` | per sub-agent tokens, tool counts, reads/edits, lines changed (agents that finished) |
| `agents` | every sub-agent conversation, finished or not: id, type, depth, turns, timing, outcome |
| `runs` | the session's runs (one SDK query each) and how each ended, including turn-limit hits |
| `trace --session ID` | one line per turn: time, thread, turn, output tokens, tools called |
| `timeline --session ID` | per-turn context size, Δctx, write/read/out, cost, spikes |
| `growth --session ID` | context-growth sparkline + detected spikes |
| `spikes` | anomalous context-growth turns + their cause |
| `search --grep TEXT` | find messages across every session **and sub-agent**; prints locators |
| `transcript --session ID` | ordered bubbles (user/assistant/tool/compact) with tokens, sub-agents nested |
| `show --session ID --item N` | full text of one transcript item (msg / tool io) |
| `issues [--session ID]` | detected inefficiencies and control-flow failures (run outcomes, delegation loops, unavailable tools) |
| `tar <provider>` | package session logs into a shareable `.tar.gz`, **excluding credentials** |

Common flags: `--format text\|json\|csv`, `--session <id-prefix>`, `--sort <col>`, `--asc`
(default is descending), `--top N`, `--provider <id>`. Run `ssa <command> --help` for the
authoritative flag list and valid sort columns.

`search`, `transcript` and `trace` share one filter set: `--grep TEXT`, `--input-grep TEXT`
(tool arguments), `--regex`, `--kind user\|assistant\|tool\|compact\|event`,
`--thread all\|main\|sub`, `--agent <id/type/description>`, `--depth N` / `--min-depth N`,
`--tool <name>`, `--errors-only`, `--run N`, `--since` / `--until` (`HH:MM[:SS]` or
`YYYY-MM-DD HH:MM`, UTC), `--from N` / `--limit N` (paging by item index), `--context N`
(messages around each hit), and `--full` (complete text, `transcript` only).

```sh
ssa runs -p LOG --session 14574d2b                        # how did each run end
ssa trace -p LOG --session 14574d2b --run 2               # one line per turn
ssa search -p LOG --grep "regression"                     # where is it, across all threads
ssa search -p LOG --kind event                            # turn limits, hooks, reminders
ssa transcript -p LOG --session 14574d2b --thread main    # just the human-facing thread
ssa agents -p LOG --session 14574d2b                      # what was delegated to whom
ssa transcript -p LOG --session 14574d2b --agent Explore --full
```

## TUI

Tabs: **Overview · Sessions · Transcript · Timeline · Tools · Sinks · Cache-attr ·
Sub-agents · Issues · Rate**. Transcript and Timeline are per-session; open one from Sessions.
The **Rate** tab shows throughput over wall-clock time and the peak rolling window.

Overview shows per-run outcomes and any tool the agent requested that the harness did not
provide. Tools flags those unavailable tools above the usage table. Sub-agents lists every
delegated conversation in chain order with depth and outcome. These mirror the `runs`,
`tools --available` and `agents` commands.

**Keyboard**: `←/→` or `1`–`9`/`0` switch tabs (`0` = Rate) · `↑/↓` move · `PgUp/PgDn` page ·
`[` / `]` change sort column · `r` reverse sort · `Enter` drill into a session /
expand a transcript bubble · `/` search the transcript (`n`/`N` next/prev) · `Esc` back ·
`q` quit.

In the **Transcript**, each sub-agent is collapsed to a single summary row (turns, tokens,
cost, outcome). `Enter` opens that conversation as its own transcript with a breadcrumb
(`main ▸ Explore#a221ec`), `Esc` exits one level, and `a` switches to a flat view with every
thread inline. The **Timeline** plots main-thread and sub-agent turns as separate series,
since each thread has its own context window.

**Mouse**: click a tab, click a **column header** to sort (click again to reverse),
click a row to select, scroll wheel to move/scroll, click a bubble to expand.

## What it measures

- **Token totals** split into fresh input, cache **write**, cache **read**, output, with
  an estimated USD cost from per-model list pricing (Opus / Sonnet / Haiku).
- **Cache efficiency**: hit rate (share of input served from cache) and churn
  (writes / reads; high churn means caches expire before paying off).
- **Amplified sinks and cache-read attribution.** Cache-read equals every resident block
  replayed on every turn it stays in context: a block of `T` tokens resident for `R` turns
  contributes `T×R`. Total cache-read is decomposed by source (reconciling to ~100%) and
  sinks are ranked by amplified cost, which surfaces content read once early and never
  evicted.
- **Context growth and spikes**: per-turn context size forms a growth curve, tracked per
  thread. Turns whose jump exceeds `mean + 2σ` are flagged as spikes and attributed to what
  was added just before them.
- **Transcript**: ordered messages with per-message token stats.
- **Sub-agents**: every sub-agent conversation, finished or not, with nesting depth,
  outcome, duration and the transcript range to read it.
- **Thread attribution**: sub-agent logs carry the parent's `sessionId`, so each message is
  tagged with the agent that produced it and nested under the turn that spawned it.
  Main-thread turns are numbered 1..N and each sub-agent restarts at 1.
- **Control flow**: the session's runs and how each ended (clean finish, turn or token
  limit, ended mid-generation, API error), read from the harness `attachment` records that
  carry markers such as `max_turns_reached`.
- **Tool availability**: the deferred tool registry, and every capability an agent requested
  via `ToolSearch` that the harness did not provide.
- **Inefficiencies**: low cache hit, churn, repeated reads, cache-read sinks, context
  spikes, tool and API errors, auto-compactions, heavy sub-agents, delegation loops, and
  identical tool calls repeated.

## Token accounting notes

Per-call `usage` is summed across every assistant turn; cost weights each bucket by list
pricing (cache write ≈ 1.25× input, cache read ≈ 0.1× input, output ≈ 5× input).
Sink/attribution token estimates derive from content byte size (~3.7 chars/token), then
the cache-read decomposition is normalized to reconcile exactly with the session's actual
cache-read total; the unexplained remainder is shown as a `(base context)` row (system
prompt, prior conversation, thinking).

## Architecture

```
main.rs         clap subcommands → headless report, or launches the TUI
model.rs        normalized, provider-agnostic event model (ordered Item stream)
provider.rs     Provider trait + registry + auto-detection
providers/      one parser per harness (claude_code.rs)
loader.rs       folder / file / archive / .claude enumeration
analysis.rs     aggregates + transcript + timeline + cache-attribution + spikes
query.rs        shared sort/filter (identical for CLI flags and TUI clicks)
report.rs       text / json / csv rendering for every view
pricing.rs      per-model pricing
tui/            Ratatui dashboard, structured like a frontend app:
  mod.rs          composition root: terminal, event loop, frame layout
  app.rs          state + input handling (the controller)
  theme.rs        design tokens: palette + shared style helpers
  format.rs       text/layout helpers (wrap, width, truncate, kv lines)
  widgets/        reusable components: tabs, table, bubble, popup, bars
  views/          one page per tab (overview, sessions, transcript, …)
```
