//! Structured fact schema and temporal eligibility.
//!
//! This is the canonical model used inside entity files. The YAML frontmatter
//! is the structured record; the Markdown body is a human rendering.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A single fact held against an entity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Fact {
    pub id: String,
    pub predicate: String,
    pub statement: String,
    pub kind: FactKind,
    pub status: FactStatus,
    pub observed_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_from_precision: Option<DatePrecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_to: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    #[serde(default)]
    pub sources: Vec<SourceRef>,
    pub visibility: Visibility,
}

impl Fact {
    /// Validate that the fact has all required fields and a usable structure.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty() {
            return Err(Error::InvalidFact("fact id required".into()));
        }
        if self.predicate.is_empty() {
            return Err(Error::InvalidFact(format!("{}: predicate required", self.id)));
        }
        if self.statement.is_empty() {
            return Err(Error::InvalidFact(format!(
                "{}: statement required",
                self.id
            )));
        }
        if let (Some(from), Some(to)) = (self.valid_from, self.valid_to) {
            if from > to {
                return Err(Error::InvalidFact(format!(
                    "{}: valid_from after valid_to",
                    self.id
                )));
            }
        }
        Ok(())
    }

    /// True if the fact is eligible for current-context retrieval at `now`.
    ///
    /// Excludes superseded, retracted, expired, and (optionally) disputed facts.
    pub fn is_eligible(&self, now: DateTime<Utc>, include_disputed: bool) -> bool {
        match self.status {
            FactStatus::Active => {}
            FactStatus::Superseded | FactStatus::Retracted | FactStatus::Expired => return false,
            FactStatus::Disputed if !include_disputed => return false,
            FactStatus::Disputed => {}
        }
        if let Some(expires) = self.expires_at {
            if expires <= now {
                return false;
            }
        }
        if let Some(to) = self.valid_to {
            if to <= now {
                return false;
            }
        }
        if let Some(from) = self.valid_from {
            if from > now {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    ExplicitAssertion,
    ObservedToolResult,
    Inference,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FactStatus {
    Active,
    Disputed,
    Superseded,
    Expired,
    Retracted,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Private,
    Shared,
    Public,
}

impl Visibility {
    pub fn allows(self, audience: Visibility) -> bool {
        use Visibility::*;
        matches!(
            (self, audience),
            (Public, _) | (Shared, Shared | Private) | (Private, Private)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceRef {
    pub event_id: String,
    pub role: Role,
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
    Note,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DatePrecision {
    Day,
    Month,
    Year,
    Instant,
}

impl DatePrecision {
    pub fn from_date(d: NaiveDate) -> DateTime<Utc> {
        DateTime::<Utc>::from_naive_utc_and_offset(d.and_hms_opt(0, 0, 0).unwrap(), Utc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_fact() -> Fact {
        Fact {
            id: "fact_1".into(),
            predicate: "home_city".into(),
            statement: "Lives in Bristol.".into(),
            kind: FactKind::ExplicitAssertion,
            status: FactStatus::Active,
            observed_at: Utc::now(),
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            expires_at: None,
            review_after: None,
            supersedes: vec![],
            sources: vec![],
            visibility: Visibility::Private,
        }
    }

    #[test]
    fn eligibility_filters_expired_and_superseded() {
        let mut fact = base_fact();
        fact.status = FactStatus::Superseded;
        assert!(!fact.is_eligible(Utc::now(), false));

        let mut fact = base_fact();
        fact.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(!fact.is_eligible(Utc::now(), false));
    }

    #[test]
    fn eligibility_allows_active() {
        let fact = base_fact();
        assert!(fact.is_eligible(Utc::now(), false));
    }

    #[test]
    fn eligibility_excludes_disputed_by_default() {
        let mut fact = base_fact();
        fact.status = FactStatus::Disputed;
        assert!(!fact.is_eligible(Utc::now(), false));
        assert!(fact.is_eligible(Utc::now(), true));
    }

    #[test]
    fn visibility_is_transitive() {
        assert!(Visibility::Public.allows(Visibility::Private));
        assert!(Visibility::Public.allows(Visibility::Shared));
        assert!(Visibility::Shared.allows(Visibility::Private));
        assert!(!Visibility::Private.allows(Visibility::Shared));
    }
}