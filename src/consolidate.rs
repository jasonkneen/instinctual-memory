//! Consolidation worker.
//!
//! Turns journal events into curated facts in the Git store. Two extractors:
//! - **rules**: a few high-confidence regex patterns, no model call.
//! - **llm**: sends user turns and memory files (AGENTS.md, CLAUDE.md, ...)
//!   to an OpenAI-compatible chat endpoint in bounded batches and asks for
//!   durable facts. A proposed fact is kept only when its evidence quote is
//!   found verbatim in the event it cites, so the model cannot invent a
//!   source. `Auto` picks llm when a key is configured, else rules.
//!
//! One run reads events after the checkpoint up to `high_water` of them,
//! and publishes the changed entity files, the advanced checkpoint, and a
//! per-batch disposition file (`state/dispositions/<request>.json`, one line
//! per outcome, keyed by journal seq) in one candidate commit.

use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkpoint::Checkpoint;
use crate::entity::EntityFile;
use crate::error::{Error, Result};
use crate::fact::{Fact, FactKind, FactStatus, Role as FactRole, SourceRef, Visibility};
use crate::journal::{Journal, JournalEvent, Role};
use crate::paths::request_id as review_id;
use crate::repo::{Change, ChangeSet, GitRepo};
use crate::validate::DomainValidator;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExtractorKind {
    Rules,
    Llm,
    /// Llm when a key is configured, else rules.
    Auto,
}

#[derive(Debug, Clone)]
pub struct ConsolidateOptions {
    pub extractor: ExtractorKind,
    pub request_id: String,
    pub high_water: u64,
    pub scope_id: Option<String>,
}

/// Result of one consolidation pass.
#[derive(Debug, Clone)]
pub struct ConsolidateReport {
    pub revision: String,
    pub through_seq: u64,
    pub events: usize,
    pub accepted: usize,
    pub facts: usize,
    pub entities_changed: usize,
    /// No events were left after the checkpoint.
    pub caught_up: bool,
    pub extractor: ExtractorKind,
}

/// Per-batch outcome record, stored next to the checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dispositions {
    pub request_id: String,
    pub extractor: ExtractorKind,
    pub from_seq: u64,
    pub through_seq: u64,
    /// Seqs that produced at least one fact.
    pub accepted: Vec<u64>,
    /// Reason -> seqs that produced none.
    pub quarantined: BTreeMap<String, Vec<u64>>,
}

/// What an extractor hands back to the publisher.
struct Extraction {
    entities: BTreeMap<String, EntityFile>,
    /// Fact ids that were forgotten or erased; never published again.
    blocked: std::collections::HashSet<String>,
    changed: BTreeSet<String>,
    accepted: BTreeSet<u64>,
    quarantined: BTreeMap<u64, String>,
    facts: usize,
}

/// Commits are capped at 64 files; leave room for checkpoint + dispositions.
const MAX_ENTITY_CHANGES: usize = 60;
/// Entity files must stay under the 64 KiB read limit.
const ENTITY_SOFT_LIMIT: usize = 56_000;

pub struct Consolidator;

impl Consolidator {
    /// Run a consolidation pass. Returns the published revision.
    pub fn run(repo: &GitRepo, journal: &Journal, options: &ConsolidateOptions) -> Result<String> {
        Ok(Self::run_batch(repo, journal, options)?.revision)
    }

    /// Run one bounded pass and report what it did.
    pub fn run_batch(
        repo: &GitRepo,
        journal: &Journal,
        options: &ConsolidateOptions,
    ) -> Result<ConsolidateReport> {
        review_id(&options.request_id)?;
        let base = repo.head()?;
        let mut checkpoint = repo
            .read_snapshot(&base, "state/checkpoint.json")?
            .map(|b| Checkpoint::parse(&b.content))
            .transpose()?
            .unwrap_or_else(Checkpoint::empty);

        let extractor = resolve_extractor(options.extractor);
        let all_after: Vec<JournalEvent> = journal.read_from(checkpoint.through_seq + 1)?;
        let Some(window_end) = all_after
            .iter()
            .take(options.high_water.max(1) as usize)
            .next_back()
            .map(|e| e.seq)
        else {
            return Ok(ConsolidateReport {
                revision: base,
                through_seq: checkpoint.through_seq,
                events: 0,
                accepted: 0,
                facts: 0,
                entities_changed: 0,
                caught_up: true,
                extractor,
            });
        };
        let from_seq = checkpoint.through_seq + 1;
        // The window is bounded by journal position so the checkpoint always
        // advances, even when the scope filter drops every event in it.
        let bounded: Vec<JournalEvent> = all_after
            .into_iter()
            .filter(|e| e.seq <= window_end)
            .filter(|e| options.scope_id.as_deref().is_none_or(|s| e.scope_id == s))
            .collect();

        let existing = load_entities(repo, &base)?;
        let controls = repo
            .read_snapshot(&base, "state/controls.json")?
            .map(|b| crate::controls::Controls::parse(&b.content))
            .transpose()?
            .unwrap_or_default();
        let blocked: std::collections::HashSet<String> = controls
            .suppressions
            .keys()
            .cloned()
            .chain(controls.deletions.iter().map(|d| d.target.clone()))
            .collect();
        let mut extraction = match extractor {
            ExtractorKind::Llm => llm_extract(existing, blocked, &bounded)?,
            _ => rules_extract(existing, blocked, &bounded),
        };
        retire_outdated_file_facts(&mut extraction, &bounded);
        if extraction.changed.len() > MAX_ENTITY_CHANGES {
            return Err(Error::BadChangeBatch(format!(
                "{} entities changed in one batch (max {MAX_ENTITY_CHANGES}); rerun with a smaller --high-water",
                extraction.changed.len()
            )));
        }

        let mut changes: Vec<(String, Change)> = Vec::new();
        for id in &extraction.changed {
            let entity = &extraction.entities[id];
            changes.push((entity_path(id), Change::Write { content: entity.render() }));
        }
        if !extraction.changed.is_empty() {
            changes.push((
                crate::index::INDEX_MD.to_string(),
                Change::Write { content: crate::index::render_index_md(extraction.entities.values()) },
            ));
        }

        let mut quarantined: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for (seq, reason) in &extraction.quarantined {
            quarantined.entry(reason.clone()).or_default().push(*seq);
        }
        let dispositions = Dispositions {
            request_id: options.request_id.clone(),
            extractor,
            from_seq,
            through_seq: window_end,
            accepted: extraction.accepted.iter().copied().collect(),
            quarantined,
        };
        changes.push((
            format!("state/dispositions/{}.json", options.request_id),
            Change::Write { content: serde_json::to_string(&dispositions)? },
        ));

        // Outcomes live in the disposition file; the checkpoint only records
        // how far the journal has been consolidated, so it stays small.
        checkpoint.accepted.clear();
        checkpoint.quarantined.clear();
        checkpoint.advance(window_end, Default::default(), Default::default())?;
        changes.push(("state/checkpoint.json".to_string(), Change::Write { content: checkpoint.render()? }));

        let change_set = ChangeSet::new(changes)?;
        let receipt = repo.publish(&base, &options.request_id, &change_set, &DomainValidator)?;
        Ok(ConsolidateReport {
            revision: receipt.revision,
            through_seq: window_end,
            events: bounded.len(),
            accepted: extraction.accepted.len(),
            facts: extraction.facts,
            entities_changed: extraction.changed.len(),
            caught_up: false,
            extractor,
        })
    }
}

/// A memory file (AGENTS.md, ...) that changed is a new event with a new id
/// (path + content hash). Facts that came only from older versions of that
/// file and were not extracted again from the new one no longer describe
/// the project: mark them superseded.
fn retire_outdated_file_facts(ex: &mut Extraction, events: &[JournalEvent]) {
    let versions: BTreeMap<String, String> = events
        .iter()
        .filter(|e| e.role == Role::Note && e.event_id.starts_with("evt_ide_md:"))
        .filter_map(|e| {
            let (prefix, _) = e.event_id.rsplit_once(':')?;
            Some((format!("{prefix}:"), e.event_id.clone()))
        })
        .collect();
    if versions.is_empty() {
        return;
    }
    for (id, entity) in ex.entities.iter_mut() {
        let mut touched = false;
        for fact in entity.facts.iter_mut().filter(|f| f.status == FactStatus::Active) {
            let outdated = !fact.sources.is_empty()
                && fact.sources.iter().all(|s| {
                    versions
                        .iter()
                        .any(|(prefix, current)| s.event_id.starts_with(prefix) && &s.event_id != current)
                });
            if outdated {
                fact.status = FactStatus::Superseded;
                touched = true;
            }
        }
        if touched {
            entity.refresh_body();
            ex.changed.insert(id.clone());
        }
    }
}

pub(crate) fn resolve_extractor(kind: ExtractorKind) -> ExtractorKind {
    match kind {
        ExtractorKind::Auto => {
            if LlmConfig::from_env().is_some() {
                ExtractorKind::Llm
            } else {
                ExtractorKind::Rules
            }
        }
        other => other,
    }
}

fn load_entities(repo: &GitRepo, base: &str) -> Result<BTreeMap<String, EntityFile>> {
    let mut entities = BTreeMap::new();
    for (_path, blob) in repo.list_eligible_entity_files(base)? {
        if let Ok(entity) = EntityFile::parse(&blob.content) {
            entities.insert(entity.id.clone(), entity);
        }
    }
    Ok(entities)
}

/// Deterministic extractor: matches a small set of high-confidence patterns
/// in user statements.
fn rules_extract(
    entities: BTreeMap<String, EntityFile>,
    blocked: std::collections::HashSet<String>,
    events: &[JournalEvent],
) -> Extraction {
    let name_re = Regex::new(r"(?i)my name is\s+([A-Z][a-zA-Z\-']+)").unwrap();
    let live_re = Regex::new(r"(?i)I (?:live|am living) in\s+([A-Z][a-zA-Z\-' ]+)").unwrap();
    let pref_re = Regex::new(r"(?i)please (?:always|never|don't)\s+([a-z][^.!?]+)").unwrap();

    let mut ex = Extraction::new(entities);
    ex.blocked = blocked;
    for event in events {
        if event.role != Role::User {
            ex.quarantined.insert(event.seq, "not a user statement".into());
            continue;
        }
        let mut found = Vec::new();
        for cap in name_re.captures_iter(&event.content) {
            let name = cap[1].to_string();
            found.push(Proposal {
                entity_id: format!("p_{}", slug(&name)),
                entity_type: "person".into(),
                title: name.clone(),
                predicate: "name".into(),
                statement: format!("User's name is {name}."),
                evidence: cap[0].to_string(),
            });
        }
        for cap in live_re.captures_iter(&event.content) {
            let city = cap[1].trim().to_string();
            found.push(Proposal {
                entity_id: "p_user".into(),
                entity_type: "person".into(),
                title: "User".into(),
                predicate: "home_city".into(),
                statement: format!("User lives in {city}."),
                evidence: cap[0].to_string(),
            });
        }
        for cap in pref_re.captures_iter(&event.content) {
            found.push(Proposal {
                entity_id: "pref_user".into(),
                entity_type: "preference".into(),
                title: "User preferences".into(),
                predicate: "comms_preference".into(),
                statement: format!("User requested: {}", cap[1].trim()),
                evidence: cap[0].to_string(),
            });
        }
        if found.is_empty() {
            ex.quarantined.insert(event.seq, "no high-confidence pattern matched".into());
            continue;
        }
        for proposal in found {
            ex.place(proposal, event);
        }
        ex.accepted.insert(event.seq);
    }
    ex
}

/// A fact the extractor wants to record, before it becomes a [`Fact`].
struct Proposal {
    entity_id: String,
    entity_type: String,
    title: String,
    predicate: String,
    statement: String,
    evidence: String,
}

impl Extraction {
    fn new(entities: BTreeMap<String, EntityFile>) -> Self {
        Self {
            entities,
            blocked: Default::default(),
            changed: BTreeSet::new(),
            accepted: BTreeSet::new(),
            quarantined: BTreeMap::new(),
            facts: 0,
        }
    }

    /// Add a proposal as an active fact. The fact id is a hash of entity,
    /// predicate, and statement, so re-extracting the same fact updates it
    /// instead of duplicating it. A full entity spills into `<id>_2`, ...
    fn place(&mut self, p: Proposal, event: &JournalEvent) {
        let digest = hex(&Sha256::digest(
            format!("{}|{}|{}", p.entity_id, p.predicate, p.statement.to_lowercase()).as_bytes(),
        ));
        let fact_id = format!("fact_{}", &digest[..12]);
        if self.blocked.contains(&fact_id) || self.restates_existing(&p, &fact_id) {
            if std::env::var("MEM_CONSOLIDATE_VERBOSE").is_ok_and(|v| v == "1") {
                eprintln!("[consolidate] dropped (forgotten, erased, or restates an existing fact): {}", p.statement);
            }
            return;
        }
        let fact = Fact {
            id: fact_id,
            predicate: p.predicate,
            statement: p.statement,
            kind: FactKind::ExplicitAssertion,
            status: FactStatus::Active,
            observed_at: event.occurred_at,
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            expires_at: None,
            review_after: None,
            supersedes: vec![],
            sources: vec![SourceRef {
                event_id: event.event_id.clone(),
                role: fact_role(event.role),
                evidence: p.evidence,
            }],
            visibility: Visibility::Private,
        };
        for n in 1..=50 {
            let id = if n == 1 { p.entity_id.clone() } else { format!("{}_{n}", p.entity_id) };
            let entity = self
                .entities
                .entry(id.clone())
                .or_insert_with(|| blank_entity(&id, &p.entity_type, &p.title));
            let exists = entity.facts.iter().any(|f| f.id == fact.id);
            if !exists && entity.render().len() + fact.statement.len() + 600 > ENTITY_SOFT_LIMIT {
                continue;
            }
            if !exists {
                self.facts += 1;
            }
            entity.upsert_fact(fact);
            entity.refresh_body();
            self.changed.insert(id);
            return;
        }
    }
}

impl Extraction {
    /// True when the entity (or one of its spill files) already holds an
    /// active fact that says nearly the same thing in other words.
    fn restates_existing(&self, p: &Proposal, fact_id: &str) -> bool {
        let words = content_words(&p.statement);
        if words.is_empty() {
            return false;
        }
        // The same rule often lands on both the project and the user's
        // preferences. Check both sides, whichever was stored first. Two
        // different projects are not compared with each other.
        let family = |base: &str, id: &str| {
            id == base || id.strip_prefix(base).and_then(|r| r.strip_prefix('_')).is_some_and(|n| n.parse::<u32>().is_ok())
        };
        let proposal_is_pref = family("pref_user", &p.entity_id);
        self.entities
            .iter()
            .filter(|(id, _)| {
                family(&p.entity_id, id)
                    || family("pref_user", id)
                    || (proposal_is_pref && id.starts_with("w_"))
            })
            .flat_map(|(_, e)| e.facts.iter())
            .filter(|f| f.status == FactStatus::Active && f.id != fact_id)
            .any(|f| {
                let other = content_words(&f.statement);
                jaccard(&words, &other) >= NEAR_DUPLICATE || contained(&words, &other)
            })
    }
}

/// Word-overlap ratio at which two statements count as the same fact.
pub(crate) const NEAR_DUPLICATE: f64 = 0.6;

const STOPWORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "be", "to", "of", "in", "on", "for", "and", "or", "with",
    "that", "this", "it", "its", "as", "by", "at", "from", "has", "have", "uses", "use",
    "project", "user", "includes", "include", "consists", "assistant", "agent", "must", "always",
    "should", "only", "when", "not", "no", "never", "do", "does", "wants", "want", "prefers",
];

pub(crate) fn content_words(s: &str) -> BTreeSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !STOPWORDS.contains(w))
        .map(crate::search::lexical::stem)
        .collect()
}

/// The shorter statement's meaningful words (three or more) are almost all
/// in the other: a paraphrase or a subset of an existing rule.
pub(crate) fn contained(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    small.len() >= 3 && small.intersection(large).count() as f64 / small.len() as f64 >= 0.75
}

pub(crate) fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 { 0.0 } else { inter / union }
}

fn fact_role(role: Role) -> FactRole {
    match role {
        Role::User => FactRole::User,
        Role::Assistant => FactRole::Assistant,
        Role::Tool => FactRole::Tool,
        Role::System => FactRole::System,
        Role::Note => FactRole::Note,
    }
}

fn blank_entity(id: &str, entity_type: &str, title: &str) -> EntityFile {
    EntityFile {
        schema_version: 2,
        id: id.to_string(),
        entity_type: entity_type.to_string(),
        title: title.to_string(),
        aliases: vec![],
        links: vec![],
        facts: vec![],
        visibility: Visibility::Private,
        body: format!("# {title}\n"),
    }
}

fn entity_path(id: &str) -> String {
    let (bucket, rest) = if let Some(rest) = id.strip_prefix("pref_") {
        ("preferences", rest)
    } else if let Some(rest) = id.strip_prefix("p_") {
        ("people", rest)
    } else if let Some(rest) = id.strip_prefix("o_") {
        ("orgs", rest)
    } else if let Some(rest) = id.strip_prefix("w_") {
        ("workstreams", rest)
    } else {
        ("conversations", id.strip_prefix("c_").unwrap_or(id))
    };
    format!("memory/{bucket}/{rest}.md")
}

pub(crate) fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    out.chars().take(60).collect::<String>().trim_end_matches('_').to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- LLM path

pub(crate) struct LlmConfig {
    url: String,
    key: String,
    model: String,
}

impl LlmConfig {
    /// `MEM_LLM_API_KEY`, else the OpenRouter key JEV uses (including one
    /// in a project `.env`).
    pub(crate) fn from_env() -> Option<Self> {
        // Unit tests never call out to a model.
        if cfg!(test) {
            return None;
        }
        crate::jev::load_jev_dotenv();
        let key = ["MEM_LLM_API_KEY", "OPENROUTER_API_KEY"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))?;
        let base = std::env::var("MEM_LLM_BASE_URL")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
        Some(Self {
            url: format!("{}/chat/completions", base.trim_end_matches('/')),
            key,
            model: std::env::var("MEM_LLM_MODEL")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "anthropic/claude-haiku-4.5".into()),
        })
    }
}

/// Characters of one event sent to the model.
const USER_EVENT_CHARS: usize = 2_000;
const NOTE_EVENT_CHARS: usize = 8_000;
/// Characters of event text per request.
const BATCH_CHARS: usize = 16_000;

const EXTRACTOR_SYSTEM_PROMPT: &str = r#"You extract durable memory for a coding agent from a user's own messages and project memory files.

A fact is worth keeping only if it will still be true and useful in a session weeks from now, in a different task: the user's identity and role, standing preferences and working rules, project facts (what it is, stack, architecture, conventions, commands, decisions and why), and named people or organisations with a stated relationship.

The test for every candidate: would a new teammate, starting fresh next month, need to know this? If it only describes what was happening in that session, it is not a fact.

Do NOT record:
- requests to change something ("move the composer up 4px", "the theme toggle should have no border", "add a terminal panel", "fix the ChatGPT connection"): they are tasks, not facts, even when phrased as "X should be ...";
- what the user was doing, running, asking, or looking at ("User is running start:electron", "User requested a review of X", "User is frustrated with the approval UI", "User's working directory is ...");
- session goals, plans, or task lists ("User set a /goal for this session", "Review scope spans ...", "three standing issues tracked");
- bug reports and current state ("X is not working", "the spinner never stops");
- questions, status checks, debugging chatter, error output, pasted logs;
- text written for an AI by a tool (skill files, system prompts, hook or goal boilerplate, conversation summaries), anything the assistant said, or guesses;
- a fact already listed under existing_entities, even in other words.

Entities: each event names its "project" when known. Project conventions, decisions, and the user's project-specific wishes are "project" facts about that event's project. Put the user's general working style in one "preference" entity named "user". Reuse an existing entity whenever one fits; do not create many small entities.

Return JSON: {"facts": [{"ref": <event ref number>, "lasting": true, "entity_type": "person"|"org"|"project"|"preference", "entity_name": "<short name; reuse an existing entity id when one fits>", "predicate": "<snake_case>", "statement": "<one self-contained sentence, written as a standing fact, not as something that happened>", "evidence": "<exact quote copied from that event, 8-200 chars>"}]}

Set "lasting" to false (or leave the fact out) for anything that fails the test above. The evidence must be copied character-for-character from the cited event. Return {"facts": []} when nothing qualifies.

user_message events: most have nothing worth keeping.
memory_file events are curated project instructions (AGENTS.md, CLAUDE.md, README.md): record every distinct standing rule, convention, architecture fact, and command in them, one fact each; skip only examples, history, and prose that states nothing."#;

#[derive(Debug, Deserialize)]
struct LlmOutput {
    #[serde(default)]
    facts: Vec<LlmFact>,
}

#[derive(Debug, Clone, Deserialize)]
struct LlmFact {
    #[serde(rename = "ref")]
    event_ref: u64,
    /// The model's judgement that this stays true beyond the session.
    #[serde(default)]
    lasting: bool,
    entity_type: String,
    entity_name: String,
    predicate: String,
    statement: String,
    evidence: String,
}

fn llm_extract(
    entities: BTreeMap<String, EntityFile>,
    blocked: std::collections::HashSet<String>,
    events: &[JournalEvent],
) -> Result<Extraction> {
    let cfg = LlmConfig::from_env().ok_or_else(|| {
        Error::Validation("LLM extractor needs MEM_LLM_API_KEY or OPENROUTER_API_KEY".into())
    })?;
    let mut ex = Extraction::new(entities);
    ex.blocked = blocked;

    // Only the user's own words and memory files can carry durable facts.
    let mut candidates: Vec<(&JournalEvent, String)> = Vec::new();
    for event in events {
        let text = match event.role {
            Role::Note => Some(event.content.clone()),
            Role::User => user_words(&event.content),
            _ => {
                ex.quarantined.insert(event.seq, "not a user statement or memory file".into());
                continue;
            }
        };
        match text.filter(|t| !t.trim().is_empty()) {
            // A memory file is read whole, in paragraph-aligned chunks, so a
            // long AGENTS.md is not cut off at the first few kilobytes.
            Some(text) if event.role == Role::Note => {
                for chunk in chunks(&text, NOTE_EVENT_CHARS) {
                    candidates.push((event, chunk));
                }
            }
            Some(text) => candidates.push((event, text)),
            None => {
                ex.quarantined.insert(event.seq, "harness-injected text, not the user's words".into());
            }
        }
    }

    let mut batches: Vec<Vec<(&JournalEvent, String)>> = Vec::new();
    let mut size = 0usize;
    for (event, text) in candidates {
        let text = clip(event.role, &text);
        let len = text.len();
        if batches.is_empty() || size + len > BATCH_CHARS {
            batches.push(Vec::new());
            size = 0;
        }
        size += len;
        batches.last_mut().expect("pushed above").push((event, text));
    }

    let known = known_entities(&ex.entities);
    let replies: Vec<Result<LlmOutput>> = batches
        .par_iter()
        .map(|batch| call_llm(&cfg, &known, batch))
        .collect();

    let by_seq: BTreeMap<u64, &JournalEvent> = events.iter().map(|e| (e.seq, e)).collect();
    let verbose = std::env::var("MEM_CONSOLIDATE_VERBOSE").is_ok_and(|v| v == "1");
    for (batch, reply) in batches.iter().zip(replies) {
        let reply = reply?;
        let in_batch: BTreeSet<u64> = batch.iter().map(|(e, _)| e.seq).collect();
        for fact in reply.facts {
            if !in_batch.contains(&fact.event_ref) {
                continue;
            }
            let event = by_seq[&fact.event_ref];
            match check_llm_fact(&fact, event, &ex.entities) {
                Ok(proposal) => {
                    ex.place(proposal, event);
                    ex.accepted.insert(event.seq);
                }
                Err(reason) if verbose => eprintln!(
                    "[consolidate] rejected ({reason}): {} | evidence: {}",
                    fact.statement, fact.evidence
                ),
                Err(_) => {}
            }
        }
    }
    // A memory file spans several chunks and batches; decide per event only
    // once every chunk has been read.
    for (event, _) in batches.iter().flatten() {
        if !ex.accepted.contains(&event.seq) {
            ex.quarantined.insert(event.seq, "no durable fact".into());
        }
    }
    Ok(ex)
}

/// Split `text` into pieces of at most `max` characters, breaking at blank
/// lines when possible, else at line ends, else mid-line.
fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for para in text.split("\n\n") {
        let para_len = para.chars().count();
        if current.chars().count() + para_len + 2 > max && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if para_len > max {
            for line in para.lines() {
                if current.chars().count() + line.chars().count() + 1 > max && !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                let mut rest: String = line.to_string();
                while rest.chars().count() > max {
                    let head: String = rest.chars().take(max).collect();
                    rest = rest.chars().skip(max).collect();
                    out.push(head);
                }
                current.push_str(&rest);
                current.push('\n');
            }
        } else {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(para);
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn clip(role: Role, text: &str) -> String {
    let cap = if role == Role::Note { NOTE_EVENT_CHARS } else { USER_EVENT_CHARS };
    text.chars().take(cap).collect()
}

/// Harness text that arrives in a user turn but was not typed by the user.
const INJECTED_PREFIXES: &[&str] = &[
    "<codex_internal_context",
    "<environment_context",
    "<user_instructions",
    "# AGENTS.md instructions for",
    "<local-command-stdout",
    "<local-command-caveat",
    "<turn_aborted",
    "<task-notification",
    "<system-reminder",
    "<skill",
    "<attached_files",
    "Base directory for this skill",
    "Caveat: The messages below were generated",
    // Claude Code compaction summaries are model-written, not the user's.
    "This session is being continued from a previous conversation",
    "Summary:\n1. Primary Request and Intent",
];

/// The user's own words in a user turn, or `None` when the whole turn is
/// harness-injected. Slash commands keep only their arguments; image tags
/// and pasted temp-file paths are removed.
fn user_words(content: &str) -> Option<String> {
    // Harness blocks can sit anywhere in a turn (a system reminder carrying
    // CLAUDE.md, command output); only the rest is the user's.
    let injected = Regex::new(
        r"(?s)<(system-reminder|local-command-stdout|local-command-caveat|task-notification|environment_context|user_instructions|codex_internal_context|skill|attached_files|turn_aborted)(?:\s[^>]*)?>.*?</(system-reminder|local-command-stdout|local-command-caveat|task-notification|environment_context|user_instructions|codex_internal_context|skill|attached_files|turn_aborted)>",
    )
    .unwrap();
    let without_blocks = injected.replace_all(content, "");
    let trimmed = without_blocks.trim_start();
    if trimmed.is_empty() || INJECTED_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return None;
    }
    if trimmed.starts_with("<command-") {
        let args = trimmed.split("<command-args>").nth(1)?.split("</command-args>").next()?;
        return Some(args.trim().to_string());
    }
    let tag = Regex::new(r"(?s)<image[^>]*>.*?</image>|<image[^>]*/?>|\[Image #\d+\]").unwrap();
    let text = tag.replace_all(trimmed, "");
    let kept: Vec<&str> = text
        .lines()
        .filter(|l| !(l.trim_start().starts_with("/var/folders/") || l.trim_start().starts_with("/tmp/")))
        .collect();
    Some(kept.join("\n"))
}

/// Entity ids, titles, and their current statements, so the model reuses
/// entities and does not restate what is already stored. Bounded in size.
fn known_entities(entities: &BTreeMap<String, EntityFile>) -> Vec<serde_json::Value> {
    let mut budget = 8_000usize;
    let mut out = Vec::new();
    for e in entities.values().take(200) {
        let mut facts = Vec::new();
        for f in e.facts.iter().filter(|f| f.status == FactStatus::Active).take(40) {
            if budget < f.statement.len() {
                break;
            }
            budget -= f.statement.len();
            facts.push(f.statement.clone());
        }
        out.push(serde_json::json!({"id": e.id, "title": e.title, "facts": facts}));
    }
    out
}

fn call_llm(
    cfg: &LlmConfig,
    known: &[serde_json::Value],
    batch: &[(&JournalEvent, String)],
) -> Result<LlmOutput> {
    let events: Vec<serde_json::Value> = batch
        .iter()
        .map(|(e, text)| {
            let kind = if e.role == Role::Note { "memory_file" } else { "user_message" };
            serde_json::json!({"ref": e.seq, "kind": kind, "project": event_project(e), "text": text})
        })
        .collect();
    let user = serde_json::json!({"existing_entities": known, "events": events}).to_string();
    chat_json(cfg, EXTRACTOR_SYSTEM_PROMPT, &user)
}

/// One chat completion that must return a JSON object of type `T`, with up
/// to three attempts.
pub(crate) fn chat_json<T: serde::de::DeserializeOwned>(cfg: &LlmConfig, system: &str, user: &str) -> Result<T> {
    let body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0,
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    let mut last_err = String::new();
    for _attempt in 0..3 {
        let sent = ureq::post(&cfg.url)
            .set("Authorization", &format!("Bearer {}", cfg.key))
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(120))
            .send_json(body.clone());
        let payload: serde_json::Value = match sent.map(|r| r.into_json()) {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                last_err = format!("decode: {e}");
                continue;
            }
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        let Some(content) = payload["choices"][0]["message"]["content"].as_str() else {
            last_err = "response has no message content".into();
            continue;
        };
        // Take the first JSON value; models sometimes add prose after it.
        let text = strip_fence(content);
        let start = text.find('{').unwrap_or(0);
        let mut values = serde_json::Deserializer::from_str(&text[start..]).into_iter::<T>();
        match values.next() {
            Some(Ok(out)) => return Ok(out),
            Some(Err(e)) => last_err = format!("parse: {e}"),
            None => last_err = "empty response".into(),
        }
    }
    Err(Error::Usage(format!(
        "model call failed after 3 attempts ({last_err}); nothing from this batch was published, re-run to resume"
    )))
}

/// The project an event belongs to: recorded at ingest, else for a memory
/// file the folder holding it (`.../agensis/AGENTS.md` -> `agensis`).
fn event_project(event: &JournalEvent) -> String {
    if let Some(name) = event.project_name() {
        return name.to_string();
    }
    let crate::journal::Source::IdeHistory { source_id, .. } = &event.source else {
        return String::new();
    };
    let path = source_id.strip_prefix("md:").unwrap_or(source_id);
    let path = path.rsplit_once(':').map_or(path, |(p, _)| p);
    std::path::Path::new(path)
        .parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

fn strip_fence(s: &str) -> &str {
    let s = s.trim();
    let s = s.strip_prefix("```json").or_else(|| s.strip_prefix("```")).unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// Accept a model-proposed fact only if its evidence is really in the event.
fn check_llm_fact(
    fact: &LlmFact,
    event: &JournalEvent,
    entities: &BTreeMap<String, EntityFile>,
) -> std::result::Result<Proposal, &'static str> {
    let evidence = fact.evidence.trim();
    let statement = fact.statement.trim();
    if !fact.lasting {
        return Err("not lasting (true for one session only)");
    }
    if evidence.chars().count() < 8 {
        return Err("evidence too short");
    }
    if !(8..=600).contains(&statement.chars().count()) {
        return Err("statement empty or over 600 characters");
    }
    if !quote_found(&event.content, evidence) {
        return Err("evidence not found in the cited event");
    }
    let predicate = slug(&fact.predicate);
    if predicate.is_empty() {
        return Err("no predicate");
    }
    let project = event_project(event);
    let is_project_fact = !matches!(fact.entity_type.as_str(), "person" | "org" | "preference");
    let entity_id = if is_project_fact && !slug(&project).is_empty() {
        // Project facts go on the event's own project, whatever name the
        // model chose, so one project's facts never land on another's.
        format!("w_{}", slug(&project))
    } else if entities.contains_key(fact.entity_name.trim()) {
        fact.entity_name.trim().to_string()
    } else {
        let prefix = match fact.entity_type.as_str() {
            "person" => "p",
            "org" => "o",
            "preference" => "pref",
            _ => "w",
        };
        let mut name = slug(&fact.entity_name);
        for known in ["pref_", "p_", "o_", "w_", "c_"] {
            if let Some(rest) = name.strip_prefix(known) {
                name = rest.to_string();
                break;
            }
        }
        if name.is_empty() {
            return Err("no entity name");
        }
        format!("{prefix}_{name}")
    };
    let title = entities.get(&entity_id).map(|e| e.title.clone()).unwrap_or_else(|| {
        if is_project_fact && !project.is_empty() {
            project.clone()
        } else {
            fact.entity_name.trim().to_string()
        }
    });
    Ok(Proposal {
        entity_type: match fact.entity_type.as_str() {
            "person" | "org" | "preference" => fact.entity_type.clone(),
            _ => "project".into(),
        },
        entity_id,
        title,
        predicate,
        statement: statement.to_string(),
        evidence: evidence.to_string(),
    })
}

/// The evidence is verbatim when every fragment of it (split where the model
/// elided text with `...` or `…`) appears in the source, in order.
fn quote_found(source: &str, evidence: &str) -> bool {
    let haystack = normalise(source);
    let fragments: Vec<String> = evidence
        .replace('…', "...")
        .split("...")
        .map(normalise)
        .filter(|f| f.chars().count() >= 8)
        .collect();
    if fragments.is_empty() {
        return false;
    }
    let mut from = 0usize;
    for fragment in &fragments {
        match haystack[from..].find(fragment.as_str()) {
            Some(at) => from += at + fragment.len(),
            None => return false,
        }
    }
    true
}

/// Text for the verbatim-evidence comparison: Markdown emphasis, code, and
/// heading marks are dropped and whitespace collapsed on both sides, so a
/// quote of `**Runtime bootstrap** — \`server/index.cjs\`` still matches.
fn normalise(s: &str) -> String {
    // List markers at line starts ("- ", "+ ", "12. ") are layout, not words.
    let unlisted: String = s
        .lines()
        .map(|line| {
            let t = line.trim_start();
            let t = t.strip_prefix("- ").or_else(|| t.strip_prefix("+ ")).unwrap_or(t);
            let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 && t[digits..].starts_with(". ") {
                t[digits + 2..].to_string()
            } else {
                t.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let stripped: String = unlisted
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '#' | '>' | '|'))
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            other => other,
        })
        .collect();
    stripped.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Redaction, Source};
    use chrono::Utc;

    fn user(journal: &Journal, source_id: &str, text: &str) {
        journal
            .append(JournalEvent::new(
                "personal",
                "session",
                Role::User,
                Source::Chat { source_id: source_id.into(), occurred_at: None },
                text,
                Redaction::None,
            ))
            .unwrap();
    }

    fn options(id: &str) -> ConsolidateOptions {
        ConsolidateOptions {
            extractor: ExtractorKind::Rules,
            request_id: id.into(),
            high_water: 100,
            scope_id: Some("personal".into()),
        }
    }

    #[test]
    fn rules_extracts_name_and_live_in() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        user(&journal, "m1", "Hello, my name is Alice.");
        user(&journal, "m2", "I live in Bristol.");
        let rev = Consolidator::run(&repo, &journal, &options("consol_1")).unwrap();
        assert!(rev.len() >= 40);
        let head = repo.head().unwrap();
        let alice = repo.read_snapshot(&head, "memory/people/alice.md").unwrap().unwrap();
        assert!(alice.content.contains("Alice"));
        assert!(alice.content.contains("## Current facts"));
    }

    #[test]
    fn second_batch_keeps_earlier_entities_and_checkpoint_stays_small() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        user(&journal, "m1", "my name is Alice");
        Consolidator::run(&repo, &journal, &options("consol_a")).unwrap();
        for i in 0..300 {
            user(&journal, &format!("n{i}"), "nothing durable here");
        }
        user(&journal, "m2", "my name is Bob");
        let report = Consolidator::run_batch(&repo, &journal, &options("consol_b")).unwrap();
        assert_eq!(report.through_seq, 101);
        while !Consolidator::run_batch(&repo, &journal, &options(&format!("consol_{}", Utc::now().timestamp_nanos_opt().unwrap())))
            .unwrap()
            .caught_up
        {}
        let head = repo.head().unwrap();
        assert!(repo.read_snapshot(&head, "memory/people/alice.md").unwrap().is_some());
        assert!(repo.read_snapshot(&head, "memory/people/bob.md").unwrap().is_some());
        let cp = repo.read_snapshot(&head, "state/checkpoint.json").unwrap().unwrap();
        assert!(cp.content.len() < 1_000, "{}", cp.content.len());
        assert!(Checkpoint::parse(&cp.content).unwrap().through_seq == 302);
    }

    #[test]
    fn suppressed_fact_is_not_extracted_again() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        user(&journal, "m1", "my name is Alice");
        Consolidator::run(&repo, &journal, &options("consol_1")).unwrap();
        let head = repo.head().unwrap();
        let alice = EntityFile::parse(
            &repo.read_snapshot(&head, "memory/people/alice.md").unwrap().unwrap().content,
        )
        .unwrap();
        let fact_id = alice.facts[0].id.clone();
        crate::erasure::erase(&repo, &journal, &fact_id, None).unwrap();
        user(&journal, "m2", "my name is Alice");
        Consolidator::run(&repo, &journal, &options("consol_2")).unwrap();
        let head = repo.head().unwrap();
        assert!(repo.read_snapshot(&head, "memory/people/alice.md").unwrap().is_none());
    }

    #[test]
    fn same_fact_twice_is_one_fact() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        user(&journal, "m1", "my name is Alice");
        user(&journal, "m2", "my name is Alice");
        Consolidator::run(&repo, &journal, &options("consol_1")).unwrap();
        let head = repo.head().unwrap();
        let alice = repo.read_snapshot(&head, "memory/people/alice.md").unwrap().unwrap();
        let entity = EntityFile::parse(&alice.content).unwrap();
        assert_eq!(entity.facts.len(), 1);
    }

    #[test]
    fn llm_fact_without_verbatim_evidence_is_rejected() {
        let event = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat { source_id: "x".into(), occurred_at: None },
            "Please always run   cargo test before pushing.",
            Redaction::None,
        );
        let entities = BTreeMap::new();
        let good = LlmFact {
            event_ref: 1,
            lasting: true,
            entity_type: "preference".into(),
            entity_name: "workflow".into(),
            predicate: "testing rule".into(),
            statement: "Run cargo test before pushing.".into(),
            evidence: "always run cargo test before pushing".into(),
        };
        let p = check_llm_fact(&good, &event, &entities).unwrap();
        assert_eq!(p.entity_id, "pref_workflow");
        assert_eq!(p.predicate, "testing_rule");
        let markdown_event = JournalEvent::new(
            "personal",
            "s",
            Role::Note,
            Source::Chat { source_id: "md".into(), occurred_at: None },
            "1. **Runtime bootstrap** — `server/index.cjs` `ensureRuntimeSchema`: idempotent",
            Redaction::None,
        );
        let quoted = LlmFact {
            evidence: "Runtime bootstrap — server/index.cjs ensureRuntimeSchema".into(),
            ..good.clone()
        };
        assert!(check_llm_fact(&quoted, &markdown_event, &entities).is_ok());
        // Elided quotes: every fragment must exist, in order.
        assert!(quote_found("Alpha beta gamma delta. Middle bit. Epsilon zeta eta theta.", "Alpha beta gamma... Epsilon zeta eta"));
        assert!(!quote_found("Alpha beta gamma delta. Epsilon zeta eta theta.", "Epsilon zeta eta... Alpha beta gamma"));
        assert!(!quote_found("Alpha beta gamma delta.", "Alpha beta gamma... invented clause here"));
        assert!(quote_found("Node\u{2019}s built-in runner \u{2014} fast", "Node's built-in runner - fast"));
        assert!(quote_found(
            "Deployed to **Fly** (`fly deploy`).\n- `netlify/functions/backend.mjs` \u{2014} serverless HTTP mirror.\n1. Runtime bootstrap first",
            "Deployed to Fly (fly deploy). netlify/functions/backend.mjs - serverless HTTP mirror. Runtime bootstrap first"
        ));
        let invented = LlmFact { evidence: "always deploy on Fridays".into(), ..good };
        assert!(check_llm_fact(&invented, &event, &entities).is_err());
    }

    #[test]
    fn injected_turns_are_not_user_words() {
        assert!(user_words("<codex_internal_context source=\"goal\">Continue</codex_internal_context>").is_none());
        assert!(user_words("# AGENTS.md instructions for /x\n\nrules").is_none());
        assert_eq!(
            user_words("<command-name>/review</command-name><command-args>check the api</command-args>").as_deref(),
            Some("check the api")
        );
        assert_eq!(
            user_words("<image name=[Image #1] path=\"/tmp/a.png\">\nwhy is this red?").as_deref(),
            Some("\nwhy is this red?")
        );
        assert_eq!(user_words("/var/folders/ab/c.png\nfix this").as_deref(), Some("fix this"));
    }

    #[test]
    fn restated_fact_is_not_added_twice() {
        let event = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat { source_id: "x".into(), occurred_at: None },
            "text",
            Redaction::None,
        );
        let mut ex = Extraction::new(BTreeMap::new());
        let proposal = |statement: &str| Proposal {
            entity_id: "w_app".into(),
            entity_type: "project".into(),
            title: "app".into(),
            predicate: "schema_rule".into(),
            statement: statement.into(),
            evidence: "text".into(),
        };
        ex.place(proposal("Schema changes must be updated in three places to ensure correctness."), &event);
        ex.place(proposal("A schema change must be updated in three places to be correct."), &event);
        ex.place(proposal("Deploys go to Fly with fly deploy."), &event);
        let mut same_words = proposal("Deploys go to Fly with fly deploy.");
        same_words.predicate = "deploy_target".into();
        ex.place(same_words, &event);
        assert_eq!(ex.facts, 2);
    }

    #[test]
    fn echoed_id_prefix_is_not_doubled_and_notes_name_their_project() {
        let event = JournalEvent::new(
            "personal",
            "s",
            Role::Note,
            Source::IdeHistory {
                source_id: "md:/Users/me/GitHub/agensis/AGENTS.md:abcd".into(),
                position: 0,
                occurred_at: None,
            },
            "Deploy with fly deploy.",
            Redaction::None,
        );
        assert_eq!(event_project(&event), "agensis");
        let fact = LlmFact {
            event_ref: 1,
            lasting: true,
            entity_type: "preference".into(),
            entity_name: "pref_user".into(),
            predicate: "deploy".into(),
            statement: "Deploy with fly deploy.".into(),
            evidence: "Deploy with fly deploy.".into(),
        };
        assert_eq!(check_llm_fact(&fact, &event, &BTreeMap::new()).unwrap().entity_id, "pref_user");
    }

    #[test]
    fn project_fact_lands_on_the_events_own_project() {
        let mut event = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat { source_id: "x".into(), occurred_at: None },
            "we always deploy with fly deploy",
            Redaction::None,
        );
        event.set_project_from("/tmp/no-such-repo/legion");
        let fact = LlmFact {
            event_ref: 1,
            lasting: true,
            entity_type: "project".into(),
            entity_name: "agensis".into(),
            predicate: "deploy".into(),
            statement: "Deploys use fly deploy.".into(),
            evidence: "always deploy with fly deploy".into(),
        };
        let p = check_llm_fact(&fact, &event, &BTreeMap::new()).unwrap();
        assert_eq!(p.entity_id, "w_legion");
        assert_eq!(p.title, "legion");
    }

    #[test]
    fn long_memory_file_is_chunked_whole() {
        let para = "Rule: run the checkout tests before merging.\n";
        let text = para.repeat(600); // ~27 KB, one paragraph of many lines
        let text = format!("{text}\n\nLast rule: deploy with fly deploy.");
        let pieces = chunks(&text, 8_000);
        assert!(pieces.len() >= 4);
        assert!(pieces.iter().all(|p| p.chars().count() <= 8_000));
        assert!(pieces.last().unwrap().contains("Last rule"));
        let joined: usize = pieces.iter().map(|p| p.matches("Rule:").count()).sum();
        assert_eq!(joined, 600);
    }

    #[test]
    fn compaction_summaries_and_reminders_are_not_user_words() {
        assert!(user_words("This session is being continued from a previous conversation that ran out of context. Summary: never push").is_none());
        let mixed = "<system-reminder>\nContents of CLAUDE.md: never push\n</system-reminder>\nplease make the sidebar collapsible";
        assert_eq!(user_words(mixed).unwrap().trim(), "please make the sidebar collapsible");
        let tags = "keep this\n<skill>\nsecret skill body\n</skill>\n<attached_files>blob</attached_files>\n<turn_aborted>stopped</turn_aborted>\nreal request";
        let kept = user_words(tags).unwrap();
        assert!(kept.contains("keep this") && kept.contains("real request"), "{kept}");
        assert!(!kept.contains("secret skill") && !kept.contains("blob") && !kept.contains("stopped"), "{kept}");
    }

    #[test]
    fn a_fact_the_model_marks_as_not_lasting_is_rejected() {
        let event = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat { source_id: "x".into(), occurred_at: None },
            "yes I am doing start:electron",
            Redaction::None,
        );
        let fact = LlmFact {
            event_ref: 1,
            lasting: false,
            entity_type: "preference".into(),
            entity_name: "user".into(),
            predicate: "activity".into(),
            statement: "User is running start:electron command.".into(),
            evidence: "I am doing start:electron".into(),
        };
        assert!(check_llm_fact(&fact, &event, &BTreeMap::new()).is_err());
    }

    #[test]
    fn paraphrased_rule_on_prefs_is_not_added_again_to_the_project() {
        let event = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat { source_id: "x".into(), occurred_at: None },
            "text",
            Redaction::None,
        );
        let mut ex = Extraction::new(BTreeMap::new());
        let p = |entity: &str, statement: &str| Proposal {
            entity_id: entity.into(),
            entity_type: "preference".into(),
            title: entity.into(),
            predicate: "push_policy".into(),
            statement: statement.into(),
            evidence: "text".into(),
        };
        ex.place(p("pref_user", "Never push; commit only when asked; never revert uncommitted code."), &event);
        // A project restating the user's rule adds nothing.
        ex.place(p("w_legion", "User commits code; assistant never pushes."), &event);
        // A project fact stays per project.
        ex.place(p("w_legion", "Backend deploys to Fly with fly deploy."), &event);
        ex.place(p("w_other", "Backend deploys to Fly with fly deploy."), &event);
        assert_eq!(ex.facts, 3, "{:?}", ex.entities.keys().collect::<Vec<_>>());
    }

    #[test]
    fn project_fact_is_not_copied_onto_preferences_afterwards() {
        let event = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat { source_id: "x".into(), occurred_at: None },
            "text",
            Redaction::None,
        );
        let mut ex = Extraction::new(BTreeMap::new());
        let p = |entity: &str, statement: &str| Proposal {
            entity_id: entity.into(),
            entity_type: "preference".into(),
            title: entity.into(),
            predicate: "push_policy".into(),
            statement: statement.into(),
            evidence: "text".into(),
        };
        ex.place(p("w_legion", "Never push; commit only when asked; never revert uncommitted code."), &event);
        ex.place(p("pref_user", "Never push; commit only when asked; never revert uncommitted code."), &event);
        ex.place(p("w_other", "Backend deploys to Fly with fly deploy."), &event);
        ex.place(p("w_legion", "Backend deploys to Fly with fly deploy."), &event);
        assert_eq!(ex.facts, 3, "{:?}", ex.entities.keys().collect::<Vec<_>>());
        assert_eq!(ex.entities.get("pref_user").map(|e| e.facts.len()).unwrap_or(0), 0);
        assert_eq!(ex.entities["w_legion"].facts.len(), 2);
    }

    #[test]
    fn facts_from_an_old_version_of_a_memory_file_retire() {
        let note = |hash: &str| {
            let mut e = JournalEvent::new(
                "personal",
                "s",
                Role::Note,
                Source::IdeHistory { source_id: format!("md:/p/AGENTS.md:{hash}"), position: 0, occurred_at: None },
                "x",
                Redaction::None,
            );
            e.seq = 1;
            e
        };
        let old = note("aaaa");
        let new = note("bbbb");
        let mut ex = Extraction::new(BTreeMap::new());
        let proposal = |statement: &str| Proposal {
            entity_id: "w_p".into(),
            entity_type: "project".into(),
            title: "p".into(),
            predicate: "rule".into(),
            statement: statement.into(),
            evidence: "x".into(),
        };
        ex.place(proposal("Deploy with fly deploy."), &old);
        ex.place(proposal("Run the checkout tests before merging."), &old);
        // The new version still says the second rule.
        ex.place(proposal("Run the checkout tests before merging."), &new);
        retire_outdated_file_facts(&mut ex, &[new]);
        let facts = &ex.entities["w_p"].facts;
        let status = |s: &str| facts.iter().find(|f| f.statement == s).unwrap().status;
        assert_eq!(status("Deploy with fly deploy."), FactStatus::Superseded);
        assert_eq!(status("Run the checkout tests before merging."), FactStatus::Active);
    }

    #[test]
    fn entity_paths_use_prefixes() {
        assert_eq!(entity_path("pref_user"), "memory/preferences/user.md");
        assert_eq!(entity_path("p_alice"), "memory/people/alice.md");
        assert_eq!(entity_path("w_project_x"), "memory/workstreams/project_x.md");
        assert_eq!(entity_path("o_acme"), "memory/orgs/acme.md");
    }
}
