//! TypeSafe JEV reranker.
//!
//! Search sends its top rows to the OpenRouter Decisions API, one request
//! per row, asking how well the row answers the query (a four-level score).
//! Only the query and the row text (clipped to 2,000 characters) are sent;
//! fact ids and paths are not. Any failure leaves the lexical order in place.
//!
//! Configuration (environment, else the nearest `.env` defining a key within
//! four parent folders, else `~/.config/mem/env`):
//! - `MEM_JEV_API_KEY` or `OPENROUTER_API_KEY`: bearer token.
//! - `MEM_JEV_BASE_URL`: the exact JEV endpoint, used verbatim. This is the
//!   knob for a self-hosted or proxied JEV that lives at its own path, where
//!   appending `/alpha/decisions` would miss the route.
//! - `OPENROUTER_DECISIONS_BASE_URL`: an OpenRouter API root;
//!   `/alpha/decisions` is appended unless it is already there. Default
//!   `https://openrouter.ai/api/alpha/decisions`.
//! - `MEM_JEV_MODEL` or `JEV_MODEL`: default `~typesafe/jev-latest`.
//! - `MEM_JEV_DOTENV_PATH`: read this file instead of searching for one.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

const DEFAULT_DECISIONS_URL: &str = "https://openrouter.ai/api/alpha/decisions";
const DEFAULT_MODEL: &str = "~typesafe/jev-latest";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// Four rubric levels, 0 through 3. The API returns a position on that scale.
const SCORE_MAX_LEVEL: f64 = 3.0;

/// A single JEV decision for one fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JevDecision {
    pub fact_id: String,
    pub score: f32,
    pub confidence: Option<f32>,
    pub evidence_ids: Vec<String>,
}

#[derive(Clone)]
pub struct JevReranker {
    decisions_url: String,
    api_key: String,
    model: String,
}

impl std::fmt::Debug for JevReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevReranker")
            .field("decisions_url", &self.decisions_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl JevReranker {
    /// Construct from explicit values. `decisions_url` may be the full
    /// decisions endpoint or an API root; both are normalized.
    pub fn new(decisions_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            decisions_url: normalize_decisions_url(&decisions_url.into()),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Construct with an exact decisions endpoint, used as-is. Use this for a
    /// proxy that serves JEV at its own path, where appending
    /// `/alpha/decisions` would miss the route.
    pub fn new_endpoint(
        decisions_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            decisions_url: decisions_url.into().trim().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
        }
    }

    /// Construct from the environment. Loads `.env` from `MEM_JEV_DOTENV_PATH`
    /// or the nearest `.env` that defines a JEV key.
    ///
    /// Key: `MEM_JEV_API_KEY` or `OPENROUTER_API_KEY`.
    /// Endpoint: `MEM_JEV_BASE_URL` is used verbatim; otherwise
    /// `OPENROUTER_DECISIONS_BASE_URL` gets `/alpha/decisions` appended.
    /// Default `https://openrouter.ai/api/alpha/decisions`.
    /// Model: `MEM_JEV_MODEL` or `JEV_MODEL`. Default `~typesafe/jev-latest`.
    pub fn from_env() -> Result<Self> {
        load_jev_dotenv();
        let api_key = first_nonempty(&["MEM_JEV_API_KEY", "OPENROUTER_API_KEY"]).ok_or_else(|| {
            Error::Jev("no API key: set MEM_JEV_API_KEY or OPENROUTER_API_KEY".into())
        })?;
        let model = first_nonempty(&["MEM_JEV_MODEL", "JEV_MODEL"])
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        // `MEM_JEV_BASE_URL` is the exact endpoint: a self-hosted or proxied
        // JEV lives at its own path, and appending `/alpha/decisions` would
        // miss it. `OPENROUTER_DECISIONS_BASE_URL` is an OpenRouter API root,
        // so it gets the decisions path appended.
        if let Some(endpoint) = first_nonempty(&["MEM_JEV_BASE_URL"]) {
            return Ok(Self::new_endpoint(endpoint, api_key, model));
        }
        let base = first_nonempty(&["OPENROUTER_DECISIONS_BASE_URL"])
            .unwrap_or_else(|| DEFAULT_DECISIONS_URL.to_string());
        Ok(Self::new(base, api_key, model))
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base_url(&self) -> &str {
        &self.decisions_url
    }

    /// Score each candidate against the query. One Decisions call per document:
    /// JEV judges a single state, so candidates are not batched into one body.
    pub fn score(&self, query: &str, candidates: &[crate::search::lexical::SearchHit]) -> Result<Vec<JevDecision>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut handles = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let url = self.decisions_url.clone();
            let key = self.api_key.clone();
            let model = self.model.clone();
            let query = query.to_string();
            let fact_id = candidate.fact_id.clone();
            let statement = candidate.statement.clone();
            handles.push(std::thread::spawn(move || {
                judge_one(&url, &key, &model, &query, &fact_id, &statement)
            }));
        }
        let mut out = Vec::with_capacity(handles.len());
        for handle in handles {
            let decision = handle
                .join()
                .map_err(|_| Error::Jev("jev worker panicked".into()))??;
            out.push(decision);
        }
        Ok(out)
    }
}

fn judge_one(
    url: &str,
    api_key: &str,
    model: &str,
    query: &str,
    fact_id: &str,
    statement: &str,
) -> Result<JevDecision> {
    let payload = serde_json::json!({
        "model": model,
        "state": clip(statement, 2000),
        "questions": {
            "relevance": {
                "type": "score",
                "instructions": format!(
                    "How well does this text answer the search query? Query: {}",
                    clip(query, 400)
                ),
                "criteria": [
                    "Unrelated to the query",
                    "Shares a topic but does not answer the query",
                    "Partly answers the query",
                    "Directly answers the query"
                ]
            }
        }
    });
    let response = ureq::post(url)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send_json(payload)
        .map_err(http_error)?;
    let body: serde_json::Value = response
        .into_json()
        .map_err(|e| Error::Jev(format!("response decode: {e}")))?;
    let (score, confidence) = relevance_from_body(&body)?;
    Ok(JevDecision {
        fact_id: fact_id.to_string(),
        score,
        confidence,
        evidence_ids: Vec::new(),
    })
}

fn http_error(err: ureq::Error) -> Error {
    match err {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            Error::Jev(format!("HTTP {code}: {}", clip(&body, 300)))
        }
        other => Error::Jev(format!("request failed: {other}")),
    }
}

/// Map a Decisions response onto a 0–1 relevance score.
/// A reported confidence of 0 means the distribution is flat, so the score
/// is not usable and comes back as 0.
pub(crate) fn relevance_from_body(body: &serde_json::Value) -> Result<(f32, Option<f32>)> {
    if let Some(err) = body.get("error") {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("decisions request failed");
        return Err(Error::Jev(clip(message, 300)));
    }
    let answer = body
        .get("answers")
        .and_then(|answers| answers.get("relevance"))
        .ok_or_else(|| Error::Jev("response missing answers.relevance".into()))?;
    let raw = answer.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let confidence = answer
        .get("confidence")
        .and_then(|v| v.as_f64())
        .map(|c| c as f32);
    let mut score = (raw / SCORE_MAX_LEVEL).clamp(0.0, 1.0) as f32;
    if let Some(confidence) = confidence {
        score *= confidence.clamp(0.0, 1.0);
    }
    Ok((score, confidence))
}

pub(crate) fn normalize_decisions_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.ends_with("/decisions") {
        url.to_string()
    } else {
        format!("{url}/alpha/decisions")
    }
}

pub(crate) fn load_jev_dotenv() {
    if let Some(path) = std::env::var("MEM_JEV_DOTENV_PATH").ok().filter(|s| !s.is_empty()) {
        load_dotenv(Path::new(&path));
        return;
    }
    let Ok(mut dir) = std::env::current_dir() else {
        return;
    };
    for _ in 0..4 {
        let candidate = dir.join(".env");
        if candidate.is_file() && dotenv_defines_key(&candidate) {
            load_dotenv(&candidate);
            return;
        }
        if !dir.pop() {
            break;
        }
    }
    if let Some(user) = user_env_file().filter(|p| p.is_file()) {
        load_dotenv(&user);
    }
}

/// Per-user settings: `$XDG_CONFIG_HOME/mem/env`, else `~/.config/mem/env`.
/// Same `KEY=value` format as a `.env` file.
pub(crate) fn user_env_file() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
    Some(base.join("mem").join("env"))
}

fn dotenv_defines_key(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    raw.lines().any(|line| {
        let line = line.trim();
        line.starts_with("OPENROUTER_API_KEY=") || line.starts_with("MEM_JEV_API_KEY=")
    })
}

fn first_nonempty(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

/// Load a `.env` file into the process environment. Idempotent.
fn load_dotenv(path: &Path) {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return;
    };
    for (k, v) in parse_dotenv(&raw) {
        // Variables already in the environment win over the file.
        if std::env::var_os(&k).is_none() {
            std::env::set_var(k, v);
        }
    }
}

/// `KEY=value` pairs from a `.env` file; comments, blank lines, `export`,
/// and surrounding quotes are handled.
fn parse_dotenv(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (k, v) = line.split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            Some((k.to_string(), v.trim().trim_matches('"').trim_matches('\'').to_string()))
        })
        .collect()
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_url_keeps_a_full_endpoint() {
        assert_eq!(
            normalize_decisions_url("https://openrouter.ai/api/alpha/decisions"),
            "https://openrouter.ai/api/alpha/decisions"
        );
        assert_eq!(
            normalize_decisions_url("https://openrouter.ai/api/v1/"),
            "https://openrouter.ai/api/v1/alpha/decisions"
        );
    }

    #[test]
    fn exact_endpoint_is_not_normalized() {
        // A proxy that serves JEV at its own path must be hit verbatim.
        let jev = JevReranker::new_endpoint(
            "https://proxy.example.test/jev/v1/systemone/",
            "test-token",
            "jev-latest",
        );
        assert_eq!(jev.base_url(), "https://proxy.example.test/jev/v1/systemone");
    }

    #[test]
    fn relevance_uses_score_and_confidence() {
        let body = serde_json::json!({
            "answers": {
                "relevance": { "type": "score", "score": 3.0, "confidence": 1.0 }
            }
        });
        let (score, confidence) = relevance_from_body(&body).unwrap();
        assert!((score - 1.0).abs() < 0.001);
        assert_eq!(confidence, Some(1.0));

        let flat = serde_json::json!({
            "answers": {
                "relevance": { "type": "score", "score": 2.0, "confidence": 0.0 }
            }
        });
        let (score, _) = relevance_from_body(&flat).unwrap();
        assert_eq!(score, 0.0);
    }

    #[test]
    fn dotenv_lines_parse() {
        let pairs = parse_dotenv("# comment\n\nexport OPENROUTER_API_KEY=\"sk-x\"\nJEV_MODEL='~typesafe/jev-latest'\n=bad\n");
        assert_eq!(
            pairs,
            vec![
                ("OPENROUTER_API_KEY".to_string(), "sk-x".to_string()),
                ("JEV_MODEL".to_string(), "~typesafe/jev-latest".to_string()),
            ]
        );
    }

    #[test]
    fn debug_output_hides_the_key() {
        let jev = JevReranker::new("https://openrouter.ai/api/v1", "sk-secret", "~typesafe/jev-latest");
        assert_eq!(jev.base_url(), "https://openrouter.ai/api/v1/alpha/decisions");
        assert!(!format!("{jev:?}").contains("sk-secret"));
    }
}