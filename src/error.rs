//! Error types for the Portable Agent Memory CLI.

use std::path::PathBuf;
use thiserror::Error;

/// Result alias used across the library.
pub type Result<T> = std::result::Result<T, Error>;

/// All errors that can surface from `mem` library code.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),

    #[error("{0}: not found (or forgotten/erased)")]
    NotFound(String),

    #[error("not a mem store (or any of the parent directories): .mem")]
    NoStore,

    #[error("no memory store at {0}; run `mem init` in a project for a local store, or `mem init --global`")]
    NotInitialized(PathBuf),

    #[error("path outside the supported memory layout: {0}")]
    BadPath(String),

    #[error("invalid revision id: {0}")]
    BadRevision(String),

    #[error("invalid request id: {0}")]
    BadRequestId(String),

    #[error("invalid change batch: {0}")]
    BadChangeBatch(String),

    #[error("file exceeds read budget: {0}")]
    FileTooLarge(PathBuf),

    #[error("invalid or oversized text file: {0}")]
    InvalidContent(String),

    #[error("expected a bare repository at {0}")]
    NotBareRepository(PathBuf),

    #[error("repository directory already exists at {0}")]
    RepositoryExists(PathBuf),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("journal error: {0}")]
    Journal(String),

    #[error("frontmatter parse error in {path}: {message}")]
    Frontmatter { path: String, message: String },

    #[error("invalid fact: {0}")]
    InvalidFact(String),

    #[error("invalid controls: {0}")]
    InvalidControls(String),

    #[error("invalid checkpoint: {0}")]
    InvalidCheckpoint(String),

    #[error("invalid change request: {0}")]
    InvalidChange(String),

    #[error("invalid entity id: {0}")]
    InvalidEntityId(String),

    #[error("invalid adapter: {0}")]
    InvalidAdapter(String),

    #[error("invalid search query: {0}")]
    InvalidQuery(String),

    #[error("search failed: {0}")]
    SearchFailed(String),

    #[error("index error: {0}")]
    Index(String),

    #[error("jev error: {0}")]
    Jev(String),

    #[error("erasure error: {0}")]
    Erasure(String),

    #[error("validator rejected candidate: {0}")]
    Validation(String),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("git error: {0}")]
    Git(String),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io {
            path: PathBuf::new(),
            source: err,
        }
    }
}

impl Error {
    pub fn io<P: Into<PathBuf>>(path: P, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}