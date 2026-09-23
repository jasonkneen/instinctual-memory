//! Reranker abstraction.
//!
//! Modes: `None` returns the lexical baseline, `Jev` calls the TypeSafe JEV
//! Decisions API, `Laya` runs the local cross-encoder. `rerank_auto` picks
//! among them without the caller naming one.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::jev::JevReranker;
use crate::search::lexical::SearchHit;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RerankMode {
    None,
    Jev,
    Laya,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankOutput {
    pub mode: RerankMode,
    pub hits: Vec<SearchHit>,
    pub judged: bool,
}

pub trait Reranker {
    fn rerank(
        &self,
        query: &str,
        baseline: Vec<SearchHit>,
        limit: usize,
    ) -> Result<RerankOutput>;
}

/// Reranker that always returns the baseline.
pub struct NoRerank;

impl Reranker for NoRerank {
    fn rerank(
        &self,
        _query: &str,
        mut baseline: Vec<SearchHit>,
        limit: usize,
    ) -> Result<RerankOutput> {
        baseline.truncate(limit);
        Ok(RerankOutput {
            mode: RerankMode::None,
            hits: baseline,
            judged: false,
        })
    }
}

/// Reranker that delegates to JEV.
pub struct JevRerankAdapter {
    inner: JevReranker,
}

impl JevRerankAdapter {
    pub fn new(inner: JevReranker) -> Self {
        Self { inner }
    }
}

impl Reranker for JevRerankAdapter {
    fn rerank(
        &self,
        query: &str,
        baseline: Vec<SearchHit>,
        limit: usize,
    ) -> Result<RerankOutput> {
        let pool = jev_pool(&baseline);
        let judged_ids: std::collections::HashSet<String> = pool.iter().map(|h| h.fact_id.clone()).collect();
        let scored = self.inner.score(query, &pool)?;
        let mut merged: Vec<(f32, SearchHit)> = pool
            .into_iter()
            .map(|mut hit| {
                let score = scored
                    .iter()
                    .find(|decision| decision.fact_id == hit.fact_id)
                    .map(|decision| decision.score)
                    .unwrap_or(0.0);
                hit.score = score;
                (score, hit)
            })
            .collect();
        merged.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        // Judged rows first, then the rest of the lexical order, so the page
        // is as full as the caller asked for.
        let hits = merged
            .into_iter()
            .map(|(_, hit)| hit)
            .chain(baseline.into_iter().filter(|h| !judged_ids.contains(&h.fact_id)))
            .take(limit)
            .collect();
        Ok(RerankOutput {
            mode: RerankMode::Jev,
            hits,
            judged: true,
        })
    }
}

/// Lexical leaders plus short sibling rows. JEV is one request per document,
/// so the pool stays small.
fn jev_pool(baseline: &[SearchHit]) -> Vec<SearchHit> {
    const CAP: usize = 8;
    let mut pool: Vec<SearchHit> = baseline
        .iter()
        .filter(|hit| hit.score > 0.0)
        .take(5)
        .cloned()
        .collect();
    if pool.is_empty() {
        return baseline.iter().take(CAP).cloned().collect();
    }
    for hit in baseline.iter().filter(|hit| hit.score == 0.0) {
        if pool.len() >= CAP {
            break;
        }
        pool.push(hit.clone());
    }
    pool
}

impl RerankOutput {
    pub fn ensure_truncated(&mut self, limit: usize) {
        if self.hits.len() > limit {
            self.hits.truncate(limit);
        }
    }
}

pub fn build_reranker(mode: RerankMode) -> Result<Box<dyn Reranker>> {
    match mode {
        RerankMode::None => Ok(Box::new(NoRerank)),
        RerankMode::Jev => {
            let jev = JevReranker::from_env()?;
            Ok(Box::new(JevRerankAdapter::new(jev)))
        }
        RerankMode::Laya => Err(Error::SearchFailed(
            "RerankMode::Laya is selected via build_local_reranker, not build_reranker".into(),
        )),
    }
}

/// Rerank with whatever is configured, no flag needed: JEV when a key is
/// present, else the local cross-encoder when its model is already cached
/// under `models_dir`, else the lexical order. `MEM_RERANK=off` keeps the
/// lexical order everywhere (no network calls). A ranker that errors or
/// declines falls through to the next. Returns (ranker name, judged, hits).
pub fn rerank_auto(
    query: &str,
    baseline: Vec<SearchHit>,
    limit: usize,
    models_dir: &Path,
) -> (&'static str, bool, Vec<SearchHit>) {
    // Unit tests never call out to the network.
    let off = cfg!(test)
        || std::env::var("MEM_RERANK").is_ok_and(|v| matches!(v.as_str(), "off" | "0" | "false"));
    if baseline.is_empty() || off {
        let mut hits = baseline;
        hits.truncate(limit);
        return ("lexical", false, hits);
    }
    match JevReranker::from_env() {
        Ok(jev) => match JevRerankAdapter::new(jev).rerank(query, baseline.clone(), limit) {
            Ok(r) if r.judged => return ("jev", true, r.hits),
            Ok(_) => eprintln!("[search] jev declined; trying local reranker"),
            Err(err) => eprintln!("[search] jev failed: {err}; trying local reranker"),
        },
        Err(Error::Jev(_)) => {} // no key configured
        Err(err) => eprintln!("[search] jev unavailable: {err}"),
    }
    if let Some(local) = crate::search::laya::try_load_cached(models_dir) {
        match local.rerank(query, baseline.clone(), limit) {
            Ok(r) if r.judged => return ("laya", true, r.hits),
            Ok(_) => {}
            Err(err) => eprintln!("[search] local reranker failed: {err}"),
        }
    }
    let mut hits = baseline;
    hits.truncate(limit);
    ("lexical", false, hits)
}

/// Build the local cross-encoder reranker if the model is available on disk
/// (or can be downloaded). Returns `Ok(None)` when the model is unreachable;
/// callers should fall back to JEV or the lexical baseline.
pub fn build_local_reranker(cache_dir: &Path) -> Result<Option<crate::search::laya::LayaReranker>> {
    crate::search::laya::LayaReranker::try_new(
        cache_dir,
        crate::search::laya::LayaReranker::model_from_env(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rerank_truncates() {
        let r = NoRerank;
        let baseline: Vec<SearchHit> = (0..10)
            .map(|i| SearchHit {
                fact_id: format!("f{i}"),
                entity_id: "p_x".into(),
                entity_path: "memory/people/x.md".into(),
                statement: "x".into(),
                score: 1.0,
                sources: vec![],
                observed_at: chrono::Utc::now(),
                snippet: None,
                source_excerpts: None,
            })
            .collect();
        let out = r.rerank("anything", baseline, 3).unwrap();
        assert_eq!(out.hits.len(), 3);
        assert!(!out.judged);
    }

    #[test]
    fn unknown_mode_errors() {
        let mode = RerankMode::Jev;
        // Without env vars set, this errors gracefully.
        let result = build_reranker(mode);
        // Either it errors because no key, or it builds successfully if env has one.
        match result {
            Ok(_) => {}
            Err(Error::Jev(_)) => {}
            Err(other) => panic!("unexpected: {other}"),
        }
    }
}