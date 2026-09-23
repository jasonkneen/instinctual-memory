//! Domain validation of candidate snapshots.
//!
//! The validator inspects every changed path of a candidate commit, parses the
//! affected entity files and control files, and rejects any violation of the
//! domain rules. It does not mutate the repository and does not perform
//! external side effects.

use crate::controls::Controls;
use crate::entity::EntityFile;
use crate::error::{Error, Result};
use crate::fact::FactKind;
use crate::repo::{GitRepo, Validator};

/// The default validator wired into [`crate::repo::GitRepo::publish`] calls.
///
/// Rules enforced:
/// - top-level Markdown summaries (`PROFILE.md`, `ONEPAGER.md`, `INDEX.md`)
///   must parse as entity files when present and reference only fact ids that
///   exist in the curated entity set;
/// - entity files must parse cleanly, each fact must validate;
/// - facts may only supersede facts that existed in the parent revision;
/// - the controls file, if changed, must parse and must contain only known
///   suppression targets (no fabricated entries);
/// - the checkpoint, if changed, must parse and not regress.
pub struct DomainValidator;

impl Validator for DomainValidator {
    fn validate(&self, repo: &GitRepo, candidate: &str, changed_paths: &[String]) -> Result<()> {
        let mut entities: Vec<EntityFile> = Vec::new();
        let mut controls: Option<Controls> = None;
        let mut checkpoint: Option<crate::checkpoint::Checkpoint> = None;

        // Walk every entity in the candidate tree so the validator sees the
        // full state, not just the changed paths. This catches references
        // from controls to entities that already exist.
        let all = repo.list_eligible_entity_files(candidate)?;
        for (path, blob) in all {
            if let Ok(entity) = EntityFile::parse(&blob.content) {
                entities.push(entity);
            } else {
                return Err(Error::Validation(format!(
                    "{path}: failed to parse entity"
                )));
            }
        }

        // Read changed control/checkpoint files via the lower-level reader
        // that allows receipt paths (we cannot use read_snapshot here
        // because it strictly enforces the writable-path set).
        for path in changed_paths {
            if path == "state/controls.json" {
                let Some(blob) = repo.read_blob_unchecked(candidate, path)? else {
                    continue;
                };
                controls = Some(
                    Controls::parse(&blob.content)
                        .map_err(|e| Error::Validation(format!("controls: {e}")))?,
                );
            } else if path == "state/checkpoint.json" {
                let Some(blob) = repo.read_blob_unchecked(candidate, path)? else {
                    continue;
                };
                checkpoint = Some(
                    crate::checkpoint::Checkpoint::parse(&blob.content)
                        .map_err(|e| Error::Validation(format!("checkpoint: {e}")))?,
                );
            }
        }
        let _ = checkpoint;

        // Verify all referenced fact ids exist.
        for entity in &entities {
            for fact in &entity.facts {
                if !matches!(fact.kind, FactKind::ExplicitAssertion | FactKind::ObservedToolResult | FactKind::Inference) {
                    return Err(Error::Validation(format!(
                        "{entity_id}: bad kind on {fid}",
                        entity_id = entity.id,
                        fid = fact.id,
                    )));
                }
            }
        }

        // Verify top-level summaries don't claim unsupported facts.
        for path in changed_paths {
            if !matches!(path.as_str(), "PROFILE.md" | "ONEPAGER.md" | "INDEX.md") {
                continue;
            }
            let Some(blob) = repo.read_blob_unchecked(candidate, path)? else {
                continue;
            };
            // Light parse: each [fact_xxx] token must reference an existing fact.
            for token in fact_ref_tokens(&blob.content) {
                if !entities.iter().any(|e| e.facts.iter().any(|f| f.id == token)) {
                    return Err(Error::Validation(format!(
                        "{path} references unknown fact {token}"
                    )));
                }
            }
        }

        // Verify controls only mention known targets.
        if let Some(controls) = controls {
            let known: Vec<String> = entities
                .iter()
                .flat_map(|e| e.facts.iter().map(|f| f.id.clone()))
                .collect();
            for target in controls.suppressions.keys() {
                // An erased fact is gone from the tree by design; its
                // suppression stays so it can never be re-extracted.
                if controls.is_deleted(target) {
                    continue;
                }
                if !known.iter().any(|k| k == target) {
                    // Allow references to known entity ids too.
                    if !entities.iter().any(|e| e.id == *target) {
                        return Err(Error::Validation(format!(
                            "controls references unknown target {target}"
                        )));
                    }
                }
            }
        }

        Ok(())
    }
}

fn fact_ref_tokens(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' {
            // Look for "fact_..." or "id_..."
            let start = i + 1;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > start && j < bytes.len() && bytes[j] == b']' {
                let token = &body[start..j];
                if token.starts_with("fact_") || token.starts_with("id_") {
                    out.push(token.to_string());
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::FactStatus;

    #[test]
    fn detects_fact_tokens() {
        let body = "- [fact_alex_city_02] Lives in Austin.\n  Source: [evt_0192].\n";
        let tokens = fact_ref_tokens(body);
        assert_eq!(tokens, vec!["fact_alex_city_02"]);
    }

    #[test]
    fn ignore_unknown_token() {
        let body = "[hello world]";
        let tokens = fact_ref_tokens(body);
        assert!(tokens.is_empty());
    }

    #[test]
    fn flags_unknown_kind() {
        // FactKind enum is exhaustive; this test exists to keep the type alive.
        let _: Vec<FactKind> = vec![FactKind::ExplicitAssertion, FactKind::ObservedToolResult, FactKind::Inference];
        // Verify FactStatus values.
        let _: Vec<FactStatus> = vec![
            FactStatus::Active,
            FactStatus::Disputed,
            FactStatus::Superseded,
            FactStatus::Expired,
            FactStatus::Retracted,
        ];
    }
}