//! Local cross-encoder reranker (Laya-style).
//!
//! Rust port of `@receptron/laya`. Uses `fastembed-rs` to download and run a
//! small ONNX cross-encoder (BAAI/bge-reranker-base by default, ~100 MB on
//! first use). The model is cached under `<memory-root>/.models/`.
//!
//! Designed to fail open: if the model is not downloaded, `try_new` returns
//! `Ok(None)`, and the search command falls back to JEV or the lexical baseline.

use std::path::Path;
use std::sync::Arc;

use fastembed::{RerankInitOptions, RerankerModel, TextRerank};
use parking_lot::Mutex;

use crate::error::{Error, Result};
use crate::search::lexical::SearchHit;
use crate::search::rerank::{RerankMode, RerankOutput, Reranker};

/// Local cross-encoder reranker backed by `fastembed`.
pub struct LayaReranker {
    inner: Arc<Mutex<TextRerank>>,
    model: ModelKind,
}

#[derive(Debug, Clone, Copy)]
pub enum ModelKind {
    BgeRerankerBase,
    JinaRerankerV1TurboEn,
    BgeRerankerV2M3,
}

impl ModelKind {
    fn fastembed(self) -> RerankerModel {
        match self {
            ModelKind::BgeRerankerBase => RerankerModel::BGERerankerBase,
            ModelKind::JinaRerankerV1TurboEn => RerankerModel::JINARerankerV1TurboEn,
            ModelKind::BgeRerankerV2M3 => RerankerModel::BGERerankerV2M3,
        }
    }

    /// Folder name hf-hub uses for this model inside the cache directory.
    pub(crate) fn hub_dir(self) -> &'static str {
        match self {
            ModelKind::BgeRerankerBase => "models--BAAI--bge-reranker-base",
            ModelKind::JinaRerankerV1TurboEn => "models--jinaai--jina-reranker-v1-turbo-en",
            ModelKind::BgeRerankerV2M3 => "models--rozgo--bge-reranker-v2-m3",
        }
    }

    /// Parse a model name as written by `name` or accepted on the command
    /// line (`bge-reranker-base`, `jina`, `bge-v2-m3`, ...).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "bge-reranker-base" | "bge" | "base" => Some(ModelKind::BgeRerankerBase),
            "jina-reranker-v1-turbo-en" | "jina" => Some(ModelKind::JinaRerankerV1TurboEn),
            "bge-reranker-v2-m3" | "bge-v2-m3" | "multilingual" => Some(ModelKind::BgeRerankerV2M3),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ModelKind::BgeRerankerBase => "bge-reranker-base",
            ModelKind::JinaRerankerV1TurboEn => "jina-reranker-v1-turbo-en",
            ModelKind::BgeRerankerV2M3 => "bge-reranker-v2-m3",
        }
    }
}

impl LayaReranker {
    /// Try to construct the reranker. Downloads the model to the given cache
    /// directory on first call. Returns `Ok(None)` when the network or disk
    /// cannot satisfy the request — callers should then fall back to another
    /// ranker.
    pub fn try_new(cache_dir: &Path, model: ModelKind) -> Result<Option<Self>> {
        std::fs::create_dir_all(cache_dir).map_err(|e| Error::io(cache_dir, e))?;
        let inner = TextRerank::try_new(
            RerankInitOptions::new(model.fastembed())
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(false),
        );
        match inner {
            Ok(r) => Ok(Some(Self {
                inner: Arc::new(Mutex::new(r)),
                model,
            })),
            Err(err) => {
                eprintln!("local reranker unavailable: {err}");
                Ok(None)
            }
        }
    }

    /// Detect the model to use from the environment, defaulting to
    /// BGE-reranker-base.
    pub fn model_from_env() -> ModelKind {
        std::env::var("MEM_LAYA_MODEL")
            .ok()
            .and_then(|v| ModelKind::parse(&v))
            .unwrap_or(ModelKind::BgeRerankerBase)
    }

    /// The model to load for `cache_dir`: `MEM_LAYA_MODEL` when set, else the
    /// one `mem models pull` recorded, else BGE-reranker-base.
    pub fn selected_model(cache_dir: &Path) -> ModelKind {
        if let Some(model) = std::env::var("MEM_LAYA_MODEL").ok().and_then(|v| ModelKind::parse(&v)) {
            return model;
        }
        std::fs::read_to_string(cache_dir.join(SELECTED_FILE))
            .ok()
            .and_then(|v| ModelKind::parse(v.trim()))
            .unwrap_or(ModelKind::BgeRerankerBase)
    }

    /// Download `model` into `cache_dir` (showing progress), check it loads,
    /// and record it as the model search uses.
    pub fn pull(cache_dir: &Path, model: ModelKind) -> Result<()> {
        std::fs::create_dir_all(cache_dir).map_err(|e| Error::io(cache_dir, e))?;
        TextRerank::try_new(
            RerankInitOptions::new(model.fastembed())
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(true),
        )
        .map_err(|e| Error::SearchFailed(format!("downloading {}: {e}", model.name())))?;
        let selected = cache_dir.join(SELECTED_FILE);
        std::fs::write(&selected, model.name()).map_err(|e| Error::io(&selected, e))
    }

    pub fn model(&self) -> ModelKind {
        self.model
    }
}

impl Reranker for LayaReranker {
    fn rerank(
        &self,
        query: &str,
        baseline: Vec<SearchHit>,
        limit: usize,
    ) -> Result<RerankOutput> {
        if baseline.is_empty() {
            return Ok(RerankOutput {
                mode: RerankMode::Laya,
                hits: Vec::new(),
                judged: false,
            });
        }
        let documents: Vec<String> = baseline.iter().map(|h| h.statement.clone()).collect();
        let doc_refs: Vec<&str> = documents.iter().map(|s| s.as_str()).collect();
        let results = {
            let mut guard = self.inner.lock();
            guard
                .rerank(query, doc_refs, false, Some(32))
                .map_err(|e| Error::SearchFailed(format!("laya rerank: {e}")))?
        };
        let mut scored: Vec<(f32, SearchHit)> = results
            .into_iter()
            .filter_map(|r| {
                baseline.get(r.index).map(|hit| {
                    let score = 1.0 / (1.0 + (-r.score).exp());
                    (score, hit.clone())
                })
            })
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        // A cross-encoder that scores every candidate near zero is not
        // ranking them. Tiny logit gaps will reshuffle the list. Keep the
        // lexical order and say the model declined.
        let best = scored.first().map(|(score, _)| *score).unwrap_or(0.0);
        if best < 0.05 {
            let mut hits = baseline;
            hits.truncate(limit);
            return Ok(RerankOutput {
                mode: RerankMode::Laya,
                hits,
                judged: false,
            });
        }
        scored.truncate(limit);
        let hits = scored
            .into_iter()
            .map(|(score, mut h)| {
                h.score = score;
                h
            })
            .collect();
        Ok(RerankOutput {
            mode: RerankMode::Laya,
            hits,
            judged: true,
        })
    }
}

/// File in the models directory naming the model `mem models pull` fetched.
const SELECTED_FILE: &str = "selected";

/// Build a LayaReranker if the model is already on disk, otherwise None.
pub fn try_load_cached(cache_dir: &Path) -> Option<LayaReranker> {
    // Only load a model whose hf-hub folder is already on disk, so a search
    // never starts a download.
    let model = LayaReranker::selected_model(cache_dir);
    if !cache_dir.join(model.hub_dir()).is_dir() {
        return None;
    }
    LayaReranker::try_new(cache_dir, model).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_from_env_defaults() {
        let m = LayaReranker::model_from_env();
        assert!(matches!(
            m,
            ModelKind::BgeRerankerBase
                | ModelKind::JinaRerankerV1TurboEn
                | ModelKind::BgeRerankerV2M3
        ));
    }
}