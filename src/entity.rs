//! Entity file parsing and rendering.
//!
//! An entity file is Markdown with a YAML frontmatter block. The frontmatter
//! is the structured record; the body is a human-readable rendering.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::fact::{Fact, Visibility};

/// A parsed entity file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntityFile {
    pub schema_version: u32,
    pub id: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    pub title: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub facts: Vec<Fact>,
    pub visibility: Visibility,
    #[serde(default)]
    pub body: String,
}

impl EntityFile {
    /// Parse an entity file from raw Markdown.
    pub fn parse(content: &str) -> Result<Self> {
        let (frontmatter, body) = split_frontmatter(content)?;
        let mut value: serde_yaml::Value = serde_yaml::from_str(&frontmatter)
            .map_err(|e| Error::Frontmatter {
                path: "<inline>".into(),
                message: e.to_string(),
            })?;
        // Normalise visibility: default to Private if absent.
        if let serde_yaml::Value::Mapping(ref mut map) = value {
            if !map.contains_key("visibility") {
                map.insert("visibility".into(), serde_yaml::Value::from("private"));
            }
        }
        let mut entity: EntityFile = serde_yaml::from_value(value).map_err(|e| Error::Frontmatter {
            path: "<inline>".into(),
            message: e.to_string(),
        })?;
        entity.body = body.to_string();
        for fact in &entity.facts {
            fact.validate()?;
        }
        Ok(entity)
    }

    /// Read an entity file from disk.
    pub fn read(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let mut parsed = Self::parse(&content)?;
        // Override the visibility default when the file does not declare one.
        if !content.contains("visibility:") {
            parsed.visibility = Visibility::Private;
        }
        Ok(parsed)
    }

    /// Render the entity back into Markdown.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("---\n");
        let mut value = serde_yaml::to_value(self).expect("entity is yaml-safe");
        if let serde_yaml::Value::Mapping(ref mut map) = value {
            map.remove("body");
            if matches!(self.visibility, Visibility::Private) {
                // Only suppress explicit visibility if it's the default.
                map.remove("visibility");
            }
        }
        let yaml = serde_yaml::to_string(&value).expect("entity is yaml-safe");
        out.push_str(&yaml);
        out.push_str("---\n\n");
        out.push_str(&self.body);
        if !self.body.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// Apply a new fact by id.
    pub fn upsert_fact(&mut self, fact: Fact) {
        for existing in &mut self.facts {
            if existing.id == fact.id {
                *existing = fact;
                return;
            }
        }
        self.facts.push(fact);
    }

    /// Rewrite the Markdown body as the human-readable form of the
    /// frontmatter: the title and every active fact with its id.
    pub fn refresh_body(&mut self) {
        let mut out = format!("# {}\n\n## Current facts\n\n", self.title);
        for fact in self.facts.iter().filter(|f| f.status == crate::fact::FactStatus::Active) {
            out.push_str(&format!("- [{}] {}\n", fact.id, fact.statement));
        }
        self.body = out;
    }

    /// Mark an existing fact as superseded by `new_id`.
    pub fn supersede(&mut self, old_id: &str, new_id: &str) -> Result<()> {
        let mut found = false;
        for fact in &mut self.facts {
            if fact.id == old_id {
                fact.status = crate::fact::FactStatus::Superseded;
                if !fact.supersedes.contains(&new_id.to_string()) {
                    fact.supersedes.push(new_id.to_string());
                }
                found = true;
            }
        }
        if !found {
            return Err(Error::InvalidFact(format!("no fact {old_id}")));
        }
        Ok(())
    }

    /// Mark an existing fact as retracted.
    pub fn retract(&mut self, fact_id: &str) -> Result<()> {
        for fact in &mut self.facts {
            if fact.id == fact_id {
                fact.status = crate::fact::FactStatus::Retracted;
                return Ok(());
            }
        }
        Err(Error::InvalidFact(format!("no fact {fact_id}")))
    }
}

fn split_frontmatter(content: &str) -> Result<(String, String)> {
    let trimmed = content.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed.strip_prefix("---\n") else {
        return Err(Error::Frontmatter {
            path: "<inline>".into(),
            message: "file must start with `---`".into(),
        });
    };
    let Some((front, body)) = rest.split_once("\n---\n") else {
        return Err(Error::Frontmatter {
            path: "<inline>".into(),
            message: "frontmatter must be closed with `---`".into(),
        });
    };
    Ok((front.to_string(), body.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{FactKind, FactStatus};

    #[test]
    fn round_trip() {
        let original = r#"---
schema_version: 2
id: p_alex
type: person
title: Alex Chen
aliases: [Alex, Alex Chen]
links: [o_acme]
facts: []
visibility: private
---

# Alex Chen
"#;
        let parsed = EntityFile::parse(original).unwrap();
        assert_eq!(parsed.id, "p_alex");
        assert_eq!(parsed.title, "Alex Chen");
        assert_eq!(parsed.body.trim(), "# Alex Chen");
    }

    #[test]
    fn parse_fact_block() {
        let content = r#"---
schema_version: 2
id: p_alex
type: person
title: Alex Chen
aliases: []
links: []
facts:
  - id: fact_alex_city_02
    predicate: home_city
    statement: Alex lives in Austin.
    kind: explicit_assertion
    status: active
    observed_at: '2026-09-20T19:41:00Z'
    valid_from: '2026-09-01T00:00:00Z'
    supersedes: []
    sources:
      - event_id: evt_0192
        role: user
        evidence: Alex lives in Austin.
    visibility: private
---

# Alex Chen
"#;
        let parsed = EntityFile::parse(content).unwrap();
        assert_eq!(parsed.facts.len(), 1);
        assert_eq!(parsed.facts[0].kind, FactKind::ExplicitAssertion);
        assert_eq!(parsed.facts[0].status, FactStatus::Active);
    }

    #[test]
    fn reject_missing_frontmatter() {
        let content = "# No frontmatter here\n";
        let err = EntityFile::parse(content).unwrap_err();
        match err {
            Error::Frontmatter { .. } => {}
            other => panic!("expected frontmatter error, got {other}"),
        }
    }
}