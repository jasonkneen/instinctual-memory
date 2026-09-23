//! Suppression and invalidation controls.
//!
//! `state/controls.json` is the canonical control file. It records:
//! - explicit suppressions (soft forget),
//! - explicit retractions of facts,
//! - deletion requests that drive the erasure pipeline,
//! - source-event redaction markers.
//!
//! Every read filters eligible facts through this control file before returning
//! a snippet.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Controls {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub suppressions: HashMap<String, Suppression>,
    #[serde(default)]
    pub retractions: HashSet<String>,
    #[serde(default)]
    pub deletions: HashSet<DeletionRecord>,
    #[serde(default)]
    pub redacted_events: HashSet<String>,
}

fn default_schema() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Suppression {
    pub target: String,
    pub kind: SuppressionKind,
    pub suppressed_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SuppressionKind {
    /// Suppressed for all ordinary retrieval; storage may persist.
    SoftForget,
    /// Suppressed for the supplied audience only.
    Audience,
    /// Suppressed until a future review date.
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct DeletionRecord {
    pub target: String,
    pub requested_at: DateTime<Utc>,
    pub pipeline_state: DeletionState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum DeletionState {
    Requested,
    InProgress,
    Completed,
}

impl Controls {
    pub fn parse(content: &str) -> Result<Self> {
        let controls: Controls = serde_json::from_str(content)
            .map_err(|e| Error::InvalidControls(e.to_string()))?;
        Ok(controls)
    }

    pub fn render(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(Error::Json)
    }

    pub fn is_suppressed(&self, target: &str) -> bool {
        self.suppressions.contains_key(target)
    }

    pub fn is_retracted(&self, target: &str) -> bool {
        self.retractions.contains(target)
    }

    pub fn is_deleted(&self, target: &str) -> bool {
        self.deletions.iter().any(|d| d.target == target)
    }

    pub fn is_redacted(&self, event_id: &str) -> bool {
        self.redacted_events.contains(event_id)
    }

    /// Apply an additional suppression. Idempotent.
    pub fn add_suppression(&mut self, suppression: Suppression) {
        self.suppressions
            .insert(suppression.target.clone(), suppression);
    }

    pub fn add_retraction(&mut self, target: impl Into<String>) {
        self.retractions.insert(target.into());
    }

    pub fn add_deletion(&mut self, target: impl Into<String>) {
        let target = target.into();
        if !self.deletions.iter().any(|d| d.target == target) {
            self.deletions.insert(DeletionRecord {
                target,
                requested_at: Utc::now(),
                pipeline_state: DeletionState::Requested,
            });
        }
    }

    pub fn add_redaction(&mut self, event_id: impl Into<String>) {
        self.redacted_events.insert(event_id.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut c = Controls::default();
        c.add_suppression(Suppression {
            target: "fact_x".into(),
            kind: SuppressionKind::SoftForget,
            suppressed_at: Utc::now(),
            reason: None,
        });
        c.add_retraction("fact_y");
        let rendered = c.render().unwrap();
        let parsed = Controls::parse(&rendered).unwrap();
        assert!(parsed.is_suppressed("fact_x"));
        assert!(parsed.is_retracted("fact_y"));
    }
}