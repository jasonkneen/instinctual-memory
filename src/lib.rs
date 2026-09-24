//! Portable Agent Memory CLI library.
//!
//! Every milestone (M0–M7) from the PRD:
//! - M0 Git snapshot core with compare-and-swap publication
//! - M1 durable JSONL source-event journal with idempotent ingestion
//! - M2 pinned-tree lexical search with eligibility filters
//! - M3 immediate validated changes for remember / correct / forget
//! - M4 rules and LLM consolidation extractors
//! - M5 canonical INDEX.md published with every write
//! - M6 JEV reranker adapter
//! - M7 erasure: tree, journal sources, branch history, and intents

pub mod error;
pub mod paths;
pub mod git;
pub mod repo;
pub mod journal;
pub mod fact;
pub mod entity;
pub mod controls;
pub mod checkpoint;
pub mod change;
pub mod consolidate;
pub mod validate;
pub mod ingest;
pub mod index;
pub mod search;
pub mod jev;
pub mod erasure;
pub mod hook;
pub mod setup;
pub mod tidy;
pub mod evaluation;
pub mod cli;
pub mod mcp;
pub mod ops;
pub mod http_serve;
pub mod shell;

pub use error::{Error, Result};
pub use repo::{GitRepo, Conflict, PublishReceipt, Change, ChangeInput, ChangeSet, Validator, FnValidator, WriterLock, Blob, Receipt, Intent};
pub use journal::{Journal, JournalEvent, Role, Redaction, Source};
pub use fact::{Fact, FactStatus, FactKind, Visibility, SourceRef, Role as FactRole, DatePrecision};
pub use entity::EntityFile;
pub use controls::Controls;
pub use checkpoint::Checkpoint;
pub use change::{ChangeKind, ChangeRequest, ChangeOutcome, apply as apply_change, apply_against_base, recover_in_dir};
pub use consolidate::{Consolidator, ConsolidateOptions, ExtractorKind};
pub use ingest::{Adapter, ChatExportAdapter, ClaudeAdapter, CodexAdapter, PiAdapter, CalendarAdapter, VoiceAdapter, IdeHistoryAdapter, IngestReport, adapter_for};
pub use index::{render_index_md, INDEX_MD};
pub use search::{Search, SearchHit, LexicalSearch, SearchQuery, eligible_facts};
pub use jev::{JevReranker, JevDecision};
pub use erasure::{erase, ErasureReport};
pub use evaluation::{evaluate, CaseScore, EvaluationReport, Rubric, RubricCase, RunReport};
pub use cli::{Cli, run as run_cli};