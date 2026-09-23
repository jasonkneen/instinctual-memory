//! Erasure: remove a fact from everywhere `mem` keeps it.
//!
//! `forget` only suppresses a fact at read time. `erase` removes it:
//!
//! 1. **Current tree.** The fact is dropped from its entity file (the file
//!    is deleted when no facts remain), and `state/controls.json` records a
//!    completed deletion plus a suppression by id, so consolidation can never
//!    publish that fact again.
//! 2. **Journal.** Every source event the fact cites is redacted in place
//!    (content becomes `[erased]`).
//! 3. **History.** Every commit on the `memory` branch is rewritten without
//!    the fact, the reflog is expired, and unreachable objects are pruned,
//!    so `git log -p` on the store cannot recover it.
//! 4. **Intents.** Durable publication intents that mention the fact are
//!    deleted.
//!
//! The target is a fact id, or an entity id to erase all of its facts.
//! Re-running for the same target finds nothing left and changes nothing.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;

use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::controls::{Controls, DeletionRecord, DeletionState, Suppression, SuppressionKind};
use crate::entity::EntityFile;
use crate::error::{Error, Result};
use crate::fact::Fact;
use crate::journal::Journal;
use crate::paths::MEMORY_REF;
use crate::repo::{Change, ChangeSet, GitRepo};

/// What one erasure did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErasureReport {
    pub target: String,
    /// Fact ids removed from the current tree.
    pub facts_erased: Vec<String>,
    /// Journal events whose content was redacted.
    pub events_redacted: usize,
    /// Commits on the memory branch rewritten to drop the fact.
    pub commits_rewritten: usize,
    /// Durable intent files deleted because they held the fact.
    pub intents_removed: usize,
    /// Head of the memory branch after erasure.
    pub revision: String,
}

/// Erase `target` (a fact id or an entity id) from the tree, the journal,
/// the branch history, and pending intents.
pub fn erase(
    repo: &GitRepo,
    journal: &Journal,
    target: &str,
    reason: Option<&str>,
) -> Result<ErasureReport> {
    if target.trim().is_empty() {
        return Err(Error::Erasure("target must not be empty".into()));
    }
    let base = repo.head()?;
    let mut controls = repo
        .read_snapshot(&base, "state/controls.json")?
        .map(|b| Controls::parse(&b.content))
        .transpose()?
        .unwrap_or_default();

    // Step 1: current tree.
    let mut changes: Vec<(String, Change)> = Vec::new();
    let mut index_updates = std::collections::BTreeMap::new();
    let mut erased: Vec<Fact> = Vec::new();
    let mut erased_entities: Vec<String> = Vec::new();
    for (path, blob) in repo.list_eligible_entity_files(&base)? {
        let Ok(mut entity) = EntityFile::parse(&blob.content) else {
            continue;
        };
        let whole_entity = entity.id == target;
        if whole_entity {
            erased_entities.push(entity.id.clone());
        }
        let (gone, kept): (Vec<Fact>, Vec<Fact>) = entity
            .facts
            .drain(..)
            .partition(|f| whole_entity || f.id == target);
        if gone.is_empty() {
            entity.facts = kept;
            continue;
        }
        entity.facts = kept;
        if entity.facts.is_empty() {
            index_updates.insert(path.clone(), None);
            changes.push((path, Change::Delete));
        } else {
            entity.refresh_body();
            index_updates.insert(path.clone(), Some(entity.clone()));
            changes.push((path, Change::Write { content: entity.render() }));
        }
        erased.extend(gone);
    }
    if erased.is_empty() && !controls.is_deleted(target) && !controls.is_suppressed(target) {
        return Err(Error::Erasure(format!("{target}: no such fact or entity")));
    }

    let mut ids: Vec<String> = erased.iter().map(|f| f.id.clone()).collect();
    if ids.is_empty() {
        ids.push(target.to_string());
    }
    let source_events: HashSet<String> = erased
        .iter()
        .flat_map(|f| f.sources.iter().map(|s| s.event_id.clone()))
        .collect();
    for id in &ids {
        if !controls.is_suppressed(id) {
            controls.add_suppression(Suppression {
                target: id.clone(),
                kind: SuppressionKind::SoftForget,
                suppressed_at: Utc::now(),
                reason: reason.map(str::to_string),
            });
        }
        controls.deletions.retain(|d| &d.target != id);
        controls.deletions.insert(DeletionRecord {
            target: id.clone(),
            requested_at: Utc::now(),
            pipeline_state: DeletionState::Completed,
        });
    }
    for event_id in &source_events {
        controls.add_redaction(event_id.clone());
    }
    if !index_updates.is_empty() {
        changes.push((
            crate::index::INDEX_MD.to_string(),
            Change::Write { content: crate::index::index_md_after(repo, &base, &index_updates)? },
        ));
    }
    changes.push(("state/controls.json".to_string(), Change::Write { content: controls.render()? }));
    let request_id = format!("erase_{}_{}", short_hash(target), Utc::now().timestamp_millis());
    repo.publish(&base, &request_id, &ChangeSet::new(changes)?, &crate::validate::DomainValidator)?;

    // Step 2: journal.
    let events_redacted = journal.redact(&source_events)?;

    // Step 3: history.
    let mut needle_ids = ids.clone();
    // An erased entity's own id is scrubbed from history too (older
    // INDEX.md rows name it).
    needle_ids.extend(erased_entities.iter().map(|id| format!("`{id}`")));
    let needles = Needles::new(&erased, &needle_ids);
    let commits_rewritten = rewrite_history(repo, &needles)?;

    // Step 4: intents.
    let intents_removed = remove_intents(repo, &needles)?;

    Ok(ErasureReport {
        target: target.to_string(),
        facts_erased: erased.iter().map(|f| f.id.clone()).collect(),
        events_redacted,
        commits_rewritten,
        intents_removed,
        revision: repo.head()?,
    })
}

/// Strings that identify erased content: the fact ids and statements.
struct Needles {
    ids: Vec<String>,
    statements: Vec<String>,
}

impl Needles {
    fn new(erased: &[Fact], ids: &[String]) -> Self {
        Self {
            ids: ids.to_vec(),
            statements: erased
                .iter()
                .map(|f| f.statement.clone())
                .filter(|s| !s.trim().is_empty())
                .collect(),
        }
    }

    fn found_in(&self, text: &str) -> bool {
        self.ids.iter().any(|id| text.contains(id.as_str()))
            || self.statements.iter().any(|s| text.contains(s.as_str()))
    }
}

/// Scrub one blob. Entity files lose the erased facts (or are deleted when
/// none remain); other Markdown loses the lines that mention them. Returns
/// `None` to delete the path, `Some(content)` to replace it.
fn scrub_blob(path: &str, content: &str, needles: &Needles) -> Option<String> {
    if path.starts_with("memory/") && path.ends_with(".md") {
        if let Ok(mut entity) = EntityFile::parse(content) {
            entity
                .facts
                .retain(|f| !needles.ids.contains(&f.id) && !needles.statements.contains(&f.statement));
            if entity.facts.is_empty() {
                return None;
            }
            entity.body = scrub_lines(&entity.body, needles);
            return Some(entity.render());
        }
    }
    Some(scrub_lines(content, needles))
}

fn scrub_lines(text: &str, needles: &Needles) -> String {
    let mut out: String = text
        .lines()
        .filter(|line| !needles.found_in(line))
        .collect::<Vec<_>>()
        .join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Rewrite every commit on the memory branch without the erased content,
/// then expire the reflog and prune unreachable objects. Returns how many
/// commits changed.
fn rewrite_history(repo: &GitRepo, needles: &Needles) -> Result<usize> {
    let _lock = repo.acquire_writer_lock()?;
    let git = repo.git();
    let head = repo.head()?;
    let commits: Vec<String> = git
        .run_args(&["rev-list", "--reverse", "--topo-order", &head], None, None)?
        .lines()
        .map(str::to_string)
        .collect();

    let mut new_for: HashMap<String, String> = HashMap::new();
    // (path, blob oid) -> replacement: None means unchanged, Some(None)
    // delete, Some(Some(oid)) replace.
    let mut blob_cache: HashMap<(String, String), Option<Option<String>>> = HashMap::new();
    let mut rewritten = 0usize;
    let tmp = tempfile::tempdir()?;
    let index = tmp.path().join("index");

    for commit in &commits {
        let parents: Vec<String> = git
            .run_args(&["rev-list", "--parents", "-n", "1", commit], None, None)?
            .split_whitespace()
            .skip(1)
            .map(str::to_string)
            .collect();
        let new_parents: Vec<String> =
            parents.iter().map(|p| new_for.get(p).cloned().unwrap_or_else(|| p.clone())).collect();
        let tree = git.run_args(&["rev-parse", &format!("{commit}^{{tree}}")], None, None)?;
        let tree = tree.trim().to_string();

        let mut edits: Vec<(String, Option<String>)> = Vec::new();
        for line in git.run_args(&["ls-tree", "-r", &tree], None, None)?.lines() {
            let Some((meta, path)) = line.split_once('\t') else { continue };
            let mut parts = meta.split_whitespace();
            let (_mode, kind, oid) = (parts.next(), parts.next(), parts.next());
            if kind != Some("blob") {
                continue;
            }
            let Some(oid) = oid else { continue };
            // Statements only live in Markdown. JSON state (controls,
            // receipts, dispositions, checkpoint) holds ids by design.
            if !path.ends_with(".md") {
                continue;
            }
            let key = (path.to_string(), oid.to_string());
            let decision = match blob_cache.get(&key) {
                Some(d) => d.clone(),
                None => {
                    let content = git.run_args(&["cat-file", "blob", oid], None, None)?;
                    let d = if needles.found_in(&content) {
                        Some(match scrub_blob(path, &content, needles) {
                            None => None,
                            Some(new) => Some(
                                git.run_args(&["hash-object", "-w", "--stdin"], Some(new.as_bytes()), None)?
                                    .trim()
                                    .to_string(),
                            ),
                        })
                    } else {
                        None
                    };
                    blob_cache.insert(key, d.clone());
                    d
                }
            };
            if let Some(replacement) = decision {
                edits.push((path.to_string(), replacement));
            }
        }

        let new_tree = if edits.is_empty() {
            tree.clone()
        } else {
            git.run_args(&["read-tree", &tree], None, Some(&index))?;
            for (path, replacement) in &edits {
                match replacement {
                    Some(oid) => {
                        git.run_args(
                            &["update-index", "--add", "--cacheinfo", &format!("100644,{oid},{path}")],
                            None,
                            Some(&index),
                        )?;
                    }
                    None => {
                        let entry = format!("0 {}\t{path}\n", "0".repeat(tree.len()));
                        git.run_args(&["update-index", "--index-info"], Some(entry.as_bytes()), Some(&index))?;
                    }
                }
            }
            git.run_args(&["write-tree"], None, Some(&index))?.trim().to_string()
        };

        if new_tree == tree && new_parents == parents {
            new_for.insert(commit.clone(), commit.clone());
            continue;
        }
        let new_commit = recommit(repo, commit, &new_tree, &new_parents)?;
        new_for.insert(commit.clone(), new_commit);
        rewritten += 1;
    }

    let new_head = new_for.get(&head).cloned().unwrap_or_else(|| head.clone());
    if new_head != head {
        git.run_args(&["update-ref", MEMORY_REF, &new_head, &head], None, None)
            .map_err(|_| Error::Conflict("memory branch moved during erasure; run erase again".into()))?;
    }
    git.run_args(&["reflog", "expire", "--expire=now", "--all"], None, None)?;
    git.run_args(&["gc", "--prune=now", "--quiet"], None, None)?;
    Ok(rewritten)
}

/// Create a copy of `commit` with a new tree and parents, keeping its
/// message and dates.
fn recommit(repo: &GitRepo, commit: &str, tree: &str, parents: &[String]) -> Result<String> {
    let git = repo.git();
    let meta = git.run_args(&["show", "-s", "--format=%aI%n%cI", commit], None, None)?;
    let mut dates = meta.lines();
    let author_date = dates.next().unwrap_or("").to_string();
    let committer_date = dates.next().unwrap_or("").to_string();
    let message = git.run_args(&["show", "-s", "--format=%B", commit], None, None)?;

    let mut args: Vec<String> = vec!["commit-tree".into(), tree.to_string()];
    for p in parents {
        args.push("-p".into());
        args.push(p.clone());
    }
    let mut cmd = git.command(args.iter().map(String::as_str));
    cmd.env("GIT_AUTHOR_DATE", author_date)
        .env("GIT_COMMITTER_DATE", committer_date)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| Error::io(repo.repo_path(), e))?;
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(message.as_bytes()).map_err(|e| Error::io(repo.repo_path(), e))?;
    }
    let out = child.wait_with_output().map_err(|e| Error::io(repo.repo_path(), e))?;
    if !out.status.success() {
        return Err(Error::Git(format!(
            "commit-tree failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn remove_intents(repo: &GitRepo, needles: &Needles) -> Result<usize> {
    let dir = repo.intents_dir();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(0);
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        if needles.found_in(&content) {
            std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn short_hash(target: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(target.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{apply, ChangeKind, ChangeRequest};
    use crate::fact::{FactKind, FactStatus, SourceRef, Visibility, Role as FactRole};
    use crate::journal::{JournalEvent, Redaction, Role, Source};

    fn fact(id: &str, statement: &str, event_id: &str) -> Fact {
        Fact {
            id: id.into(),
            predicate: "note".into(),
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
            sources: vec![SourceRef { event_id: event_id.into(), role: FactRole::User, evidence: statement.into() }],
            visibility: Visibility::Private,
        }
    }

    fn remember(repo: &GitRepo, req: &str, f: Fact) {
        apply(
            repo,
            ChangeRequest {
                kind: ChangeKind::Remember,
                request_id: req.into(),
                scope_id: "personal".into(),
                session_id: "s".into(),
                source_event_id: f.sources[0].event_id.clone(),
                entity_id: "p_alex".into(),
                fact: Some(f),
                target_fact_id: None,
                reason: None,
                proposed_at: Utc::now(),
            },
        )
        .unwrap();
    }

    #[test]
    fn erase_removes_fact_from_tree_journal_and_history() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        let secret = journal
            .append(JournalEvent::new(
                "personal",
                "s",
                Role::User,
                Source::Chat { source_id: "m1".into(), occurred_at: None },
                "My home address is 12 Quiet Lane.",
                Redaction::None,
            ))
            .unwrap();
        remember(&repo, "req_keep", fact("fact_keep", "Alex writes Rust.", "evt_other"));
        remember(&repo, "req_secret", fact("fact_secret", "Alex lives at 12 Quiet Lane.", &secret.event_id));

        let report = erase(&repo, &journal, "fact_secret", Some("user asked")).unwrap();
        assert_eq!(report.facts_erased, vec!["fact_secret".to_string()]);
        assert_eq!(report.events_redacted, 1);
        assert!(report.commits_rewritten >= 1);

        // Current tree: gone; the other fact stays.
        let head = repo.head().unwrap();
        let alex = repo.read_snapshot(&head, "memory/people/alex.md").unwrap().unwrap();
        assert!(!alex.content.contains("Quiet Lane"));
        assert!(alex.content.contains("Alex writes Rust."));

        // History and every object in the store: gone.
        let log = repo.git().run_args(&["log", "-p", MEMORY_REF], None, None).unwrap();
        assert!(!log.contains("Quiet Lane"), "history still holds the statement");
        let all = repo
            .git()
            .run_args(&["cat-file", "--batch-all-objects", "--batch"], None, None)
            .unwrap();
        assert!(!all.contains("Quiet Lane"), "an object still holds the statement");

        // Journal: redacted.
        let events = journal.read_all().unwrap();
        assert_eq!(events[0].content, crate::journal::ERASED);

        // Controls: completed deletion; a second erase reports nothing new.
        let controls = Controls::parse(
            &repo.read_snapshot(&head, "state/controls.json").unwrap().unwrap().content,
        )
        .unwrap();
        assert!(controls.is_deleted("fact_secret"));
        let again = erase(&repo, &journal, "fact_secret", None).unwrap();
        assert!(again.facts_erased.is_empty());
        assert_eq!(again.events_redacted, 0);
    }

    #[test]
    fn unknown_target_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        assert!(erase(&repo, &journal, "fact_nope", None).is_err());
    }
}
