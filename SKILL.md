---
name: analyze-claude-sessions
description: >
  Use ssa to investigate token consumption, cost, cache efficiency,
  context growth, sub-agents and token sinks in Claude Code (and other agent-harness)
  session logs. Trigger when asked "why did this session cost so much / burn my
  subscription / grow so large", "where are the token sinks", "what did this session do",
  or to compare/aggregate many sessions and projects. Works headless (text/json/csv) so
  it can be driven entirely from the command line.
---

# Analyzing agent session logs with `ssa` (session-analyzer)

`ssa` reads raw session logs and answers *how many* tokens were spent,
**where**, and **why**. Every view is a headless subcommand emitting `text`/`json`/`csv`,
so you can script it and parse the JSON.

## Build & invoke

```sh
cargo build --release          # once; binary at ./target/release/ssa
ssa <command> -p <PATH> [--format json] [--session <id-prefix>] [--sort <col>] [--top N]
ssa -p <PATH>      # no command → interactive TUI (needs a TTY; not for agents)
```

`PATH` = a folder, a single `.jsonl`, an archive (`.zip`/`.tar`/`.tar.gz`/`.tgz`), or a
`.claude` project tree. For `.claude` trees only `projects/**` is read and results are
grouped per project. `-p` is **repeatable** (`-p A -p B`) to merge sources — e.g.
`pressure`/`compare` an archive against `~/.claude` in one run. **Agents should always
pass `--format json`** and parse it — do not scrape the text tables.

## Mental model — the four token buckets (read this first)

Every assistant call reports usage in four buckets. They are **not** interchangeable:

| bucket | meaning | relative price | who cares |
|---|---|---|---|
| `input` | fresh, uncached prompt tokens | 1× | cost |
| `cache_creation` (**cache write**) | new content written into the prompt cache | ~1.25× (most expensive/token) | **cost + rate limits** |
| `cache_read` | context replayed from cache each turn | ~0.1× (cheap) | bulk of "total", but cheap |
| `output` | generated tokens | ~5× | cost |

Key consequences you must reason with:

- **"Total tokens" is dominated by `cache_read`** — the same context replayed every turn.
  A session with billions of total tokens may be cheap and light: it just carried a big
  context for many turns. **Do not equate total tokens with expense or with hitting a
  subscription limit.**
- **`cache_write` is the expensive, limit-driving bucket.** It spikes when new content
  enters context: file reads, sub-agent fan-out, and **compactions** (which re-cache the
  whole conversation). High cache-write *rate* is what burns subscription budgets.
- **`cost` (this tool's USD estimate)** uses API list pricing and weights `cache_read` at
  0.1×. It is good for "what would the API bill be", but it is **not** the same as
  subscription-limit pressure (see Gotchas).

## Commands and when to use each

| command | use it to… |
|---|---|
| `overview` | totals, cost, cache hit/churn, **cache-write /h + fresh /h**, main-vs-sub split. Start here. |
| `rate` | **throughput & subscription pressure**: fresh/h, cache-write/h, peak continuous burst, peak turns/min, max concurrent sub-agents, and **peak per-minute RPM / ITPM / OTPM** (Anthropic's three rate limiters; ITPM excludes cache-read) (`--window 5`). |
| `pressure` | **rank all sessions by sustained cache-write/hour and auto-flag `BURST`** (unattended `sdk-ts` runs with 0 idle gaps that concentrate load into one window). The one-command answer to "which session will burn my subscription and why". |
| `sessions` | rank every session; `--sort cost\|tokens\|turns\|active\|fresh_rate\|cwrate\|burst\|turnsmin`, `--grep <text>` (title/cwd). Shows `entrypoint`, `active_hours`, `idle_gaps`, `model`; JSON includes all rate fields. |
| `projects` | aggregate per working directory across a `.claude` tree. |
| `models` | per-model turns / tokens / **fresh tokens** / cost (opus vs sonnet, etc.). |
| `compare` | line up 2+ sessions side by side (repeat `--session`) incl. fresh/h and peak-window. |
| `cache-attr` | **decompose cache-read by the content that stayed resident** — "where did the N cache-read tokens come from". Reconciles to ~100%. |
| `sinks` | rank token sinks by **amplified cost** = size × turns-resident. The file read once and never evicted that quietly cost the most. |
| `tools` | per-tool call counts, result-token footprint, errors. |
| `subagents` | per sub-agent tokens, model, tool mix, reads/edits, duration. |
| `timeline --session ID` | per-**turn** context size, Δctx, write/read/out, cost. |
| `growth --session ID` | context-size sparkline + detected spikes. |
| `spikes` | anomalous context-growth turns and their cause. |
| `transcript --session ID` | ordered messages (add `--full` for complete text). |
| `show --session ID --item N` | full text of one transcript item. |
| `issues` | heuristic inefficiencies (low hit, churn, spikes, repeated reads, errors, compactions). |

Every table command takes `--sort <col> [--desc] --top N`; run `ssa <cmd>
--help` for a command's valid sort columns. Filters: `--min-tokens N` (sinks/tools/
cache-attr), `--model <substr>` (subagents/timeline), `--grep <text>` (sessions/transcript).

## Analysis recipes

**A. "Why was this session expensive / why did it burn the subscription?"**
Fastest path: **`pressure`** (add `-p A -p B` to span an archive + `~/.claude`) — it ranks
every session by sustained cache-write/hour and flags `BURST` (unattended `sdk-ts`, 0 idle
gaps). The flagged top rows are your culprits in one command.

No single token metric reliably explains a lockout — a small session can hit the limit
while a much larger one on the same plan does not. To reason it through across axes:
1. `overview` / `sessions` → **`entrypoint` (harness) and continuity**. A `sdk-ts`
   (headless `-p`) run, and/or one with **0 idle gaps** (fully continuous, `active ≈ span`),
   concentrates its whole load into one rolling window — the strongest observed signal.
   Compare to `cli` sessions spread over days with many `idle_gaps`.
2. `rate` → `cache_write_per_hour`, `peak_burst_fresh` (continuity-aware) **and**
   `peak_window_fresh` (naive; can smear across idle gaps — trust the *burst* one). Note:
   these can be HIGHER for an innocent big session, so they are evidence, not proof.
3. `models` → model **generation/mix** (e.g. opus-4.6 vs 4.8) — different generations may
   carry different limit weights.
4. `subagents` → concurrent fan-out (parallel agents multiply instantaneous request rate).
5. `cache-attr` / `sinks` → what drove the resident context; `compare` two sessions to see
   which axis differs. Remember the true limit formula is server-side and may not be fully
   determinable from logs — state what the data can and cannot prove.

**B. "Where are the token sinks?"** → `sinks --sort amplified` then `cache-attr`.
The `(base context)` row in `cache-attr` = system prompt + prior conversation always
resident; large real sinks are files/tools with high `share`.

**C. "What did the agent do / read a session?"** → `transcript --session ID` (bubbles),
`show --item N` for full content, `timeline` + `growth` to see where context ballooned.

**D. "Compare many sessions / projects"** → `sessions --sort cost` / `projects`.

## Interpretation heuristics

- **Cache hit rate** ≥90% good; <80% means context keeps changing (re-processing).
- **Cache churn** (writes÷reads) low is good; >0.5 means caches expire before paying off.
- **Context spikes** (`spikes`) = a turn where context jumped far above trend; the `cause`
  names the tool result that bloated it.
- **Repeated reads** (in `issues`) = the same file read many times → wasted context.
- **Compactions**, especially `auto`, mean the context hit the window and was summarized —
  costly (re-caching) and lossy.

## Gotchas (important — the tool will mislead you if you ignore these)

1. **Total tokens ≠ expense ≠ subscription pressure.** Use `rate` (`cache_write_per_hour`,
   `peak_window_fresh`), not `total_tokens`. Total is mostly cheap replayed cache-read.
2. **`cost` (list price) ≠ subscription-limit pressure.** Cost blurs the signal (cache-read
   weighted 0.1×); the limit tracks fresh-token throughput. Rank by `rate`, not by `$`.
3. **Sub-agent sidechains are merged into the parent session** (`projects/**/subagents/*.jsonl`
   share the parent `sessionId`). `overview` now shows the **main vs sub-agent split** and
   whether sidechain detail is present (`has_sidechain_detail`), but per-session totals still
   include sub-agent internals. The `subagents` metric is a *different* measure
   (`toolUseResult.totalTokens`) and won't match the merged totals. A single exported
   `.jsonl` (no `subagents/` dir) is **blind** to sub-agent internal consumption — check
   `has_sidechain_detail`.
4. **`timeline` x-axis is turn index, not wall-clock time** — you cannot see bursts or gaps
   in time from it.
5. **Sink/attribution token counts are estimates** (~3.7 chars/token), normalized to
   reconcile with the real cache-read total. Treat them as proportions, not exact tokens.
