//! CLI plumbing.
//!
//! Implements the command surface described in section 14 of the source PRD:
//! - `init`
//! - `ingest <files...> [--format <adapter>]`
//! - `change remember|correct|forget [args]`
//! - `consolidate [--extractor rules|llm]`
//! - `search <query> [--limit N]`
//! - `read <entity-id>`
//! - `history <entity-id>`
//! - `index`
//! - `status`
//!
//! Every command resolves the published branch to one revision up-front and
//! passes it through the appropriate module so concurrent runs see a stable
//! view.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};

use crate::change::{self, ChangeKind, ChangeRequest};
use crate::consolidate::{ConsolidateOptions, Consolidator, ExtractorKind};
use crate::entity::EntityFile;
use crate::error::{Error, Result};
use crate::fact::{Fact, FactKind, FactStatus, Role as FactRole, SourceRef, Visibility};
use crate::ingest::{adapter_for, ChatExportAdapter, ClaudeAdapter, CodexAdapter, PiAdapter};
use crate::ingest::Adapter as IngestAdapter;
use crate::journal::{Journal, JournalEvent, Role, Source};
use crate::paths::request_id as validate_request_id;
use crate::repo::GitRepo;

#[derive(Debug, Parser)]
#[command(
    name = "mem",
    version,
    about = "Git-first durable memory for AI agents",
    long_about = "Git-first durable memory for AI agents.\n\nBackfill Claude Code, Codex, OpenCode, pi, and omp sessions and AGENTS.md-style files into a journal, consolidate them into curated facts in a bare Git repository, search facts and sessions together, and write a project's memory back into its AGENTS.md. Serve the same memory to agents over MCP (`mem serve --stdio`)."
)]
pub struct Cli {
    /// Store directory to use (holds memory.git, journal/, intents/,
    /// .models/). Without it: --global/--local, else $MEM_ROOT, else the
    /// nearest local store (.mem/ in this folder or a parent), else the
    /// global store (~/.mem).
    #[arg(long = "root", global = true, value_name = "DIR")]
    pub root_arg: Option<PathBuf>,
    /// Use the global store (~/.mem, or $MEM_HOME).
    #[arg(long, global = true, conflicts_with_all = ["local", "root_arg"])]
    pub global: bool,
    /// Use the nearest local store; with `init`, create one at the project
    /// root (the Git repository containing this folder).
    #[arg(long, global = true, conflicts_with = "root_arg")]
    pub local: bool,
    /// The store this invocation uses, set by [`Cli::resolve_store`].
    #[arg(skip)]
    pub root: PathBuf,
    /// Why that store was chosen.
    #[arg(skip)]
    pub store: StoreKind,
    /// True when no flag, `$MEM_ROOT`, or local store chose the store and it
    /// fell back to the global one.
    #[arg(skip)]
    pub store_fallback: bool,
    /// Tenant scope stamped on ingested events and used by consolidation.
    #[arg(long, global = true, default_value = "personal")]
    pub scope: String,
    /// Session label stamped on ingested events.
    #[arg(long, global = true, default_value = "default")]
    pub session: String,
    /// Output of `search`, `status`, and `evaluate`. `json` is one object.
    /// `stream-json` is one JSON object per line, flushed as the command
    /// goes. `text` is a readable list.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Json)]
    pub output_format: OutputFormat,
    #[command(subcommand)]
    pub command: Command,
}

/// How the store for this invocation was chosen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StoreKind {
    /// `--root DIR`.
    Explicit,
    /// `$MEM_ROOT`.
    Env,
    /// A project's `.mem/` (or a legacy `mem/`) store.
    #[default]
    Local,
    /// `~/.mem`.
    Global,
}

impl StoreKind {
    pub fn label(self) -> &'static str {
        match self {
            StoreKind::Explicit => "explicit (--root)",
            StoreKind::Env => "MEM_ROOT",
            StoreKind::Local => "local",
            StoreKind::Global => "global",
        }
    }
}

/// Folder name of a project-local store.
pub const LOCAL_STORE: &str = ".mem";

/// The global store: `$MEM_HOME`, else `~/.mem`.
pub fn global_store() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("MEM_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(home));
    }
    std::env::var_os("HOME")
        .map(|h| PathBuf::from(h).join(".mem"))
        .ok_or_else(|| Error::Usage("HOME is not set; pass --root or set MEM_HOME".into()))
}

/// The nearest local store at or above `dir`: a `.mem/` (or legacy `mem/`)
/// folder holding `memory.git`. The global store is never treated as local.
pub fn find_local_store(dir: &Path) -> Option<PathBuf> {
    let global = global_store().ok().and_then(|g| g.canonicalize().ok());
    for ancestor in dir.ancestors() {
        for name in [LOCAL_STORE, "mem"] {
            let candidate = ancestor.join(name);
            if !candidate.join("memory.git").is_dir() {
                continue;
            }
            if global.is_some() && candidate.canonicalize().ok() == global {
                continue;
            }
            return Some(candidate);
        }
    }
    None
}

/// The store an agent session in `dir` should read: `$MEM_ROOT`, else the
/// nearest local store, else the global store if it exists. The second value
/// is the project name to filter by when the store is shared (not local).
pub fn store_for_dir(dir: &Path) -> Option<(PathBuf, Option<String>)> {
    let project = || {
        Path::new(&crate::journal::project_root(dir))
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
    };
    if let Some(env) = std::env::var_os("MEM_ROOT").filter(|v| !v.is_empty()) {
        let root = PathBuf::from(env);
        return root.join("memory.git").is_dir().then(|| (root, project()));
    }
    if let Some(local) = find_local_store(dir) {
        return Some((local, None));
    }
    let global = global_store().ok()?;
    global.join("memory.git").is_dir().then(|| (global, project()))
}

impl Cli {
    /// Decide which store this invocation uses: `--root`, then
    /// `--global`/`--local`, then `$MEM_ROOT`, then the nearest local store,
    /// then the global store. `init` with no flag creates a local store at
    /// the project root.
    pub fn resolve_store(&mut self) -> Result<()> {
        let cwd = std::env::current_dir().map_err(|e| Error::io("cwd", e))?;
        let init = matches!(self.command, Command::Init);
        let new_local = || PathBuf::from(crate::journal::project_root(&cwd)).join(LOCAL_STORE);
        let (root, kind) = if let Some(root) = &self.root_arg {
            (root.clone(), StoreKind::Explicit)
        } else if self.global {
            (global_store()?, StoreKind::Global)
        } else if self.local {
            match find_local_store(&cwd) {
                Some(root) => (root, StoreKind::Local),
                None if init => (new_local(), StoreKind::Local),
                None => {
                    return Err(Error::Usage(format!(
                        "no local store in {} or its parents; run `mem init` here",
                        cwd.display()
                    )))
                }
            }
        } else if let Some(env) = std::env::var_os("MEM_ROOT").filter(|v| !v.is_empty()) {
            (PathBuf::from(env), StoreKind::Env)
        } else if let Some(root) = find_local_store(&cwd) {
            (root, StoreKind::Local)
        } else if init {
            (new_local(), StoreKind::Local)
        } else {
            self.store_fallback = true;
            (global_store()?, StoreKind::Global)
        };
        self.root = root;
        self.store = kind;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Json,
    Text,
    #[value(name = "stream-json")]
    StreamJson,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialise a new memory root.
    Init,
    /// Ingest files into the journal. Repeating an ingest adds nothing.
    Ingest {
        /// Files to ingest (see `mem formats`).
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Force a format instead of detecting it from the file.
        #[arg(long, value_enum)]
        format: Option<Adapter>,
        /// Project these events belong to, for events whose source does not
        /// name one (the project's root folder).
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Ingest one project's history: Claude Code, Codex, OpenCode, pi, and
    /// omp sessions whose working directory is inside the project,
    /// transcripts inside it, and its AGENTS.md / CLAUDE.md / README.md.
    /// Re-running adds only new events.
    BackfillLocal {
        /// Override the project root (defaults to the current working dir).
        #[arg(long)]
        project: Option<PathBuf>,
        /// Show what would be ingested without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Ingest every local Claude Code, Codex, OpenCode, pi, and omp session
    /// from every project. Each event records its project, so `search
    /// --project` and `writeback` still pull out one project's memory.
    BackfillAll {
        /// Apply to the listed sources only. Defaults to
        /// `claude,codex,opencode,pi,omp`.
        #[arg(long, value_delimiter = ',', default_values = &["claude", "codex", "opencode", "pi", "omp"])]
        source: Vec<String>,
        /// Show what would be ingested without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Apply an immediate, validated change: remember a fact, correct one
    /// (the old fact is superseded), or forget one (suppressed everywhere).
    Change {
        #[arg(value_enum)]
        kind: ChangeCommand,
        /// Entity id: p_<person>, o_<org>, w_<project>, pref_<name>.
        #[arg(long)]
        entity: String,
        /// Id for the new fact (remember/correct). Generated when omitted.
        #[arg(long)]
        fact_id: Option<String>,
        /// Fact to correct or forget.
        #[arg(long, required_if_eq_any([("kind", "correct"), ("kind", "forget")]))]
        target: Option<String>,
        /// The fact, as one sentence (remember/correct).
        #[arg(long, required_if_eq_any([("kind", "remember"), ("kind", "correct")]))]
        statement: Option<String>,
        /// Short snake_case label for what the fact is about.
        #[arg(long)]
        predicate: Option<String>,
        /// Idempotency key: repeating a request id with the same change
        /// replays it instead of applying it twice.
        #[arg(long)]
        request_id: Option<String>,
        /// Why, recorded with a forget.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Consolidate journal events into curated facts, batch by batch, until
    /// the checkpoint reaches the end of the journal.
    Consolidate {
        /// `auto` uses the LLM extractor when a key is configured, else rules.
        #[arg(long, value_enum, default_value_t = ExtractorArg::Auto)]
        extractor: ExtractorArg,
        /// Journal events per published batch.
        #[arg(long, default_value = "200")]
        high_water: u64,
        /// Names one batch, so a retry replays it; the command then stops
        /// after that batch.
        #[arg(long)]
        request_id: Option<String>,
    },
    /// Search curated facts and journal events together. Rows are reranked
    /// by JEV when a key is set, else by the local model if downloaded.
    Search {
        query: String,
        /// Hits to return (at most 20).
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Include facts marked disputed.
        #[arg(long)]
        include_disputed: bool,
        /// Only journal events with this scope.
        #[arg(long = "filter-scope")]
        filter_scope: Option<String>,
        /// Only rows whose source event id starts with one of these
        /// prefixes, comma-separated (e.g. `evt_chat_claude:,evt_chat_codex:`).
        #[arg(long, value_delimiter = ',')]
        source: Vec<String>,
        /// Only this project's memory (folder name or root path): its facts,
        /// the user's general preferences, and its journal events.
        #[arg(long)]
        project: Option<String>,
        /// Keep the lexical order. By default the top rows are reranked by
        /// JEV when a key is configured, else by the local model if cached.
        #[arg(long)]
        no_rerank: bool,
        /// Only curated facts; leave out raw session excerpts.
        #[arg(long)]
        facts_only: bool,
        /// Include an N-word snippet of each fact's statement centred on the
        /// first matched term. Set to 0 to disable.
        #[arg(long)]
        snippet: Option<usize>,
        /// Include the first N non-empty lines of each source journal event
        /// that contributed to a hit. Set to 0 to disable. Loads the journal
        /// once per query.
        #[arg(long)]
        source_lines: Option<usize>,
    },
    /// Read a fact, an entity's current facts, or a journal event by id.
    /// Forgotten and erased facts read as not found.
    Read {
        /// Fact id, entity id, or `journal:<event_id>#<seq>` from a search hit.
        #[arg(value_name = "ID")]
        entity_id: String,
    },
    /// Every fact of an entity with its status (current, superseded,
    /// retracted). Forgotten and erased facts are left out.
    History {
        /// Fact id or entity id.
        #[arg(value_name = "ID")]
        entity_id: String,
    },
    /// Republish the canonical INDEX.md (every entity, its type, and its
    /// active fact count) from the current head.
    Index,
    /// Journal size and projects, consolidation lag, facts, pending intents,
    /// and which reranker and extractor are active.
    Status,
    /// Score a rubric of questions against the search, lexical and reranked.
    Evaluate {
        /// Rubric file (defaults to <root>/rubric.json).
        #[arg(long)]
        rubric: Option<PathBuf>,
        /// Hits per question that count toward a pass.
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Erase a fact (or every fact of an entity) from the current tree, its
    /// source journal events, the memory branch history, and pending
    /// intents. Unlike `change forget`, nothing is left to recover.
    Erase {
        /// Fact id, or an entity id to erase all of its facts.
        target: String,
        /// Why, recorded in the controls file.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Ingest files and directories (recursively), routing each file to its
    /// format. Repeating a backfill adds nothing.
    Backfill {
        /// Files or directories.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Disable recursive directory traversal.
        #[arg(long)]
        no_recurse: bool,
        /// Show what would be ingested without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip unknown file extensions instead of erroring on them.
        #[arg(long)]
        ignore_unknown: bool,
        /// Project these events belong to, for events whose source does not
        /// name one (the project's root folder).
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// List the source formats the tool can ingest and where it looks for
    /// them on disk.
    Formats,
    /// Write a project's active facts and the user's preferences into its
    /// AGENTS.md (or CLAUDE.md, ...), inside a managed block, so agents read
    /// them on startup.
    Writeback {
        /// File to write. Defaults to ./AGENTS.md in the current working
        /// directory.
        #[arg(long)]
        to: Option<PathBuf>,
        /// What kind of file this is (used in the heading).
        #[arg(long, value_enum, default_value_t = WritebackKindArg::Agents)]
        kind: WritebackKindArg,
        /// Project whose facts to write: its project entity plus the user's
        /// general preferences. Defaults to the name of the folder that
        /// holds the target file.
        #[arg(long)]
        project: Option<String>,
        /// Write every active fact in the store, not just one project's
        /// (a local store always writes everything).
        #[arg(long, conflicts_with = "project")]
        all: bool,
        /// Also write facts that came only from AGENTS.md/CLAUDE.md/README.md
        /// (left out by default: agents already read those files).
        #[arg(long)]
        include_file_facts: bool,
        /// Replace the whole file. Without it, an existing file keeps its
        /// content and only the `mem:begin`/`mem:end` block is rewritten.
        #[arg(long)]
        force: bool,
    },
    /// Read memory commands from stdin until `quit` or `exit`.
    Shell,
    /// Serve the memory root over MCP stdio or loopback HTTP.
    Serve {
        /// JSON-RPC on stdin, one message per line. This is the MCP stdio transport.
        #[arg(long)]
        stdio: bool,
        /// Loopback HTTP JSON and server-sent events.
        #[arg(long)]
        http: bool,
        /// Bind address. Must be loopback, for example `127.0.0.1:8765`.
        #[arg(long)]
        listen: Option<String>,
    },
    /// Make agents use memory automatically: Claude Code hooks (preferences
    /// at session start, matching facts on every prompt, sync at session
    /// end), the mem MCP server for Claude Code, Codex, and omp, the /mem
    /// skill, and a pi/omp extension that injects memory and exposes the
    /// memory tools. Backs up every file it edits; running it again changes
    /// nothing.
    Setup {
        #[arg(value_enum, default_value_t = SetupTarget::All)]
        target: SetupTarget,
        /// Undo what setup installed.
        #[arg(long)]
        remove: bool,
        /// With `omp`: skip the MCP server, and expose memory through the
        /// extension's own tools instead.
        #[arg(long)]
        no_mcp: bool,
    },
    /// Find facts that should not be in memory: duplicates, and facts that
    /// only described one session ("User is running X"). Lists them; with
    /// --apply, forgets them all in one commit (hidden from every read,
    /// kept in the store's Git history).
    Tidy {
        #[arg(long)]
        apply: bool,
        /// Fact ids to leave alone (comma-separated), for findings you disagree with.
        #[arg(long, value_delimiter = ',')]
        keep: Vec<String>,
    },
    /// Agent hook entry point (installed by `mem setup`): reads the hook's
    /// JSON on stdin, prints context for the agent, never fails the session.
    Hook {
        #[arg(value_enum)]
        event: HookArg,
        /// Print only the context text, without the Claude Code
        /// `hookSpecificOutput` JSON envelope. Used by the pi/omp extension.
        #[arg(long)]
        plain: bool,
        /// With `--plain`: the session's working directory, instead of the
        /// Claude hook JSON on stdin.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// With `--plain`: the user's prompt, for `mem hook prompt`.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Run one MCP operation and print its JSON payload. Hidden; used by the
    /// pi/omp extension so its tools match the MCP toolset exactly.
    #[command(hide = true)]
    Op {
        /// Operation name, e.g. `memory_search`.
        operation: String,
        /// Arguments as a JSON object on stdin, or inline with `--args`.
        #[arg(long)]
        args: Option<String>,
    },
    /// Manage the local reranker model used when no JEV key is set.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
    /// Time the search against the current memory root: each query in
    /// lexical order and reranked, with min / median / p95 milliseconds.
    Bench {
        /// Queries to time, comma-separated or repeated.
        #[arg(long, value_delimiter = ',', required = true)]
        queries: Vec<String>,
        /// Timed runs per query for the lexical search.
        #[arg(long, default_value_t = 10)]
        iterations: usize,
        /// Timed runs per query for the reranked search (each may call JEV).
        #[arg(long, default_value_t = 3)]
        rerank_iterations: usize,
        /// Only this project's memory.
        #[arg(long)]
        project: Option<String>,
        /// Hits per search.
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SetupTarget {
    All,
    Claude,
    Codex,
    Pi,
    Omp,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HookArg {
    /// Session start: the user's preferences and how to reach the rest.
    Start,
    /// Each prompt: the curated facts that match it.
    Prompt,
    /// Session end: sync in the background.
    End,
    /// Backfill this project's new sessions, consolidate if enough built up.
    Sync,
}

#[derive(Debug, Subcommand)]
pub enum ModelsAction {
    /// Download a local cross-encoder into <root>/.models and make search
    /// use it whenever JEV is not configured.
    Pull {
        /// bge-reranker-base (default, ~280 MB), jina (English, smaller),
        /// or bge-v2-m3 (multilingual, larger).
        #[arg(long, default_value = "bge-reranker-base")]
        model: String,
    },
    /// Show which local model search would load, and whether it is on disk.
    Status,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Adapter {
    Chat,
    Claude,
    Codex,
    Pi,
    Markdown,
    Calendar,
    Voice,
    IdeHistory,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ChangeCommand {
    Remember,
    Correct,
    Forget,
}

impl ChangeCommand {
    fn kind(self) -> ChangeKind {
        match self {
            ChangeCommand::Remember => ChangeKind::Remember,
            ChangeCommand::Correct => ChangeKind::Correct,
            ChangeCommand::Forget => ChangeKind::Forget,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ExtractorArg {
    Auto,
    Rules,
    Llm,
}

impl ExtractorArg {
    fn kind(self) -> ExtractorKind {
        match self {
            ExtractorArg::Rules => ExtractorKind::Rules,
            ExtractorArg::Llm => ExtractorKind::Llm,
            ExtractorArg::Auto => ExtractorKind::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum WritebackKindArg {
    Agents,
    Claude,
    Codex,
    Readme,
}

impl WritebackKindArg {
    fn heading(self) -> &'static str {
        match self {
            WritebackKindArg::Agents => "AGENTS",
            WritebackKindArg::Claude => "CLAUDE",
            WritebackKindArg::Codex => "CODEX",
            WritebackKindArg::Readme => "Project Memory",
        }
    }
}

pub fn run(cli: &Cli) -> Result<()> {
    let needs_root = !matches!(
        cli.command,
        Command::Init | Command::Formats | Command::Setup { .. } | Command::Hook { .. }
    );
    if needs_root && !cli.root.join("memory.git").exists() {
        // Like git outside a repository: nothing here, above, or global.
        if cli.store_fallback {
            return Err(Error::NoStore);
        }
        return Err(Error::NotInitialized(cli.root.clone()));
    }
    match &cli.command {
        Command::Init => cmd_init(&cli.root, cli.store),
        Command::Ingest { files, format, project } => cmd_ingest(cli, files, *format, project.as_deref()),
        Command::BackfillLocal { project, dry_run } => {
            cmd_backfill_local(cli, project.as_ref(), *dry_run)
        }
        Command::BackfillAll { source, dry_run } => {
            cmd_backfill_all(cli, source, *dry_run)
        }
        Command::Change {
            kind,
            entity,
            fact_id,
            target,
            statement,
            predicate,
            request_id,
            reason,
        } => cmd_change(
            cli,
            kind.kind(),
            entity,
            fact_id.as_deref(),
            target.as_deref(),
            statement.as_deref(),
            predicate.as_deref(),
            request_id.as_deref(),
            reason.as_deref(),
        ),
        Command::Consolidate {
            extractor,
            high_water,
            request_id,
        } => cmd_consolidate(cli, extractor.kind(), *high_water, request_id.as_deref()),
        Command::Search {
            query,
            limit,
            include_disputed,
            filter_scope,
            source,
            project,
            no_rerank,
            facts_only,
            snippet,
            source_lines,
        } => cmd_search(
            cli,
            query,
            *limit,
            *include_disputed,
            filter_scope.as_deref(),
            source,
            project.as_deref(),
            *no_rerank,
            *facts_only,
            *snippet,
            *source_lines,
        ),
        Command::Read { entity_id } => cmd_read(cli, entity_id),
        Command::History { entity_id } => cmd_history(cli, entity_id),
        Command::Index => cmd_index(cli),
        Command::Status => cmd_status(cli),
        Command::Evaluate { rubric, limit } => cmd_evaluate(cli, rubric.as_ref(), *limit),
        Command::Erase { target, reason } => cmd_erase(cli, target, reason.as_deref()),
        Command::Backfill {
            paths,
            no_recurse,
            dry_run,
            ignore_unknown,
            project,
        } => cmd_backfill(
            cli,
            paths,
            *no_recurse,
            *dry_run,
            *ignore_unknown,
            project.as_deref(),
        ),
        Command::Formats => cmd_formats(),
        Command::Writeback { to, kind, project, all, include_file_facts, force } => cmd_writeback(
            cli,
            to.as_ref(),
            *kind,
            project.as_deref(),
            *all,
            *include_file_facts,
            *force,
        ),
        Command::Shell => crate::shell::run(
            &cli.root,
            &cli.scope,
            &cli.session,
            std::io::stdin().lock(),
            std::io::stdout(),
        ),
        Command::Serve { stdio, http, listen } => {
            if *stdio && *http {
                eprintln!("mem serve accepts either --stdio or --http, not both");
                std::process::exit(2);
            }
            if *stdio {
                crate::mcp::serve_stdio(&cli.root, &cli.scope, &cli.session)
            } else if *http {
                let listen = listen.clone().unwrap_or_else(|| "127.0.0.1:8765".into());
                let server = crate::http_serve::Server::bind(
                    &listen,
                    cli.root.clone(),
                    &cli.scope,
                    &cli.session,
                )?;
                eprintln!("listening {}", server.addr);
                let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                server.serve(shutdown)
            } else {
                eprintln!("mem serve needs --stdio or --http");
                std::process::exit(2);
            }
        }
        Command::Models { action } => cmd_models(cli, action),
        Command::Tidy { apply, keep } => cmd_tidy(cli, *apply, keep),
        Command::Setup { target, remove, no_mcp } => {
            let target = match target {
                SetupTarget::All => crate::setup::Target::All,
                SetupTarget::Claude => crate::setup::Target::Claude,
                SetupTarget::Codex => crate::setup::Target::Codex,
                SetupTarget::Pi => crate::setup::Target::Pi,
                SetupTarget::Omp => crate::setup::Target::Omp,
            };
            let options = crate::setup::Options { remove: *remove, no_mcp: *no_mcp };
            for line in crate::setup::run(target, options)? {
                println!("{line}");
            }
            Ok(())
        }
        Command::Hook { event, plain, cwd, prompt } => {
            let event = match event {
                HookArg::Start => crate::hook::HookEvent::Start,
                HookArg::Prompt => crate::hook::HookEvent::Prompt,
                HookArg::End => crate::hook::HookEvent::End,
                HookArg::Sync => crate::hook::HookEvent::Sync,
            };
            if *plain {
                let payload = serde_json::json!({
                    "cwd": cwd.as_ref().map(|p| p.display().to_string()),
                    "prompt": prompt,
                });
                let bytes = serde_json::to_vec(&payload)?;
                crate::hook::run(event, std::io::Cursor::new(bytes), std::io::stdout().lock(), true);
            } else {
                crate::hook::run(event, std::io::stdin().lock(), std::io::stdout().lock(), false);
            }
            Ok(())
        }
        Command::Op { operation, args } => cmd_op(cli, operation, args.as_deref()),
        Command::Bench {
            queries,
            iterations,
            rerank_iterations,
            project,
            limit,
        } => cmd_bench(cli, queries, *iterations, *rerank_iterations, project.as_deref(), *limit),
    }
}

fn cmd_backfill_local(cli: &Cli, project: Option<&PathBuf>, dry_run: bool) -> Result<()> {
    let project_root = project
        .cloned()
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| Error::InvalidAdapter("no project root given".into()))?;
    let canonical = project_root
        .canonicalize()
        .unwrap_or(project_root.clone());

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::InvalidAdapter("HOME not set".into()))?;
    let claude_root = home.join(".claude").join("projects");
    let claude_archived = home.join(".claude").join("archived_projects");
    let codex_root = home.join(".codex").join("sessions");
    let codex_archived = home.join(".codex").join("archived_sessions");
    let opencode_root = home.join(".local").join("share").join("opencode").join("storage");
    let pi_root = pi_agent_dir(&home).join("sessions");
    let omp_root = omp_agent_dir(&home).join("sessions");

    println!("Project filter: {}", canonical.display());
    println!("Roots:");
    for (label, path) in [
        ("Claude Code", &claude_root),
        ("Claude Code (archived)", &claude_archived),
        ("Codex", &codex_root),
        ("Codex (archived)", &codex_archived),
        ("OpenCode", &opencode_root),
        ("pi", &pi_root),
        ("omp", &omp_root),
    ] {
        let status = if path.exists() { "present" } else { "absent" };
        println!("  [{label}] {status}  {}", path.display());
    }
    println!();

    let journal = crate::journal::BulkWriter::open(&cli.root.join("journal"))?;
    let mut ingested = 0usize;
    let mut skipped = 0usize;

    println!("[Local transcripts] scanning {}", canonical.display());
    let mut local_files = 0usize;
    for path in walk_local_transcripts(&canonical) {
        local_files += 1;
        if dry_run {
            println!("  would ingest: {}", path.display());
            continue;
        }
        match read_local_transcript(&path, &cli.scope, &cli.session) {
            Ok(events) if events.is_empty() => {}
            Ok(mut events) => {
                let n = events.len();
                for event in &mut events {
                    event.set_project_from(&canonical.display().to_string());
                }
                ingested += journal.append_many(events)?;
                println!("  {}: {n} events", path.display());
            }
            Err(err) => {
                eprintln!("  {}: {err}", path.display());
                skipped += 1;
            }
        }
    }
    println!("[Local transcripts] {local_files} jsonl file(s) under the project");

    // Project-memory Markdown files (AGENTS.md, CLAUDE.md, CODEX.md,
    // README.md, etc.) live next to the project. We pick them up by name
    // so the tool doesn't have to walk every file in the tree.
    let canonical_canon = canonical.clone();
    println!("[Project memory files] scanning {}", canonical_canon.display());
    let candidates = [
        "AGENTS.md",
        "CLAUDE.md",
        "CODEX.md",
        "CODEX.local.md",
        "README.md",
        ".github/copilot-instructions.md",
        ".windsurfrules",
    ];
    for name in candidates {
        let path = canonical_canon.join(name);
        if !path.exists() {
            continue;
        }
        println!("[Project memory files] {}: {}", name, path.display());
        if dry_run {
            continue;
        }
        let adapter = crate::ingest::MarkdownAdapter;
        match adapter.read(&path, &cli.scope, &cli.session) {
            Ok(events) => {
                let n = events.len();
                for e in events {
                    if journal.append(e)?.is_some() {
                        ingested += 1;
                    }
                }
                println!("  {}: {} events", path.display(), n);
            }
            Err(err) => {
                eprintln!("  {}: {err}", path.display());
                skipped += 1;
            }
        }
    }

    // Claude Code: scan the project directory whose decoded name matches.
    for root in [&claude_root, &claude_archived] {
        if !root.exists() {
            continue;
        }
        let project_dirs = match std::fs::read_dir(root) {
            Ok(d) => d,
            Err(err) => {
                eprintln!("{}: {err}", root.display());
                continue;
            }
        };
        for entry in project_dirs.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Some(decoded) = decode_claude_project_dir(&dir) else {
                continue;
            };
            // The Claude folder name is lossy (`agensis-agent` decodes as
            // `agensis/agent`). Only an encoded path that exists on disk and
            // sits at or under the project is this project.
            if !decoded.exists() || !paths_match(&decoded, &canonical) {
                continue;
            }
            println!("[Claude Code] project dir: {}", dir.display());
            for jsonl in walk_jsonl_in(&dir) {
                if dry_run {
                    println!("  would ingest: {}", jsonl.display());
                    continue;
                }
                let adapter = crate::ingest::ClaudeAdapter;
                match IngestAdapter::read(&adapter, &jsonl, &cli.scope, &cli.session) {
                    Ok(events) => {
                        let n = events.len();
                        for e in events {
                            if journal.append(e)?.is_some() {
                                ingested += 1;
                            }
                        }
                        println!("  {}: {} events", jsonl.display(), n);
                    }
                    Err(err) => {
                        eprintln!("  {}: {err}", jsonl.display());
                        skipped += 1;
                    }
                }
            }
        }
    }

    // Codex: scan rollout-*.jsonl and filter by session_meta.payload.cwd.
    for root in [&codex_root, &codex_archived] {
        if !root.exists() {
            continue;
        }
        println!("[Codex] scanning {}", root.display());
        let mut seen = 0usize;
        for jsonl in walk_jsonl_in(root) {
            seen += 1;
            if seen.is_multiple_of(200) {
                println!("  [Codex] scanned {seen} rollouts, {ingested} events so far");
            }
            let Some(name) = jsonl.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with("rollout-") {
                continue;
            }
            let cwd = match read_codex_cwd(&jsonl) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if !paths_match(&cwd, &canonical) {
                continue;
            }
            if dry_run {
                println!("  would ingest: {}", jsonl.display());
                continue;
            }
            let adapter = crate::ingest::CodexAdapter;
            match IngestAdapter::read(&adapter, &jsonl, &cli.scope, &cli.session) {
                Ok(events) => {
                    let n = events.len();
                    for e in events {
                        if journal.append(e)?.is_some() {
                            ingested += 1;
                        }
                    }
                    println!("  {}: {} events", jsonl.display(), n);
                }
                Err(err) => {
                    eprintln!("  {}: {err}", jsonl.display());
                    skipped += 1;
                }
            }
        }
    }

    // pi and omp: session files live under `<agent>/sessions/<encoded>/`.
    // The header carries the session's cwd, which is the reliable filter.
    for (label, root) in [("pi", &pi_root), ("omp", &omp_root)] {
        if !root.exists() {
            continue;
        }
        println!("[{label}] scanning {}", root.display());
        for jsonl in walk_session_files(root) {
            let Some(cwd) = crate::ingest::pi_session_cwd(&jsonl) else {
                continue;
            };
            if !paths_match(Path::new(&cwd), &canonical) {
                continue;
            }
            if dry_run {
                println!("  would ingest: {}", jsonl.display());
                continue;
            }
            let adapter = crate::ingest::PiAdapter;
            match IngestAdapter::read(&adapter, &jsonl, &cli.scope, &cli.session) {
                Ok(events) => {
                    let n = events.len();
                    for e in events {
                        if journal.append(e)?.is_some() {
                            ingested += 1;
                        }
                    }
                    println!("  {}: {} events", jsonl.display(), n);
                }
                Err(err) => {
                    eprintln!("  {}: {err}", jsonl.display());
                    skipped += 1;
                }
            }
        }
    }

    // OpenCode: walk the multi-file storage layout and filter by session directory.
    if opencode_root.exists() {
        println!("[OpenCode] scanning {}", opencode_root.display());
        let sessions_root = opencode_root.join("session");
        let messages_root = opencode_root.join("message");
        let parts_root = opencode_root.join("part");
        if sessions_root.exists() {
            for entry in walkdir::WalkDir::new(&sessions_root)
                .max_depth(2)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let raw = match std::fs::read_to_string(entry.path()) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let session: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(sid) = session.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let session_dir = session
                    .get("directory")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from);
                let cwd = session_dir
                    .as_ref()
                    .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()));
                let Some(cwd) = cwd else { continue };
                if !paths_match(&cwd, &canonical) {
                    continue;
                }
                let title = session
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                println!("[OpenCode] session {sid}: {title}");
                if dry_run {
                    continue;
                }
                ingest_opencode_session(&journal, &messages_root, &parts_root, sid, &cli.scope, &cli.session, &canonical, &mut ingested)?;
            }
        }
    }

    println!(
        "\nBackfill complete: {ingested} ingested, {skipped} skipped",
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ingest_opencode_session(
    journal: &crate::journal::BulkWriter,
    messages_root: &Path,
    parts_root: &Path,
    session_id: &str,
    scope_id: &str,
    session_label: &str,
    project: &Path,
    ingested: &mut usize,
) -> Result<()> {
    let msgs_dir = messages_root.join(session_id);
    if !msgs_dir.exists() {
        return Ok(());
    }
    let mut msgs: Vec<(String, String, String)> = Vec::new();
    for entry in std::fs::read_dir(&msgs_dir).map_err(|e| Error::io(&msgs_dir, e))? {
        let entry = entry.map_err(|e| Error::io(&msgs_dir, e))?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(mid) = v.get("id").and_then(|v| v.as_str()) else { continue };
        let Some(role) = v.get("role").and_then(|v| v.as_str()) else { continue };
        if !matches!(role, "user" | "assistant") {
            continue;
        }
        msgs.push((mid.to_string(), role.to_string(), raw));
    }
    for (mid, role, raw) in msgs {
        let parts_dir = parts_root.join(&mid);
        if !parts_dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&parts_dir).map_err(|e| Error::io(&parts_dir, e))? {
            let entry = entry.map_err(|e| Error::io(&parts_dir, e))?;
            if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let part_raw = std::fs::read_to_string(entry.path()).unwrap_or_default();
            let part: serde_json::Value = match serde_json::from_str(&part_raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if part.get("type").and_then(|v| v.as_str()) != Some("text") {
                continue;
            }
            let Some(text) = part.get("text").and_then(|v| v.as_str()) else { continue };
            let cleaned = text.replace('\u{0}', "");
            let trimmed = cleaned.trim();
            if trimmed.is_empty() {
                continue;
            }
            let role_enum = match role.as_str() {
                "user" => crate::journal::Role::User,
                _ => crate::journal::Role::Assistant,
            };
            let mut event = JournalEvent::new(
                scope_id.to_string(),
                session_label.to_string(),
                role_enum,
                crate::journal::Source::IdeHistory {
                    source_id: format!("opencode:{session_id}:{mid}:{}", part_key(&entry.path())),
                    position: 0,
                    occurred_at: None,
                },
                text,
                crate::journal::Redaction::None,
            );
            event.set_project_from(&project.display().to_string());
            if journal.append(event)?.is_some() {
                *ingested += 1;
            }
            let _ = raw;
        }
    }
    Ok(())
}

fn read_codex_cwd(path: &Path) -> Result<PathBuf> {
    let raw = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            if let Some(cwd) = v.get("payload").and_then(|p| p.get("cwd")).and_then(|c| c.as_str()) {
                let pb = PathBuf::from(cwd);
                return Ok(pb.canonicalize().unwrap_or(pb));
            }
        }
    }
    Err(Error::InvalidAdapter("no session_meta cwd in rollout".into()))
}

/// Decode a Claude Code project directory name back to its working directory.
/// The encoded name replaces every `/` with `-`, but the convention is to
/// keep the `-` separators around a leading `/` (so `-private-tmp-...` maps
/// to `/private/tmp/...`). A bare `-` (no path content after the prefix)
/// represents the root `/`, which we skip here because it would otherwise
/// match every project.
fn decode_claude_project_dir(dir: &Path) -> Option<PathBuf> {
    let name = dir.file_name()?.to_str()?;
    let stripped = name.trim_start_matches('-');
    if stripped.is_empty() {
        return None;
    }
    let decoded = format!("/{}", stripped.replace('-', "/"));
    let pb = PathBuf::from(decoded);
    Some(pb.canonicalize().unwrap_or(pb))
}

/// True if `session_cwd` is at or below `project_root`. Used to filter
/// sessions to the project the user actually cares about, rather than
/// every project under a shared ancestor (e.g. `$HOME`).
fn paths_match(session_cwd: &Path, project_root: &Path) -> bool {
    if session_cwd == project_root {
        return true;
    }
    session_cwd.starts_with(project_root)
}

fn walk_local_transcripts(root: &Path) -> Vec<PathBuf> {
    let skip = [".git", "node_modules", "target", "dist", ".next"];
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            e.file_name()
                .to_str()
                .map(|name| {
                    !skip.contains(&name)
                        && !(e.file_type().is_dir() && name.to_ascii_lowercase().contains("subagent"))
                })
                .unwrap_or(true)
        })
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path.to_path_buf());
        }
    }
    out
}

fn read_local_transcript(
    path: &Path,
    scope_id: &str,
    session_id: &str,
) -> Result<Vec<JournalEvent>> {
    let claude = crate::ingest::ClaudeAdapter;
    let events = IngestAdapter::read(&claude, path, scope_id, session_id)?;
    if !events.is_empty() {
        return Ok(events);
    }
    let raw = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let mut out = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = value.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(role, "user" | "assistant") {
            continue;
        }
        let text = value
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }
        let role_enum = if role == "user" {
            crate::journal::Role::User
        } else {
            crate::journal::Role::Assistant
        };
        out.push(JournalEvent::new(
            scope_id.to_string(),
            session_id.to_string(),
            role_enum,
            crate::journal::Source::Chat {
                source_id: format!("local:{}:{i}", path.display()),
                occurred_at: None,
            },
            text.to_string(),
            crate::journal::Redaction::None,
        ));
    }
    Ok(out)
}

fn walk_jsonl_in(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_jsonl(root, &mut out);
    out
}

/// Session files directly under `sessions/<project>/`. pi and omp put the
/// user's transcript there and any advisor/worker sidecars in a sibling
/// directory named after the session, so only this top level is real
/// conversation; the rest is agent-to-agent and the parent already covers it.
fn walk_session_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return out;
    };
    for project in projects.flatten() {
        let dir = project.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn walk_jsonl(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip subagent transcripts; the parent session already covers
            // the agent's work as ordinary messages.
            // Folder names vary by tool (`subagents`, `.pi-subagents`, ...).
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.to_ascii_lowercase().contains("subagent") {
                continue;
            }
            walk_jsonl(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

fn cmd_backfill_all(cli: &Cli, sources: &[String], dry_run: bool) -> Result<()> {
    use rayon::prelude::*;

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::InvalidAdapter("HOME not set".into()))?;
    let claude_roots = [
        home.join(".claude").join("projects"),
        home.join(".claude").join("archived_projects"),
    ];
    let codex_roots = [
        home.join(".codex").join("sessions"),
        home.join(".codex").join("archived_sessions"),
    ];
    let opencode_root = home.join(".local").join("share").join("opencode").join("storage");
    let pi_root = pi_agent_dir(&home).join("sessions");
    let omp_root = omp_agent_dir(&home).join("sessions");

    let want_claude = sources.iter().any(|s| s == "claude");
    let want_codex = sources.iter().any(|s| s == "codex");
    let want_opencode = sources.iter().any(|s| s == "opencode");
    let want_pi = sources.iter().any(|s| s == "pi");
    let want_omp = sources.iter().any(|s| s == "omp");

    // ----- Phase 1: collect every candidate file in a fast sequential walk.
    let mut claude_files: Vec<(String, PathBuf)> = Vec::new();
    if want_claude {
        for root in &claude_roots {
            if !root.exists() {
                continue;
            }
            println!("[Claude Code] scanning {}", root.display());
            for project_dir in std::fs::read_dir(root)
                .map_err(|e| Error::io(root, e))?
                .flatten()
            {
                let dir = project_dir.path();
                if !dir.is_dir() {
                    continue;
                }
                let Some(decoded) = decode_claude_project_dir(&dir) else {
                    continue;
                };
                let project_label = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                println!("  project {} ({})", decoded.display(), project_label);
                for jsonl in walk_jsonl_in(&dir) {
                    claude_files.push((project_label.clone(), jsonl));
                }
            }
        }
    }

    let mut codex_files: Vec<PathBuf> = Vec::new();
    if want_codex {
        for root in &codex_roots {
            if !root.exists() {
                continue;
            }
            println!("[Codex] scanning {}", root.display());
            for jsonl in walk_jsonl_in(root) {
                let Some(name) = jsonl.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !name.starts_with("rollout-") {
                    continue;
                }
                codex_files.push(jsonl);
            }
        }
    }

    let mut pi_files: Vec<PathBuf> = Vec::new();
    if want_pi && pi_root.exists() {
        println!("[pi] scanning {}", pi_root.display());
        pi_files = walk_session_files(&pi_root);
    }
    let mut omp_files: Vec<PathBuf> = Vec::new();
    if want_omp && omp_root.exists() {
        println!("[omp] scanning {}", omp_root.display());
        omp_files = walk_session_files(&omp_root);
    }

    let mut opencode_sessions: Vec<(String, PathBuf)> = Vec::new(); // (sid, session.json path)
    if want_opencode && opencode_root.exists() {
        println!("[OpenCode] scanning {}", opencode_root.display());
        let sessions_root = opencode_root.join("session");
        if sessions_root.exists() {
            for entry in walkdir::WalkDir::new(&sessions_root)
                .max_depth(2)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let raw = match std::fs::read_to_string(entry.path()) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let session: serde_json::Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(sid) = session.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let title = session
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                println!("  session {sid}: {title}");
                opencode_sessions.push((sid.to_string(), entry.path().to_path_buf()));
            }
        }
    }

    if dry_run {
        println!("\nDry run summary:");
        println!("  Claude Code files: {}", claude_files.len());
        println!("  Codex files:       {}", codex_files.len());
        println!("  pi files:          {}", pi_files.len());
        println!("  omp files:         {}", omp_files.len());
        println!("  OpenCode sessions: {}", opencode_sessions.len());
        return Ok(());
    }

    // ----- Phase 2: open the bulk writer. Amortised fsync, in-memory seq,
    // reused day-file handles. This is the single biggest performance win.
    let journal_dir = cli.root.join("journal");
    let writer = crate::journal::BulkWriter::open(&journal_dir)?;
    let started = std::time::Instant::now();
    let mut ingested = 0usize;
    let mut skipped = 0usize;

    if want_claude && !claude_files.is_empty() {
        let session = cli.session.clone();
        let total = claude_files.len();
        let counter = std::sync::atomic::AtomicUsize::new(0);
        type ClaudeOut = (usize, Vec<crate::journal::JournalEvent>); // (count, events)
        let parsed: Vec<Option<ClaudeOut>> = claude_files
            .par_iter()
            .map(|(project, path)| -> Option<ClaudeOut> {
                let adapter = crate::ingest::ClaudeAdapter;
                let _ = project;
                let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if done.is_multiple_of(200) || done == total {
                    eprintln!("  [claude] {done}/{total} files");
                }
                match adapter.read(path, &cli.scope, &session) {
                    Ok(events) => Some((events.len(), events)),
                    Err(err) => {
                        eprintln!("  {}: {err}", path.display());
                        None
                    }
                }
            })
            .collect();
        for item in parsed {
            match item {
                Some((n, events)) => {
                    let _ = n;
                    ingested += writer.append_many(events)?;
                }
                None => skipped += 1,
            }
        }
    }

    if want_codex && !codex_files.is_empty() {
        let total = codex_files.len();
        let counter = std::sync::atomic::AtomicUsize::new(0);
        // Single-pass per file: extract cwd (from session_meta) AND events
        // in one read. Returns (scope_label, events).
        type CodexOut = (String, Vec<crate::journal::JournalEvent>);
        let parsed: Vec<Option<CodexOut>> = codex_files
            .par_iter()
            .map(|path| -> Option<CodexOut> {
                let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if done.is_multiple_of(200) || done == total {
                    eprintln!("  [codex] {done}/{total} files");
                }
                parse_codex_file(path, &cli.scope, &cli.session).ok()
            })
            .collect();
        let mut codex_ingested = 0usize;
        let mut codex_skipped = 0usize;
        for item in parsed {
            match item {
                Some((_label, events)) => {
                    for e in events {
                        if writer.append(e)?.is_some() {
                            codex_ingested += 1;
                        }
                    }
                }
                None => codex_skipped += 1,
            }
        }
        ingested += codex_ingested;
        skipped += codex_skipped;
    }

    let pi_all: Vec<PathBuf> = pi_files.into_iter().chain(omp_files).collect();
    if !pi_all.is_empty() {
        let total = pi_all.len();
        let counter = std::sync::atomic::AtomicUsize::new(0);
        let parsed: Vec<Option<Vec<crate::journal::JournalEvent>>> = pi_all
            .par_iter()
            .map(|path| {
                let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if done.is_multiple_of(200) || done == total {
                    eprintln!("  [pi/omp] {done}/{total} files");
                }
                let adapter = crate::ingest::PiAdapter;
                IngestAdapter::read(&adapter, path, &cli.scope, &cli.session).ok()
            })
            .collect();
        let mut pi_ingested = 0usize;
        let mut pi_skipped = 0usize;
        for item in parsed {
            match item {
                Some(events) => {
                    for e in events {
                        if writer.append(e)?.is_some() {
                            pi_ingested += 1;
                        }
                    }
                }
                None => pi_skipped += 1,
            }
        }
        ingested += pi_ingested;
        skipped += pi_skipped;
    }

    if want_opencode && !opencode_sessions.is_empty() {
        let messages_root = opencode_root.join("message");
        let parts_root = opencode_root.join("part");
        let total = opencode_sessions.len();
        let counter = std::sync::atomic::AtomicUsize::new(0);
        type OcOut = Vec<crate::journal::JournalEvent>;
        let parsed: Vec<Option<OcOut>> = opencode_sessions
            .par_iter()
            .map(|(sid, session_path)| -> Option<OcOut> {
                let done = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if done.is_multiple_of(100) || done == total {
                    eprintln!("  [opencode] {done}/{total} sessions");
                }
                let raw = std::fs::read_to_string(session_path).ok()?;
                let session: serde_json::Value = serde_json::from_str(&raw).ok()?;
                let session_cwd = session
                    .get("directory")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from);
                let mut events = opencode_session_events(
                    &messages_root,
                    &parts_root,
                    sid,
                    &cli.scope,
                    &cli.session,
                );
                if let Some(cwd) = &session_cwd {
                    for event in &mut events {
                        event.set_project_from(&cwd.display().to_string());
                    }
                }
                if events.is_empty() {
                    None
                } else {
                    Some(events)
                }
            })
            .collect();
        let mut oc_ingested = 0usize;
        let mut oc_skipped = 0usize;
        for item in parsed {
            match item {
                Some(events) => {
                    for e in events {
                        if writer.append(e)?.is_some() {
                            oc_ingested += 1;
                        }
                    }
                }
                None => oc_skipped += 1,
            }
        }
        ingested += oc_ingested;
        skipped += oc_skipped;
    }

    writer.flush()?;
    let elapsed = started.elapsed();
    eprintln!(
        "\nBackfill complete: {ingested} new event(s) in {:.2}s; {skipped} file(s) or session(s) had no messages or could not be read",
        elapsed.as_secs_f64()
    );
    Ok(())
}

/// Stable per-part key for an OpenCode message part file (its file stem).
fn part_key(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("part").to_string()
}

/// Single-pass Codex file read: returns `(scope_label, events)`. Reads the
/// rollout JSONL once, extracts the cwd from the first `session_meta` line,
/// then continues parsing message events from the same BufReader.
fn parse_codex_file(path: &Path, scope_id: &str, session_id: &str) -> Result<(String, Vec<crate::journal::JournalEvent>)> {
    use std::fs::File;
    use std::io::{BufRead, BufReader};

    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    let reader = BufReader::with_capacity(64 * 1024, file);
    let mut events = Vec::new();
    let mut session_uuid: Option<String> = None;
        // Forked rollouts reuse the parent's session id, so the rollout file
        // name is part of each message id.
        let rollout = path.file_stem().and_then(|s| s.to_str()).unwrap_or("rollout").to_string();
    let mut cwd: Option<PathBuf> = None;
    for (line_no, line) in reader.lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match value.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") => {
                // Subagent rollouts hold prompts one agent wrote to another, not
                // the user's words. Claude subagent transcripts are skipped the same way.
                if crate::ingest::is_codex_subagent(&value) {
                    return Ok((String::new(), Vec::new()));
                }
                session_uuid = value
                    .get("payload")
                    .and_then(|p| p.get("id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                cwd = value
                    .get("payload")
                    .and_then(|p| p.get("cwd"))
                    .and_then(|c| c.as_str())
                    .map(PathBuf::from);
            }
            Some("response_item") => {
                let Some(payload) = value.get("payload") else { continue };
                if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
                    continue;
                }
                let Some(role_str) = payload.get("role").and_then(|v| v.as_str()) else {
                    continue;
                };
                let role_enum = match role_str {
                    "user" => crate::journal::Role::User,
                    "assistant" => crate::journal::Role::Assistant,
                    _ => continue,
                };
                if role_enum == crate::journal::Role::Assistant {
                    if let Some(channel) = payload.get("channel").and_then(|v| v.as_str()) {
                        if channel != "final" {
                            continue;
                        }
                    }
                }
                let mut text = crate::ingest::extract_text_parts(payload.get("content"));
                if role_enum == crate::journal::Role::User
                    && text.contains("</environment_context>")
                {
                    if let Some(pos) = text.rfind("</environment_context>") {
                        text = text[pos + "</environment_context>".len()..].to_string();
                    }
                }
                if text.trim().is_empty() {
                    continue;
                }
                let occurred_at = value
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&chrono::Utc));
                let sid = session_uuid.clone().unwrap_or_else(|| session_id.to_string());
                let mut event = crate::journal::JournalEvent::new(
                    scope_id.to_string(),
                    session_id.to_string(),
                    role_enum,
                    crate::journal::Source::Chat {
                        source_id: format!("codex:{sid}:{rollout}:line{line_no}"),
                        occurred_at,
                    },
                    text,
                    crate::journal::Redaction::None,
                );
                if let Some(cwd) = &cwd {
                    event.set_project_from(&cwd.display().to_string());
                }
                events.push(event);
            }
            _ => {}
        }
    }
    let scope_label = match cwd {
        Some(c) => format!("codex:{}", c.display()),
        None => "codex:unknown".to_string(),
    };
    Ok((scope_label, events))
}

/// Returns all journal events for one OpenCode session by reading its
/// message and part files. Parallel-safe (no shared state).
fn opencode_session_events(
    messages_root: &Path,
    parts_root: &Path,
    session_id: &str,
    scope_id: &str,
    session_label: &str,
) -> Vec<crate::journal::JournalEvent> {
    let msgs_dir = messages_root.join(session_id);
    if !msgs_dir.exists() {
        return Vec::new();
    }
    let mut events = Vec::new();
    let entries = match std::fs::read_dir(&msgs_dir) {
        Ok(e) => e,
        Err(_) => return events,
    };
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let raw = match std::fs::read_to_string(entry.path()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(mid) = v.get("id").and_then(|v| v.as_str()) else { continue };
        let Some(role) = v.get("role").and_then(|v| v.as_str()) else { continue };
        if !matches!(role, "user" | "assistant") {
            continue;
        }
        let parts_dir = parts_root.join(mid);
        if !parts_dir.exists() {
            continue;
        }
        let part_entries = match std::fs::read_dir(&parts_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for part_entry in part_entries.flatten() {
            if part_entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let part_raw = match std::fs::read_to_string(part_entry.path()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let part: serde_json::Value = match serde_json::from_str(&part_raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if part.get("type").and_then(|v| v.as_str()) != Some("text") {
                continue;
            }
            let Some(text) = part.get("text").and_then(|v| v.as_str()) else { continue };
            let cleaned = text.replace('\u{0}', "");
            let trimmed = cleaned.trim();
            if trimmed.is_empty() {
                continue;
            }
            let role_enum = match role {
                "user" => crate::journal::Role::User,
                _ => crate::journal::Role::Assistant,
            };
            events.push(crate::journal::JournalEvent::new(
                scope_id.to_string(),
                session_label.to_string(),
                role_enum,
                crate::journal::Source::IdeHistory {
                    source_id: format!("opencode:{session_id}:{mid}:{}", part_key(&part_entry.path())),
                    position: 0,
                    occurred_at: None,
                },
                text.to_string(),
                crate::journal::Redaction::None,
            ));
        }
    }
    events
}

fn cmd_formats() -> Result<()> {
    println!("Source formats accepted by `mem ingest` and `mem backfill`:");
    println!("  .jsonl           claude_code       Claude Code transcripts (detected by content)");
    println!("  .jsonl           codex             Codex rollouts (detected by content)");
    println!("  .jsonl           pi                pi / omp sessions (detected by content)");
    println!("  .jsonl / .json   chat_export       one JSON object per line");
    println!("                                     fields: id, role, text, ts?, redaction?");
    println!("  .ics             calendar          iCalendar (RFC 5545) feeds");
    println!("  .vtt / .srt      voice             WebVTT / SubRip transcripts");
    println!("                                     (each cue becomes a journal event)");
    println!("  *.ide.json       ide_history       {{\"events\":[...]}} exports");
    println!("  *.md             markdown           project-memory files:");
    println!("                                     AGENTS.md, CLAUDE.md, CODEX.md, README.md,");
    println!("                                     copilot-instructions.md, .windsurfrules, etc.");
    println!();
    println!("Recursive directory traversal is on by default. Pass --no-recurse to");
    println!("restrict backfill to the given path's own files.");
    println!();
    println!("Discovered automatically by `mem backfill-local` and `mem backfill-all`:");
    println!("  ~/.claude/projects, ~/.claude/archived_projects          Claude Code");
    println!("  ~/.codex/sessions, ~/.codex/archived_sessions            Codex");
    println!("  ~/.local/share/opencode/storage                          OpenCode");
    println!("  ~/.pi/agent/sessions                                     pi");
    println!("  ~/.omp/agent/sessions                                    omp");
    println!();
    println!("Overrides: MEM_PI_AGENT_DIR / PI_CODING_AGENT_DIR for pi,");
    println!("           MEM_OMP_AGENT_DIR for omp (the folder above `sessions/`).");
    Ok(())
}

fn cmd_writeback(
    cli: &Cli,
    to: Option<&PathBuf>,
    kind: WritebackKindArg,
    project: Option<&str>,
    all: bool,
    include_file_facts: bool,
    force: bool,
) -> Result<()> {
    let target = match to.cloned() {
        Some(p) => p,
        None => match std::env::current_dir() {
            Ok(cwd) => cwd.join("AGENTS.md"),
            Err(err) => return Err(Error::io("cwd", err)),
        },
    };
    // An existing file keeps its hand-written content; only the managed
    // block between the markers is replaced. `--force` replaces the whole file.
    let existing = if target.exists() && !force {
        Some(std::fs::read_to_string(&target).map_err(|e| Error::io(&target, e))?)
    } else {
        None
    };
    // A local store is one project's memory: everything in it belongs. A
    // shared store (global, MEM_ROOT, --root) holds many projects, so a
    // project's file gets only its own entity (and spill files) plus the
    // user's general preferences.
    let all = all || (project.is_none() && cli.store == StoreKind::Local);
    let project = match project {
        Some(name) => name.to_string(),
        None => target
            .canonicalize()
            .unwrap_or_else(|_| target.clone())
            .parent()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
    };
    let project_id = format!("w_{}", crate::consolidate::slug(&project));
    let repo = GitRepo::open(cli.root.join("memory.git"))?;
    let head = repo.head()?;
    let entities = repo.list_eligible_entity_files(&head)?;
    let mut sections: Vec<(String, Vec<WritebackRow>)> = Vec::new();
    for (path, blob) in entities {
        let entity = match EntityFile::parse(&blob.content) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entity.facts.is_empty() {
            continue;
        }
        if !all && !crate::ops::project_entity_includes(&entity.id, &project_id) {
            continue;
        }
        let bucket = bucket_for(&path);
        let mut active = Vec::new();
        for fact in &entity.facts {
            if matches!(
                fact.status,
                crate::fact::FactStatus::Superseded
                    | crate::fact::FactStatus::Retracted
                    | crate::fact::FactStatus::Expired
            ) {
                continue;
            }
            if !fact.visibility.allows(Visibility::Private) {
                continue;
            }
            // Facts that only restate AGENTS.md/CLAUDE.md/README.md are
            // already in files agents read; writing them back duplicates them.
            let from_files_only = !fact.sources.is_empty()
                && fact.sources.iter().all(|s| s.event_id.starts_with("evt_ide_md:"));
            if from_files_only && !include_file_facts {
                continue;
            }
            active.push((fact.id.clone(), fact.statement.clone(), fact.visibility));
        }
        if !active.is_empty() {
            sections.push((bucket, active));
        }
    }

    let mut out = String::new();
    out.push_str(&format!("# {} memory\n\n", kind.heading()));
    out.push_str(&format!(
        "_Generated by `mem writeback` at {} from commit {}._\n\n",
        chrono::Utc::now().to_rfc3339(),
        &head[..10.min(head.len())]
    ));
    // Project knowledge first; the user's general preferences last.
    // Headings are merged after humanising, so two buckets that render as
    // the same heading become one section.
    let merged = merge_writeback_sections(sections);
    if merged.is_empty() {
        out.push_str("> No active facts yet. Run `mem change remember` or `mem consolidate` first.\n");
    } else {
        for (bucket, facts) in &merged {
            out.push_str(&format!("## {bucket}\n\n"));
            for (id, statement, visibility) in facts {
                let vis_tag = match visibility {
                    Visibility::Private => String::new(),
                    Visibility::Shared => " _(shared)_".into(),
                    Visibility::Public => " _(public)_".into(),
                };
                out.push_str(&format!("- [{id}] {statement}{vis_tag}\n"));
            }
            out.push('\n');
        }
    }

    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
    }
    let out = match existing {
        Some(current) => splice_managed_block(&current, &out),
        None => format!("{WRITEBACK_BEGIN}\n{out}{WRITEBACK_END}\n"),
    };
    std::fs::write(&target, &out).map_err(|e| Error::io(&target, e))?;
    println!(
        "Wrote {} active fact(s) to {}",
        merged.iter().map(|(_, f)| f.len()).sum::<usize>(),
        target.display()
    );
    Ok(())
}

const WRITEBACK_BEGIN: &str = "<!-- mem:begin (generated by `mem writeback`; edits inside are replaced) -->";
const WRITEBACK_END: &str = "<!-- mem:end -->";

/// Replace the managed block in `current` with `body`, or append a new block
/// when the file has none. Content outside the markers is left untouched.
pub(crate) fn splice_managed_block(current: &str, body: &str) -> String {
    let block = format!("{WRITEBACK_BEGIN}\n{body}{WRITEBACK_END}");
    let begin = current.find("<!-- mem:begin");
    let end = current.find(WRITEBACK_END);
    match (begin, end) {
        (Some(b), Some(e)) if e > b => {
            let tail = &current[e + WRITEBACK_END.len()..];
            format!("{}{}{}", &current[..b], block, tail)
        }
        _ => {
            let sep = if current.is_empty() || current.ends_with("\n\n") {
                ""
            } else if current.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{current}{sep}{block}\n")
        }
    }
}

fn bucket_for(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("memory/") {
        rest.split('/').next().unwrap_or("misc").to_string()
    } else if let Some(rest) = path.strip_prefix("sessions/") {
        rest.split('/').next().unwrap_or("misc").to_string()
    } else {
        path.trim_end_matches(".md").to_string()
    }
}

type WritebackRow = (String, String, Visibility);

/// One section per heading. Buckets that humanise to the same heading
/// (for example `team_notes` and `team notes`) share a section.
fn merge_writeback_sections(mut sections: Vec<(String, Vec<WritebackRow>)>) -> Vec<(String, Vec<WritebackRow>)> {
    let mut merged: Vec<(String, Vec<WritebackRow>)> = Vec::new();
    sections.sort_by_key(|(bucket, _)| bucket_order(bucket));
    for (bucket, facts) in sections {
        let heading = humanise_bucket(&bucket);
        match merged.iter_mut().find(|(h, _)| *h == heading) {
            Some((_, rows)) => rows.extend(facts),
            None => merged.push((heading, facts)),
        }
    }
    merged
}

fn bucket_order(bucket: &str) -> u8 {
    match bucket {
        "workstreams" => 0,
        "people" => 1,
        "orgs" => 2,
        "conversations" => 3,
        "preferences" => 5,
        _ => 4,
    }
}

fn humanise_bucket(bucket: &str) -> String {
    match bucket {
        "workstreams" => "Project".to_string(),
        "people" => "People".to_string(),
        "orgs" => "Organisations".to_string(),
        "preferences" => "Preferences".to_string(),
        "conversations" => "Conversations".to_string(),
        other => other.replace('_', " "),
    }
}

fn cmd_backfill(
    cli: &Cli,
    paths: &[PathBuf],
    no_recurse: bool,
    dry_run: bool,
    ignore_unknown: bool,
    project: Option<&Path>,
) -> Result<()> {
    let files = collect_backfill_files(paths, no_recurse)?;
    if files.is_empty() {
        eprintln!("No supported files found under the given paths.");
        eprintln!("Run `mem formats` for the accepted source formats.");
        return Err(Error::InvalidAdapter(
            "no supported files to backfill".into(),
        ));
    }

    // Group by adapter so we can show what each file is routed to.
    let mut plan: Vec<(&'static str, &PathBuf)> = Vec::new();
    let mut skipped_unknown = 0usize;
    for path in &files {
        match adapter_for(path) {
            Ok(adapter) => plan.push((adapter.name(), path)),
            Err(_) => {
                if ignore_unknown {
                    skipped_unknown += 1;
                } else {
                    return Err(Error::InvalidAdapter(format!(
                        "{}: no adapter for this file (use --ignore-unknown to skip)",
                        path.display()
                    )));
                }
            }
        }
    }
    if plan.is_empty() {
        eprintln!(
            "Nothing to ingest after filtering unknown formats ({} skipped).",
            skipped_unknown
        );
        return Ok(());
    }

    println!("Backfill plan:");
    for (adapter, path) in &plan {
        println!("  [{}]  {}", adapter, path.display());
    }
    if skipped_unknown > 0 {
        println!("  (skipped {skipped_unknown} unknown file(s))");
    }

    if dry_run {
        println!("Dry run; nothing written.");
        return Ok(());
    }

    let journal = crate::journal::BulkWriter::open(&cli.root.join("journal"))?;
    let mut ingested = 0usize;
    let mut skipped = 0usize;
    for (adapter_name, path) in &plan {
        let adapter = match adapter_for(path) {
            Ok(adapter) => adapter,
            Err(err) => {
                eprintln!("{}: {err}", path.display());
                skipped += 1;
                continue;
            }
        };
        match adapter.read(path, &cli.scope, &cli.session) {
            Ok(mut events) => {
                tag_project(&mut events, project);
                let n = events.len();
                for event in events {
                    if journal.append(event)?.is_some() {
                        ingested += 1;
                    }
                }
                println!("{}: ingested {} event(s) via {adapter_name}", path.display(), n);
            }
            Err(err) => {
                eprintln!("{}: failed via {adapter_name}: {err}", path.display());
                skipped += 1;
            }
        }
    }
    journal.flush()?;
    println!(
        "Backfill complete: {} ingested, {} skipped, {} total files",
        ingested,
        skipped,
        plan.len()
    );
    Ok(())
}

fn collect_backfill_files(paths: &[PathBuf], no_recurse: bool) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for path in paths {
        if !path.exists() {
            return Err(Error::InvalidAdapter(format!(
                "path does not exist: {}",
                path.display()
            )));
        }
        if path.is_file() {
            out.push(path.clone());
            continue;
        }
        if !path.is_dir() {
            return Err(Error::InvalidAdapter(format!(
                "not a file or directory: {}",
                path.display()
            )));
        }
        collect_into(&mut out, path, no_recurse)?;
    }
    out.sort();
    out.dedup();
    Ok(out)
}

fn collect_into(out: &mut Vec<PathBuf>, dir: &Path, no_recurse: bool) -> Result<()> {
    for entry in std::fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
        let entry = entry.map_err(|e| Error::io(dir, e))?;
        let path = entry.path();
        if path.is_file() {
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_lowercase());
            match ext.as_deref() {
                Some("jsonl")
                | Some("json")
                | Some("ics")
                | Some("vtt")
                | Some("srt")
                | Some("md")
                | Some("markdown") => out.push(path),
                _ => {}
            }
        } else if path.is_dir() && !no_recurse {
            // Skip vendored or build outputs that are almost never useful.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(
                name,
                ".git"
                    | "node_modules"
                    | "target"
                    | "dist"
                    | "build"
                    | ".next"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
                    | ".cache"
                    | ".claude"
                    | ".codex"
            ) {
                continue;
            }
            collect_into(out, &path, no_recurse)?;
        }
    }
    Ok(())
}

/// Keep a local store out of commits: add it to the repository's
/// `.git/info/exclude` (not the tracked .gitignore). Best effort.
fn exclude_from_git(store: &Path) {
    let Some(project) = store.parent() else { return };
    let git_dir = project.join(".git");
    if !git_dir.is_dir() {
        return;
    }
    let Some(name) = store.file_name().and_then(|n| n.to_str()) else { return };
    let entry = format!("/{name}/");
    let exclude = git_dir.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == entry) {
        return;
    }
    let _ = std::fs::create_dir_all(git_dir.join("info"));
    let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
    let _ = std::fs::write(&exclude, format!("{current}{sep}{entry}\n"));
}

fn cmd_init(root: &PathBuf, store: StoreKind) -> Result<()> {
    if root.join("memory.git").exists() {
        println!("{} store already initialised at {}", store.label(), root.display());
        return Ok(());
    }
    GitRepo::create(root)?;
    let journal_dir = root.join("journal");
    std::fs::create_dir_all(&journal_dir).map_err(|e| crate::error::Error::io(&journal_dir, e))?;
    let intents_dir = root.join("intents");
    std::fs::create_dir_all(&intents_dir).map_err(|e| crate::error::Error::io(&intents_dir, e))?;
    if store == StoreKind::Local {
        exclude_from_git(root);
    }
    println!("Initialised {} store at {}", store.label(), root.display());
    Ok(())
}

/// Run one MCP operation (the hidden `mem op` command) and print its JSON
/// payload. The pi/omp extension calls this so its native tools behave
/// exactly like the MCP tools: same operations, same arguments, same result.
fn cmd_op(cli: &Cli, operation: &str, args: Option<&str>) -> Result<()> {
    let raw = match args {
        Some(a) => a.to_string(),
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| Error::io("stdin", e))?;
            buf
        }
    };
    let args: serde_json::Value = if raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(raw.trim())
            .map_err(|e| Error::Usage(format!("arguments are not valid JSON: {e}")))?
    };
    let result = crate::ops::execute(&cli.root, &cli.scope, &cli.session, operation, &args);
    println!("{}", result.payload);
    if let Some(err) = result.error {
        return Err(Error::Usage(err));
    }
    if result.not_found || result.conflict {
        std::process::exit(1);
    }
    Ok(())
}

/// pi's agent directory: `$MEM_PI_AGENT_DIR`, else `$PI_CODING_AGENT_DIR`,
/// else `~/.pi/agent`. Sessions live in its `sessions/` subdirectory.
fn pi_agent_dir(home: &Path) -> PathBuf {
    for key in ["MEM_PI_AGENT_DIR", "PI_CODING_AGENT_DIR"] {
        if let Some(v) = std::env::var_os(key).filter(|v| !v.is_empty()) {
            return PathBuf::from(v);
        }
    }
    home.join(".pi").join("agent")
}

/// omp's agent directory: `$MEM_OMP_AGENT_DIR`, else `~/.omp/agent`.
fn omp_agent_dir(home: &Path) -> PathBuf {
    if let Some(v) = std::env::var_os("MEM_OMP_AGENT_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    home.join(".omp").join("agent")
}

/// Tag events that do not name a project with the one given on the command line.
fn tag_project(events: &mut [JournalEvent], project: Option<&Path>) {
    if let Some(project) = project {
        let root = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
        for event in events {
            event.set_project_from(&root.display().to_string());
        }
    }
}

fn cmd_ingest(cli: &Cli, files: &[PathBuf], format: Option<Adapter>, project: Option<&Path>) -> Result<()> {
    let journal_dir = cli.root.join("journal");
    let journal = crate::journal::BulkWriter::open(&journal_dir)?;
    let mut ingested = 0;
    for path in files {
        let adapter: Box<dyn crate::ingest::Adapter> = match format {
            Some(Adapter::Chat) => Box::new(ChatExportAdapter),
            Some(Adapter::Claude) => Box::new(ClaudeAdapter),
            Some(Adapter::Codex) => Box::new(CodexAdapter),
            Some(Adapter::Pi) => Box::new(PiAdapter),
            Some(Adapter::Markdown) => Box::new(crate::ingest::MarkdownAdapter),
            Some(Adapter::Calendar) => Box::new(crate::ingest::CalendarAdapter),
            Some(Adapter::Voice) => Box::new(crate::ingest::VoiceAdapter),
            Some(Adapter::IdeHistory) => Box::new(crate::ingest::IdeHistoryAdapter),
            None => adapter_for(path)?,
        };
        let mut events = adapter.read(path, &cli.scope, &cli.session)?;
        tag_project(&mut events, project);
        for event in events {
            if journal.append(event)?.is_some() {
                ingested += 1;
            }
        }
        println!("{}: ingested via {}", path.display(), adapter.name());
    }
    journal.flush()?;
    println!("{ingested} new events appended (repeats skipped)");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_change(
    cli: &Cli,
    kind: ChangeKind,
    entity: &str,
    fact_id: Option<&str>,
    target: Option<&str>,
    statement: Option<&str>,
    predicate: Option<&str>,
    request_id: Option<&str>,
    reason: Option<&str>,
) -> Result<()> {
    let repo = GitRepo::open(cli.root.join("memory.git"))?;
    let req_id = request_id
        .map(String::from)
        .unwrap_or_else(|| format!("cli_{}", chrono::Utc::now().timestamp_millis()));
    validate_request_id(&req_id)?;
    let now = chrono::Utc::now();
    let fact = match kind {
        ChangeKind::Remember | ChangeKind::Correct => Some(Fact {
            id: fact_id
                .map(String::from)
                .unwrap_or_else(|| format!("fact_{}", now.timestamp_millis())),
            predicate: predicate.unwrap_or("note").to_string(),
            statement: statement.unwrap_or("(empty)").to_string(),
            kind: FactKind::ExplicitAssertion,
            status: FactStatus::Active,
            observed_at: now,
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            expires_at: None,
            review_after: None,
            supersedes: vec![],
            sources: vec![SourceRef {
                event_id: format!("cli_{now}"),
                role: FactRole::User,
                evidence: statement.unwrap_or("").to_string(),
            }],
            visibility: Visibility::Private,
        }),
        ChangeKind::Forget => None,
    };
    let request = ChangeRequest {
        kind,
        request_id: req_id,
        scope_id: cli.scope.clone(),
        session_id: cli.session.clone(),
        source_event_id: format!("cli_{}", now.timestamp_millis()),
        entity_id: entity.to_string(),
        fact,
        target_fact_id: target.map(String::from),
        reason: reason.map(String::from),
        proposed_at: now,
    };
    for recovered in change::recover_in_dir(&repo)? {
        eprintln!("recovered interrupted change: {}", serde_json::to_string(&recovered)?);
    }
    let outcome = change::apply(&repo, request)?;
    println!(
        "{} on {} -> revision {} (replayed={})",
        match kind {
            ChangeKind::Remember => "remember",
            ChangeKind::Correct => "correct",
            ChangeKind::Forget => "forget",
        },
        outcome.target_id,
        outcome.revision,
        outcome.replayed
    );
    Ok(())
}

fn cmd_consolidate(
    cli: &Cli,
    extractor: ExtractorKind,
    high_water: u64,
    request_id: Option<&str>,
) -> Result<()> {
    let repo = GitRepo::open(cli.root.join("memory.git"))?;
    let journal = Journal::open(cli.root.join("journal"))?;
    // A given --request-id names one batch, so a retry replays it. Without
    // one, each batch gets a fresh id and the loop runs until caught up.
    let stamp = chrono::Utc::now().timestamp_millis();
    let mut batch = 0usize;
    let mut totals = (0usize, 0usize, 0usize);
    loop {
        batch += 1;
        let batch_id = match request_id {
            Some(id) => id.to_string(),
            None => format!("consol_{stamp}_{batch}"),
        };
        let options = ConsolidateOptions {
            extractor,
            request_id: batch_id,
            high_water,
            scope_id: Some(cli.scope.clone()),
        };
        let report = Consolidator::run_batch(&repo, &journal, &options)?;
        if report.caught_up {
            println!(
                "Consolidated: {} event(s), {} with facts, {} new fact(s); head {}",
                totals.0, totals.1, totals.2, report.revision
            );
            return Ok(());
        }
        totals.0 += report.events;
        totals.1 += report.accepted;
        totals.2 += report.facts;
        eprintln!(
            "[consolidate] batch {batch} ({:?}): through seq {}, {} event(s), {} new fact(s), {} entit(ies) changed",
            report.extractor, report.through_seq, report.events, report.facts, report.entities_changed
        );
        if request_id.is_some() {
            println!("Consolidated to {}", report.revision);
            return Ok(());
        }
    }
}

/// Judge an existing shortlist with JEV. The journal scan has already run.
#[allow(clippy::too_many_arguments)]
fn cmd_search(
    cli: &Cli,
    query: &str,
    limit: usize,
    include_disputed: bool,
    scope_filter: Option<&str>,
    source_filter: &[String],
    project: Option<&str>,
    no_rerank: bool,
    facts_only: bool,
    snippet_words: Option<usize>,
    source_lines: Option<usize>,
) -> Result<()> {
    let found = crate::ops::combined_search(
        &cli.root,
        &crate::ops::SearchParams {
            query,
            limit,
            rerank: !no_rerank,
            include_disputed,
            scope_filter,
            source_filter,
            project,
            facts_only,
        },
    )?;
    emit_search_progress(cli, found.scanned, found.hits.len());
    let (ranker_name, judged, hits, revision) = (found.ranker, found.judged, found.hits, found.revision);
    let _ = scope_filter;

    // Optional snippet + source-excerpt enrichment. Both are off by default
    // so the default output shape is unchanged.
    let snippet_n = snippet_words.unwrap_or(0);
    let source_n = source_lines.unwrap_or(0);
    let tokens: Vec<String> = if snippet_n > 0 {
        crate::search::lexical::tokenize(query)
    } else {
        Vec::new()
    };
    // Load the journal once if any hit needs source excerpts.
    let journal_index: Option<std::collections::HashMap<String, String>> = if source_n > 0 {
        let j = Journal::open(cli.root.join("journal"))?;
        let mut map = std::collections::HashMap::new();
        let iter = j.iter()?;
        for ev in iter.flatten() {
            map.insert(ev.event_id.clone(), ev.content);
        }
        Some(map)
    } else {
        None
    };

    let mut hits = hits;
    for hit in hits.iter_mut() {
        if snippet_n > 0 {
            hit.snippet = Some(crate::search::lexical::snippet_around_terms(
                &hit.statement,
                &tokens,
                snippet_n,
            ));
        }
        if source_n > 0 {
            if let Some(map) = journal_index.as_ref() {
                let mut excerpts = Vec::with_capacity(hit.sources.len());
                for sid in &hit.sources {
                    if let Some(text) = map.get(sid) {
                        excerpts.push(crate::search::lexical::SourceExcerpt {
                            event_id: sid.clone(),
                            text: crate::search::lexical::excerpt_lines(text, source_n),
                        });
                    } else {
                        excerpts.push(crate::search::lexical::SourceExcerpt {
                            event_id: sid.clone(),
                            text: "(source event not found in journal)".to_string(),
                        });
                    }
                }
                hit.source_excerpts = Some(excerpts);
            }
        }
    }

    let payload = serde_json::json!({
        "revision": revision,
        "ranker": ranker_name,
        "judged": judged,
        "hits": hits,
    });
    emit_search_result(cli, &payload)?;
    Ok(())
}

fn emit_search_progress(cli: &Cli, scanned: u64, candidates: usize) {
    if cli.output_format == OutputFormat::StreamJson {
        let line = serde_json::json!({
            "type": "progress",
            "stage": "scan",
            "scanned": scanned,
            "candidates": candidates,
        });
        println!("{}", serde_json::to_string(&line).unwrap_or_default());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        return;
    }
    eprintln!("[search] scanned {scanned} journal event(s); {candidates} candidate(s)");
}

fn emit_search_result(cli: &Cli, payload: &serde_json::Value) -> Result<()> {
    match cli.output_format {
        OutputFormat::Text => {
            let hits = payload
                .get("hits")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if hits.is_empty() {
                println!("(no hits)");
            }
            for (i, hit) in hits.iter().enumerate() {
                let score = hit.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let statement = hit
                    .get("statement")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .replace('\n', " ");
                println!("{}. {score:.3} {statement}", i + 1);
            }
        }
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(payload)?);
        }
        OutputFormat::StreamJson => {
            let mut line = serde_json::json!({"type": "result"});
            if let Some(obj) = line.as_object_mut() {
                if let Some(fields) = payload.as_object() {
                    for (k, v) in fields {
                        obj.insert(k.clone(), v.clone());
                    }
                }
            }
            println!("{}", serde_json::to_string(&line)?);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_tidy(cli: &Cli, apply: bool, keep: &[String]) -> Result<()> {
    let repo = GitRepo::open(cli.root.join("memory.git"))?;
    let report = crate::tidy::tidy(&repo, apply, keep)?;
    if cli.output_format != OutputFormat::Text {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let dups: Vec<_> = report.findings.iter().filter(|f| f.kind == "duplicate").collect();
    let session: Vec<_> = report.findings.iter().filter(|f| f.kind == "session_only").collect();
    println!("Checked {} facts.", report.facts_checked);
    if !report.checked_session_only {
        println!("(No LLM key configured: only duplicates were checked.)");
    }
    println!("\nSession-only ({}):", session.len());
    for f in &session {
        println!("  [{}] {}", f.fact_id, f.statement);
    }
    println!("\nDuplicates ({}):", dups.len());
    for f in &dups {
        println!("  [{}] {}  (same as {})", f.fact_id, f.statement, f.duplicate_of.as_deref().unwrap_or(""));
    }
    match &report.applied_at {
        Some(rev) => println!("\nForgot {} fact(s) at {rev}.", report.findings.len()),
        None if report.findings.is_empty() => println!("\nNothing to tidy."),
        None => println!(
            "\nNothing changed. Run `mem tidy --apply` to forget these {} fact(s) (add `--keep id,id` for any to leave).",
            report.findings.len()
        ),
    }
    Ok(())
}

fn cmd_models(cli: &Cli, action: &ModelsAction) -> Result<()> {
    use crate::search::laya::{LayaReranker, ModelKind};
    let dir = cli.root.join(".models");
    match action {
        ModelsAction::Pull { model } => {
            let kind = ModelKind::parse(model).ok_or_else(|| {
                Error::Usage(format!(
                    "unknown model {model}; use bge-reranker-base, jina, or bge-v2-m3"
                ))
            })?;
            LayaReranker::pull(&dir, kind)?;
            println!("{} downloaded to {}; search uses it when JEV is not configured", kind.name(), dir.display());
        }
        ModelsAction::Status => {
            let kind = LayaReranker::selected_model(&dir);
            let present = crate::search::laya::try_load_cached(&dir).is_some();
            println!("local reranker: {} ({})", kind.name(), if present { "downloaded" } else { "not downloaded; run `mem models pull`" });
            let jev = crate::jev::JevReranker::from_env().is_ok();
            println!("jev: {}", if jev { "configured (used first)" } else { "not configured" });
        }
    }
    Ok(())
}

fn cmd_bench(
    cli: &Cli,
    queries: &[String],
    iterations: usize,
    rerank_iterations: usize,
    project: Option<&str>,
    limit: usize,
) -> Result<()> {
    use std::time::Instant;

    let time = |query: &str, rerank: bool, runs: usize| -> Result<(Vec<f64>, &'static str, usize)> {
        let mut ms = Vec::with_capacity(runs);
        let mut ranker = "lexical";
        let mut hits = 0;
        for _ in 0..runs.max(1) {
            let start = Instant::now();
            let found = crate::ops::combined_search(
                &cli.root,
                &crate::ops::SearchParams { query, limit, rerank, project, ..Default::default() },
            )?;
            ms.push(start.elapsed().as_secs_f64() * 1000.0);
            ranker = found.ranker;
            hits = found.hits.len();
        }
        Ok((ms, ranker, hits))
    };

    println!("memory_root: {}", cli.root.display());
    println!("{:<40} {:>9} {:>9} {:>9}  {:<8} hits", "query", "min ms", "median", "p95", "ranker");
    let mut all_lexical = Vec::new();
    let mut all_reranked = Vec::new();
    for query in queries {
        let (ms, ranker, hits) = time(query, false, iterations)?;
        print_bench_row(query, &ms, ranker, hits);
        all_lexical.extend(ms);
        if rerank_iterations > 0 {
            let (ms, ranker, hits) = time(query, true, rerank_iterations)?;
            print_bench_row("  (reranked)", &ms, ranker, hits);
            all_reranked.extend(ms);
        }
    }
    print_bench_row("all lexical", &all_lexical, "", 0);
    if !all_reranked.is_empty() {
        print_bench_row("all reranked", &all_reranked, "", 0);
    }
    Ok(())
}

fn print_bench_row(label: &str, ms: &[f64], ranker: &str, hits: usize) {
    let mut sorted = ms.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pick = |q: f64| sorted[((sorted.len() as f64 - 1.0) * q).round() as usize];
    let label: String = label.chars().take(40).collect();
    if sorted.is_empty() {
        return;
    }
    println!(
        "{:<40} {:>9.1} {:>9.1} {:>9.1}  {:<8} {}",
        label,
        sorted[0],
        pick(0.5),
        pick(0.95),
        ranker,
        if ranker.is_empty() { String::new() } else { hits.to_string() }
    );
}


fn cmd_read(cli: &Cli, id: &str) -> Result<()> {
    run_op(cli, "memory_read", id)
}

fn cmd_history(cli: &Cli, id: &str) -> Result<()> {
    run_op(cli, "memory_history", id)
}

/// Run a shared read operation and print its JSON, failing when the id is
/// unknown (or suppressed/erased, which reads the same).
fn run_op(cli: &Cli, operation: &str, id: &str) -> Result<()> {
    let result = crate::ops::execute(
        &cli.root,
        &cli.scope,
        &cli.session,
        operation,
        &serde_json::json!({"id": id}),
    );
    if result.not_found {
        return Err(Error::NotFound(id.to_string()));
    }
    if let Some(err) = result.error {
        return Err(Error::Usage(err));
    }
    println!("{}", serde_json::to_string_pretty(&result.payload)?);
    Ok(())
}

fn cmd_index(cli: &Cli) -> Result<()> {
    let repo = GitRepo::open(cli.root.join("memory.git"))?;
    let before = repo.head()?;
    let after = crate::index::rebuild(&repo)?;
    if after == before {
        println!("INDEX.md already current at {after}");
    } else {
        println!("INDEX.md published at {after}");
    }
    Ok(())
}

fn cmd_status(cli: &Cli) -> Result<()> {
    let mut report = crate::ops::status_report(&cli.root)?;
    report["store"] = serde_json::json!(cli.store.label());
    match cli.output_format {
        OutputFormat::Text => {
            let r = &report;
            let projects: Vec<&str> = r["journal"]["projects"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            println!("store:                {} ({})", cli.store.label(), r["root"].as_str().unwrap_or(""));
            println!("head:                 {}", r["head"].as_str().unwrap_or(""));
            println!("journal events:       {} (last seq {})", r["journal"]["events"], r["journal"]["last_seq"]);
            println!("projects:             {}", projects.join(", "));
            println!(
                "consolidated through: seq {} ({} to go)",
                r["consolidated_through"], r["unconsolidated_events"]
            );
            println!("entities:             {} ({} active facts)", r["entities"], r["active_facts"]);
            println!("intents:              {} pending, {} stale", r["intents"]["pending"], r["intents"]["stale"]);
            println!("reranker:             {}", r["reranker"].as_str().unwrap_or(""));
            println!("extractor:            {}", r["extractor"].as_str().unwrap_or(""));
        }
        _ => println!("{}", serde_json::to_string_pretty(&report)?),
    }
    Ok(())
}

fn cmd_evaluate(cli: &Cli, rubric: Option<&std::path::PathBuf>, limit: usize) -> Result<()> {
    let rubric_path = rubric.cloned().unwrap_or_else(|| cli.root.join("rubric.json"));
    let report = crate::evaluation::evaluate(&cli.root, &rubric_path, limit)?;
    match cli.output_format {
        OutputFormat::Text => print!("{report}"),
        _ => println!("{}", serde_json::to_string_pretty(&report)?),
    }
    Ok(())
}

fn cmd_erase(cli: &Cli, target: &str, reason: Option<&str>) -> Result<()> {
    let repo = GitRepo::open(cli.root.join("memory.git"))?;
    let journal = Journal::open(cli.root.join("journal"))?;
    let report = crate::erasure::erase(&repo, &journal, target, reason)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

pub fn infer_entity_path(entity_id: &str) -> String {
    let (bucket, stripped) = if entity_id.starts_with("pref_") {
        ("preferences", entity_id.strip_prefix("pref_").unwrap_or(entity_id))
    } else {
        let bucket = match entity_id.chars().next().unwrap_or('p') {
            'p' => "people",
            'o' => "orgs",
            'w' => "workstreams",
            'c' => "conversations",
            _ => "people",
        };
        let stripped = entity_id
            .strip_prefix(|c: char| matches!(c, 'p' | 'o' | 'w' | 'c'))
            .unwrap_or(entity_id)
            .trim_start_matches('_');
        (bucket, stripped)
    };
    format!("memory/{bucket}/{stripped}.md")
}

#[allow(dead_code)]
fn _touch_unused() {
    let _ = Role::User;
    let _ = Source::Chat {
        source_id: String::new(),
        occurred_at: None,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_path_for_known_buckets() {
        assert_eq!(infer_entity_path("p_alex"), "memory/people/alex.md");
        assert_eq!(infer_entity_path("o_acme"), "memory/orgs/acme.md");
    }

    #[test]
    fn splice_appends_block_and_keeps_hand_written_text() {
        let out = splice_managed_block("# Project\n\nRun tests.\n", "## People\n\n- a\n");
        assert!(out.starts_with("# Project\n\nRun tests.\n\n<!-- mem:begin"));
        assert!(out.contains("- a\n<!-- mem:end -->"));
    }

    #[test]
    fn splice_replaces_only_the_existing_block() {
        let first = splice_managed_block("head\n", "old\n");
        let with_tail = format!("{first}tail\n");
        let out = splice_managed_block(&with_tail, "new\n");
        assert!(out.starts_with("head\n"));
        assert!(out.ends_with("tail\n"));
        assert!(out.contains("new\n"));
        assert!(!out.contains("old"));
        assert_eq!(out.matches("<!-- mem:begin").count(), 1);
    }

    #[test]
    fn writeback_takes_one_project_and_user_preferences() {
        use crate::ops::project_entity_includes as includes;
        assert!(includes("w_agensis", "w_agensis"));
        assert!(includes("w_agensis_2", "w_agensis"));
        assert!(includes("pref_user", "w_agensis"));
        assert!(!includes("w_agensis_agent", "w_agensis"));
        assert!(!includes("w_legion", "w_agensis"));
    }

    #[test]
    fn buckets_with_the_same_heading_merge() {
        assert_eq!(humanise_bucket("people"), "People");
        assert_eq!(humanise_bucket("team_notes"), "team notes");
        let sections = vec![
            (
                "team_notes".into(),
                vec![("fact_a".into(), "Alpha rule stands.".into(), Visibility::Private)],
            ),
            (
                "team notes".into(),
                vec![("fact_b".into(), "Beta rule stands.".into(), Visibility::Private)],
            ),
            (
                "people".into(),
                vec![("fact_c".into(), "Ada reviews releases.".into(), Visibility::Private)],
            ),
        ];
        let merged = merge_writeback_sections(sections);
        let notes: Vec<_> = merged.iter().filter(|(h, _)| h == "team notes").collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].1.len(), 2);
        assert!(merged.iter().any(|(h, rows)| h == "People" && rows.len() == 1));
    }
}