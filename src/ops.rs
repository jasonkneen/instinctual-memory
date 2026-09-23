//! One operation layer for the CLI, the shell, HTTP, and MCP.
//!
//! Search reads the journal files and, when present, the facts in `memory.git`.
//! Changes go through the existing Git publisher. Tasks are one versioned
//! file in the memory root.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use chrono::Utc;
use fs4::FileExt;
use serde_json::{json, Value};

use crate::change::{self, ChangeKind, ChangeRequest};
use crate::entity::EntityFile;
use crate::error::Result;
use crate::fact::{Fact, FactKind, FactStatus, Visibility};
use crate::journal::Journal;
use crate::repo::GitRepo;
use crate::controls::Controls;
use crate::search::journal::JournalSearch;
use crate::search::{LexicalSearch, SearchHit, SearchQuery};

#[derive(Debug, Clone)]
pub struct OpResult {
    pub payload: Value,
    /// Counts and a stage only. Never the query text or a fact statement.
    pub progress: Value,
    pub conflict: bool,
    pub not_found: bool,
    pub error: Option<String>,
}

pub fn execute(
    root: &Path,
    scope_id: &str,
    session_id: &str,
    operation: &str,
    args: &Value,
) -> OpResult {
    match operation {
        "memory_search" => search(root, args),
        "memory_read" => read(root, args),
        "memory_history" => history(root, args),
        "memory_status" => status(root),
        "memory_request_change" => request_change(root, scope_id, session_id, args),
        "tasks_read" => tasks_read(root),
        "tasks_update" => tasks_update(root, args),
        other => fail(&format!("unknown operation: {other}")),
    }
}

fn search(root: &Path, args: &Value) -> OpResult {
    let Some(query) = args.get("query").and_then(|v| v.as_str()).filter(|q| !q.trim().is_empty())
    else {
        return fail("memory_search requires query");
    };
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(8)
        .clamp(1, 20) as usize;
    // Reranking is on unless the caller passes `"rerank": false`.
    let rerank = args.get("rerank").and_then(|v| v.as_bool()).unwrap_or(true);
    let project = args.get("project").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let found = match combined_search(root, &SearchParams { query, limit, rerank, project, ..Default::default() }) {
        Ok(found) => found,
        Err(err) => return fail(&err.to_string()),
    };
    let hits: Vec<Value> = found
        .hits
        .iter()
        .map(|hit| {
            json!({
                "fact_id": hit.fact_id,
                "entity_id": hit.entity_id,
                "score": hit.score,
                "statement": hit.statement,
                "source": hit.entity_path,
            })
        })
        .collect();
    OpResult {
        progress: json!({"type": "progress", "stage": "scan", "scanned": found.scanned}),
        payload: json!({"ranker": found.ranker, "judged": found.judged, "revision": found.revision, "hits": hits}),
        conflict: false,
        not_found: false,
        error: None,
    }
}

/// Inputs to [`combined_search`].
#[derive(Default)]
pub struct SearchParams<'a> {
    pub query: &'a str,
    pub limit: usize,
    pub rerank: bool,
    pub include_disputed: bool,
    pub scope_filter: Option<&'a str>,
    pub source_filter: &'a [String],
    /// Only this project's memory: its project entity (and spill files), the
    /// user's general preferences, and journal events recorded for it.
    pub project: Option<&'a str>,
    /// Curated facts only; skip the journal scan.
    pub facts_only: bool,
}

/// Whether an entity belongs to a project's memory: the project entity, its
/// spill files (`<id>_2`, ...), and the user's general preference entity.
pub fn project_entity_includes(entity_id: &str, project_id: &str) -> bool {
    let is_or_spill = |base: &str| {
        entity_id == base
            || entity_id
                .strip_prefix(base)
                .and_then(|rest| rest.strip_prefix('_'))
                .is_some_and(|n| n.parse::<u32>().is_ok())
    };
    is_or_spill(project_id) || is_or_spill("pref_user")
}

/// `query` as-is when it fits in `max` characters, else its distinct
/// non-stopword words, in order, cut at a word boundary.
pub fn condense_query(query: &str, max: usize) -> String {
    if query.chars().count() <= max {
        return query.to_string();
    }
    use crate::search::lexical::{stem, tokenize, STOPWORDS};
    let mut seen = std::collections::HashSet::new();
    let mut out = String::new();
    for word in tokenize(query) {
        let s = stem(&word);
        if s.chars().count() <= 2 || STOPWORDS.contains(&s.as_str()) || !seen.insert(s) {
            continue;
        }
        if out.chars().count() + word.chars().count() + 1 > max {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&word);
    }
    out
}

/// Result of [`combined_search`].
pub struct SearchFound {
    pub ranker: &'static str,
    pub judged: bool,
    pub revision: String,
    pub scanned: u64,
    pub hits: Vec<SearchHit>,
}

/// The one search behind the CLI, MCP, HTTP, and the shell. Curated facts
/// (eligibility and suppression enforced by `LexicalSearch`) and raw journal
/// events are both searched; their rows are interleaved so each side
/// reaches the reranker, and one rerank orders the combined pool.
pub fn combined_search(root: &Path, p: &SearchParams) -> Result<SearchFound> {
    let mut fact_hits: Vec<SearchHit> = Vec::new();
    let mut revision = String::new();
    let repo_path = root.join("memory.git");
    if repo_path.exists() {
        let repo = GitRepo::open(&repo_path)?;
        revision = repo.head()?;
        if !repo.list_eligible_entity_files(&revision)?.is_empty() {
            let controls = repo
                .read_snapshot(&revision, "state/controls.json")?
                .map(|b| Controls::parse(&b.content))
                .transpose()?
                .unwrap_or_default();
            // The query validator caps a single lexical call at 20 hits and
            // 200 characters; a long question keeps its meaningful words.
            let q = SearchQuery {
                query: condense_query(p.query, 200),
                limit: if p.rerank { 20 } else { p.limit.min(20) },
                include_disputed: p.include_disputed,
                audience: Visibility::Private,
            };
            fact_hits = LexicalSearch::search(&repo, &revision, &q, &controls)?;
            if let Some(project) = p.project {
                let project_id = format!("w_{}", crate::consolidate::slug(project));
                fact_hits.retain(|h| project_entity_includes(&h.entity_id, &project_id));
            }
            if !p.source_filter.is_empty() {
                fact_hits.retain(|h| {
                    h.sources.iter().any(|src| p.source_filter.iter().any(|f| src.starts_with(f)))
                });
            }
        }
    }

    // Short rows that share a hit's event id are pulled into the pool too.
    let pool = if p.rerank { p.limit.max(20) } else { p.limit };
    let scan = if p.facts_only {
        crate::search::journal::JournalScan { hits: Vec::new(), scanned: 0 }
    } else {
        JournalSearch::search_detailed(
            &root.join("journal"),
            p.query,
            pool,
            p.scope_filter,
            p.source_filter,
            p.project,
            true,
        )?
    };
    let journal_hits: Vec<SearchHit> = scan
        .hits
        .into_iter()
        .filter(|h| h.score > 0.0 || p.rerank)
        .map(|h| SearchHit {
            fact_id: format!("journal:{}#{}", h.event_id, h.seq),
            entity_id: h.scope_id.clone(),
            entity_path: format!("journal/{}", h.source_kind),
            statement: h.excerpt.clone(),
            score: h.score,
            sources: vec![h.event_id.clone()],
            observed_at: h.occurred_at.unwrap_or(h.ingested_at),
            snippet: None,
            source_excerpts: None,
        })
        .collect();

    let mut baseline = Vec::with_capacity(fact_hits.len() + journal_hits.len());
    let mut facts = fact_hits.into_iter();
    let mut events = journal_hits.into_iter();
    loop {
        match (facts.next(), events.next()) {
            (None, None) => break,
            (f, e) => baseline.extend(f.into_iter().chain(e)),
        }
    }
    let (ranker, judged, hits) = if p.rerank {
        crate::search::rerank::rerank_auto(p.query, baseline, p.limit, &root.join(".models"))
    } else {
        baseline.retain(|h| h.score > 0.0);
        baseline.truncate(p.limit);
        ("lexical", false, baseline)
    };
    Ok(SearchFound { ranker, judged, revision, scanned: scan.scanned, hits })
}

fn read(root: &Path, args: &Value) -> OpResult {
    let Some(id) = args.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
        return fail("memory_read requires id");
    };
    if let Some((event_id, seq)) = parse_journal_id(id) {
        return match find_journal_event(root, &event_id, seq) {
            Ok(Some(statement)) => ok(json!({
                "id": id,
                "statement": statement,
                "label": "current",
            })),
            Ok(None) => missing(),
            Err(err) => fail(&err.to_string()),
        };
    }
    match find_fact(root, id) {
        Ok(Some(found)) => {
            if found.entity_id == id {
                let Some(current) = found
                    .facts
                    .iter()
                    .find(|fact| fact.status == FactStatus::Active)
                else {
                    return missing();
                };
                let facts: Vec<Value> = found
                    .facts
                    .iter()
                    .filter(|fact| fact.status == FactStatus::Active)
                    .map(|fact| json!({"id": fact.id, "statement": fact.statement}))
                    .collect();
                ok(json!({
                    "id": id,
                    "statement": current.statement,
                    "label": "current",
                    "fact_id": current.id,
                    "facts": facts,
                }))
            } else {
                let Some(fact) = found.facts.iter().find(|fact| fact.id == id) else {
                    return missing();
                };
                ok(json!({
                    "id": fact.id,
                    "statement": fact.statement,
                    "label": label_for(fact.status),
                    "entity_id": found.entity_id,
                }))
            }
        }
        Ok(None) => missing(),
        Err(err) => fail(&err.to_string()),
    }
}

fn history(root: &Path, args: &Value) -> OpResult {
    let Some(id) = args.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
        return fail("memory_history requires id");
    };
    if parse_journal_id(id).is_some() {
        return match read(root, args) {
            got if got.not_found => missing(),
            got => ok(json!({"id": id, "records": [got.payload]})),
        };
    }
    match find_fact(root, id) {
        Ok(Some(found)) => {
            let records: Vec<Value> = found
                .facts
                .iter()
                .map(|fact| {
                    json!({
                        "id": fact.id,
                        "statement": fact.statement,
                        "status": fact.status,
                        "label": label_for(fact.status),
                    })
                })
                .collect();
            if records.is_empty() {
                missing()
            } else {
                ok(json!({"id": found.entity_id, "records": records}))
            }
        }
        Ok(None) => missing(),
        Err(err) => fail(&err.to_string()),
    }
}

fn status(root: &Path) -> OpResult {
    match status_report(root) {
        Ok(report) => ok(report),
        Err(err) => fail(&err.to_string()),
    }
}

/// Journal size and projects, consolidation lag, entities and active facts,
/// pending intents, and the active reranker and extractor.
pub fn status_report(root: &Path) -> Result<Value> {
    let repo = GitRepo::open(root.join("memory.git"))?;
    let head = repo.head()?;

    let journal = Journal::open(root.join("journal"))?;
    let (mut events, mut last_seq, mut projects) = (0u64, 0u64, std::collections::BTreeSet::new());
    for event in journal.iter()? {
        let event = event?;
        events += 1;
        last_seq = last_seq.max(event.seq);
        if let Some(name) = event.project_name() {
            projects.insert(name.to_string());
        }
    }
    let consolidated_through = repo
        .read_snapshot(&head, "state/checkpoint.json")?
        .map(|b| crate::checkpoint::Checkpoint::parse(&b.content))
        .transpose()?
        .map_or(0, |c| c.through_seq);
    let (mut entities, mut facts) = (0usize, 0usize);
    for (_path, blob) in repo.list_eligible_entity_files(&head)? {
        if let Ok(entity) = EntityFile::parse(&blob.content) {
            entities += 1;
            facts += entity.facts.iter().filter(|f| f.status == FactStatus::Active).count();
        }
    }
    let (mut pending, mut stale) = (0usize, 0usize);
    if let Ok(dir) = std::fs::read_dir(repo.intents_dir()) {
        for entry in dir.flatten() {
            match entry.path().extension().and_then(|s| s.to_str()) {
                Some("json") => pending += 1,
                Some("stale") => stale += 1,
                _ => {}
            }
        }
    }
    let models = root.join(".models");
    let reranker = if std::env::var("MEM_RERANK").is_ok_and(|v| matches!(v.as_str(), "off" | "0" | "false")) {
        "off (MEM_RERANK)"
    } else if crate::jev::JevReranker::from_env().is_ok() {
        "jev"
    } else if models.join(crate::search::laya::LayaReranker::selected_model(&models).hub_dir()).is_dir() {
        "local"
    } else {
        "lexical"
    };
    let extractor = match crate::consolidate::resolve_extractor(crate::consolidate::ExtractorKind::Auto) {
        crate::consolidate::ExtractorKind::Llm => "llm",
        _ => "rules",
    };
    Ok(serde_json::json!({
        "root": root.display().to_string(),
        "head": head,
        "journal": {"events": events, "last_seq": last_seq, "projects": projects},
        "consolidated_through": consolidated_through,
        "unconsolidated_events": last_seq.saturating_sub(consolidated_through),
        "entities": entities,
        "active_facts": facts,
        "intents": {"pending": pending, "stale": stale},
        "reranker": reranker,
        "extractor": extractor,
    }))
}

fn request_change(root: &Path, scope_id: &str, session_id: &str, args: &Value) -> OpResult {
    let kind = match args.get("kind").and_then(|v| v.as_str()).unwrap_or("remember") {
        "remember" => ChangeKind::Remember,
        "correct" => ChangeKind::Correct,
        "forget" => ChangeKind::Forget,
        other => return fail(&format!("unknown change kind: {other}")),
    };
    let Some(request_id) = text_field(args, "request_id") else {
        return fail("memory_request_change requires request_id");
    };
    let Some(entity_id) = text_field(args, "entity_id") else {
        return fail("memory_request_change requires entity_id");
    };
    let statement = text_field(args, "statement").unwrap_or_default();
    let fact_id = text_field(args, "fact_id").unwrap_or_else(|| format!("fact_{request_id}"));
    let target_fact_id = text_field(args, "target_fact_id");
    let fact = if matches!(kind, ChangeKind::Remember | ChangeKind::Correct) {
        if statement.is_empty() {
            return fail("statement required");
        }
        Some(make_fact(&fact_id, &statement))
    } else {
        None
    };
    let repo = match GitRepo::open(root.join("memory.git")) {
        Ok(repo) => repo,
        Err(err) => return fail(&err.to_string()),
    };
    if let Ok(head) = repo.head() {
        if repo
            .read_receipt(&head, &request_id)
            .ok()
            .flatten()
            .is_some()
        {
            let stored = count_statement(&repo, &entity_id, &statement).unwrap_or(0);
            return ok(json!({
                "request_id": request_id,
                "replayed": true,
                "fact_id": fact_id,
                "entity_id": entity_id,
                "statement": statement,
                "stored": stored,
            }));
        }
    }
    let request = ChangeRequest {
        kind,
        request_id: request_id.clone(),
        scope_id: scope_id.to_string(),
        session_id: session_id.to_string(),
        source_event_id: format!("evt_{request_id}"),
        entity_id: entity_id.clone(),
        fact,
        target_fact_id: target_fact_id.clone(),
        reason: text_field(args, "reason"),
        proposed_at: Utc::now(),
    };
    if let Err(err) = change::recover_in_dir(&repo) {
        return fail(&format!("recovering interrupted changes: {err}"));
    }
    let outcome = match change::apply(&repo, request) {
        Ok(outcome) => outcome,
        Err(err) => return fail(&err.to_string()),
    };
    let stored = count_statement(&repo, &entity_id, &statement).unwrap_or(0);
    ok(json!({
        "request_id": outcome.request_id,
        "revision": outcome.revision,
        "replayed": outcome.replayed,
        "fact_id": outcome.target_id,
        "entity_id": entity_id,
        "statement": statement,
        "stored": stored,
    }))
}

fn tasks_read(root: &Path) -> OpResult {
    match load_tasks(root) {
        Ok(file) => ok(json!({"version": file.version, "tasks": file.tasks})),
        Err(err) => fail(&err.to_string()),
    }
}

fn tasks_update(root: &Path, args: &Value) -> OpResult {
    let Some(title) = text_field(args, "title") else {
        return fail("tasks_update requires title");
    };
    let Some(expected) = args.get("expected_version").and_then(|v| v.as_u64()) else {
        return fail("tasks_update requires expected_version");
    };
    let locked = with_task_lock(root, || {
        let mut file = read_tasks_file(root)?;
        if file.version != expected {
            return Ok(OpResult {
                payload: json!({
                    "conflict": true,
                    "version": file.version,
                    "tasks": file.tasks,
                }),
                progress: json!({"type": "progress", "stage": "tasks", "scanned": 0}),
                conflict: true,
                not_found: false,
                error: None,
            });
        }
        file.version += 1;
        file.tasks.push(TaskItem {
            id: format!("t_{}", file.version),
            title: title.clone(),
        });
        write_tasks_file(root, &file)?;
        Ok(ok(json!({
            "conflict": false,
            "version": file.version,
            "tasks": file.tasks,
        })))
    });
    match locked {
        Ok(result) => result,
        Err(err) => fail(&err.to_string()),
    }
}

fn ok(payload: Value) -> OpResult {
    OpResult {
        payload,
        progress: json!({"type": "progress", "stage": "done", "scanned": 0}),
        conflict: false,
        not_found: false,
        error: None,
    }
}

fn fail(message: &str) -> OpResult {
    OpResult {
        payload: json!({"error": message}),
        progress: json!({"type": "progress", "stage": "done", "scanned": 0}),
        conflict: false,
        not_found: false,
        error: Some(message.to_string()),
    }
}

fn missing() -> OpResult {
    OpResult {
        payload: json!({"error": "not found"}),
        progress: json!({"type": "progress", "stage": "done", "scanned": 0}),
        conflict: false,
        not_found: true,
        error: Some("not found".into()),
    }
}

fn text_field(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn label_for(status: FactStatus) -> &'static str {
    match status {
        FactStatus::Active => "current",
        _ => "non-current",
    }
}

fn make_fact(id: &str, statement: &str) -> Fact {
    Fact {
        id: id.to_string(),
        predicate: "note".into(),
        statement: statement.to_string(),
        kind: FactKind::ExplicitAssertion,
        status: FactStatus::Active,
        observed_at: chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .expect("fixed timestamp")
            .with_timezone(&Utc),
        valid_from: None,
        valid_from_precision: None,
        valid_to: None,
        expires_at: None,
        review_after: None,
        supersedes: Vec::new(),
        sources: Vec::new(),
        visibility: Visibility::Private,
    }
}

struct FoundFact {
    entity_id: String,
    facts: Vec<Fact>,
}

fn find_fact(root: &Path, id: &str) -> Result<Option<FoundFact>> {
    let repo_path = root.join("memory.git");
    if !repo_path.exists() {
        return Ok(None);
    }
    let repo = GitRepo::open(&repo_path)?;
    let head = match repo.head() {
        Ok(head) => head,
        Err(_) => return Ok(None),
    };
    // Suppressed (forgotten) and erased facts are never returned, by current
    // or historical reads.
    let controls = repo
        .read_snapshot(&head, "state/controls.json")?
        .map(|b| Controls::parse(&b.content))
        .transpose()?
        .unwrap_or_default();
    if controls.is_suppressed(id) || controls.is_deleted(id) {
        return Ok(None);
    }
    let mut by_fact: Option<FoundFact> = None;
    for (_path, blob) in repo.list_eligible_entity_files(&head)? {
        let mut entity = match EntityFile::parse(&blob.content) {
            Ok(entity) => entity,
            Err(_) => continue,
        };
        entity
            .facts
            .retain(|f| !controls.is_suppressed(&f.id) && !controls.is_deleted(&f.id));
        if entity.id == id {
            return Ok(Some(FoundFact {
                entity_id: entity.id,
                facts: entity.facts,
            }));
        }
        if entity.facts.iter().any(|fact| fact.id == id) && by_fact.is_none() {
            by_fact = Some(FoundFact {
                entity_id: entity.id,
                facts: entity.facts,
            });
        }
    }
    Ok(by_fact)
}

fn count_statement(repo: &GitRepo, entity_id: &str, statement: &str) -> Result<usize> {
    if statement.is_empty() {
        return Ok(0);
    }
    let head = repo.head()?;
    let mut n = 0usize;
    for (_path, blob) in repo.list_eligible_entity_files(&head)? {
        let entity = match EntityFile::parse(&blob.content) {
            Ok(entity) => entity,
            Err(_) => continue,
        };
        if entity.id != entity_id {
            continue;
        }
        n += entity
            .facts
            .iter()
            .filter(|fact| fact.statement == statement)
            .count();
    }
    Ok(n)
}

fn parse_journal_id(id: &str) -> Option<(String, u64)> {
    let rest = id.strip_prefix("journal:")?;
    let (event_id, seq) = rest.rsplit_once('#')?;
    Some((event_id.to_string(), seq.parse().ok()?))
}

fn find_journal_event(root: &Path, event_id: &str, seq: u64) -> Result<Option<String>> {
    let journal = Journal::open(root.join("journal"))?;
    for event in journal.iter()? {
        let event = event?;
        if event.event_id == event_id && event.seq == seq {
            return Ok(Some(event.content));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TaskFile {
    version: u64,
    tasks: Vec<TaskItem>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct TaskItem {
    id: String,
    title: String,
}

fn load_tasks(root: &Path) -> Result<TaskFile> {
    with_task_lock(root, || read_tasks_file(root))
}

fn read_tasks_file(root: &Path) -> Result<TaskFile> {
    let path = root.join("tasks.json");
    if !path.exists() {
        return Ok(TaskFile {
            version: 0,
            tasks: Vec::new(),
        });
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| crate::error::Error::io(&path, e))?;
    serde_json::from_str(&raw).map_err(|e| crate::error::Error::InvalidContent(e.to_string()))
}

fn write_tasks_file(root: &Path, file: &TaskFile) -> Result<()> {
    std::fs::create_dir_all(root).map_err(|e| crate::error::Error::io(root, e))?;
    let path = root.join("tasks.json");
    let mut out = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| crate::error::Error::io(&path, e))?;
    let body = serde_json::to_string_pretty(file)?;
    out.write_all(body.as_bytes())
        .map_err(|e| crate::error::Error::io(&path, e))?;
    out.sync_all().map_err(|e| crate::error::Error::io(&path, e))?;
    Ok(())
}

fn with_task_lock<T>(root: &Path, body: impl FnOnce() -> Result<T>) -> Result<T> {
    std::fs::create_dir_all(root).map_err(|e| crate::error::Error::io(root, e))?;
    let path = root.join("tasks.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| crate::error::Error::io(&path, e))?;
    FileExt::lock_exclusive(&file).map_err(|e| crate::error::Error::io(&path, e))?;
    let result = body();
    let _ = FileExt::unlock(&file);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{JournalEvent, Redaction, Role, Source};

    fn root_with_phrase(phrase: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        journal
            .append(JournalEvent::new(
                "personal",
                "s",
                Role::User,
                Source::Chat {
                    source_id: "only-here".into(),
                    occurred_at: None,
                },
                phrase,
                Redaction::None,
            ))
            .unwrap();
        tmp
    }

    #[test]
    fn long_queries_are_condensed_not_rejected() {
        let long = "Do not read any files. Answer from context: where exactly must a schema change be made, and how do the frontend and backend each deploy to production? Cite any memory fact ids you used please.";
        let q = condense_query(long, 200);
        assert!(q.chars().count() <= 200);
        assert!(q.contains("schema") && q.contains("deploy") && q.contains("frontend"));
        assert_eq!(condense_query("short one", 200), "short one");
    }

    #[test]
    fn a_long_query_still_hits_a_known_fact() {
        let tmp = tempfile::tempdir().unwrap();
        GitRepo::create(tmp.path()).unwrap();
        let marker = "keelstripe4401";
        let statement = format!("Payments identified by {marker} go through Stripe.");
        let stored = execute(
            tmp.path(),
            "personal",
            "default",
            "memory_request_change",
            &json!({
                "kind": "remember",
                "request_id": "req_long",
                "entity_id": "w_shop",
                "fact_id": "fact_long",
                "statement": statement,
            }),
        );
        assert!(stored.error.is_none(), "{:?}", stored.error);
        let query = format!(
            "{marker} {}",
            "where exactly must this schema change be applied in both services ".repeat(8)
        );
        assert!(query.chars().count() > 200, "{}", query.chars().count());
        let found = combined_search(
            tmp.path(),
            &SearchParams { query: &query, limit: 5, rerank: false, ..Default::default() },
        )
        .unwrap();
        assert!(
            found.hits.iter().any(|h| h.statement.contains(marker)),
            "combined_search missed {marker}: {:?}",
            found.hits.iter().map(|h| h.statement.clone()).collect::<Vec<_>>()
        );
        let via_mcp = execute(
            tmp.path(),
            "personal",
            "default",
            "memory_search",
            &json!({"query": query, "limit": 5, "rerank": false}),
        );
        assert!(via_mcp.error.is_none(), "{:?}", via_mcp.error);
        let hits = via_mcp.payload["hits"].as_array().unwrap();
        assert!(hits.iter().any(|h| h["statement"].as_str().unwrap().contains(marker)));
    }

    #[test]
    fn a_forgotten_fact_is_absent_from_shared_search() {
        let tmp = tempfile::tempdir().unwrap();
        GitRepo::create(tmp.path()).unwrap();
        let remember = |request: &str, fact: &str, statement: &str| {
            let got = execute(
                tmp.path(),
                "personal",
                "default",
                "memory_request_change",
                &json!({
                    "kind": "remember",
                    "request_id": request,
                    "entity_id": "w_shop",
                    "fact_id": fact,
                    "statement": statement,
                }),
            );
            assert!(got.error.is_none(), "{:?}", got.error);
        };
        remember("req_keep", "fact_keep", "Keep the violet ledger rule.");
        remember("req_drop", "fact_drop", "Drop the amber ledger rule.");
        let hit = |query: &str| {
            let found = combined_search(
                tmp.path(),
                &SearchParams { query, limit: 8, rerank: false, ..Default::default() },
            )
            .unwrap();
            let mcp = execute(
                tmp.path(),
                "personal",
                "default",
                "memory_search",
                &json!({"query": query, "limit": 8, "rerank": false}),
            );
            assert!(mcp.error.is_none(), "{:?}", mcp.error);
            let cli: Vec<String> = found.hits.iter().map(|h| h.fact_id.clone()).collect();
            let mcp_ids: Vec<String> = mcp.payload["hits"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|h| h["fact_id"].as_str().map(str::to_string))
                .collect();
            assert_eq!(cli, mcp_ids);
            cli
        };
        let before = hit("ledger");
        assert!(before.iter().any(|id| id == "fact_keep"));
        assert!(before.iter().any(|id| id == "fact_drop"));
        let forgotten = execute(
            tmp.path(),
            "personal",
            "default",
            "memory_request_change",
            &json!({
                "kind": "forget",
                "request_id": "req_forget",
                "entity_id": "w_shop",
                "target_fact_id": "fact_drop",
            }),
        );
        assert!(forgotten.error.is_none(), "{:?}", forgotten.error);
        let after = hit("amber ledger");
        assert!(!after.iter().any(|id| id == "fact_drop"), "{after:?}");
        let kept = hit("violet ledger");
        assert!(kept.iter().any(|id| id == "fact_keep"), "{kept:?}");
    }

    #[test]
    fn search_read_journal_id_and_unknown_is_not_found() {
        let phrase = "violet-keel-9912 only in this journal";
        let tmp = root_with_phrase(phrase);
        let found = execute(tmp.path(), "personal", "default", "memory_search", &json!({"query": phrase, "limit": 3, "rerank": false}));
        assert_eq!(found.payload["ranker"], "lexical");
        assert_eq!(found.payload["judged"], false);
        let statement = found.payload["hits"][0]["statement"].as_str().unwrap();
        assert!(statement.contains(phrase), "{statement}");
        let id = found.payload["hits"][0]["fact_id"].as_str().unwrap();
        assert!(id.starts_with("journal:"));
        let got = execute(tmp.path(), "personal", "default", "memory_read", &json!({"id": id}));
        assert!(got.payload["statement"].as_str().unwrap().contains(phrase));
        let missing = execute(tmp.path(), "personal", "default", "memory_read", &json!({"id": "journal:missing#1"}));
        assert!(missing.not_found);
        assert_eq!(missing.payload["error"], "not found");
        assert!(!missing.payload.to_string().contains(phrase));
        eprintln!("checked {phrase}");
    }

    #[test]
    fn correct_labels_old_statement_non_current_and_request_id_is_once() {
        let tmp = tempfile::tempdir().unwrap();
        GitRepo::create(tmp.path()).unwrap();
        let a = "statement-alpha-keel";
        let b = "statement-beta-keel";
        let first = execute(tmp.path(), "personal", "default", "memory_request_change", &json!({
            "kind": "remember",
            "request_id": "req_once",
            "entity_id": "p_demo",
            "fact_id": "fact_a",
            "statement": a,
        }));
        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(first.payload["stored"], 1);
        let again = execute(tmp.path(), "personal", "default", "memory_request_change", &json!({
            "kind": "remember",
            "request_id": "req_once",
            "entity_id": "p_demo",
            "fact_id": "fact_a",
            "statement": a,
        }));
        assert_eq!(again.payload["replayed"], true);
        assert_eq!(again.payload["stored"], 1);
        let corrected = execute(tmp.path(), "personal", "default", "memory_request_change", &json!({
            "kind": "correct",
            "request_id": "req_correct",
            "entity_id": "p_demo",
            "target_fact_id": "fact_a",
            "fact_id": "fact_b",
            "statement": b,
        }));
        assert!(corrected.error.is_none(), "{:?}", corrected.error);
        let current = execute(tmp.path(), "personal", "default", "memory_read", &json!({"id": "p_demo"}));
        assert_eq!(current.payload["statement"], b);
        assert_eq!(current.payload["label"], "current");
        let past = execute(tmp.path(), "personal", "default", "memory_history", &json!({"id": "p_demo"}));
        let records = past.payload["records"].as_array().unwrap();
        let old = records.iter().find(|rec| rec["statement"] == a).unwrap();
        assert_eq!(old["label"], "non-current");
        let live = records.iter().find(|rec| rec["statement"] == b).unwrap();
        assert_eq!(live["label"], "current");
        let searched = execute(tmp.path(), "personal", "default", "memory_search", &json!({"query": b}));
        assert!(searched.payload.to_string().contains(b));
        eprintln!("checked {a} non-current {b}");
    }

    #[test]
    fn task_update_conflicts_on_stale_version() {
        let tmp = tempfile::tempdir().unwrap();
        let title = "Ship the rail";
        let saved = execute(tmp.path(), "personal", "default", "tasks_update", &json!({
            "title": title,
            "expected_version": 0,
        }));
        assert!(!saved.conflict, "{:?}", saved.payload);
        let read = execute(tmp.path(), "personal", "default", "tasks_read", &json!({}));
        assert_eq!(read.payload["tasks"][0]["title"], title);
        let stale = execute(tmp.path(), "personal", "default", "tasks_update", &json!({
            "title": "A different title",
            "expected_version": 0,
        }));
        assert!(stale.conflict);
        assert_eq!(stale.payload["tasks"][0]["title"], title);
        let again = execute(tmp.path(), "personal", "default", "tasks_read", &json!({}));
        assert_eq!(again.payload["tasks"][0]["title"], title);
        assert_eq!(again.payload["tasks"].as_array().unwrap().len(), 1);
        assert!(tmp.path().join("tasks.json").is_file());
        eprintln!("checked {title}");
    }
}
