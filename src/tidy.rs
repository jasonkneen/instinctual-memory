//! `mem tidy`: find facts that should not be in memory, and forget them.
//!
//! Two kinds:
//! - **Duplicates**: a fact that restates another in the same project (or
//!   the user's preferences), by the same word-overlap rules consolidation
//!   uses. The most complete statement is kept.
//! - **Session-only**: facts that described one session ("User is running
//!   start:electron", "User set a /goal ...") rather than something still
//!   true later. The LLM judges these in batches; without a key this step
//!   is skipped and says so.
//!
//! Without `--apply` nothing changes. With it, every listed fact is
//! forgotten in one commit: retracted in its entity file and suppressed in
//! `state/controls.json` with the reason, so it disappears from every read
//! and consolidation never re-adds it. Git history keeps the old versions.

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::consolidate::{chat_json, contained, content_words, jaccard, LlmConfig, NEAR_DUPLICATE};
use crate::controls::{Controls, Suppression, SuppressionKind};
use crate::entity::EntityFile;
use crate::error::Result;
use crate::fact::FactStatus;
use crate::repo::{Change, ChangeSet, GitRepo};

/// One fact tidy would forget.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub fact_id: String,
    pub entity_id: String,
    pub statement: String,
    /// `duplicate` or `session_only`.
    pub kind: &'static str,
    /// For a duplicate, the fact that is kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TidyReport {
    pub facts_checked: usize,
    pub findings: Vec<Finding>,
    /// False when no LLM key is set: only duplicates were checked.
    pub checked_session_only: bool,
    /// Head after `--apply`, if anything was forgotten.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<String>,
}

const JUDGE_PROMPT: &str = r#"You review facts stored in a coding agent's long-term memory.

For each fact decide whether it is LASTING: still true and useful to a new teammate weeks from now, in a different task. Standing rules, preferences, architecture, conventions, commands, and decisions (with their reasons) are lasting.

It is NOT lasting when it describes one session: what the user was doing, running, asking, looking at, or feeling ("User is running start:electron", "User requested a review of X", "User is frustrated with the approval UI"); a session goal, plan, phase, scope, or task list ("User set a /goal ...", "Review scope spans ...", "Phase 4 is implemented"); a requested change ("the composer should move up 4px", "add a terminal panel", "fix the ChatGPT connection"); the current state of a bug; or the working directory.

How the code works IS lasting: what a module, function, or file does, an invariant, a test setup requirement, a design rule and its reason. Keep those even when they are detailed.

Return JSON: {"session_only": ["<id>", ...]} listing only the ids that are NOT lasting. When unsure, leave the id out."#;

#[derive(Deserialize)]
struct Judgement {
    #[serde(default)]
    session_only: Vec<String>,
}

/// Facts per LLM request.
const JUDGE_BATCH: usize = 60;

/// Check the store; with `apply`, forget what was found except `keep`.
pub fn tidy(repo: &GitRepo, apply: bool, keep: &[String]) -> Result<TidyReport> {
    let head = repo.head()?;
    let controls = repo
        .read_snapshot(&head, "state/controls.json")?
        .map(|b| Controls::parse(&b.content))
        .transpose()?
        .unwrap_or_default();
    let mut entities: BTreeMap<String, (String, EntityFile)> = BTreeMap::new();
    for (path, blob) in repo.list_eligible_entity_files(&head)? {
        if let Ok(entity) = EntityFile::parse(&blob.content) {
            entities.insert(entity.id.clone(), (path, entity));
        }
    }
    // (entity id, fact id, statement) of every live fact.
    let live: Vec<(String, String, String)> = entities
        .values()
        .flat_map(|(_, e)| {
            e.facts
                .iter()
                .filter(|f| f.status == FactStatus::Active && !controls.is_suppressed(&f.id) && !controls.is_deleted(&f.id))
                .map(move |f| (e.id.clone(), f.id.clone(), f.statement.clone()))
        })
        .collect();

    let mut findings: Vec<Finding> = Vec::new();
    let mut flagged: BTreeSet<String> = BTreeSet::new();

    // Duplicates: longest statement first, so the most complete one is kept.
    let mut by_length: Vec<&(String, String, String)> = live.iter().collect();
    by_length.sort_by_key(|(_, id, s)| (std::cmp::Reverse(s.len()), id.clone()));
    let mut kept: Vec<(&str, &str, BTreeSet<String>)> = Vec::new();
    for (entity_id, fact_id, statement) in by_length {
        let words = content_words(statement);
        let same_family = |other_entity: &str| family(other_entity) == family(entity_id) || family(other_entity) == "pref_user";
        let dup = kept.iter().find(|(e, _, w)| {
            same_family(e) && (jaccard(&words, w) >= NEAR_DUPLICATE || contained(&words, w))
        });
        match dup {
            Some((_, keep_id, _)) if !words.is_empty() => {
                findings.push(Finding {
                    fact_id: fact_id.clone(),
                    entity_id: entity_id.clone(),
                    statement: statement.clone(),
                    kind: "duplicate",
                    duplicate_of: Some(keep_id.to_string()),
                });
                flagged.insert(fact_id.clone());
            }
            _ => kept.push((entity_id, fact_id, words)),
        }
    }

    // Session-only: ask the model about everything not already flagged.
    let cfg = LlmConfig::from_env();
    if let Some(cfg) = &cfg {
        let remaining: Vec<&(String, String, String)> =
            live.iter().filter(|(_, id, _)| !flagged.contains(id)).collect();
        for batch in remaining.chunks(JUDGE_BATCH) {
            let facts: Vec<serde_json::Value> = batch
                .iter()
                .map(|(_, id, s)| serde_json::json!({"id": id, "statement": s}))
                .collect();
            let judged: Judgement = chat_json(cfg, JUDGE_PROMPT, &serde_json::json!({"facts": facts}).to_string())?;
            let ids: BTreeSet<&str> = judged.session_only.iter().map(String::as_str).collect();
            for (entity_id, fact_id, statement) in batch {
                if ids.contains(fact_id.as_str()) && flagged.insert(fact_id.clone()) {
                    findings.push(Finding {
                        fact_id: fact_id.clone(),
                        entity_id: entity_id.clone(),
                        statement: statement.clone(),
                        kind: "session_only",
                        duplicate_of: None,
                    });
                }
            }
        }
    }

    findings.retain(|f| !keep.contains(&f.fact_id));
    let mut report = TidyReport {
        facts_checked: live.len(),
        findings,
        checked_session_only: cfg.is_some(),
        applied_at: None,
    };
    if apply && !report.findings.is_empty() {
        report.applied_at = Some(forget_all(repo, &head, entities, controls, &report.findings)?);
    }
    Ok(report)
}

/// Entity id without a spill suffix (`w_app_2` -> `w_app`).
fn family(entity_id: &str) -> &str {
    match entity_id.rsplit_once('_') {
        Some((base, n)) if n.parse::<u32>().is_ok() && base.contains('_') => base,
        _ => entity_id,
    }
}

fn forget_all(
    repo: &GitRepo,
    head: &str,
    mut entities: BTreeMap<String, (String, EntityFile)>,
    mut controls: Controls,
    findings: &[Finding],
) -> Result<String> {
    let reasons: BTreeMap<&str, String> = findings
        .iter()
        .map(|f| {
            let reason = match &f.duplicate_of {
                Some(keep) => format!("tidy: duplicate of {keep}"),
                None => "tidy: session-only, not lasting".to_string(),
            };
            (f.fact_id.as_str(), reason)
        })
        .collect();
    let mut changes: Vec<(String, Change)> = Vec::new();
    let mut index_updates = BTreeMap::new();
    for (path, entity) in entities.values_mut() {
        let mut touched = false;
        for fact in entity.facts.iter_mut() {
            if let Some(reason) = reasons.get(fact.id.as_str()) {
                fact.status = FactStatus::Retracted;
                controls.add_suppression(Suppression {
                    target: fact.id.clone(),
                    kind: SuppressionKind::SoftForget,
                    suppressed_at: Utc::now(),
                    reason: Some(reason.clone()),
                });
                controls.add_retraction(fact.id.clone());
                touched = true;
            }
        }
        if touched {
            entity.refresh_body();
            index_updates.insert(path.clone(), Some(entity.clone()));
            changes.push((path.clone(), Change::Write { content: entity.render() }));
        }
    }
    changes.push((
        crate::index::INDEX_MD.to_string(),
        Change::Write { content: crate::index::index_md_after(repo, head, &index_updates)? },
    ));
    changes.push(("state/controls.json".to_string(), Change::Write { content: controls.render()? }));
    let request_id = format!("tidy_{}", Utc::now().timestamp_millis());
    Ok(repo
        .publish(head, &request_id, &ChangeSet::new(changes)?, &crate::validate::DomainValidator)?
        .revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{apply, ChangeKind, ChangeRequest};
    use crate::fact::{Fact, FactKind, Visibility};

    fn remember(repo: &GitRepo, req: &str, entity: &str, id: &str, statement: &str) {
        apply(
            repo,
            ChangeRequest {
                kind: ChangeKind::Remember,
                request_id: req.into(),
                scope_id: "personal".into(),
                session_id: "s".into(),
                source_event_id: "evt_1".into(),
                entity_id: entity.into(),
                fact: Some(Fact {
                    id: id.into(),
                    predicate: "rule".into(),
                    statement: statement.into(),
                    kind: FactKind::ExplicitAssertion,
                    status: FactStatus::Active,
                    observed_at: Utc::now(),
                    valid_from: None,
                    valid_from_precision: None,
                    valid_to: None,
                    expires_at: None,
                    review_after: None,
                    supersedes: vec![],
                    sources: vec![],
                    visibility: Visibility::Private,
                }),
                target_fact_id: None,
                reason: None,
                proposed_at: Utc::now(),
            },
        )
        .unwrap();
    }

    #[test]
    fn duplicates_are_found_and_forgotten_keeping_the_fullest() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        remember(&repo, "r1", "pref_user", "fact_full", "Never push; commit only when asked; never revert uncommitted code.");
        remember(&repo, "r2", "w_app", "fact_short", "User commits code; assistant never pushes.");
        remember(&repo, "r3", "w_app", "fact_other", "The backend deploys to Fly with fly deploy.");
        // Unit tests have no LLM key configured in LlmConfig unless the env
        // has one; duplicates do not need it.
        let kept = tidy(&repo, false, &["fact_short".to_string()]).unwrap();
        assert!(kept.findings.is_empty(), "--keep leaves a finding out");
        let dry = tidy(&repo, false, &[]).unwrap();
        let dup: Vec<_> = dry.findings.iter().filter(|f| f.kind == "duplicate").collect();
        assert_eq!(dup.len(), 1);
        assert_eq!(dup[0].fact_id, "fact_short");
        assert_eq!(dup[0].duplicate_of.as_deref(), Some("fact_full"));
        assert!(dry.applied_at.is_none());

        let head = repo.head().unwrap();
        let only_dups: Vec<Finding> = dup.into_iter().cloned().collect();
        let entities = repo
            .list_eligible_entity_files(&head)
            .unwrap()
            .into_iter()
            .map(|(p, b)| {
                let e = EntityFile::parse(&b.content).unwrap();
                (e.id.clone(), (p, e))
            })
            .collect();
        forget_all(&repo, &head, entities, Controls::default(), &only_dups).unwrap();
        let head = repo.head().unwrap();
        let controls = Controls::parse(&repo.read_snapshot(&head, "state/controls.json").unwrap().unwrap().content).unwrap();
        assert!(controls.is_suppressed("fact_short"));
        assert!(!controls.is_suppressed("fact_full"));
        assert!(!controls.is_suppressed("fact_other"));
    }
}
