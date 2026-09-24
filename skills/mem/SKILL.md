---
name: mem
description: Durable project memory with the `mem` CLI. Use when the user types /mem, asks what is remembered or known about a project ("what do we know about deploys", "check memory", "have we decided X before"), asks to remember, correct, or forget something, or asks to sync memory or update AGENTS.md from memory. Finds or creates the project's memory store, brings it up to date from past Claude Code / Codex / OpenCode / pi / omp sessions and AGENTS.md, then answers or changes memory.
argument-hint: "[question | sync | remember <fact> | forget <what> | writeback | tidy | status]"
allowed-tools:
  - Bash
  - Read
---

# mem: project memory

`mem` keeps a project's durable memory: curated facts (with sources) in a Git store, plus the raw journal of past agent sessions. Run every command from the project root unless told otherwise.

In pi and omp, `mem setup` also installs an extension that injects memory automatically: the user's preferences and matching facts are appended to the system prompt, `memory_search` / `memory_read` / `memory_history` / `memory_status` / `memory_request_change` are available as native tools (omp uses the `mem` MCP server instead), and the session is backfilled in the background on exit. This skill is the fallback and the `/mem` entry point for every harness.

## 1. Make sure memory is ready (every time)

```bash
command -v mem || echo "mem not installed"
mem status
```

- `mem` missing: tell the user `mem` is not on PATH (install: `cargo install --path <mem repo>`, or link its `target/release/mem` into `~/.local/bin`). Stop.- `fatal: not a mem store`: no local store here or above, and no global one. Create a local store at the project root with `mem init`, then continue with step 2. (If the user wants one memory for every project instead, `mem init --global` and add `--global` to every command below.)
- Otherwise `mem status` prints JSON. Read `store`, `journal.events`, `unconsolidated_events`, `active_facts`, `extractor`, `reranker`.

## 2. Bring it up to date (when the user asks to sync, when the journal is empty, or before answering if the last sync looks stale)

```bash
mem backfill-local        # this project's sessions + AGENTS.md/CLAUDE.md/README.md; only new events are added
mem status                # check unconsolidated_events
mem consolidate           # turn new events into facts; only processes what is new
```

- If `extractor` is `rules`, no API key is configured: consolidation will find very little. Tell the user to add `OPENROUTER_API_KEY=...` to `~/.config/mem/env` and ask whether to continue anyway.
- `consolidate` makes model calls: roughly 2–3 minutes and a few cents per ~3,000 events. When `unconsolidated_events` is above ~500, run it in the background and tell the user it is running. It is safe to interrupt and re-run.
- Nothing new (`unconsolidated_events` is 0 after backfill): skip consolidate.

## 3. Do what was asked

**A question** (`/mem how do we deploy`, "what do we know about X"):

```bash
mem search "<the question in plain words>" --limit 8
```

JSON `hits`, best first. `fact_…` ids are curated facts (trust these first); `journal:…` ids are raw session excerpts (context, may be outdated). For more on one hit: `mem read <id>`. Answer from the hits and cite fact ids. If nothing relevant comes back, say memory has nothing on it; do not guess.

**Remember something** (`/mem remember we deploy with fly deploy`):

```bash
mem change remember --entity w_<project-folder-name> --statement "<one self-contained sentence>" --predicate <snake_case_topic>
```

Use `pref_user` as the entity for the user's general working preferences, `p_<name>` for a person.

**Correct something**: find the fact with `mem search`, then
`mem change correct --entity <entity_id from the hit> --target <fact_id> --statement "<corrected sentence>"`.

**Forget something**: find it with `mem search`, confirm the exact fact with the user, then
`mem change forget --entity <entity_id> --target <fact_id>`. It disappears from every read; it stays in the store's Git history.

**Erase completely** (the user says erase, delete permanently, remove all trace): confirm first, because it cannot be undone. `mem erase <fact_id>` removes it from the facts, redacts its source session messages, and rewrites the store's history.

**Write memory into AGENTS.md** (`/mem writeback`):

```bash
mem writeback --to AGENTS.md
```

Only the `<!-- mem:begin --> … <!-- mem:end -->` block is rewritten; everything else in the file is left as it was. Tell the user AGENTS.md changed.

**Status** (`/mem status`): run `mem --output-format text status` and summarise it.

**Clean up** (`/mem tidy`, or when memory looks noisy): run `mem --output-format text tidy`. It lists duplicates and facts that only described one session. Show the list to the user, ask which (if any) to keep, then run `mem tidy --apply --keep <ids to keep>`. Never apply without the user seeing the list.

**`/mem` with no arguments**: do steps 1 and 2, then report what memory holds (`mem status`) and offer search, remember, or writeback.

## Notes

- `mem` never needs `--root` inside a project: it finds `.mem/` (or the legacy `mem/`) in the current folder or a parent. Use `--global` only for the shared `~/.mem` store.
- One global store can hold many projects; add `--project <folder-name>` to `search` to keep answers to this project.
- Never edit files under `.mem/` directly; always go through `mem`.
