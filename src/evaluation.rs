//! Held-out evaluation.
//!
//! Runs every case in a rubric through the real search (curated facts and
//! the journal, the same code as `mem search` and MCP `memory_search`) twice:
//! once in lexical order and once reranked. A case passes when every
//! expected id is in the top `limit` hits, every `must_include` string
//! appears in them, and no `must_not_include` string does. The two runs are
//! reported side by side so a reranker is only trusted where it helps.
//!
//! Rubric (`<root>/rubric.json` or `--rubric PATH`):
//! ```json
//! {
//!   "cases": [
//!     {
//!       "id": "deploy_target",
//!       "category": "direct_recall",
//!       "question": "where do we deploy",
//!       "project": "agensis",
//!       "expected_fact_ids": ["fact_8df0f3e8738e"],
//!       "must_include": ["Fly"],
//!       "must_not_include": ["Netlify is the deploy target"]
//!     }
//!   ]
//! }
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ops::{combined_search, SearchParams};

#[derive(Debug, Clone, Deserialize)]
pub struct Rubric {
    pub cases: Vec<RubricCase>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RubricCase {
    pub id: String,
    #[serde(default)]
    pub category: String,
    pub question: String,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub expected_fact_ids: Vec<String>,
    #[serde(default)]
    pub must_include: Vec<String>,
    #[serde(default)]
    pub must_not_include: Vec<String>,
}

/// One case scored under one ranking.
#[derive(Debug, Clone, Serialize)]
pub struct CaseScore {
    pub id: String,
    pub category: String,
    pub passed: bool,
    /// Every expected id was in the hits.
    pub recall: bool,
    pub includes_ok: bool,
    pub excludes_ok: bool,
    /// 1-based rank of the first expected id, if found.
    pub first_expected_rank: Option<usize>,
}

/// Results for one ranking over the whole rubric.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// `lexical`, or the reranker that judged (`jev`, `laya`).
    pub ranker: String,
    pub total: usize,
    pub passed: usize,
    pub recall: f32,
    pub cases: Vec<CaseScore>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvaluationReport {
    pub lexical: RunReport,
    /// Absent when no reranker is available (no JEV key, no local model).
    pub reranked: Option<RunReport>,
}

/// Read a rubric and score it against the memory root.
pub fn evaluate(root: &Path, rubric_path: &Path, limit: usize) -> Result<EvaluationReport> {
    let raw = std::fs::read_to_string(rubric_path).map_err(|e| {
        Error::Usage(format!(
            "cannot read rubric {}: {e}. A rubric is JSON: {{\"cases\": [{{\"id\", \"question\", \"expected_fact_ids\", \"must_include\", \"must_not_include\", \"project\"?}}]}}",
            rubric_path.display()
        ))
    })?;
    let rubric: Rubric = serde_json::from_str(&raw)
        .map_err(|e| Error::Usage(format!("rubric {}: {e}", rubric_path.display())))?;
    if rubric.cases.is_empty() {
        return Err(Error::Usage(format!("rubric {} has no cases", rubric_path.display())));
    }
    let lexical = run(root, &rubric, limit, false)?;
    let reranked = run(root, &rubric, limit, true)?;
    let reranked = if reranked.ranker == "lexical" { None } else { Some(reranked) };
    Ok(EvaluationReport { lexical, reranked })
}

fn run(root: &Path, rubric: &Rubric, limit: usize, rerank: bool) -> Result<RunReport> {
    let mut cases = Vec::with_capacity(rubric.cases.len());
    let mut ranker = "lexical".to_string();
    for case in &rubric.cases {
        let found = combined_search(
            root,
            &SearchParams {
                query: &case.question,
                limit,
                rerank,
                project: case.project.as_deref(),
                ..Default::default()
            },
        )?;
        if found.judged {
            ranker = found.ranker.to_string();
        }
        let ids: Vec<&str> = found.hits.iter().map(|h| h.fact_id.as_str()).collect();
        let text = found
            .hits
            .iter()
            .map(|h| h.statement.to_lowercase())
            .collect::<Vec<_>>()
            .join("\n");
        let recall = case.expected_fact_ids.iter().all(|id| ids.contains(&id.as_str()));
        let first_expected_rank = ids
            .iter()
            .position(|id| case.expected_fact_ids.iter().any(|e| e == id))
            .map(|i| i + 1);
        let includes_ok = case.must_include.iter().all(|s| text.contains(&s.to_lowercase()));
        let excludes_ok = case.must_not_include.iter().all(|s| !text.contains(&s.to_lowercase()));
        cases.push(CaseScore {
            id: case.id.clone(),
            category: case.category.clone(),
            passed: recall && includes_ok && excludes_ok,
            recall,
            includes_ok,
            excludes_ok,
            first_expected_rank,
        });
    }
    let total = cases.len();
    let passed = cases.iter().filter(|c| c.passed).count();
    let recall = cases.iter().filter(|c| c.recall).count() as f32 / total as f32;
    Ok(RunReport { ranker, total, passed, recall, cases })
}

impl std::fmt::Display for EvaluationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for run in std::iter::once(&self.lexical).chain(self.reranked.as_ref()) {
            writeln!(
                f,
                "{}: {}/{} passed, recall {:.2}",
                run.ranker, run.passed, run.total, run.recall
            )?;
            for case in &run.cases {
                writeln!(
                    f,
                    "  {:<24} {:<14} {} rank={}",
                    case.id,
                    case.category,
                    if case.passed { "pass" } else { "FAIL" },
                    case.first_expected_rank.map_or("-".to_string(), |r| r.to_string()),
                )?;
            }
        }
        if self.reranked.is_none() {
            writeln!(f, "reranked: no reranker available (set OPENROUTER_API_KEY or run `mem models pull`)")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{apply, ChangeKind, ChangeRequest};
    use crate::fact::{Fact, FactKind, FactStatus, Visibility};
    use crate::repo::GitRepo;

    #[test]
    fn scores_real_search_results() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let fact = Fact {
            id: "fact_deploy".into(),
            predicate: "deploy".into(),
            statement: "The app deploys to Fly with fly deploy.".into(),
            kind: FactKind::ExplicitAssertion,
            status: FactStatus::Active,
            observed_at: chrono::Utc::now(),
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            expires_at: None,
            review_after: None,
            supersedes: vec![],
            sources: vec![],
            visibility: Visibility::Private,
        };
        apply(
            &repo,
            ChangeRequest {
                kind: ChangeKind::Remember,
                request_id: "r1".into(),
                scope_id: "personal".into(),
                session_id: "s".into(),
                source_event_id: "evt_1".into(),
                entity_id: "w_app".into(),
                fact: Some(fact),
                target_fact_id: None,
                reason: None,
                proposed_at: chrono::Utc::now(),
            },
        )
        .unwrap();
        let rubric = tmp.path().join("rubric.json");
        std::fs::write(
            &rubric,
            r#"{"cases":[
                {"id":"hit","category":"direct_recall","question":"deploys fly","expected_fact_ids":["fact_deploy"],"must_include":["Fly"]},
                {"id":"miss","category":"direct_recall","question":"deploys fly","expected_fact_ids":["fact_other"]}
            ]}"#,
        )
        .unwrap();
        let report = evaluate(tmp.path(), &rubric, 5).unwrap();
        assert_eq!(report.lexical.total, 2);
        assert_eq!(report.lexical.passed, 1);
        assert_eq!(report.lexical.cases[0].first_expected_rank, Some(1));
        assert!(!report.lexical.cases[1].passed);
    }

    #[test]
    fn missing_rubric_explains_the_format() {
        let tmp = tempfile::tempdir().unwrap();
        let err = evaluate(tmp.path(), &tmp.path().join("nope.json"), 5).unwrap_err();
        assert!(err.to_string().contains("expected_fact_ids"));
    }
}
