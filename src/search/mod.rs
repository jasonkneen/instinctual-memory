//! Search and retrieval.
//!
//! - a pinned-tree lexical scan over curated facts (eligibility and
//!   suppression enforced);
//! - a journal scan over raw ingested events;
//! - reranking: TypeSafe JEV when a key is set, else a local cross-encoder
//!   (fastembed) when its model is downloaded (`mem models pull`).

pub mod lexical;
pub mod rerank;
pub mod laya;
pub mod journal;

pub use lexical::{LexicalSearch, SearchHit, SearchQuery};
pub use rerank::{Reranker, RerankMode, RerankOutput};
pub use laya::{LayaReranker, ModelKind};
pub use journal::{JournalSearch, JournalSearchHit};

use crate::controls::Controls;
use crate::entity::EntityFile;
use crate::error::Result;
use crate::fact::{Fact, FactStatus, Visibility};
use crate::repo::GitRepo;

/// A search result returned to the caller.
#[derive(Debug, Clone)]
pub struct Search {
    pub hits: Vec<SearchHit>,
    pub revision: String,
    pub truncated: bool,
    pub ranker: &'static str,
    pub judged: bool,
}

/// Build the audit-list of eligible facts from the pinned revision.
pub fn eligible_facts(
    repo: &GitRepo,
    revision: &str,
    audience: Visibility,
    controls: &Controls,
    include_disputed: bool,
) -> Result<Vec<(String, Fact)>> {
    let mut out = Vec::new();
    let entries = repo.list_eligible_entity_files(revision)?;
    for (path, blob) in entries {
        let entity = match EntityFile::parse(&blob.content) {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entity.visibility.allows(audience) {
            continue;
        }
        for fact in entity.facts {
            if controls.is_suppressed(&fact.id)
                || controls.is_retracted(&fact.id)
                || controls.is_deleted(&fact.id)
            {
                continue;
            }
            if !visibility_ok(fact.visibility, audience) {
                continue;
            }
            if !fact.is_eligible(chrono::Utc::now(), include_disputed) {
                continue;
            }
            if matches!(fact.status, FactStatus::Superseded) {
                continue;
            }
            out.push((path.clone(), fact));
        }
    }
    Ok(out)
}

fn visibility_ok(v: Visibility, audience: Visibility) -> bool {
    v.allows(audience)
}
