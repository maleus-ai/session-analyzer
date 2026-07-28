---
name: analyze-claude-sessions
description: >
  Use ssa to investigate what an agent session actually DID and why it went wrong, and
  what it cost, from Claude Code (and other agent-harness) session logs. Two kinds of
  question, both first-class. BEHAVIOUR: why a run stopped or produced no answer, turn/token
  limits hit, runs that were cut off mid-generation, delegation loops and runaway sub-agent
  recursion, the same tool call repeated, a capability the agent needed and the harness did
  not provide, which sub-agent did what, reading or searching a transcript across every
  sub-agent, and post-incident root-cause analysis. COST: token consumption, cache
  efficiency and churn, context growth and spikes, token sinks, subscription/rate pressure,
  and comparing or aggregating many sessions and projects. It is equally strong on both —
  cost work includes ranking token sinks by amplified cost (size x turns resident),
  decomposing cache-read by the content that stayed in context, per-run cost, and rolling
  rate-window pressure. Trigger on "why did this session fail / stop / hang / loop / do
  nothing", "what went wrong in this session", "analyse this session log or .claude
  capture", "why did it keep spawning agents", "what tools did the agent have", "why did
  this cost so much / burn my subscription", "reduce the cost", "where are the token sinks",
  "compare these sessions", "what did this session do". Takes a whole .claude tree or a
  .zip/.tar.gz archive as well as individual .jsonl files, reconstructs each sub-agent as
  its own thread, and also packages a capture for sharing without leaking credentials or
  audits one you were handed. Works headless (text/json/csv), plus an interactive TUI.
---

# Analyzing agent session logs with `ssa` (session-analyzer)

`ssa` reads raw session logs and answers two different kinds of question:

- **What happened, and why did it go wrong** — how each run ended, whether the agent ever
  answered, delegation loops, repeated calls, capabilities it needed and could not reach,
  and what any sub-agent did. This is the side to reach for when a session *failed*, not
  just when it was expensive.
- **What it cost** — tokens by bucket, cache efficiency, context growth, token sinks and
  subscription pressure.

Every view is a headless subcommand emitting `text`/`json`/`csv`, so you can script it and
parse the JSON. **A session can fail without costing much, and cost a lot without failing —
do not assume the question is about money.**

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
| `tools --available` | **what the agent could reach**: the deferred registry, and every tool it searched for that the harness could not provide. The usual root cause of workaround loops. |
| `audit <capture>` | does an archive you were handed contain credentials? Exits non-zero if so. |
| `subagents` | per sub-agent tokens, model, tool mix, reads/edits, duration — **only agents that returned a result**. |
| `agents` | **every sub-agent conversation in the log**, finished or not: id, type, nesting depth, turns, tokens, timing, outcome, and the item range to read it. |
| `runs` | **the session's runs and how each one ended** (`completed` / `limit-hit` / `truncated` / `errored`). The first thing to check when a session stopped unexpectedly. |
| `trace --session ID` | **one line per turn**: time, thread, tools called. The "what did it actually do" view — a loop is visible as the same call repeating. Takes every `transcript` filter (`--run N`, `--agent`, `--since`, …). |
| `timeline --session ID` | per-**turn** context size, Δctx, write/read/out, cost. |
| `growth --session ID` | context-size sparkline + detected spikes. |
| `spikes` | anomalous context-growth turns and their cause. |
| `search --grep TEXT` | **find messages across all sessions and sub-agents**; one locator row per hit. Start here when you don't know where something is. |
| `transcript --session ID` | ordered messages, sub-agent conversations nested under the turn that spawned them (add `--full` for complete text). |
| `show --session ID --item N` | full text of one transcript item. |
| `issues [--session ID]` | heuristic inefficiencies **and control-flow failures**: how runs ended, delegation loops, repeated tool calls, spikes, cache churn, repeated reads, errors, compactions. Scoped and unscoped report the same findings. |

**All displayed times are UTC**, in `HH:MM:SS` (transcript, trace) or `YYYY-MM-DD HH:MM`
(runs, overview). `--since` / `--until` are UTC too. File mtimes you see elsewhere are local
time — do not mix them.

Most table commands take `--sort <col> [--asc] --top N`; **`ssa <cmd> --help` is the
authoritative flag list** for any command, not just its sort columns. Notable exceptions:
`agents` sorts on `depth|tokens|turns|cost|tools|start|duration` and defaults to
first-appearance order (the order a delegation chain happened in); `runs` and `audit` do not
sort at all. Filters: `--min-tokens N` (sinks/tools/
cache-attr), `--model <substr>` (subagents/timeline), `--grep <text>` (sessions).

## Reading and searching conversations

`search` and `transcript` share one selection vocabulary, so you can narrow a search and
then read the hits with the same flags:

| flag | effect |
|---|---|
| `--grep TEXT` | case-insensitive substring over message text, thinking, tool names/targets and tool output |
| `--kind user\|assistant\|tool\|compact\|event` | role; `event` = harness events (turn limits, reminders, hook results, memory loads) |
| `--thread all\|main\|sub` | whole conversation (default), main thread only, or sub-agents only |
| `--agent SUBSTR` | only sub-agents whose **id, type or description** matches (implies `--thread sub`) |
| `--depth N` / `--min-depth N` | only sub-agents at exactly / at least that nesting level (implies `--thread sub`) |
| `--tool NAME` | messages that call that tool or carry its result |
| `--errors-only` | only failures: tool results that errored, and assistant turns that hit an API error. `tools` reports error *counts*; this is how you read them |
| `--input-grep TEXT` | match the tool **input** (the prompt handed to an Agent, the command given to Bash) |
| `--regex` | treat `--grep` / `--input-grep` as case-insensitive regular expressions |
| `--run N` | only messages in run N (see `ssa runs`) |
| `--since` / `--until` | wall-clock window: `HH:MM[:SS]`, or `YYYY-MM-DD HH:MM[:SS]` (UTC) |
| `--from N` / `--limit N` | page through a long transcript (`N` = the `[#N]` item index) |
| `--context N` | also return N messages either side of each `--grep` hit |

`--kind` accepts `event` as well as the message roles: harness events (turn-limit hits,
injected reminders, hook results, memory loads) are transcript items like any other. In
text output every message is prefixed with its wall-clock time, and JSON items carry
`ts_ms`, `timestamp`, `run`, `agent` and (for turns) `stop_reason` and tool `input`.

**The search loop an agent should use:**

```sh
ssa search -p LOG --grep "regression" --format json          # locate: session + item index
ssa show -p LOG --session 14574d2b --item 90                 # read one message in full
ssa transcript -p LOG --session 14574d2b --from 88 --limit 6 --full   # read around it
ssa transcript -p LOG --session 14574d2b --thread main --full # the human-facing thread only
ssa agents -p LOG --session 14574d2b                         # which sub-agents exist
ssa transcript -p LOG --session 14574d2b --agent a221ec --full # read one sub-agent
```

In the TUI, sub-agent conversations are **collapsed** to one row and opened with `Enter`
(`Esc` walks back out); the headless equivalents are `--thread main` and `--agent <id>`.
Sub-agent messages are **tagged and indented** (`▎Explore#a221ec`), a sub-agent's opening
message is labelled `TASK FROM PARENT` (it is the parent delegating, not the human), and
turns are numbered **per thread** — main turns are 1..N and each agent restarts at 1. In
JSON every transcript item carries `agent: {id, type, depth}` (`null` on the main thread)
and `turn_label`. Never treat a sidechain message as something the main agent said.

## Sharing a capture safely

**Never `tar czf ~/.claude`.** A `.claude` tree keeps live OAuth credentials
(`.credentials.json`), full config backups (`backups/.claude.json.backup.*`) and shell
snapshots (which capture the environment, API keys included) beside the logs. Anyone handed
such an archive holds the user's credentials.

```sh
ssa tar claude --dry-run              # show exactly what would be included and excluded
ssa tar claude -o session.tar.gz      # package ~/.claude, credentials excluded
ssa tar claude --session 14574d2b     # just one session and its sub-agent logs
ssa audit capture.tar.gz              # the reverse: does a capture you were HANDED leak?
```

**When you are handed a capture, audit it.** `ssa` reads only `projects/**`, so an archive
full of credentials looks clean from inside every analysis command. `ssa audit` inspects the
file list without extracting and exits non-zero if it finds any; every command also prints a
one-line warning to stderr on load. If a leaky archive was already sent, say so in your
report and recommend rotating the credentials — that is usually more urgent than whatever
you were asked to analyse.

Packaging is an **allowlist**: only files the provider recognises as session data
(`projects/**` logs and `subagents/*.meta.json` sidecars) are included, so a file added by a
future harness version is left out rather than leaked. Excluded credential stores are listed
by name so you can see they were dropped on purpose. `--source PATH` packages a tree that is
not under `$HOME`.

`ssa tar` also scans the payload for credential-shaped strings and warns — transcripts embed
whatever the agent read, so a clean *file list* does not mean a clean archive. Treat that
warning as a prompt to review, not as a guarantee either way.

## JSON shapes

`--format json` output is **not** uniformly shaped, and the column headers in text mode are
not always the JSON keys. Check here before writing a parser:

| command | top level | notable keys |
|---|---|---|
| `overview`, `rate` | object | `usage.*`, `terminal_events[]`, `skipped_sessions[]` |
| `sessions`, `agents`, `runs`, `timeline`, `spikes`, `tools`, `sinks`, `cache-attr`, `subagents`, `issues`, `trace`, `transcript` | **array** | — |
| `search` | **object** | `total_matches`, `returned`, `matches[]` |
| `tools --available` | object | `deferred_registry_size`, `tools[]`, `unavailable[]` |

Header → key mismatches worth knowing: `agents` prints `TYPE`/`TOKENS` but emits
`agent_type` and `usage.{input,output,…}_tokens`; assistant transcript items carry both
`turn` (session-wide sequence) and `turn_label` (per-thread, what's printed).

## Start here — pick by symptom

The recipes below are lettered, not ordered by importance. Route from the symptom:

| the user says… | start with | recipe |
|---|---|---|
| "it went wrong", "something broke", or nothing specific | `issues` then `runs` | **G** |
| "it never answered / produced nothing" | `runs`, then `trace --thread main` | **G** |
| "it stopped / hit a limit / was cut off" | `runs` | **G**, **I** |
| "it looped / repeated itself / kept spawning agents" | `issues`, then `agents` | **F**, **H** |
| "it couldn't do X" | `tools --available` | **E2** |
| "what did it actually do?" | `trace`, then `transcript` | **C** |
| "find where X was discussed" | `search` | **E** |
| "why so expensive / which sessions burn my quota" | `pressure`, then `overview` | **A** |
| "where are the token sinks" | `sinks`, then `cache-attr` | **B** |
| handed a `.claude` capture by someone else | `audit` **first** | — |

**When in doubt, run `ssa issues --session <id>` first.** It reports control-flow failures
(how runs ended, delegation loops, repeated calls, missing capabilities) *and* cost
findings in one pass, and names the command that verifies each one. It is the single best
entry point for "something went wrong and I don't know what".

## Analysis recipes

**A. "Why was this session expensive / why did it burn the subscription?"**
Fastest path: **`pressure`** (add `-p A -p B` to span an archive + `~/.claude`) — it ranks
every session by sustained cache-write/hour and flags `BURST` (unattended `sdk-ts`, 0 idle
gaps). The flagged top rows are your culprits in one command.

No single token metric reliably explains a lockout — a small session can hit the limit
while a much larger one on the same plan does not. To reason it through across axes:
1. `overview` / `sessions` → **`entrypoint` (harness) and continuity**. A `sdk-ts`
   (headless `-p`) run that is genuinely continuous concentrates its whole load into one
   rolling window — the strongest observed signal. Judge continuity from **`active_ms` vs
   `duration_ms` together with `longest_gap_ms`**, not from `idle_gaps` alone: `idle_gaps`
   counts only pauses over 15 minutes (the rate-window threshold), so a session can show
   `idle_gaps: 0` and still contain a 12-minute human pause. `pressure`'s `BURST` flag uses
   the stricter "no pause over 5 minutes" test.
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
Use `--thread main` first to read what the user actually saw, then `agents` +
`transcript --agent <id>` to drill into any delegated work.

**E. "Find where X was discussed / who did X"** → `search --grep X --format json` across
every session and sub-agent, then `show`/`transcript --from` on the returned locators.
Add `--thread sub` to search only delegated work, or `--tool Bash` to find command runs.

**E2. "What could the agent actually do?"** → `tools --available`. Lists the deferred
registry, what was called, and — the part that matters — **every tool an agent searched for
and the harness could not provide**. A missing capability is the usual root cause behind
workaround loops: the agent improvises, and every sub-agent it spawns improvises the same
way. An empty registry means "not logged", not "no tools".

**F. "Did delegation go wrong?"** → `agents --session ID`. `--spinning` narrows to agents
whose every tool call spawned another agent — the exact set the `Delegation loop` finding
counts, so its number is reproducible rather than something to take on trust.
`--depth N` / `--min-depth N` slice the chain, and `trace --min-depth N` reads it.
Watch the `D` (depth) column:
an agent chain that keeps growing (d1 → d2 → … each spawning the next, all `truncated`)
is a runaway delegation loop, and every level re-pays the whole prompt as `cache_write`.
`issues` raises **Delegation loop** for this directly, with the wasted-token count.

**I. "A run says `truncated` — was it stopped, or still running?"** The log cannot settle
this, so `ssa` reports what is observable and leaves the call to you.

- `runs` gives the mechanical detail: `last turn stopped on \`tool_use\` and no result
  followed`, or `last response has no stop_reason — it was still being generated`.
- `audit <capture>` lists **harness state files** (`sessions/<pid>.json`, `history.jsonl`).
  A session state file naming a pid and a start time is the best available evidence about
  whether work was still in flight when the capture was taken. Read it yourself.
- File mtimes will **not** help: archives preserve them, so the newest mtime is just the
  last log write. `overview` mentions it only when files were touched well *after* the last
  record, which is the one case where it tells you something.

Say which reading the evidence supports, and say plainly what it does not exclude.

**G. "Why did this run stop / what is this max-turns error?"** → `runs --session ID`.
Each row carries an outcome and, for a limit, the exact event
(`max_turns_reached: hit the turn limit: 21 of 20 turns`). Remember **limits are per run**:
a session showing 23 turns against `maxTurns: 20` is two runs, not a contradiction. Then
`trace --session ID --run N` to see the turns that consumed the budget. And
`search --kind event` for every harness event in scope — turn limits, injected reminders,
hook results, memory loads. That last one also lets you state a *negative* finding with
confidence ("no cancellation or depth-limit event exists in this log").

**H. "It repeated itself — what was it stuck on?"** → `trace --session ID` and read the
WHAT column; a loop is the same tool and target line after line. `issues` reports
**Repeated tool call** and **Delegation loop**, and `--input-grep` finds every call that
was handed the same instruction.

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
   share the parent `sessionId`). They are attributed to their agent and nested under the
   turn that spawned them, but per-session **totals still include sub-agent internals** —
   use `--thread main` / the `usage_main` vs `usage_sidechain` split in `overview` when you
   want the main thread alone. The `subagents` metric is a *different* measure
   (`toolUseResult.totalTokens`) and won't match the merged totals. A single exported
   `.jsonl` (no `subagents/` dir) is **blind** to sub-agent internal consumption — check
   `has_sidechain_detail`.
6. **`subagents` only lists agents that finished.** An agent still running when the log was
   captured has no result record, so it appears in `agents` (reconstructed from its
   messages) but not in `subagents`, and its `toolStats` are unknown.
   `agents` gives it an `outcome`. **`truncated` is an observation, not a verdict**: the
   last response has no closing `stop_reason` and nothing follows it. Whether the process
   was stopped, crashed, or was simply still running when the capture was taken is not
   determinable from the log, and `ssa` does not guess — see recipe I.
7. **Cache attribution pools every thread.** `sinks` / `cache-attr` treat one session as one
   context, but each sub-agent has its own window — with heavy delegation, read those as
   "what this session paid for overall", not "what the main thread carried".
4. **`timeline` x-axis is turn index, not wall-clock time** — you cannot see bursts or gaps
   in time from it.
8. **Turn numbers are per thread everywhere** (`timeline`, `spikes`, `trace`, transcript):
   `7` is main-thread turn 7, `Explore#a221ec ▸ 3` is that agent's third turn. The
   session-wide sequence number still exists in JSON as `turn`, but is not what's printed.
9. **A session with no assistant turns is not analysed** (a bare `/login`, a truncated
   file). It is *reported* as `skipped_sessions` in `overview` rather than dropped, so
   "sessions: N" can be read as a complete statement.
10. **`ssa` never reads credentials**, only `projects/**`. But a raw `.claude` tree contains
   `.credentials.json` and `.claude.json` backups — use `ssa tar claude` to package a
   capture for sharing; it excludes those by construction. Never hand-roll `tar czf ~/.claude`.
5. **Sink/attribution token counts are estimates** (~3.7 chars/token), normalized to
   reconcile with the real cache-read total. Treat them as proportions, not exact tokens.
