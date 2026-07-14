# session-analyzer

A Rust **TUI + CLI** for deep analysis of **token consumption** in agent session logs.
Point it at raw session logs (a folder, a single `.jsonl`, a `.zip`, or a `~/.claude`
project tree) and it shows *how many* tokens were spent, **where**, and **why** — per
message, per tool, per turn — plus cache (in)efficiency, the real token sinks, context
growth and its anomalies.

Everything the interactive dashboard shows is also a **headless command** emitting
text / json / csv, so an LLM agent can drive the whole tool without a terminal.

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
| `tools` | per-tool calls, result-token footprint, errors |
| `sinks` | token sinks ranked by **amplified cost** (size × turns resident) |
| `cache-attr` | decomposition of cache-read by the content that stayed resident |
| `subagents` | per sub-agent tokens, tool counts, reads/edits, lines changed |
| `timeline --session ID` | per-turn context size, Δctx, write/read/out, cost, spikes |
| `growth --session ID` | context-growth sparkline + detected spikes |
| `spikes` | anomalous context-growth turns + their cause |
| `transcript --session ID` | ordered bubbles (user/assistant/tool/compact) with tokens |
| `show --session ID --item N` | full text of one transcript item (msg / tool io) |
| `issues` | detected inefficiencies |

Common flags: `--format text\|json\|csv`, `--session <id-prefix>`, `--sort <col>`,
`--desc`, `--top N`, `--provider <id>`. `transcript` also takes `--kind
user\|assistant\|tool\|compact` and `--full` (full text). Sort columns per view are
listed in `src/query.rs` (e.g. sinks: `amplified|size|calls|residency|contribution`).

The old `--cli --report <section>` form still works as a deprecated alias.

## TUI

Tabs: **Overview · Sessions · Transcript · Timeline · Tools · Sinks · Cache-attr ·
Sub-agents · Issues · Rate**. Transcript/Timeline are per-session — open one from Sessions.
The **Rate** tab shows throughput over wall-clock time and the peak rolling window.

**Keyboard**: `←/→` or `1`–`9`/`0` switch tabs (`0` = Rate) · `↑/↓` move · `PgUp/PgDn` page ·
`[` / `]` change sort column · `r` reverse sort · `Enter` drill into a session /
expand a transcript bubble · `Esc` back · `q` quit.

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
- **Sub-agents**: per-agent tokens, tool mix, reads/searches/bash/edits, lines changed.
- **Inefficiencies**: low cache hit, churn, repeated reads, cache-read sinks, context
  spikes, tool/API errors, auto-compactions, heavy sub-agents.

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
