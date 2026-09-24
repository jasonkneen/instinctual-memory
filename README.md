# mem

Git-first durable memory for AI agents.

`mem` reads the conversations you have already had with Claude Code, Codex, OpenCode, pi, and omp, plus your `AGENTS.md`/`CLAUDE.md`/`README.md` files, and turns them into a small set of curated, sourced facts stored in a plain Git repository. Agents search that memory (facts and raw sessions together), read it over MCP, change it through a validated path, and get it written back into each project's `AGENTS.md`.

No database, no vector store, no server to run: one bare Git repository and a journal directory.

## Install

Requirements: Rust 1.88+ and Git 2.28+.

```bash
cargo install --path .
mem --version
```

## Quick start

```bash
cd ~/code/shop
mem init                                   # local store: ~/code/shop/.mem (kept out of git)
mem backfill-local                         # this project's sessions + AGENTS.md/CLAUDE.md/README.md
mem consolidate                            # sessions -> curated facts in memory.git
mem search "how do we deploy" --project shop
mem writeback --to ~/code/shop/AGENTS.md   # the project's facts, inside a managed block
```

## Global and local stores

Stores work like Git repositories:

- **Local**: `mem init` inside a project creates `.mem/` at the project root (the Git repository containing the current folder) and adds it to `.git/info/exclude`. Commands run anywhere inside that project use it.
- **Global**: `mem init --global` creates `~/.mem` (or `$MEM_HOME`). Commands run outside any project use it; `--global` selects it from inside one. `mem backfill-all` into the global store gives one memory for every project, and `--project` still separates them.

A command picks its store from `--root DIR`, else `--global`/`--local`, else `$MEM_ROOT`, else the nearest `.mem/` in this folder or a parent, else the global store. With none of those present it stops, like Git outside a repository:

```
fatal: not a mem store (or any of the parent directories): .mem
```

`mem init` never modifies an existing store; `mem status` shows which store is in use and why.

## How memory flows

```
sessions, AGENTS.md ──ingest/backfill──▶ journal/ (append-only, one event per message)
                                             │
                                   consolidate (LLM or rules)
                                             ▼
                          memory.git (entity Markdown + facts, INDEX.md)
                                             │
                 search · read · history · MCP · HTTP · shell · writeback
```

- **Journal** (`journal/events-YYYY-MM-DD.jsonl`): every user and assistant message and every memory file, with a stable per-message id, its project, and its timestamp. Re-ingesting never duplicates an event.
- **Consolidation** reads events after its checkpoint in batches. The LLM extractor sees only the user's own words and memory files (it skips assistant turns, subagent transcripts, conversation-compaction summaries, and text the tools inject, such as `<system-reminder>` blocks carrying CLAUDE.md). It keeps only facts that pass a "still true for a new teammate next month" test (requests like "move the button 4px" and "User is running X" are tasks and session activity, not facts), and keeps a fact only if its evidence quote appears verbatim in the cited event (quoted fragments joined by an ellipsis must each appear, in order; Markdown marks are ignored). Memory files are read whole, in chunks. Facts that restate an existing one (in the same project or the user's preferences) are dropped. When AGENTS.md or CLAUDE.md changes, facts that came only from its old version and were not extracted again are marked superseded. Each batch is published as one Git commit with the changed entities, `INDEX.md`, the checkpoint, and a disposition file listing what happened to each event.
- **Facts** live in `memory/<people|orgs|workstreams|preferences>/<name>.md`: YAML frontmatter holds each fact with its id, status, timestamps, and sources; the Markdown body lists the active facts. A project's facts are on its `w_<project>` entity; general working preferences are on `pref_user`.
- **Projects**: each event records the root of the Git repository its session ran in. `search --project`, MCP `project`, and `writeback` use it, so one store can hold every project and still give each one only its own memory.

## Commands

| Command | What it does |
|---|---|
| `mem init [--global]` | Create a local store at the project root, or the global store. Never touches an existing one. |
| `mem backfill-local [--project DIR]` | One project's Claude Code, Codex, OpenCode, pi, and omp sessions (working directory inside the project), transcripts inside it, and its memory files. |
| `mem backfill-all [--source claude,codex,opencode,pi,omp]` | Every local session from every project. |
| `mem backfill PATHS... [--project DIR]` | Files and directories, each routed to its format (`mem formats`). |
| `mem ingest FILES... [--format F] [--project DIR]` | Specific files. |
| `mem consolidate [--extractor auto\|llm\|rules] [--high-water N]` | Journal → facts, batch by batch until caught up. `auto` uses the LLM when a key is set. |
| `mem search QUERY [--project P] [--limit N] [--no-rerank] [--snippet N] [--source-lines N]` | Facts and journal events together, reranked automatically. |
| `mem read ID` / `mem history ID` | A fact, an entity's current facts, or a journal event; history lists every fact with its status. |
| `mem change remember\|correct\|forget --entity E ...` | Immediate validated change. `correct` supersedes the old fact; `forget` hides it from every read. |
| `mem erase TARGET [--reason R]` | Remove a fact (or a whole entity) from the tree, redact its source events, rewrite the store's history without it, and prune the old objects. |
| `mem writeback [--to FILE] [--project P \| --all] [--include-file-facts] [--force]` | Write the project's facts (then the user's preferences) into a `<!-- mem:begin -->…<!-- mem:end -->` block. A local store writes all of its facts; a shared store writes the project's. Facts that only restate AGENTS.md/CLAUDE.md/README.md are left out, since agents already read those. Text outside the block is never touched. |
| `mem tidy [--apply] [--keep id,…]` | List duplicates and facts that only described one session; `--apply` forgets them in one commit (history keeps them). |
| `mem index` | Republish `INDEX.md` (it is also published with every write). |
| `mem status` | Events, projects, consolidation lag, facts, pending changes, active reranker and extractor. |
| `mem evaluate [--rubric FILE]` | Score a question rubric against the search, lexical and reranked side by side. |
| `mem bench --queries a,b` | Time the search, lexical and reranked. |
| `mem models pull [--model M]` / `mem models status` | Download the local reranker used when no JEV key is set. |
| `mem serve --stdio` / `mem serve --http --listen 127.0.0.1:8765` | MCP over stdio, or loopback HTTP with server-sent events. |
| `mem shell` | Line-oriented memory shell (`help` lists its commands). |
| `mem setup [claude\|codex\|pi\|omp] [--remove] [--no-mcp]` | Install hooks, MCP, the pi/omp extension, and the `/mem` skill so agents use memory automatically. |
| `mem hook start\|prompt\|end\|sync` | Hook entry points (reads the hook JSON on stdin); `sync` backfills and consolidates by hand. |

`--output-format json|text|stream-json` controls `search`, `status`, and `evaluate`.

## Search and reranking

`mem search` scans curated facts (suppressed, retracted, superseded, and expired facts are never returned) and the journal, interleaves them, and reranks the pool:

1. **JEV** (TypeSafe JEV on the OpenRouter Decisions API) when a key is configured. Only the query and each row's text go to OpenRouter.
2. Otherwise the **local cross-encoder** (fastembed, `bge-reranker-base` by default) when `mem models pull` has downloaded it. Nothing leaves the machine.
3. Otherwise the lexical order.

`--no-rerank` (or `MEM_RERANK=off`) skips reranking: no network, milliseconds instead of about a second.

## Forget vs. erase

- `mem change forget` suppresses a fact: it disappears from search, read, and history immediately, and consolidation never re-creates it. The fact remains in Git history.
- `mem erase` removes it: from the entity file, from the journal events it came from (their text becomes `[erased]`), from every commit on the `memory` branch, and from any pending change intents; unreachable Git objects are pruned.

## Make agents use it: `mem setup`

```bash
mem setup            # Claude Code + Codex + pi + omp
mem setup pi         # just the pi extension + /mem skill
mem setup omp        # omp extension + MCP server + /mem skill
mem setup --remove   # undo everything setup installed
```

Every file it edits is backed up first (`*.mem-backup-<time>`), and running it again changes nothing. It installs:

- **Claude Code hooks** in `~/.claude/settings.json`, so memory reaches the model without it having to ask:
  - `SessionStart` → `mem hook start`: the user's working preferences, and a note that project memory exists and how to query it.
  - `UserPromptSubmit` → `mem hook prompt`: the curated facts that match the message (a fact must share at least two meaningful words with it; short prompts and slash commands are skipped). Lexical only, about half a second, no network.
  - `SessionEnd` → `mem hook end`: in the background, backfill the finished session into the journal, and consolidate once 200+ new events have built up and an LLM key is set (`MEM_AUTO_CONSOLIDATE=<n>`, or `0` to turn that off).
  - Hooks use the store for the session's folder (local, else global with `--project` filtering), stay silent when there is none, and never fail a session (`MEM_HOOK_DEBUG=1` shows why they did nothing).
- **The `mem` MCP server** for Claude Code (user scope), Codex (`~/.codex/config.toml`), and omp (`~/.omp/agent/mcp.json`): `memory_search`, `memory_read`, `memory_history`, `memory_status`, `memory_request_change`, `tasks_read`, `tasks_update`.
- **The pi/omp extension** (`~/.pi/agent/extensions/mem.ts`, `~/.omp/agent/extensions/mem.ts`): appends the user's preferences and the facts that match each prompt to the system prompt, exposes `memory_search` / `memory_read` / `memory_history` / `memory_status` / `memory_request_change` as native tools (omp gets them from MCP instead, so the model never sees two sets), and backfills the finished session in the background on exit. Set `MEM_BIN` to point at a `mem` that is not on PATH, or `MEM_EXTENSION_TOOLS=1|0` to force the tools on or off.
- **The `/mem` skill** (`~/.claude/skills/mem` and the shared `~/.agents/skills/mem`, read by pi and omp): `/mem <question>`, `/mem sync`, `/mem remember …`, `/mem writeback`. It finds or creates the store, syncs it, then answers or changes memory.

Codex, OpenCode, pi, and omp also read `AGENTS.md`; keep it current with `mem writeback --to AGENTS.md`.

Manual MCP registration, if you prefer: `claude mcp add mem -- mem serve --stdio` (local store of the session's project) or `… mem --global serve --stdio`; any MCP client can run `mem --root /abs/path serve --stdio` for a fixed store. `mem op <operation> --args '<json>'` runs any MCP operation from the shell. Tools: `memory_search`, `memory_read`, `memory_history`, `memory_status`, `memory_request_change` (remember/correct/forget), `tasks_read`, `tasks_update`.

## HTTP

`mem serve --http --listen 127.0.0.1:8765` binds loopback only.

- `POST /v1/operations` with `{"operation": "memory_search", "arguments": {"query": "..."}}` runs any MCP operation and returns its result plus an `operation_id`.
- `GET /v1/operations/{id}/events` streams `progress` then `result` as server-sent events.
- `GET /health`.

## Configuration

Keys and settings come from the environment, else the nearest `.env` that defines a key (up to four parent folders), else `~/.config/mem/env` (`KEY=value` lines).

| Variable | Purpose | Default |
|---|---|---|
| `MEM_ROOT` | Use this store unless a flag says otherwise | — |
| `MEM_HOME` | Location of the global store | `~/.mem` |
| `OPENROUTER_API_KEY` (or `MEM_JEV_API_KEY`, `MEM_LLM_API_KEY`) | JEV reranking and the LLM extractor | — |
| `MEM_LLM_MODEL` | Consolidation model | `anthropic/claude-haiku-4.5` |
| `MEM_LLM_BASE_URL` | OpenAI-compatible endpoint for consolidation | `https://openrouter.ai/api/v1` |
| `MEM_JEV_MODEL` / `JEV_MODEL` | JEV model | `~typesafe/jev-latest` |
| `OPENROUTER_DECISIONS_BASE_URL` | Decisions API endpoint | `https://openrouter.ai/api/alpha/decisions` |
| `MEM_JEV_DOTENV_PATH` | Read keys from this file instead of searching | — |
| `MEM_RERANK=off` | Never rerank (no network calls from search) | on |
| `MEM_LAYA_MODEL` | Local reranker: `bge-reranker-base`, `jina`, `bge-v2-m3` | what `models pull` chose |
| `MEM_AUTO_CONSOLIDATE` | New events that make the session-end hook consolidate (`0` = never) | `200` |
| `MEM_PI_AGENT_DIR` / `PI_CODING_AGENT_DIR` | Where pi stores its sessions (`sessions/` inside it) | `~/.pi/agent` |
| `MEM_OMP_AGENT_DIR` | Where omp stores its sessions (`sessions/` inside it) | `~/.omp/agent` |
| `MEM_BIN` | `mem` binary the pi/omp extension runs | `mem` on PATH |
| `MEM_EXTENSION_TOOLS` | pi/omp extension: `1` force native tools, `0` skip them (MCP) | auto |
| `MEM_HOOK_DEBUG=1` | Print why a hook did nothing | off |
| `MEM_CONSOLIDATE_VERBOSE=1` | Log each proposed fact that consolidation rejected or dropped, with the reason | off |
| `MEM_JOURNAL_FSYNC=0` | Skip fsync during bulk backfill (the journal can be rebuilt from its sources) | fsync on |

## Evaluation rubric

`mem evaluate` reads `<root>/rubric.json`:

```json
{"cases": [
  {"id": "deploy", "category": "direct_recall", "question": "where do we deploy",
   "project": "shop", "expected_fact_ids": ["fact_8df0f3e8738e"],
   "must_include": ["Fly"], "must_not_include": ["Netlify"]}
]}
```

A case passes when every expected id is in the top hits, every `must_include` string appears, and no `must_not_include` string does. The report shows the lexical run and the reranked run side by side.

## On disk

```
<root>/
  memory.git/        bare repository, branch `memory`
    memory/…/*.md    entities and facts
    INDEX.md         one line per entity
    state/           controls (suppressions, deletions), checkpoint, dispositions, receipts
  journal/           events-YYYY-MM-DD.jsonl, framed records
  intents/           durable change intents (replayed after a crash; stale ones kept as .stale)
  .models/           local reranker, if pulled
```

## Trust model

- The model never writes memory directly: changes go through `memory_request_change` or consolidation, are validated, and publish with compare-and-swap on the `memory` branch, so a concurrent writer can never overwrite another's commit.
- Every change carries a request id; repeating it replays the stored receipt instead of applying it twice.
- A change interrupted by a crash is finished (or set aside as stale) before the next change runs.
- The Git wrapper ignores inherited `GIT_*` variables, system and global config, hooks, aliases, and signing.
- HTTP binds loopback only and has no other authentication; run it only on machines you trust.

## Development

```bash
cargo test                       # unit and end-to-end tests, offline
cargo test -- --ignored          # also the local-reranker tests (needs `mem models pull`)
cargo clippy --all-targets
```

The product requirements are in `PRD-portable-agent-memory.md`.

## License

Apache-2.0
