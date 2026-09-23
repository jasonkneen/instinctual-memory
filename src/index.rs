//! The canonical `INDEX.md`.
//!
//! `INDEX.md` lists every entity in the store with its type, title, and how
//! many active facts it holds. It is published by the writer in the same
//! commit as the entities it describes (consolidation, `change`, `erase`),
//! so it always matches its snapshot. `mem index` republishes it from the
//! current head, and reports the head unchanged when the file is already
//! current. One line per entity keeps it far below the 64 KiB
//! file limit; past that budget the remainder is counted, not listed.

use std::collections::BTreeMap;

use crate::entity::EntityFile;
use crate::error::Result;
use crate::fact::FactStatus;
use crate::repo::{Change, ChangeSet, GitRepo};

/// Path of the index in the store.
pub const INDEX_MD: &str = "INDEX.md";

/// Bytes of entity rows before the rest are summarised.
const INDEX_MD_BUDGET: usize = 60_000;

/// Render `INDEX.md` for a set of entities.
pub fn render_index_md<'a>(entities: impl IntoIterator<Item = &'a EntityFile>) -> String {
    let mut rows: Vec<&EntityFile> = entities.into_iter().collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    let mut body = String::from("# Memory index\n\n");
    let mut listed = 0usize;
    for entity in &rows {
        let active = entity.facts.iter().filter(|f| f.status == FactStatus::Active).count();
        let row = format!(
            "- `{}` ({}) {} — {active} active fact{}\n",
            entity.id,
            entity.entity_type,
            entity.title,
            if active == 1 { "" } else { "s" }
        );
        if body.len() + row.len() > INDEX_MD_BUDGET {
            break;
        }
        body.push_str(&row);
        listed += 1;
    }
    if listed < rows.len() {
        body.push_str(&format!("- … and {} more entities\n", rows.len() - listed));
    }
    body
}

/// Render `INDEX.md` for the tree at `base` after replacing (`Some`) or
/// deleting (`None`) the entity files at the given paths.
pub fn index_md_after(
    repo: &GitRepo,
    base: &str,
    updates: &BTreeMap<String, Option<EntityFile>>,
) -> Result<String> {
    let mut by_path: BTreeMap<String, EntityFile> = BTreeMap::new();
    for (path, blob) in repo.list_eligible_entity_files(base)? {
        if let Ok(entity) = EntityFile::parse(&blob.content) {
            by_path.insert(path, entity);
        }
    }
    for (path, update) in updates {
        match update {
            Some(entity) => {
                by_path.insert(path.clone(), entity.clone());
            }
            None => {
                by_path.remove(path);
            }
        }
    }
    Ok(render_index_md(by_path.values()))
}

/// Republish `INDEX.md` from the current head. Returns the new revision, or
/// the head unchanged when the index is already current.
pub fn rebuild(repo: &GitRepo) -> Result<String> {
    let base = repo.head()?;
    let body = index_md_after(repo, &base, &BTreeMap::new())?;
    let current = repo.read_snapshot(&base, INDEX_MD)?.map(|b| b.content);
    if current.as_deref() == Some(body.as_str()) {
        return Ok(base);
    }
    let request_id = format!("index_{}", chrono::Utc::now().timestamp_millis());
    let changes = ChangeSet::new(vec![(INDEX_MD.to_string(), Change::Write { content: body })])?;
    Ok(repo.publish(&base, &request_id, &changes, &crate::validate::DomainValidator)?.revision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuild_publishes_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let first = rebuild(&repo).unwrap();
        let head = repo.head().unwrap();
        assert_eq!(first, head);
        let body = repo.read_snapshot(&head, INDEX_MD).unwrap().unwrap().content;
        assert!(body.starts_with("# Memory index"));
        assert_eq!(rebuild(&repo).unwrap(), head, "second rebuild changes nothing");
    }

    #[test]
    fn rows_are_bounded() {
        let entities: Vec<EntityFile> = (0..5000)
            .map(|i| EntityFile {
                schema_version: 2,
                id: format!("w_project_{i:05}"),
                entity_type: "project".into(),
                title: format!("Project number {i} with a fairly long descriptive title"),
                aliases: vec![],
                links: vec![],
                facts: vec![],
                visibility: crate::fact::Visibility::Private,
                body: String::new(),
            })
            .collect();
        let body = render_index_md(entities.iter());
        assert!(body.len() <= INDEX_MD_BUDGET + 100);
        assert!(body.contains("more entities"));
    }
}
