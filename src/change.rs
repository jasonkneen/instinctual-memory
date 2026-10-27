//! Immediate validated change operations.
//!
//! The only path through which the live agent writes to memory. Each
//! `ChangeRequest` is validated, persisted as a durable intent, published
//! through the trusted snapshot core with CAS, and acknowledged with a receipt.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::controls::Controls;
use crate::entity::EntityFile;
use crate::error::{Error, Result};
use crate::fact::Fact;
use crate::paths::{request_id as validate_request_id, revision_id};
use crate::repo::{Change, ChangeSet, GitRepo, Intent};
use crate::consolidate::{blank_entity, ENTITY_SOFT_LIMIT};
use crate::validate::DomainValidator;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Remember,
    Correct,
    Forget,
}

/// A validated change request from the live agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRequest {
    pub kind: ChangeKind,
    pub request_id: String,
    pub scope_id: String,
    pub session_id: String,
    pub source_event_id: String,
    pub entity_id: String,
    pub fact: Option<Fact>,
    pub target_fact_id: Option<String>,
    pub reason: Option<String>,
    pub proposed_at: DateTime<Utc>,
}

impl ChangeRequest {
    pub fn validate(&self) -> Result<()> {
        validate_request_id(&self.request_id)?;
        if self.entity_id.is_empty() {
            return Err(Error::InvalidChange("entity_id required".into()));
        }
        match self.kind {
            ChangeKind::Remember | ChangeKind::Correct => {
                if self.fact.is_none() {
                    return Err(Error::InvalidChange(format!(
                        "{} requires a fact payload",
                        self.kind_name()
                    )));
                }
            }
            ChangeKind::Forget => {
                if self.target_fact_id.is_none() {
                    return Err(Error::InvalidChange(
                        "forget requires target_fact_id".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn kind_name(&self) -> &'static str {
        match self.kind {
            ChangeKind::Remember => "remember",
            ChangeKind::Correct => "correct",
            ChangeKind::Forget => "forget",
        }
    }
}

/// The published outcome of a change request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeOutcome {
    pub request_id: String,
    pub revision: String,
    pub replayed: bool,
    pub change_kind: ChangeKind,
    pub target_id: String,
}

/// Apply a single change request.
pub fn apply(repo: &GitRepo, request: ChangeRequest) -> Result<ChangeOutcome> {
    request.validate()?;
    let base = repo.head()?;
    apply_against_base(repo, &base, request)
}

/// Apply a change request against an explicit base.
pub fn apply_against_base(
    repo: &GitRepo,
    base: &str,
    request: ChangeRequest,
) -> Result<ChangeOutcome> {
    request.validate()?;
    revision_id(base)?;

    let controls_path = "state/controls.json";
    let mut controls = repo
        .read_snapshot(base, controls_path)?
        .map(|b| Controls::parse(&b.content))
        .transpose()?
        .unwrap_or_default();
    let family = load_family(repo, base, &request.entity_id)?;
    // The fact this request is about, if it already lives in the entity or
    // one of its spill files (`<id>_2`, ...).
    let about = match request.kind {
        ChangeKind::Remember => request.fact.as_ref().map(|f| f.id.clone()),
        _ => request
            .target_fact_id
            .clone()
            .or_else(|| request.fact.as_ref().map(|f| f.id.clone())),
    };
    let holder = about
        .as_deref()
        .and_then(|id| family.iter().position(|(_, e)| e.facts.iter().any(|f| f.id == id)));

    let (entity_id, new_entity, affected_controls, affected_checkpoint) = match request.kind {
        ChangeKind::Remember => {
            let fact = request.fact.clone().expect("validated");
            // Upsert where the fact already is; otherwise the first file of the
            // family with room, or a new spill file once they are all full.
            let (id, mut entity) = match holder {
                Some(i) => family[i].clone(),
                None => family
                    .iter()
                    .find(|(_, e)| {
                        let mut pruned = e.clone();
                        pruned.prune_forgotten(&controls);
                        pruned.render().len() + fact.statement.len() + 600 <= ENTITY_SOFT_LIMIT
                    })
                    .cloned()
                    .unwrap_or_else(|| next_spill(&request.entity_id, &family)),
            };
            entity.upsert_fact(fact);
            (id, entity, None, None)
        }
        ChangeKind::Correct => {
            let Some((id, mut entity)) = holder.map(|i| family[i].clone()).or_else(|| family.first().cloned()) else {
                return Err(Error::InvalidChange(format!(
                    "correct: entity {} does not exist",
                    request.entity_id
                )));
            };
            let new_fact = request.fact.clone().expect("validated");
            let old_id = request
                .target_fact_id
                .clone()
                .unwrap_or_else(|| new_fact.id.clone());
            entity.supersede(&old_id, &new_fact.id)?;
            entity.upsert_fact(new_fact.clone());
            (id, entity, None, None)
        }
        ChangeKind::Forget => {
            let Some((id, mut entity)) = holder.map(|i| family[i].clone()).or_else(|| family.first().cloned()) else {
                return Err(Error::InvalidChange(format!(
                    "forget: entity {} does not exist",
                    request.entity_id
                )));
            };
            let target = request.target_fact_id.clone().expect("validated");
            entity.retract(&target)?;

            // The suppression record hides the fact and blocks re-extraction.
            controls.add_suppression(crate::controls::Suppression {
                target: target.clone(),
                kind: crate::controls::SuppressionKind::SoftForget,
                suppressed_at: Utc::now(),
                reason: request.reason.clone(),
            });
            controls.add_retraction(target.clone());
            (id, entity, Some((controls_path.to_string(), controls.render()?)), None)
        }
    };
    let entity_path = entity_path_for(&entity_id, &request.scope_id)?;
    // Forgotten facts are dropped from the file as it is written, so a full
    // entity frees up; Git history keeps their text.
    let mut new_entity = new_entity;
    new_entity.prune_forgotten(&controls);

    // Build the change set: entity, INDEX.md, controls (if any), and
    // checkpoint (if any).
    new_entity.refresh_body();
    let index_md = crate::index::index_md_after(
        repo,
        base,
        &[(entity_path.clone(), Some(new_entity.clone()))].into_iter().collect(),
    )?;
    let mut changes: Vec<(String, Change)> = Vec::new();
    changes.push((entity_path.clone(), Change::Write { content: new_entity.render() }));
    changes.push((crate::index::INDEX_MD.to_string(), Change::Write { content: index_md }));
    if let Some((path, content)) = affected_controls {
        changes.push((path, Change::Write { content }));
    }
    if let Some((path, content)) = affected_checkpoint {
        changes.push((path, Change::Write { content }));
    }

    let change_set = ChangeSet::new(changes)?;

    // Persist a durable intent so retries can recover the original base.
    let intent = Intent::new(&request.request_id, base, &change_set)?;
    repo.write_intent(&intent)?;

    let receipt = repo.publish(base, &request.request_id, &change_set, &DomainValidator)?;
    repo.complete_intent(&request.request_id)?;

    let target_id = match request.kind {
        ChangeKind::Forget => request.target_fact_id.clone().expect("validated"),
        _ => request.fact.as_ref().expect("validated").id.clone(),
    };
    Ok(ChangeOutcome {
        request_id: request.request_id.clone(),
        revision: receipt.revision,
        replayed: receipt.replayed,
        change_kind: request.kind,
        target_id,
    })
}

/// The entity and its spill files (`<id>_2`, `<id>_3`, ...) in order. A
/// missing base file yields a fresh entity, so a first remember creates it.
fn load_family(repo: &GitRepo, base: &str, entity_id: &str) -> Result<Vec<(String, EntityFile)>> {
    let mut family = Vec::new();
    for n in 1..=50 {
        let id = if n == 1 { entity_id.to_string() } else { format!("{entity_id}_{n}") };
        match repo.read_snapshot(base, &entity_path_for(&id, "")?)? {
            Some(blob) => family.push((id, EntityFile::parse(&blob.content)?)),
            None if n == 1 => family.push((id.clone(), new_entity(&id))),
            None => break,
        }
    }
    Ok(family)
}

/// A new, empty spill file after the last one in `family`.
fn next_spill(entity_id: &str, family: &[(String, EntityFile)]) -> (String, EntityFile) {
    let id = format!("{entity_id}_{}", family.len() + 1);
    let base = &family[0].1;
    let entity = blank_entity(&id, &base.entity_type, &base.title);
    (id, entity)
}

fn new_entity(id: &str) -> EntityFile {
    let entity_type = if id.starts_with("pref_") {
        "preference"
    } else {
        match id.chars().next().unwrap_or('p') {
            'p' => "person",
            'o' => "org",
            'w' => "workstream",
            'c' => "conversation",
            _ => "thing",
        }
    };
    EntityFile {
        schema_version: 2,
        id: id.to_string(),
        entity_type: entity_type.to_string(),
        title: id.to_string(),
        aliases: Vec::new(),
        links: Vec::new(),
        facts: Vec::new(),
        visibility: crate::fact::Visibility::Private,
        body: format!("# {id}\n"),
    }
}

fn entity_path_for(entity_id: &str, _scope_id: &str) -> Result<String> {
    let (bucket, stripped) = if entity_id.starts_with("pref_") {
        ("preferences", entity_id.strip_prefix("pref_").unwrap_or(entity_id))
    } else {
        let bucket = match entity_id.chars().next().unwrap_or('p') {
            'p' => "people",
            'o' => "orgs",
            'w' => "workstreams",
            'c' => "conversations",
            _ => return Err(Error::InvalidEntityId(entity_id.to_string())),
        };
        let stripped = entity_id
            .strip_prefix(|c: char| matches!(c, 'p' | 'o' | 'w' | 'c'))
            .unwrap_or(entity_id)
            .trim_start_matches('_');
        (bucket, stripped)
    };
    Ok(format!("memory/{bucket}/{stripped}.md"))
}

/// What happened to one durable intent left behind by an interrupted change.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Recovered {
    /// The change had not been published; it is now.
    Published { request_id: String, revision: String },
    /// The change had been published before the interruption.
    AlreadyPublished { request_id: String },
    /// Memory moved on since the intent was written, so it cannot be applied
    /// as-is. The intent is kept as `<request-id>.stale` for inspection and
    /// the change should be requested again.
    Stale { request_id: String },
}

/// Finish or set aside every intent in the durable intents directory. Run
/// before applying a new change so an interrupted one is never lost.
pub fn recover_in_dir(repo: &GitRepo) -> Result<Vec<Recovered>> {
    let mut outcomes = Vec::new();
    let dir = repo.intents_dir();
    if !dir.exists() {
        return Ok(outcomes);
    }
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| Error::io(dir, e))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    for path in paths {
        let bytes = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
        let intent: Intent = match serde_json::from_slice(&bytes) {
            Ok(intent) => intent,
            Err(_) => {
                set_aside(&path)?;
                continue;
            }
        };
        let change_set = ChangeSet::new(intent.changes.into_iter().collect::<Vec<_>>())?;
        match repo.publish(&intent.base, &intent.request_id, &change_set, &DomainValidator) {
            Ok(receipt) => {
                repo.complete_intent(&intent.request_id)?;
                outcomes.push(if receipt.replayed {
                    Recovered::AlreadyPublished { request_id: intent.request_id }
                } else {
                    Recovered::Published { request_id: intent.request_id, revision: receipt.revision }
                });
            }
            Err(Error::Conflict(_)) => {
                set_aside(&path)?;
                outcomes.push(Recovered::Stale { request_id: intent.request_id });
            }
            Err(err) => return Err(err),
        }
    }
    Ok(outcomes)
}

fn set_aside(path: &std::path::Path) -> Result<()> {
    let stale = path.with_extension("stale");
    std::fs::rename(path, &stale).map_err(|e| Error::io(path, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::FactStatus;
    use crate::journal::Role;
    use crate::fact::{FactKind, Visibility};
    use crate::journal::{JournalEvent, Source};

    fn remember_request(id: &str, statement: &str) -> ChangeRequest {
        ChangeRequest {
            kind: ChangeKind::Remember,
            request_id: id.into(),
            scope_id: "personal".into(),
            session_id: "s".into(),
            source_event_id: "evt_1".into(),
            entity_id: "p_alex".into(),
            fact: Some(crate::fact::Fact {
                id: format!("fact_{id}"),
                predicate: "note".into(),
                statement: statement.into(),
                kind: FactKind::ExplicitAssertion,
                status: crate::fact::FactStatus::Active,
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
        }
    }

    #[test]
    fn interrupted_change_is_recovered_and_stale_one_set_aside() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let base = repo.head().unwrap();
        // An intent written but never published (process died before publish).
        let pending = ChangeSet::new(vec![(
            "memory/people/alex.md".to_string(),
            Change::Write {
                content: {
                    let mut e = new_entity("p_alex");
                    e.upsert_fact(remember_request("r1", "Alex likes tea.").fact.unwrap());
                    e.refresh_body();
                    e.render()
                },
            },
        )])
        .unwrap();
        repo.write_intent(&Intent::new("r1", &base, &pending).unwrap()).unwrap();
        let out = recover_in_dir(&repo).unwrap();
        assert!(matches!(&out[..], [Recovered::Published { .. }]));
        assert!(repo.read_intent("r1").unwrap().is_none());
        // An intent against a base that is no longer current.
        repo.write_intent(&Intent::new("r2", &base, &pending).unwrap()).unwrap();
        let out = recover_in_dir(&repo).unwrap();
        assert_eq!(out, vec![Recovered::Stale { request_id: "r2".into() }]);
        assert!(repo.intents_dir().join("r2.stale").exists());
        // A new change still applies.
        apply(&repo, remember_request("r3", "Alex likes coffee.")).unwrap();
    }

    #[test]
    fn entity_path_construction() {
        assert_eq!(entity_path_for("p_alex", "s").unwrap(), "memory/people/alex.md");
        assert_eq!(entity_path_for("o_acme", "s").unwrap(), "memory/orgs/acme.md");
        assert_eq!(
            entity_path_for("w_office", "s").unwrap(),
            "memory/workstreams/office.md"
        );
    }

    #[test]
    fn remember_creates_new_entity() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let mut fact = Fact {
            id: "fact_x".into(),
            predicate: "home_city".into(),
            statement: "Lives in Bristol.".into(),
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
        };
        // Allow the synthetic id used in tests.
        fact.id = "fact_x".into();
        let req = ChangeRequest {
            kind: ChangeKind::Remember,
            request_id: "req_test_1".into(),
            scope_id: "personal".into(),
            session_id: "s".into(),
            source_event_id: "evt_0001".into(),
            entity_id: "p_test".into(),
            fact: Some(fact),
            target_fact_id: None,
            reason: None,
            proposed_at: Utc::now(),
        };
        let outcome = apply(&repo, req).unwrap();
        assert!(!outcome.replayed);
        let head = repo.head().unwrap();
        let body = repo.read_snapshot(&head, "memory/people/test.md").unwrap().unwrap();
        assert!(body.content.contains("Bristol"));
    }

    #[test]
    fn forget_adds_suppression() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let fact = Fact {
            id: "fact_x".into(),
            predicate: "home_city".into(),
            statement: "Lives in Bristol.".into(),
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
        };
        let req = ChangeRequest {
            kind: ChangeKind::Remember,
            request_id: "req_1".into(),
            scope_id: "personal".into(),
            session_id: "s".into(),
            source_event_id: "evt_0001".into(),
            entity_id: "p_test".into(),
            fact: Some(fact.clone()),
            target_fact_id: None,
            reason: None,
            proposed_at: Utc::now(),
        };
        apply(&repo, req).unwrap();
        let forget = ChangeRequest {
            kind: ChangeKind::Forget,
            request_id: "req_2".into(),
            scope_id: "personal".into(),
            session_id: "s".into(),
            source_event_id: "evt_0002".into(),
            entity_id: "p_test".into(),
            fact: None,
            target_fact_id: Some("fact_x".into()),
            reason: Some("user asked to forget".into()),
            proposed_at: Utc::now(),
        };
        apply(&repo, forget).unwrap();
        let head = repo.head().unwrap();
        let controls_body = repo
            .read_snapshot(&head, "state/controls.json")
            .unwrap()
            .unwrap();
        let controls = Controls::parse(&controls_body.content).unwrap();
        assert!(controls.is_suppressed("fact_x"));
        let _ = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat {
                source_id: "x".into(),
                occurred_at: None,
            },
            "y",
            crate::journal::Redaction::None,
        );
    }

    fn read_entity(repo: &GitRepo, path: &str) -> Option<EntityFile> {
        let head = repo.head().unwrap();
        repo.read_snapshot(&head, path).unwrap().map(|b| EntityFile::parse(&b.content).unwrap())
    }

    fn forget_request(id: &str, entity: &str, target: &str) -> ChangeRequest {
        ChangeRequest {
            kind: ChangeKind::Forget,
            request_id: id.into(),
            scope_id: "personal".into(),
            session_id: "s".into(),
            source_event_id: "evt_f".into(),
            entity_id: entity.into(),
            fact: None,
            target_fact_id: Some(target.into()),
            reason: Some("test".into()),
            proposed_at: Utc::now(),
        }
    }

    #[test]
    fn remember_spills_into_the_next_file_when_the_entity_is_full() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let long = "Alex keeps a detailed note about the project. ".repeat(40);
        let mut n = 0;
        while read_entity(&repo, "memory/people/alex_2.md").is_none() {
            n += 1;
            assert!(n < 200, "never spilled");
            apply(&repo, remember_request(&format!("r{n}"), &format!("{n}: {long}"))).unwrap();
        }
        let base = read_entity(&repo, "memory/people/alex.md").unwrap();
        assert!(base.render().len() <= ENTITY_SOFT_LIMIT, "the base file stays under the limit");
        let spill = read_entity(&repo, "memory/people/alex_2.md").unwrap();
        assert_eq!(spill.id, "p_alex_2");
        assert_eq!(spill.facts.len(), 1);

        // Re-remembering a fact updates it where it already lives.
        let again = remember_request(&format!("r{n}"), "updated");
        apply(&repo, ChangeRequest { request_id: "again".into(), ..again }).unwrap();
        let spill = read_entity(&repo, "memory/people/alex_2.md").unwrap();
        assert_eq!(spill.facts[0].statement, "updated");

        // Correct and forget find a fact in a spill file.
        apply(&repo, forget_request("f_spill", "p_alex", &format!("fact_r{n}"))).unwrap();
        let spill = read_entity(&repo, "memory/people/alex_2.md").unwrap();
        assert!(spill.facts.is_empty(), "the forgotten fact is pruned from its spill file");
    }

    #[test]
    fn forgotten_facts_free_their_room() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        apply(&repo, remember_request("keep", "Alex likes tea.")).unwrap();
        apply(&repo, remember_request("drop", &"Alex once said something long. ".repeat(50))).unwrap();
        let before = read_entity(&repo, "memory/people/alex.md").unwrap().render().len();
        apply(&repo, forget_request("f1", "p_alex", "fact_drop")).unwrap();
        let entity = read_entity(&repo, "memory/people/alex.md").unwrap();
        assert!(entity.render().len() < before / 2, "the file shrinks");
        assert_eq!(entity.facts.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), ["fact_keep"]);
        let head = repo.head().unwrap();
        let controls = Controls::parse(&repo.read_snapshot(&head, "state/controls.json").unwrap().unwrap().content).unwrap();
        assert!(controls.is_suppressed("fact_drop"), "still hidden and blocked from re-extraction");
    }
}
