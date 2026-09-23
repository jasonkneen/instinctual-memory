# AGENTS

Rust CLI `mem`: Git-first durable memory for AI agents. See `PRD-portable-agent-memory.md`.

- Build: `cargo build --release` (binary at `./target/release/mem`; `~/.local/bin/mem` links to it). Stores: a project's local store is `.mem/` (`mem init`); the global store is `~/.mem`. `mem/.models` here only caches the local reranker for the ignored tests.
- Always run `cargo test` before pushing.
- `./mem` holds a real local journal; do not wipe it without asking.
- `.env` holds API keys and is not tracked. Never commit it.
- `mem writeback` only rewrites the `mem:begin`/`mem:end` block in this file.
