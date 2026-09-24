//! Agent hooks: put memory in front of the model without it having to ask.
//!
//! Claude Code runs these as `SessionStart`, `UserPromptSubmit`, and
//! `SessionEnd` hooks (installed by `mem setup`). Each reads the hook's JSON
//! payload on stdin and uses the store for the session's working directory
//! (`$MEM_ROOT`, the nearest local store, or the global store):
//!
//! - `start`: the user's working preferences and how to reach the rest.
//! - `prompt`: the curated facts that match what the user just asked.
//! - `end`: in the background, backfill the finished session into the
//!   journal and consolidate once enough new events have built up.
//!
//! A hook never fails the session: with no store, a bad payload, or any
//! error, it prints nothing and exits 0. Set `MEM_HOOK_DEBUG=1` to see why.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::Result;
use crate::ops::{combined_search, SearchParams};

/// Facts injected per prompt at most.
const PROMPT_FACTS: usize = 6;
/// Unconsolidated events that trigger a background consolidation at session
/// end (override with `MEM_AUTO_CONSOLIDATE=<n>`, or `0` to turn it off).
const AUTO_CONSOLIDATE_AT: u64 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    Start,
    Prompt,
    End,
    /// The work `End` starts in the background; also runnable by hand.
    Sync,
}

/// Run a hook. Errors are swallowed (and printed only with MEM_HOOK_DEBUG=1)
/// so a memory problem never blocks the agent. With `plain`, context is
/// printed as bare text instead of the Claude Code `hookSpecificOutput`
/// envelope, which is what the pi/omp extension consumes.
pub fn run(event: HookEvent, input: impl Read, output: impl Write, plain: bool) {
    if let Err(err) = run_inner(event, input, output, plain) {
        if std::env::var("MEM_HOOK_DEBUG").is_ok_and(|v| v == "1") {
            eprintln!("mem hook: {err}");
        }
    }
}

fn run_inner(event: HookEvent, mut input: impl Read, mut output: impl Write, plain: bool) -> Result<()> {
    let mut raw = String::new();
    if event != HookEvent::Sync {
        input.read_to_string(&mut raw).map_err(|e| crate::error::Error::io("stdin", e))?;
    }
    let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let cwd = payload["cwd"]
        .as_str()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    let Some((root, project)) = crate::cli::store_for_dir(&cwd) else {
        return Ok(());
    };
    let context = match event {
        HookEvent::Start => session_context(&root, project.as_deref())?,
        HookEvent::Prompt => prompt_context(&root, project.as_deref(), payload["prompt"].as_str().unwrap_or(""))?,
        HookEvent::End => {
            spawn_sync(&cwd);
            None
        }
        HookEvent::Sync => {
            sync(&root, &cwd)?;
            None
        }
    };
    if let Some(text) = context {
        if plain {
            write!(output, "{text}").map_err(|e| crate::error::Error::io("stdout", e))?;
        } else {
            let name = if event == HookEvent::Start { "SessionStart" } else { "UserPromptSubmit" };
            let out = json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": text}});
            writeln!(output, "{out}").map_err(|e| crate::error::Error::io("stdout", e))?;
        }
    }
    Ok(())
}

/// Session start: preferences in full, and where the rest of memory is.
fn session_context(root: &Path, project: Option<&str>) -> Result<Option<String>> {
    let status = crate::ops::status_report(root)?;
    let facts = status["active_facts"].as_u64().unwrap_or(0);
    if facts == 0 {
        return Ok(None);
    }
    let read = crate::ops::execute(root, "personal", "default", "memory_read", &json!({"id": "pref_user"}));
    let prefs: Vec<String> = read.payload["facts"]
        .as_array()
        .map(|a| a.iter().filter_map(|f| f["statement"].as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let scope = match project {
        Some(p) => format!(" (shared store; this project is `{p}`, search with `--project {p}`)"),
        None => String::new(),
    };
    let mut text = format!(
        "This project has durable memory from past sessions and its AGENTS.md: {facts} curated facts in `mem`{scope}. \
Facts relevant to each message are added automatically. Before answering about past decisions, conventions, architecture, \
or the user's preferences, check memory (`mem search \"...\"`, or the /mem skill) and prefer it over guessing. \
When the user states a new standing rule or decision, record it with `mem change remember`.\n"
    );
    if !prefs.is_empty() {
        text.push_str("\nThe user's working preferences:\n");
        for p in prefs.iter().take(20) {
            text.push_str(&format!("- {p}\n"));
        }
    }
    Ok(Some(text))
}

/// Each prompt: the curated facts that match it, if any match well.
fn prompt_context(root: &Path, project: Option<&str>, prompt: &str) -> Result<Option<String>> {
    let prompt = prompt.trim();
    if prompt.starts_with('/') || prompt.split_whitespace().count() < 3 {
        return Ok(None);
    }
    let query: String = prompt.chars().take(600).collect();
    let found = combined_search(
        root,
        &SearchParams {
            query: &query,
            limit: PROMPT_FACTS * 2,
            rerank: false,
            facts_only: true,
            project,
            ..Default::default()
        },
    )?;
    // Injected context has to earn its place: a fact must share at least
    // two of the prompt's meaningful words (one when the prompt has only
    // one), so a single incidental word match never pulls a fact in.
    let terms = content_terms(&query);
    if terms.is_empty() {
        return Ok(None);
    }
    let needed = terms.len().min(2);
    let hits: Vec<_> = found
        .hits
        .iter()
        .filter(|h| {
            let words = content_terms(&h.statement);
            terms.iter().filter(|t| words.contains(*t)).count() >= needed
        })
        .take(PROMPT_FACTS)
        .collect();
    if hits.is_empty() {
        return Ok(None);
    }
    let mut text = String::from(
        "Relevant project memory (mem; recorded from earlier sessions and AGENTS.md, so check against the code if it may have changed):\n",
    );
    for hit in hits {
        text.push_str(&format!("- [{}] {}\n", hit.fact_id, hit.statement));
    }
    Ok(Some(text))
}

/// Stemmed words of `text` that carry meaning (stopwords dropped).
fn content_terms(text: &str) -> std::collections::BTreeSet<String> {
    use crate::search::lexical::{stem, tokenize, STOPWORDS};
    tokenize(text)
        .iter()
        .map(|t| stem(t))
        .filter(|t| t.chars().count() > 2 && !STOPWORDS.contains(&t.as_str()))
        .collect()
}

/// Start `mem hook sync` detached so the session can end immediately.
fn spawn_sync(cwd: &Path) {
    let Ok(exe) = std::env::current_exe() else { return };
    let _ = std::process::Command::new(exe)
        .args(["hook", "sync"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Backfill this project's new session events, then consolidate if enough
/// have built up and an LLM key is configured.
fn sync(root: &Path, cwd: &Path) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| crate::error::Error::io("current_exe", e))?;
    let project = crate::journal::project_root(cwd);
    let quiet = |args: &[&str]| {
        std::process::Command::new(&exe)
            .arg("--root")
            .arg(root)
            .args(args)
            .current_dir(&project)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    };
    let _ = quiet(&["backfill-local", "--project", &project]);
    let threshold = std::env::var("MEM_AUTO_CONSOLIDATE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(AUTO_CONSOLIDATE_AT);
    if threshold == 0 {
        return Ok(());
    }
    let status = crate::ops::status_report(root)?;
    let waiting = status["unconsolidated_events"].as_u64().unwrap_or(0);
    if waiting >= threshold && status["extractor"] == "llm" {
        let _ = quiet(&["consolidate"]);
    }
    Ok(())
}
