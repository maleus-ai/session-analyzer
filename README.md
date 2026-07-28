# session-analyzer

A Rust **TUI + CLI** for deep analysis of **token consumption** in agent session logs.
Point it at raw session logs (a folder, a single `.jsonl`, a `.zip`, or a `~/.claude`
project tree) and it shows *how many* tokens were spent, **where**, and **why** — per
message, per tool, per turn — plus cache (in)efficiency, the real token sinks, context
growth and its anomalies.

Everything the interactive dashboard shows is also a **headless command** emitting
text / json / csv, so an LLM agent can drive the whole tool without a terminal.

## Installation

```sh
# Install the latest release (static musl on Linux, native on macOS) into ~/.local/bin
curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash

# A specific version
curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash -s v0.1.0

# glibc build instead of musl, or a custom install dir
curl -sSfL https://raw.githubusercontent.com/maleus-ai/session-analyzer/master/get-ssa.sh | bash -s -- --gnu --bin-dir /usr/local/bin
```

Prebuilt binaries (`x86_64`/`aarch64`, Linux gnu+musl and macOS) are published on every
`v*` tag by [`.github/workflows/release.yml`](.github/workflows/release.yml).

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
Adding a harness = implement `Provider` in `src/providers/` and register it — everything
downstream (analysis, TUI, CLI) is provider-agnostic.

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
| `runs` / `trace` | per-run outcomes; one line per turn (`--depth`/`--min-depth` to slice a chain) |
| `sinks` | token sinks ranked by **amplified cost** (size × turns resident) |
| `cache-attr` | decomposition of cache-read by the content that stayed resident |
| `subagents` | per sub-agent tokens, tool counts, reads/edits, lines changed (agents that finished) |
| `agents` | every sub-agent conversation, finished or not: id, type, depth, turns, timing, outcome |
| `runs` | the session's runs (SDK query / prompt cycle) and how each ended — incl. turn-limit hits |
| `trace --session ID` | one line per turn: time, thread, tools called — the "what did it do" view |
| `timeline --session ID` | per-turn context size, Δctx, write/read/out, cost, spikes |
| `growth --session ID` | context-growth sparkline + detected spikes |
| `spikes` | anomalous context-growth turns + their cause |
| `search --grep TEXT` | find messages across every session **and sub-agent**; prints locators |
| `transcript --session ID` | ordered bubbles (user/assistant/tool/compact) with tokens, sub-agents nested |
| `show --session ID --item N` | full text of one transcript item (msg / tool io) |
| `issues [--session ID]` | detected inefficiencies **and control-flow failures** (run outcomes, delegation loops) |
| `tar <provider>` | package session logs into a shareable `.tar.gz`, **excluding credentials** |

Common flags: `--format text\|json\|csv`, `--session <id-prefix>`, `--sort <col>`,
`--desc`, `--top N`, `--provider <id>`. Sort columns per view are listed in `src/query.rs`
(e.g. sinks: `amplified|size|calls|residency|contribution`).

`search`, `transcript` and `trace` share a selection vocabulary: `--grep TEXT`,
`--input-grep TEXT` (tool arguments), `--regex`, `--kind
user\|assistant\|tool\|compact\|event`, `--thread all\|main\|sub`,
`--agent <id/type/description>`, `--tool <name>`, `--run N`, `--since`/`--until`
(`HH:MM[:SS]` or `YYYY-MM-DD HH:MM`), `--from N` / `--limit N` (paging by item index),
`--context N` (messages around each hit), and `--full` (complete text, `transcript` only).
So:

```sh
ssa runs -p LOG --session 14574d2b                        # how did each run end
ssa trace -p LOG --session 14574d2b --run 2               # one line per turn
ssa search -p LOG --grep "regression"                     # where is it, across all threads
ssa search -p LOG --kind event                            # turn limits, hooks, reminders
ssa transcript -p LOG --session 14574d2b --thread main    # just the human-facing thread
ssa agents -p LOG --session 14574d2b                      # what was delegated to whom
ssa transcript -p LOG --session 14574d2b --agent Explore --full
```

The old `--cli --report <section>` form still works as a deprecated alias.

## TUI

Tabs: **Overview · Sessions · Transcript · Timeline · Tools · Sinks · Cache-attr ·
Sub-agents · Issues · Rate**. Transcript/Timeline are per-session — open one from Sessions.
The **Rate** tab shows throughput over wall-clock time and the peak rolling window.

Overview leads with how each **run** ended and any tool the agent asked for and could not
get; **Tools** flags those unavailable tools above the usage table; **Sub-agents** lists
every delegated conversation in chain order with its depth and outcome. All of it mirrors
the headless commands (`runs`, `tools --available`, `agents`).

**Keyboard**: `←/→` or `1`–`9`/`0` switch tabs (`0` = Rate) · `↑/↓` move · `PgUp/PgDn` page ·
`[` / `]` change sort column · `r` reverse sort · `Enter` drill into a session /
expand a transcript bubble · `/` search the transcript (`n`/`N` next/prev) · `Esc` back ·
`q` quit.

In the **Transcript**, each sub-agent is collapsed to a single summary row (turns, tokens,
cost, outcome). `Enter` on it opens that conversation as its own transcript with a
breadcrumb (`main ▸ Explore#a221ec`); `Esc` walks back out one level. `a` switches to a flat
view with every thread inline. The **Timeline** plots the main thread and sub-agents as
separate series, because each has its own context window — a merged line makes an agent
starting and finishing look like a compaction that never happened.

**Mouse**: click a tab, click a **column header** to sort (click again to reverse),
click a row to select, scroll wheel to move/scroll, click a bubble to expand.

## What it measures — and the ideas behind it

- **Token totals** split into fresh input, cache **write**, cache **read**, output, with
  an estimated USD cost from per-model list pricing (Opus / Sonnet / Haiku).
- **Cache efficiency**: hit rate (share of input served from cache) and *churn*
  (writes ÷ reads — high churn = caches expire before paying off).
- **Amplified sinks / cache-read attribution** — the key idea. Cache-read is usually the
  dominant cost, and it equals *every resident block replayed every turn it stays in
  context*. A block of `T` tokens resident for `R` turns contributes `T×R` to cache-read.
  So the tool decomposes total cache-read by source (reconciling to ~100%) and ranks
  sinks by **amplified cost** — revealing the file read once early and never evicted that
  quietly cost more than any one-off big result.
- **Context growth & spikes**: per-turn context size forms a growth curve (with
  compaction sawtooth resets); turns whose jump exceeds `mean + 2σ` are flagged as
  **spikes**, attributed to what was added just before them.
- **Transcript**: full ordered messages with per-message token stats, so you can read a
  session and see exactly where writes/reads/sinks occur.
- **Sub-agents**: every sub-agent conversation — finished or not — with nesting depth,
  outcome, duration and the transcript range to read it.
- **Thread attribution**: sub-agent logs carry the *parent's* `sessionId`, so every message
  is tagged with the agent that produced it and nested under the turn that spawned it —
  main-thread turns are numbered 1..N and each sub-agent restarts at 1.
- **Control flow**: the session's runs and how each ended (clean finish, turn/token limit,
  ended mid-generation, API error), read from the harness `attachment` records that carry
  markers like `max_turns_reached`.
- **Inefficiencies**: low cache hit, churn, repeated reads, cache-read sinks, context
  spikes, tool/API errors, auto-compactions, heavy sub-agents, **delegation loops** and
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
