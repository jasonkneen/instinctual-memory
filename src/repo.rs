//! Publication layer for the canonical memory repository.
//!
//! This is the trusted snapshot core. It is **not** a model-facing API: only the
//! service calls into it. Readers receive opaque IDs; writers receive validated
//! content. Publication is a compare-and-swap against the published branch so
//! retries are safe and stale writers are rejected.
//!
//! The layer is deliberately small. It does not parse facts, enforce scopes,
//! or apply domain rules. Those responsibilities live in [`crate::validate`]
//! and the modules above it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::git::{ensure_dir, Git};
use crate::paths::{
    receipt_path, request_id as validate_request_id, revision_id, PathRules, MEMORY_REF,
};

/// Upper bound on one published change batch. Consolidation may legitimately
/// rewrite up to 60 entities of ~56 KiB each, plus `INDEX.md` and the
/// dispositions, so the cap has to clear that; it only bounds a single commit
/// and the in-memory digest, it is not meant to police normal batches.
const MAX_CHANGE_BYTES: usize = 8 * 1024 * 1024;

/// Conflict during publication. The caller should reconcile and retry.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Conflict(pub String);

/// Successful publish result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishReceipt {
    /// The commit id of the published candidate.
    pub revision: String,
    /// True when this request was already published under the same request id;
    /// the caller should treat this as success.
    pub replayed: bool,
}

/// A snapshot read of a single file from a pinned revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Blob {
    pub revision: String,
    pub path: String,
    pub content: String,
}

/// Trusted snapshot core. Holds a single bare repository and the path rules.
pub struct GitRepo {
    git: Git,
    rules: PathRules,
    intents_dir: PathBuf,
    lock_path: PathBuf,
}

impl GitRepo {
    /// Open an existing repository. The path must point to a bare repository.
    pub fn open(repo: impl AsRef<Path>) -> Result<Self> {
        let git = Git::new(repo.as_ref())?;
        Ok(Self::from_git(git, repo.as_ref()))
    }

    fn from_git(git: Git, repo: &Path) -> Self {
        let root = repo.parent().unwrap_or_else(|| Path::new("."));
        Self {
            git,
            rules: PathRules::new(),
            intents_dir: root.join("intents"),
            lock_path: root.join("writer.lock"),
        }
    }

    /// Bootstrap a brand-new bare repository under `root` and return the store.
    pub fn create(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let repo_dir = root.join("memory.git");
        let journal_dir = root.join("journal");
        ensure_dir(root)?;
        ensure_dir(&journal_dir)?;
        let git = Git::create(&repo_dir)?;
        Ok(Self::from_git(git, &repo_dir))
    }

    /// The bare Git directory.
    pub fn repo_path(&self) -> &Path {
        self.git.repo()
    }

    /// Directory used for durable publication intents.
    pub fn intents_dir(&self) -> &Path {
        &self.intents_dir
    }

    /// The hardened Git runner for this repository.
    pub(crate) fn git(&self) -> &Git {
        &self.git
    }

    /// File used for the namespace writer lock.
    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    /// Path validation rules.
    pub fn rules(&self) -> &PathRules {
        &self.rules
    }

    /// Resolve the published branch to a full commit id.
    pub fn head(&self) -> Result<String> {
        let out = self
            .git
            .run_args(&["rev-parse", "--verify", &format!("{MEMORY_REF}^{{commit}}")], None, None)?;
        Ok(revision_id(out.trim())?.to_string())
    }

    /// Acquire the namespace writer lock for the short publication phase.
    pub fn acquire_writer_lock(&self) -> Result<WriterLock<'_>> {
        ensure_dir(self.lock_path.parent().unwrap_or_else(|| Path::new(".")))?;
        WriterLock::acquire(self)
    }

    /// Read a single file from a pinned revision. The path must be a public
    /// writable path; receipt paths are read through [`Self::read_receipt`].
    pub fn read_snapshot(&self, revision: &str, path: &str) -> Result<Option<Blob>> {
        self.rules.validate(path)?;
        self.read_blob(revision, path)
    }

    /// Walk the pinned tree and return every eligible entity file as
    /// `(path, blob)`. Eligibility here means the file exists; status and
    /// suppression filtering happens at the caller.
    pub fn list_eligible_entity_files(&self, revision: &str) -> Result<Vec<(String, Blob)>> {
        revision_id(revision)?;
        let tree = self
            .git
            .run_args(&["ls-tree", "-r", "--name-only", revision], None, None)?;
        let mut out = Vec::new();
        for line in tree.lines() {
            let path = line.trim();
            if !path.starts_with("memory/") || !path.ends_with(".md") {
                continue;
            }
            if let Some(blob) = self.read_blob(revision, path)? {
                out.push((path.to_string(), blob));
            }
        }
        Ok(out)
    }

    /// Read a receipt for a known request id (internal use only).
    pub fn read_receipt(&self, revision: &str, req_id: &str) -> Result<Option<Receipt>> {
        validate_request_id(req_id)?;
        let path = receipt_path(req_id);
        match self.read_blob(revision, &path)? {
            Some(b) => Ok(Some(serde_json::from_str(&b.content)?)),
            None => Ok(None),
        }
    }

    /// Read the durable intent for a request id, if any.
    pub fn read_intent(&self, req_id: &str) -> Result<Option<Intent>> {
        validate_request_id(req_id)?;
        let path = self.intents_dir.join(format!("{req_id}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(Error::io(path, err)),
        }
    }

    /// Persist a durable intent before publication so a retry can recover.
    pub fn write_intent(&self, intent: &Intent) -> Result<()> {
        ensure_dir(&self.intents_dir)?;
        let path = self.intents_dir.join(format!("{}.json", intent.request_id));
        let json = serde_json::to_vec_pretty(intent)?;
        write_atomic(&path, &json)
    }

    /// Mark an intent as completed by removing it (best-effort).
    pub fn complete_intent(&self, req_id: &str) -> Result<()> {
        validate_request_id(req_id)?;
        let path = self.intents_dir.join(format!("{req_id}.json"));
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(Error::io(path, err)),
        }
    }

    /// Publish a candidate change set against `base` under `request_id`.
    ///
    /// The `validator` is called once with the candidate revision and the
    /// ordered list of changed paths. It must inspect the candidate tree and
    /// reject any violation of the domain rules. The validator must not modify
    /// the repository or perform external side effects.
    pub fn publish(
        &self,
        base: &str,
        req_id: &str,
        changes: &ChangeSet,
        validator: &dyn Validator,
    ) -> Result<PublishReceipt> {
        revision_id(base)?;
        validate_request_id(req_id)?;
        changes.validate()?;

        // Compute the change digest for replay detection.
        let payload = serde_json::to_vec(&changes.canonical(base))?;
        if payload.len() > MAX_CHANGE_BYTES {
            return Err(Error::BadChangeBatch(format!(
                "change batch exceeds {} MiB budget",
                MAX_CHANGE_BYTES / (1024 * 1024)
            )));
        }
        let mut hasher = Sha256::new();
        hasher.update(&payload);
        let digest = format!("{:x}", hasher.finalize());

        let current = self.head()?;
        // Replay check: an identical request_id+changes was already published.
        // The receipt lookup runs against the current head; retries that use
        // the same request_id and changes must return the existing receipt
        // even when newer commits have advanced the published branch.
        if let Some(receipt) = self.read_receipt(&current, req_id)? {
            if receipt.digest != digest {
                return Err(Error::Conflict(format!(
                    "request id reused with different changes: {req_id}"
                )));
            }
            return Ok(PublishReceipt {
                revision: current,
                replayed: true,
            });
        }
        if current != base {
            return Err(Error::Conflict(
                "published memory changed; re-read and reconcile".into(),
            ));
        }

        let recv_path = receipt_path(req_id);
        let receipt_blob = serde_json::to_vec(&Receipt {
            digest: digest.clone(),
            base: base.to_string(),
        })?;
        let receipt_text = std::str::from_utf8(&receipt_blob)
            .map_err(|_| Error::InvalidContent("receipt not utf-8".into()))?
            .to_string();

        // Prepare the candidate in an isolated index.
        let tmp = tempfile::tempdir()?;
        let index = tmp.path().join("index");
        self.git
            .run_args(&["read-tree", base], None, Some(&index))?;

        // Build the change map including the receipt.
        let mut updates: Vec<(String, Change)> = Vec::with_capacity(changes.len() + 1);
        for (path, change) in changes.iter() {
            updates.push((path.clone(), change.clone()));
        }
        updates.push((recv_path.clone(), Change::Write { content: receipt_text }));

        let null_oid = "0".repeat(base.len());
        for (path, change) in &updates {
            let _ = self.rules.validate(path).or_else(|_| {
                if self.rules.is_receipt(path) {
                    Ok(path.as_str())
                } else {
                    Err(Error::BadPath(path.clone()))
                }
            })?;
            match change {
                Change::Write { content } => {
                    let blob = self
                        .git
                        .run_args(&["hash-object", "-w", "--stdin"], Some(content.as_bytes()), Some(&index))?;
                    let blob = blob.trim();
                    self.git.run_args(
                        &["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{path}")],
                        None,
                        Some(&index),
                    )?;
                }
                Change::Delete => {
                    let entry = format!("0 {null_oid}\t{path}\n");
                    self.git.run_args(
                        &["update-index", "--index-info"],
                        Some(entry.as_bytes()),
                        Some(&index),
                    )?;
                }
            }
        }
        let tree = self
            .git
            .run_args(&["write-tree"], None, Some(&index))?
            .trim()
            .to_string();
        let candidate = self
            .git
            .run_args(
                &["commit-tree", &tree, "-p", base],
                Some(format!("memory: {req_id}\n").as_bytes()),
                None,
            )?
            .trim()
            .to_string();
        revision_id(&candidate)?;

        let changed_paths: Vec<String> = changes.iter().map(|(p, _)| p.clone()).collect();
        validator.validate(self, &candidate, &changed_paths)?;

        // Compare-and-swap on the published branch.
        let update_result = self
            .git
            .run_args(&["update-ref", MEMORY_REF, &candidate, base], None, None);
        if let Err(err) = update_result {
            let now = self.head();
            if !matches!(now, Ok(ref h) if h == base) {
                return Err(Error::Conflict(
                    "another writer published first; reconcile and retry".into(),
                ));
            }
            return Err(err);
        }

        Ok(PublishReceipt {
            revision: candidate,
            replayed: false,
        })
    }

    /// Read a single file from a pinned revision without enforcing the
/// writable-path set. Use this for internal validation paths that may be
/// writable-path set. Use this for internal validation paths that may be
/// receipts or other internal state; external callers should use
/// [`Self::read_snapshot`].
    pub fn read_blob_unchecked(&self, revision: &str, path: &str) -> Result<Option<Blob>> {
        revision_id(revision)?;
        self.read_blob(revision, path)
    }

fn read_blob(&self, revision: &str, path: &str) -> Result<Option<Blob>> {
        revision_id(revision)?;
        let raw = self
            .git
            .run_args(&["ls-tree", "-z", revision, "--", path], None, None)?;
        let entries: Vec<&str> = raw.split('\0').filter(|s| !s.is_empty()).collect();
        if entries.is_empty() {
            return Ok(None);
        }
        if entries.len() != 1 {
            return Err(Error::InvalidContent(format!(
                "expected one entry for {path}, got {}",
                entries.len()
            )));
        }
        let (metadata, name) = entries[0]
            .split_once('\t')
            .ok_or_else(|| Error::InvalidContent(format!("bad ls-tree entry: {}", entries[0])))?;
        let parts: Vec<&str> = metadata.split(' ').collect();
        if parts.len() != 3 {
            return Err(Error::InvalidContent(format!(
                "bad ls-tree metadata: {metadata}"
            )));
        }
        let (mode, kind, oid) = (parts[0], parts[1], parts[2]);
        if name != path || mode != "100644" || kind != "blob" {
            return Err(Error::InvalidContent(format!(
                "unsupported entry mode/kind {mode} {kind}"
            )));
        }
        let size_out = self.git.run_args(&["cat-file", "-s", oid], None, None)?;
        let size: usize = size_out
            .trim()
            .parse()
            .map_err(|_| Error::InvalidContent(format!("bad size for {oid}")))?;
        if size > 65_536 {
            return Err(Error::FileTooLarge(PathBuf::from(path)));
        }
        let body = self.git.run_args(&["cat-file", "blob", oid], None, None)?;
        Ok(Some(Blob {
            revision: revision.to_string(),
            path: path.to_string(),
            content: body,
        }))
    }
}

/// A single file change: either write new content or delete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    Write { content: String },
    Delete,
}

/// Ordered set of file changes for one publication.
///
/// The set is validated when constructed: paths must be public writable paths
/// or the internal receipt path, text files must be valid UTF-8 ≤ 64 KiB, and
/// JSON files must parse.
#[derive(Debug, Clone)]
pub struct ChangeSet {
    inner: BTreeMap<String, Change>,
}

impl ChangeSet {
    /// Construct a new change set from an iterable of `(path, content)` pairs.
    /// A `None` content represents a deletion.
    pub fn new<I, P, C>(changes: I) -> Result<Self>
    where
        I: IntoIterator<Item = (P, C)>,
        P: Into<String>,
        C: Into<ChangeInput>,
    {
        let rules = PathRules::new();
        let mut inner = BTreeMap::new();
        for (path, content) in changes {
            let path = path.into();
            if !rules.is_public(&path) && !rules.is_receipt(&path) {
                return Err(Error::BadPath(path));
            }
            let change = match content.into() {
                ChangeInput::Write(text) => {
                    if text.len() > 65_536 {
                        return Err(Error::InvalidContent(format!(
                            "{path} exceeds 64 KiB"
                        )));
                    }
                    if path.ends_with(".json") {
                        serde_json::from_str::<serde_json::Value>(&text)
                            .map_err(Error::Json)?;
                    }
                    Change::Write { content: text }
                }
                ChangeInput::Delete => Change::Delete,
            };
            if inner.insert(path.clone(), change).is_some() {
                return Err(Error::BadChangeBatch(format!("duplicate path: {path}")));
            }
        }
        Ok(Self { inner })
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Change)> {
        self.inner.iter()
    }

    /// Validate counts and per-change rules.
    pub fn validate(&self) -> Result<()> {
        if self.inner.is_empty() {
            return Err(Error::BadChangeBatch("empty change set".into()));
        }
        if self.inner.len() > 64 {
            return Err(Error::BadChangeBatch(format!(
                "{} files exceeds 64-change limit",
                self.inner.len()
            )));
        }
        Ok(())
    }

    fn canonical(&self, base: &str) -> CanonicalValue {
        CanonicalValue {
            base: base.to_string(),
            changes: self.inner.clone(),
        }
    }
}

/// Either write some content or delete a file.
#[derive(Debug, Clone)]
pub enum ChangeInput {
    Write(String),
    Delete,
}

impl<S: Into<String>> From<S> for ChangeInput {
    fn from(value: S) -> Self {
        ChangeInput::Write(value.into())
    }
}

impl From<Change> for ChangeInput {
    fn from(c: Change) -> Self {
        match c {
            Change::Write { content: s } => ChangeInput::Write(s),
            Change::Delete => ChangeInput::Delete,
        }
    }
}

#[derive(Serialize)]
struct CanonicalValue {
    base: String,
    changes: BTreeMap<String, Change>,
}

/// A receipt committed alongside the change set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub digest: String,
    pub base: String,
}

/// A durable publication intent.
///
/// Persisting the intent lets a new process resume a request after a crash
/// without re-resolving the published branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub schema_version: u32,
    pub request_id: String,
    pub base: String,
    pub changes: BTreeMap<String, Change>,
    pub created_at: String,
}

impl Intent {
    pub fn new(req_id: &str, base: &str, changes: &ChangeSet) -> Result<Self> {
        validate_request_id(req_id)?;
        revision_id(base)?;
        Ok(Self {
            schema_version: 1,
            request_id: req_id.to_string(),
            base: base.to_string(),
            changes: changes.inner.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
        })
    }
}

/// A validator inspects a candidate commit before publication.
///
/// Implementations must:
/// - parse eligible facts only;
/// - check schema, scope, ownership, expected revisions, payload limits;
/// - not mutate the repository;
/// - not perform external side effects.
pub trait Validator {
    fn validate(&self, repo: &GitRepo, candidate: &str, changed_paths: &[String]) -> Result<()>;
}

/// Wrapper that satisfies the [`Validator`] trait for closures.
pub struct FnValidator<F>(pub F);

impl<F: Fn(&GitRepo, &str, &[String]) -> Result<()> + Send + Sync> Validator for FnValidator<F> {
    fn validate(&self, repo: &GitRepo, candidate: &str, changed_paths: &[String]) -> Result<()> {
        (self.0)(repo, candidate, changed_paths)
    }
}

/// Wrapper around a file lock guarding the short publication step.
pub struct WriterLock<'a> {
    file: std::fs::File,
    _repo: std::marker::PhantomData<&'a GitRepo>,
}

impl<'a> WriterLock<'a> {
    fn acquire(repo: &'a GitRepo) -> Result<Self> {
        let lock_path = repo.lock_path.clone();
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&lock_path)
            .map_err(|e| Error::io(&lock_path, e))?;
        fs4::FileExt::lock_exclusive(&file).map_err(|e| Error::io(&lock_path, e))?;
        Ok(Self { file, _repo: std::marker::PhantomData })
    }
}

impl<'a> Drop for WriterLock<'a> {
    fn drop(&mut self) {
        let _ = fs4::FileExt::unlock(&self.file);
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other("path has no parent"),
        })?;
    ensure_dir(parent)?;
    let tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| Error::io(parent, e))?;
    std::fs::write(tmp.path(), bytes).map_err(|e| Error::io(tmp.path(), e))?;
    tmp.persist(path).map_err(|e| Error::io(path, e.error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type BoxedCheck = Box<dyn Fn(&GitRepo, &str, &[String]) -> Result<()> + Send + Sync>;

    fn fixture_validator() -> FnValidator<BoxedCheck> {
        let f: BoxedCheck =
            Box::new(|store, candidate, changed| {
                for path in changed {
                    let body = if store.rules().is_receipt(path)
                        || path == "state/checkpoint.json"
                        || path == "state/controls.json"
                    {
                        store.read_blob_unchecked(candidate, path)?
                    } else {
                        store.read_snapshot(candidate, path)?
                    };
                    if let Some(b) = body {
                        if path.ends_with(".md") && !b.content.starts_with("# ") {
                            return Err(Error::Validation(format!(
                                "{path} must start with a Markdown heading"
                            )));
                        }
                    }
                }
                Ok(())
            });
        FnValidator(f)
    }

    #[test]
    fn create_initial_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let head = repo.head().unwrap();
        assert_eq!(head.len(), 40); // SHA-1 by default
    }

    #[test]
    fn publish_then_read() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let base = repo.head().unwrap();

        let changes = ChangeSet::new(vec![
            (
                "memory/people/p_alex.md".to_string(),
                "# Alex\nLives in Bristol.\n".to_string(),
            ),
            ("PROFILE.md".to_string(), "# Profile\nAlex lives in Bristol.\n".to_string()),
            (
                "state/checkpoint.json".to_string(),
                "{\"through_seq\":1}\n".to_string(),
            ),
        ])
        .unwrap();
        let receipt = repo
            .publish(&base, "batch_1", &changes, &fixture_validator())
            .unwrap();
        assert!(!receipt.replayed);

        let head = repo.head().unwrap();
        let profile = repo.read_snapshot(&head, "PROFILE.md").unwrap().unwrap();
        assert!(profile.content.contains("Bristol"));
    }

    #[test]
    fn duplicate_request_returns_replay() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let base = repo.head().unwrap();

        let changes = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nHi.\n".to_string(),
        )])
        .unwrap();

        let first = repo
            .publish(&base, "batch_1", &changes, &fixture_validator())
            .unwrap();
        assert!(!first.replayed);

        // Same request id, same base: replay returns the existing revision.
        let receipt = repo
            .publish(&base, "batch_1", &changes, &fixture_validator())
            .unwrap();
        assert!(receipt.replayed);
        assert_eq!(receipt.revision, repo.head().unwrap());
    }

    #[test]
    fn request_id_reused_with_different_changes_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let base = repo.head().unwrap();
        let first = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nA\n".to_string(),
        )])
        .unwrap();
        repo.publish(&base, "batch_1", &first, &fixture_validator())
            .unwrap();

        let second = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nB\n".to_string(),
        )])
        .unwrap();
        let head = repo.head().unwrap();
        let err = repo
            .publish(&head, "batch_1", &second, &fixture_validator())
            .unwrap_err();
        match err {
            Error::Conflict(_) => {}
            other => panic!("expected conflict, got {other}"),
        }
    }

    #[test]
    fn stale_base_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let base = repo.head().unwrap();

        let first = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nA\n".to_string(),
        )])
        .unwrap();
        let first_revision = repo
            .publish(&base, "batch_1", &first, &fixture_validator())
            .unwrap()
            .revision;

        let second = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nB\n".to_string(),
        )])
        .unwrap();
        repo.publish(&first_revision, "batch_2", &second, &fixture_validator())
            .unwrap();

        // Now try to publish against the original base using a new id.
        let third = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nC\n".to_string(),
        )])
        .unwrap();
        let err = repo
            .publish(&base, "batch_3", &third, &fixture_validator())
            .unwrap_err();
        match err {
            Error::Conflict(_) => {}
            other => panic!("expected conflict, got {other}"),
        }
    }

    #[test]
    fn deleted_file_absent_in_new_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let base = repo.head().unwrap();
        let add = ChangeSet::new(vec![(
            "memory/people/p_alex.md".to_string(),
            "# Alex\nLives in Bristol.\n".to_string(),
        )])
        .unwrap();
        let first = repo
            .publish(&base, "batch_1", &add, &fixture_validator())
            .unwrap()
            .revision;

        let del = ChangeSet::new(vec![(
            "memory/people/p_alex.md".to_string(),
            ChangeInput::Delete,
        )])
        .unwrap();
        let second = repo
            .publish(&first, "batch_2", &del, &fixture_validator())
            .unwrap()
            .revision;

        // New snapshot no longer contains the file.
        assert!(repo
            .read_snapshot(&second, "memory/people/p_alex.md")
            .unwrap()
            .is_none());
        // Old snapshot still does.
        assert!(repo
            .read_snapshot(&first, "memory/people/p_alex.md")
            .unwrap()
            .is_some());
    }

    #[test]
    fn path_and_size_restrictions_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let base = repo.head().unwrap();
        let oversized = "a".repeat(70_000);
        let bad = ChangeSet::new(vec![(
            "memory/people/p_alex.md".to_string(),
            oversized,
        )]);
        assert!(bad.is_err());

        // Path traversal rejected.
        let traversal = ChangeSet::new(vec![(
            "../escape.md".to_string(),
            "# Nope\n".to_string(),
        )]);
        assert!(traversal.is_err());

        // Receipt path is allowed.
        let receipt = ChangeSet::new(vec![(
            "state/receipts/manual.json".to_string(),
            "{\"digest\":\"abc\",\"base\":\"def\"}\n".to_string(),
        )])
        .unwrap();
        repo.publish(&base, "manual", &receipt, &fixture_validator())
            .unwrap();
    }

    #[test]
    fn competing_publishers_resolve_to_one_winner() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let repo1 = GitRepo::create(&root).unwrap();
        let repo2 = GitRepo::open(root.join("memory.git")).unwrap();
        let base = repo1.head().unwrap();

        let first_changes = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nA\n".to_string(),
        )])
        .unwrap();
        let second_changes = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nB\n".to_string(),
        )])
        .unwrap();

        // Two writers prepare independently and race for the CAS.
        let r1 = std::thread::spawn({
            let repo = repo1;
            let changes = first_changes;
            let base = base.clone();
            move || repo.publish(&base, "r1", &changes, &fixture_validator())
        });
        let r2 = std::thread::spawn({
            let repo = repo2;
            let changes = second_changes;
            let base = base.clone();
            move || repo.publish(&base, "r2", &changes, &fixture_validator())
        });

        let r1 = r1.join().unwrap();
        let r2 = r2.join().unwrap();
        let successes = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
        let conflicts = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Err(Error::Conflict(_))))
            .count();
        assert_eq!(successes, 1);
        assert_eq!(conflicts, 1);
    }

    #[test]
    fn reopening_preserves_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let repo1 = GitRepo::create(&root).unwrap();
        let base = repo1.head().unwrap();
        let changes = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "# Profile\nPersisted.\n".to_string(),
        )])
        .unwrap();
        repo1
            .publish(&base, "batch_1", &changes, &fixture_validator())
            .unwrap();

        // Reopen and check head.
        let repo2 = GitRepo::open(root.join("memory.git")).unwrap();
        let head = repo2.head().unwrap();
        let profile = repo2.read_snapshot(&head, "PROFILE.md").unwrap().unwrap();
        assert!(profile.content.contains("Persisted"));

        // Separate repository has nothing.
        let other_tmp = tempfile::tempdir().unwrap();
        let other = GitRepo::create(other_tmp.path()).unwrap();
        let other_head = other.head().unwrap();
        assert!(other
            .read_snapshot(&other_head, "PROFILE.md")
            .unwrap()
            .is_none());
    }

    #[test]
    fn validation_failure_leaves_branch_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = GitRepo::create(tmp.path()).unwrap();
        let base = repo.head().unwrap();
        let bad = ChangeSet::new(vec![(
            "PROFILE.md".to_string(),
            "no heading\n".to_string(),
        )])
        .unwrap();
        let err = repo
            .publish(&base, "batch_1", &bad, &fixture_validator())
            .unwrap_err();
        match err {
            Error::Validation(_) => {}
            other => panic!("expected validation error, got {other}"),
        }
        // Branch and checkpoint still at the empty initial commit.
        assert_eq!(repo.head().unwrap(), base);
    }
}