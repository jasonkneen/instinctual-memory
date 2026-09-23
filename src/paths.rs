//! Path and revision validation.
//!
//! The repository is the only writable surface; the agent only ever talks to it
//! through validated paths. Allowed paths are restricted to:
//! - top-level Markdown summaries (`PROFILE.md`, `ONEPAGER.md`, `INDEX.md`),
//! - entity files under `memory/<bucket>/<id>.md`,
//! - session recap and task files under `sessions/<session-id>/`,
//! - the small JSON control files (`state/checkpoint.json`, `state/controls.json`),
//! - and the internal receipt path `state/receipts/<request-id>.json`.
//!
//! The receipt path is constructed only by the trusted publisher; it is not a
//! writable user surface.

use crate::error::{Error, Result};
use ::regex::{Regex, RegexSet};

/// Regex set used to validate writable paths.
pub struct PathRules {
    set: RegexSet,
    entity_id: Regex,
    session_id: Regex,
}

impl PathRules {
    pub fn new() -> Self {
        let patterns = [
            r"(?:PROFILE|ONEPAGER|INDEX)\.md",
            r"memory/(?:people|orgs|preferences|conversations|workstreams)/[A-Za-z0-9_-]{1,80}\.md",
            r"sessions/[A-Za-z0-9_-]{1,80}/(?:RECAP|TASKS)\.md",
            r"state/(?:checkpoint|controls)\.json",
            r"state/dispositions/[A-Za-z0-9_-]{1,80}\.json",
        ];
        let set = RegexSet::new(patterns).expect("static patterns compile");
        let entity_id = Regex::new(r"^(?:p|o|pref|c|w)_(?:[A-Za-z0-9_-]{1,80})$")
            .expect("static pattern compiles");
        let session_id = Regex::new(r"^[A-Za-z0-9_-]{1,80}$").expect("static pattern");
        Self {
            set,
            entity_id,
            session_id,
        }
    }

    /// Validate a path against the writable set.
    pub fn validate<'a>(&self, path: &'a str) -> Result<&'a str> {
        if !self.set.is_match(path) {
            return Err(Error::BadPath(path.to_string()));
        }
        Ok(path)
    }

    /// True if `path` looks like a public writable path.
    pub fn is_public(&self, path: &str) -> bool {
        self.set.is_match(path)
    }

    /// True if `path` is the internal receipt path.
    pub fn is_receipt(&self, path: &str) -> bool {
        let Some(rest) = path.strip_prefix("state/receipts/") else {
            return false;
        };
        let Some(name) = rest.strip_suffix(".json") else {
            return false;
        };
        // Request IDs are validated separately for stricter rules; we are
        // permissive here so the caller can return the strict error.
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && name.len() <= 80
    }

    /// Validate an entity id like `p_alex`.
    pub fn validate_entity_id<'a>(&self, id: &'a str) -> Result<&'a str> {
        if !self.entity_id.is_match(id) {
            return Err(Error::InvalidEntityId(id.to_string()));
        }
        Ok(id)
    }

    /// Validate a session id.
    pub fn validate_session_id<'a>(&self, id: &'a str) -> Result<&'a str> {
        if !self.session_id.is_match(id) {
            return Err(Error::BadPath(format!("session id: {id}")));
        }
        Ok(id)
    }
}

impl Default for PathRules {
    fn default() -> Self {
        Self::new()
    }
}

/// Validate that a candidate revision is a full hex object id.
///
/// Accepts 40-character SHA-1 and 64-character SHA-256 hashes.
pub fn revision_id(value: &str) -> Result<&str> {
    let len = value.len();
    if !matches!(len, 40 | 64) {
        return Err(Error::BadRevision(value.to_string()));
    }
    if !value.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        return Err(Error::BadRevision(value.to_string()));
    }
    Ok(value)
}

/// Validate a request id (alphanumeric, underscore, dash; 1..=80).
pub fn request_id(value: &str) -> Result<&str> {
    if value.is_empty() || value.len() > 80 {
        return Err(Error::BadRequestId(value.to_string()));
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::BadRequestId(value.to_string()));
    }
    Ok(value)
}

/// The published branch reference.
pub const MEMORY_REF: &str = "refs/heads/memory";

/// Internal receipt directory.
pub const RECEIPTS_DIR: &str = "state/receipts";

/// Compose the canonical receipt path for a request id.
pub fn receipt_path(request_id: &str) -> String {
    format!("{RECEIPTS_DIR}/{request_id}.json")
}

/// Compose a deletion sentinel for the null SHA-1 object ID (40 zeros).
pub fn null_oid(hex_len: usize) -> String {
    "0".repeat(hex_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_top_level_summaries() {
        let r = PathRules::new();
        assert!(r.validate("PROFILE.md").is_ok());
        assert!(r.validate("ONEPAGER.md").is_ok());
        assert!(r.validate("INDEX.md").is_ok());
    }

    #[test]
    fn accepts_entity_files() {
        let r = PathRules::new();
        assert!(r.validate("memory/people/p_alex.md").is_ok());
        assert!(r.validate("memory/orgs/o_acme.md").is_ok());
        assert!(r.validate("memory/preferences/pref_comms.md").is_ok());
        assert!(r.validate("memory/conversations/c_example.md").is_ok());
        assert!(r.validate("memory/workstreams/w_office.md").is_ok());
    }

    #[test]
    fn accepts_session_files() {
        let r = PathRules::new();
        assert!(r.validate("sessions/s_2026_09_20/RECAP.md").is_ok());
        assert!(r.validate("sessions/s_2026_09_20/TASKS.md").is_ok());
    }

    #[test]
    fn accepts_state_files() {
        let r = PathRules::new();
        assert!(r.validate("state/checkpoint.json").is_ok());
        assert!(r.validate("state/controls.json").is_ok());
    }

    #[test]
    fn rejects_other_paths() {
        let r = PathRules::new();
        assert!(r.validate("../etc/passwd").is_err());
        assert!(r.validate("README.md").is_err());
        assert!(r.validate("memory/people/../../escape.md").is_err());
        assert!(r.validate("").is_err());
    }

    #[test]
    fn revision_validation_accepts_sha1_and_sha256() {
        let sha1 = "0".repeat(40);
        let sha256 = "a".repeat(64);
        assert!(revision_id(&sha1).is_ok());
        assert!(revision_id(&sha256).is_ok());
    }

    #[test]
    fn revision_validation_rejects_garbage() {
        assert!(revision_id("").is_err());
        assert!(revision_id("not-a-hash").is_err());
        assert!(revision_id(&"a".repeat(39)).is_err());
        assert!(revision_id(&"a".repeat(65)).is_err());
        assert!(revision_id(&"A".repeat(40)).is_err()); // uppercase rejected
    }

    #[test]
    fn request_id_validation() {
        assert!(request_id("batch_1").is_ok());
        assert!(request_id("req_01JABCDEFG").is_ok());
        assert!(request_id("").is_err());
        assert!(request_id(&"a".repeat(81)).is_err());
        assert!(request_id("bad id!").is_err());
    }

    #[test]
    fn receipt_path_validation() {
        let r = PathRules::new();
        assert!(r.is_receipt("state/receipts/batch_1.json"));
        assert!(!r.is_receipt("state/receipts/.json"));
        assert!(!r.is_receipt("state/checkpoint.json"));
    }
}