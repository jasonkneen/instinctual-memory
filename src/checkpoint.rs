//! Consolidation checkpoint.
//!
//! The checkpoint records the high-water mark of accepted journal events plus
//! the disposition of quarantined events. It is published in the same Git
//! commit as the facts it covers so they advance atomically.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Checkpoint {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub through_seq: u64,
    #[serde(default)]
    pub accepted: HashSet<String>,
    #[serde(default)]
    pub quarantined: BTreeMap<String, Quarantine>,
    #[serde(default)]
    pub last_consolidated_at: Option<DateTime<Utc>>,
}

fn default_schema() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Quarantine {
    pub event_id: String,
    pub reason: String,
    pub noted_at: DateTime<Utc>,
}

impl Checkpoint {
    pub fn empty() -> Self {
        Self {
            schema_version: 1,
            through_seq: 0,
            accepted: HashSet::new(),
            quarantined: BTreeMap::new(),
            last_consolidated_at: None,
        }
    }

    pub fn parse(content: &str) -> Result<Self> {
        let cp: Checkpoint =
            serde_json::from_str(content).map_err(|e| Error::InvalidCheckpoint(e.to_string()))?;
        Ok(cp)
    }

    pub fn render(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(Error::Json)
    }

    /// Advance the checkpoint, recording accepted event ids.
    ///
    /// `events` must be a contiguous run ending at `through_seq`; non-monotonic
    /// input is rejected. Accepted ids are recorded verbatim; quarantined ids
    /// are recorded with their reasons.
    pub fn advance(
        &mut self,
        through_seq: u64,
        accepted: HashSet<String>,
        quarantined: BTreeMap<String, Quarantine>,
    ) -> Result<()> {
        if through_seq < self.through_seq {
            return Err(Error::InvalidCheckpoint(format!(
                "through_seq {} < existing {}",
                through_seq, self.through_seq
            )));
        }
        self.through_seq = through_seq;
        for id in accepted {
            self.accepted.insert(id);
        }
        for (id, q) in quarantined {
            self.quarantined.insert(id, q);
        }
        self.last_consolidated_at = Some(Utc::now());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut cp = Checkpoint::empty();
        cp.advance(5, HashSet::from(["evt_1".into()]), BTreeMap::new())
            .unwrap();
        let rendered = cp.render().unwrap();
        let parsed = Checkpoint::parse(&rendered).unwrap();
        assert_eq!(parsed.through_seq, 5);
        assert!(parsed.accepted.contains("evt_1"));
    }

    #[test]
    fn rejects_non_monotonic_advance() {
        let mut cp = Checkpoint::empty();
        cp.advance(5, HashSet::new(), BTreeMap::new()).unwrap();
        let err = cp
            .advance(4, HashSet::new(), BTreeMap::new())
            .unwrap_err();
        match err {
            Error::InvalidCheckpoint(_) => {}
            other => panic!("unexpected: {other}"),
        }
    }
}