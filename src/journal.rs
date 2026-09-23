//! Durable source-event journal.
//!
//! The journal is the canonical input to memory. It lives outside the Git
//! repository and is governed independently of memory snapshots.
//!
//! Properties:
//! - one trusted writer assigns sequence numbers under a namespace lock;
//! - each event carries a stable id, source identity, role, observed and
//!   ingested timestamps, content, content hash, and redaction policy;
//! - records are framed, flushed, and `fsync`'d before acknowledgement;
//! - startup detects and recovers an incomplete tail;
//! - deduplication uses a stable source event id when available, with a
//!   fallback key that includes source identity and position, not content.
//!
//! Two write paths:
//! - [`Journal::append`] — single event, fsync per call. Safe for ad-hoc
//!   changes where each event is its own commit boundary.
//! - [`BulkWriter`] — amortised fsync across many events, cached sequence
//!   counter, reused per-day file handles. Used for backfill, ingest
//!   pipelines, and any other "thousands of events at once" workload.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// How often the [`BulkWriter`] issues a real fsync. Every event still gets
/// `flush()`'d to the OS, but `sync_all()` is amortised. Tuned to balance
/// crash-window size against throughput.
pub const DEFAULT_FLUSH_EVERY: usize = 256;

/// Environment knobs. `MEM_JOURNAL_FSYNC=0` disables fsync entirely for bulk
/// backfill — only safe when the journal is reproducible from source files
/// (which is always true in this codebase).
pub fn fsync_enabled() -> bool {
    !matches!(std::env::var("MEM_JOURNAL_FSYNC").ok().as_deref(), Some("0" | "false" | "no"))
}

pub fn flush_every_from_env() -> usize {
    if let Ok(v) = std::env::var("MEM_JOURNAL_FLUSH_EVERY") {
        if let Ok(n) = v.parse::<usize>() {
            return n.max(1);
        }
    }
    DEFAULT_FLUSH_EVERY
}

/// A single framed journal record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JournalEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub seq: u64,
    pub scope_id: String,
    pub session_id: String,
    pub occurred_at: DateTime<Utc>,
    pub ingested_at: DateTime<Utc>,
    pub role: Role,
    pub source: Source,
    pub content: String,
    pub content_sha256: String,
    pub redaction: Redaction,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl JournalEvent {
    /// Construct a new event record, computing the content hash and timestamps.
    pub fn new(
        scope_id: impl Into<String>,
        session_id: impl Into<String>,
        role: Role,
        source: Source,
        content: impl Into<String>,
        redaction: Redaction,
    ) -> Self {
        let content = content.into();
        let content_sha256 = sha256_hex(content.as_bytes());
        Self {
            schema_version: 1,
            event_id: source.event_id(),
            seq: 0,
            scope_id: scope_id.into(),
            session_id: session_id.into(),
            occurred_at: source.occurred_at().unwrap_or_else(Utc::now),
            ingested_at: Utc::now(),
            role,
            source,
            content,
            content_sha256,
            redaction,
            metadata: serde_json::Value::Object(Default::default()),
        }
    }

    /// Record the project this event belongs to, as the project's root
    /// folder (see [`project_root`]). An existing project is kept.
    pub fn set_project_from(&mut self, cwd: &str) {
        if cwd.is_empty() || self.project().is_some() {
            return;
        }
        let root = project_root(Path::new(cwd));
        if !self.metadata.is_object() {
            self.metadata = serde_json::Value::Object(Default::default());
        }
        self.metadata["project"] = serde_json::Value::String(root);
    }

    /// Absolute path of the project root this event belongs to, if known.
    pub fn project(&self) -> Option<&str> {
        self.metadata.get("project").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
    }

    /// Folder name of the project root (`/x/GitHub/agensis` -> `agensis`).
    pub fn project_name(&self) -> Option<&str> {
        self.project().and_then(|p| Path::new(p).file_name()).and_then(|n| n.to_str())
    }

    /// Fallback deduplication key combining source identity and position.
    pub fn dedup_key(&self) -> String {
        match &self.source {
            Source::Chat { source_id, .. } => format!("chat:{source_id}"),
            Source::Calendar { source_id, position, .. } => {
                format!("cal:{source_id}#{position}")
            }
            Source::Voice { source_id, position, .. } => {
                format!("voice:{source_id}#{position}")
            }
            Source::IdeHistory { source_id, position, .. } => {
                format!("ide:{source_id}#{position}")
            }
            Source::Custom { custom_kind, source_id, position, .. } => {
                format!("{custom_kind}:{source_id}#{position}")
            }
        }
    }
}

/// The project a working directory belongs to: the nearest ancestor that
/// holds a `.git` entry (repo or worktree root), else the directory itself.
/// Sessions often run in a subfolder; their project is still the repo.
pub fn project_root(cwd: &Path) -> String {
    static CACHE: once_cell::sync::Lazy<Mutex<HashMap<PathBuf, String>>> =
        once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));
    if let Some(hit) = CACHE.lock().get(cwd) {
        return hit.clone();
    }
    let root = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .display()
        .to_string();
    CACHE.lock().insert(cwd.to_path_buf(), root.clone());
    root
}

/// Content left in place of an erased event.
pub const ERASED: &str = "[erased]";

/// Role of the message or side event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    Tool,
    System,
    Note,
}

/// Source identity for an event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source_kind", rename_all = "snake_case")]
pub enum Source {
    Chat {
        source_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        occurred_at: Option<DateTime<Utc>>,
    },
    Calendar {
        source_id: String,
        position: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        occurred_at: Option<DateTime<Utc>>,
    },
    Voice {
        source_id: String,
        position: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        occurred_at: Option<DateTime<Utc>>,
    },
    IdeHistory {
        source_id: String,
        position: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        occurred_at: Option<DateTime<Utc>>,
    },
    Custom {
        custom_kind: String,
        source_id: String,
        position: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        occurred_at: Option<DateTime<Utc>>,
    },
}

impl Source {
    /// Stable identifier supplied by the adapter, when available.
    pub fn event_id(&self) -> String {
        match self {
            Source::Chat { source_id, .. } => format!("evt_chat_{source_id}"),
            Source::Calendar { source_id, position, .. } => {
                format!("evt_cal_{source_id}_{position}")
            }
            Source::Voice { source_id, position, .. } => {
                format!("evt_voice_{source_id}_{position}")
            }
            Source::IdeHistory { source_id, position, .. } => {
                format!("evt_ide_{source_id}_{position}")
            }
            Source::Custom { custom_kind, source_id, position, .. } => {
                format!("evt_{custom_kind}_{source_id}_{position}")
            }
        }
    }

    pub fn occurred_at(&self) -> Option<DateTime<Utc>> {
        match self {
            Source::Chat { occurred_at, .. }
            | Source::Calendar { occurred_at, .. }
            | Source::Voice { occurred_at, .. }
            | Source::IdeHistory { occurred_at, .. }
            | Source::Custom { occurred_at, .. } => *occurred_at,
        }
    }
}

/// Redaction policy attached to the stored content.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Redaction {
    #[default]
    None,
    Partial,
    Full,
}

/// Durable journal on disk.
#[derive(Debug)]
pub struct Journal {
    dir: PathBuf,
    sequence_lock: PathBuf,
}

impl Journal {
    /// Open an existing journal at `dir`, recovering from any incomplete tail.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        let sequence_lock = dir.join(".seq.lock");
        let j = Self { dir, sequence_lock };
        j.recover()?;
        Ok(j)
    }

    /// Directory used by the journal.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Recover any incomplete tail across the day's frames.
    pub fn recover(&self) -> Result<()> {
        for file in self.day_files()? {
            recover_tail(&file)?;
        }
        Ok(())
    }

    /// Append a single event, assigning the next sequence number under a lock.
    /// Always fsyncs before returning. Use this for ad-hoc change/ingest
    /// commands where each event is a commit boundary. For bulk workloads
    /// (backfill, bulk ingest), prefer [`BulkWriter`].
    pub fn append(&self, mut event: JournalEvent) -> Result<JournalEvent> {
        let max_seq = max_seq_in(&self.dir)?;
        event.seq = max_seq + 1;
        let bytes = serde_json::to_vec(&event)?;
        let line = framed_record(&bytes);

        // Serialize sequence assignment + append.
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.sequence_lock)
            .map_err(|e| Error::io(&self.sequence_lock, e))?;
        fs4::FileExt::lock_exclusive(&lock).map_err(|e| Error::io(&self.sequence_lock, e))?;

        let path = self.day_file_for(&event.ingested_at);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))?;
        file.write_all(&line).map_err(|e| Error::io(&path, e))?;
        file.flush().map_err(|e| Error::io(&path, e))?;
        if fsync_enabled() {
            file.sync_all().map_err(|e| Error::io(&path, e))?;
        }

        let _ = self.write_seq(event.seq + 1);
        drop(lock);

        Ok(event)
    }

    /// Iterate over every event in the journal in insertion order.
    /// Erase the content of the given events in place: each becomes
    /// `[erased]` with full redaction and its metadata cleared except the
    /// project. Day files are rewritten atomically under the sequence lock.
    /// Returns how many events were redacted by this call.
    pub fn redact(&self, event_ids: &std::collections::HashSet<String>) -> Result<usize> {
        if event_ids.is_empty() {
            return Ok(0);
        }
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.sequence_lock)
            .map_err(|e| Error::io(&self.sequence_lock, e))?;
        fs4::FileExt::lock_exclusive(&lock).map_err(|e| Error::io(&self.sequence_lock, e))?;
        let mut redacted = 0usize;
        for file in collect_day_files(&self.dir)? {
            let events: Vec<JournalEvent> = JournalIter::single(&file)?.collect::<Result<_>>()?;
            if !events.iter().any(|e| event_ids.contains(&e.event_id) && e.redaction != Redaction::Full) {
                continue;
            }
            let mut bytes = Vec::new();
            for mut event in events {
                if event_ids.contains(&event.event_id) && event.redaction != Redaction::Full {
                    event.content = ERASED.to_string();
                    event.content_sha256 = sha256_hex(ERASED.as_bytes());
                    event.redaction = Redaction::Full;
                    let project = event.metadata.get("project").cloned();
                    event.metadata = serde_json::json!({});
                    if let Some(project) = project {
                        event.metadata["project"] = project;
                    }
                    redacted += 1;
                }
                bytes.extend(framed_record(&serde_json::to_vec(&event)?));
            }
            let tmp = tempfile::NamedTempFile::new_in(&self.dir).map_err(|e| Error::io(&self.dir, e))?;
            std::fs::write(tmp.path(), &bytes).map_err(|e| Error::io(tmp.path(), e))?;
            tmp.as_file().sync_all().map_err(|e| Error::io(tmp.path(), e))?;
            tmp.persist(&file).map_err(|e| Error::io(&file, e.error))?;
        }
        if let Ok(dir) = File::open(&self.dir) {
            let _ = dir.sync_all();
        }
        drop(lock);
        Ok(redacted)
    }

    pub fn iter(&self) -> Result<JournalIter> {
        JournalIter::new(&self.dir)
    }

    /// Read all events into a vector (useful for tests and small consolidations).
    pub fn read_all(&self) -> Result<Vec<JournalEvent>> {
        let mut out = Vec::new();
        for event in self.iter()? {
            out.push(event?);
        }
        Ok(out)
    }

    /// Read events starting at `from_seq` (inclusive).
    pub fn read_from(&self, from_seq: u64) -> Result<Vec<JournalEvent>> {
        let mut out = Vec::new();
        for event in self.iter()? {
            let event = event?;
            if event.seq >= from_seq {
                out.push(event);
            }
        }
        Ok(out)
    }

    /// Open a [`BulkWriter`] over this journal. The writer amortises fsync,
    /// reuses day-file handles, and caches the next-seq counter for the
    /// lifetime of the process. Drop or call `flush()` to durably persist.
    pub fn bulk_writer(&self) -> Result<BulkWriter> {
        BulkWriter::open(&self.dir)
    }

    fn day_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&self.dir).map_err(|e| Error::io(&self.dir, e))? {
            let entry = entry.map_err(|e| Error::io(&self.dir, e))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("events-") && name.ends_with(".jsonl") {
                files.push(entry.path());
            }
        }
        files.sort();
        Ok(files)
    }

    fn day_file_for(&self, when: &DateTime<Utc>) -> PathBuf {
        self.dir.join(format!("events-{}.jsonl", when.format("%Y-%m-%d")))
    }

    fn write_seq(&self, seq: u64) -> Result<()> {
        let path = self.dir.join("seq");
        let tmp = tempfile::NamedTempFile::new_in(&self.dir).map_err(|e| Error::io(&self.dir, e))?;
        std::fs::write(tmp.path(), format!("{seq}\n").as_bytes())
            .map_err(|e| Error::io(tmp.path(), e))?;
        let _ = tmp.persist(&path).map_err(|e| Error::io(&path, e.error))?;
        Ok(())
    }
}

/// Find the largest sequence number present in the journal directory.
/// Scans every event — O(N) but only run once at startup.
fn max_seq_in(dir: &Path) -> Result<u64> {
    let mut max = 0u64;
    let iter = JournalIter::new(dir)?;
    for event in iter {
        let event = event?;
        if event.seq > max {
            max = event.seq;
        }
    }
    Ok(max)
}

/// Bulk journal writer. Holds the next-seq counter in memory, reuses open
/// per-day file handles, and amortises `sync_all()` calls. Thread-safe via
/// internal `Mutex` for the file-handle map (the hot path is the buffered
/// write, not the lock acquisition).
pub struct BulkWriter {
    dir: PathBuf,
    fsync: bool,
    flush_every: usize,
    next_seq: AtomicU64,
    pending: AtomicUsize,
    handles: Mutex<HashMap<String, BufWriter<File>>>,
    /// Event ids already in the journal. A repeated id is skipped, so
    /// re-running an ingest or backfill does not duplicate events.
    seen: Mutex<std::collections::HashSet<String>>,
    /// Held for the writer's lifetime so no other writer or redaction runs
    /// against the journal at the same time.
    _lock: File,
}

impl BulkWriter {
    /// Open a bulk writer for the journal directory. The starting seq is
    /// computed once by scanning existing events.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        let lock_path = dir.join(".seq.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| Error::io(&lock_path, e))?;
        // Wait briefly: a lock can be held for a moment by a child process
        // that has not exec'd yet, or by a short write in another process.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while fs4::FileExt::try_lock_exclusive(&lock).is_err() {
            if std::time::Instant::now() >= deadline {
                return Err(Error::Journal(format!(
                    "{} is being written by another mem process; try again when it finishes",
                    dir.display()
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // Recover any torn tail before computing the seq baseline.
        for file in collect_day_files(dir)? {
            recover_tail(&file)?;
        }
        let mut start_seq = 0u64;
        let mut seen = std::collections::HashSet::new();
        for event in JournalIter::new(dir)? {
            let event = event?;
            start_seq = start_seq.max(event.seq);
            seen.insert(event.event_id);
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            fsync: fsync_enabled(),
            flush_every: flush_every_from_env(),
            next_seq: AtomicU64::new(start_seq + 1),
            pending: AtomicUsize::new(0),
            handles: Mutex::new(HashMap::new()),
            seen: Mutex::new(seen),
            _lock: lock,
        })
    }

    /// Append one event. Assigns `seq` from the in-memory counter. Writes
    /// are buffered; `sync_all` runs every `flush_every` events. Returns
    /// `None` when an event with the same id is already in the journal.
    pub fn append(&self, mut event: JournalEvent) -> Result<Option<JournalEvent>> {
        if !self.seen.lock().insert(event.event_id.clone()) {
            return Ok(None);
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        event.seq = seq;
        let bytes = serde_json::to_vec(&event)?;
        let line = framed_record(&bytes);

        let day_key = event.ingested_at.format("%Y-%m-%d").to_string();
        let path = self.dir.join(format!("events-{day_key}.jsonl"));

        let mut handles = self.handles.lock();
        let writer = match handles.get_mut(&day_key) {
            Some(w) => w,
            None => {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| Error::io(&path, e))?;
                handles.insert(day_key.clone(), BufWriter::with_capacity(64 * 1024, file));
                handles.get_mut(&day_key).expect("just inserted")
            }
        };
        writer.write_all(&line).map_err(|e| Error::io(&path, e))?;
        // BufWriter writes are in-memory until full; flush pushes to the OS.
        // We don't flush per-event — that defeats the purpose of BufWriter.
        // Instead, we flush+sync every `flush_every` events.

        let pending = self.pending.fetch_add(1, Ordering::Relaxed) + 1;
        if pending >= self.flush_every {
            self.flush_inner_locked(&mut handles)?;
            self.pending.store(0, Ordering::Relaxed);
        }

        Ok(Some(event))
    }

    /// Append many events in one call. Returns how many were new.
    pub fn append_many<I: IntoIterator<Item = JournalEvent>>(&self, events: I) -> Result<usize> {
        let mut n = 0usize;
        for event in events {
            if self.append(event)?.is_some() {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Force the OS to flush any buffered bytes and (if enabled) fsync every
    /// open day file. Always call this before exit.
    pub fn flush(&self) -> Result<()> {
        let mut handles = self.handles.lock();
        self.flush_inner_locked(&mut handles)?;
        self.pending.store(0, Ordering::Relaxed);
        Ok(())
    }

    fn flush_inner_locked(&self, handles: &mut HashMap<String, BufWriter<File>>) -> Result<()> {
        for (day, writer) in handles.iter_mut() {
            writer.flush().map_err(|e| Error::io(self.dir.join(format!("events-{day}.jsonl")), e))?;
            if self.fsync {
                let path = self.dir.join(format!("events-{day}.jsonl"));
                writer
                    .get_ref()
                    .sync_all()
                    .map_err(|e| Error::io(&path, e))?;
            }
        }
        Ok(())
    }

    /// Number of events written since the last flush.
    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Next sequence number that will be assigned.
    pub fn next_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed)
    }
}

impl Drop for BulkWriter {
    fn drop(&mut self) {
        // Best-effort flush on drop. We can't propagate errors from drop, but
        // ignoring them would lose data on a panic — log to stderr instead.
        let mut handles = self.handles.lock();
        if let Err(err) = self.flush_inner_locked(&mut handles) {
            eprintln!("mem: bulk writer drop flush failed: {err}");
        }
        // Write the seq hint so the next open sees the right baseline.
        let seq = self.next_seq.load(Ordering::Relaxed);
        let path = self.dir.join("seq");
        let _ = std::fs::write(&path, format!("{seq}\n").as_bytes());
    }
}

/// Helper: list `events-*.jsonl` files in `dir` without constructing a `Journal`.
fn collect_day_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let read = std::fs::read_dir(dir).map_err(|e| Error::io(dir, e))?;
    for entry in read {
        let entry = entry.map_err(|e| Error::io(dir, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("events-") && name.ends_with(".jsonl") {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// A framed JSONL record.
fn framed_record(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + 16);
    out.extend_from_slice(format!("mem-event v1 {}\n", bytes.len()).as_bytes());
    out.extend_from_slice(bytes);
    out.push(b'\n');
    out
}

/// Recover from an incomplete tail (a truncated final record).
fn recover_tail(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let size = std::fs::metadata(path).map_err(|e| Error::io(path, e))?.len();
    let reader_file = File::open(path).map_err(|e| Error::io(path, e))?;
    let reader = BufReader::new(&reader_file);
    let mut last_good = 0u64;
    let mut offset = 0u64;
    let mut header_len: Option<usize> = None;
    let mut truncate_from: Option<u64> = None;
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // truncated or undecodable; truncate from current offset
        };
        let consumed = line.len() as u64 + 1;
        if header_len.is_none() {
            // Header line: "mem-event v1 <length>"
            let mut parts = line.split(' ');
            if parts.next() != Some("mem-event") || parts.next() != Some("v1") {
                // Corrupt header at this position.
                truncate_from = Some(offset);
                break;
            }
            let len = match parts.next().and_then(|s| s.trim().parse::<usize>().ok()) {
                Some(l) => l,
                None => {
                    truncate_from = Some(offset);
                    break;
                }
            };
            header_len = Some(len);
            offset += consumed;
            continue;
        }
        // Body line.
        let expected = header_len.unwrap();
        if line.len() != expected {
            // Truncated body — drop this record entirely.
            truncate_from = Some(last_good);
            break;
        }
        last_good = offset + consumed;
        offset += consumed;
        header_len = None;
    }
    let truncate_to = truncate_from.unwrap_or(last_good);
    if truncate_to < size {
        // Open in write mode to allow truncation.
        let mut w = OpenOptions::new()
            .write(true)
            .open(path)
            .map_err(|e| Error::io(path, e))?;
        w.seek(SeekFrom::Start(truncate_to))
            .map_err(|e| Error::io(path, e))?;
        w.set_len(truncate_to).map_err(|e| Error::io(path, e))?;
        w.sync_all().map_err(|e| Error::io(path, e))?;
    }
    Ok(())
}

/// Iterates over journal events across all day files in order.
pub struct JournalIter {
    /// Remaining day files, in ascending date order.
    files: std::collections::VecDeque<PathBuf>,
    current: Option<(PathBuf, BufReader<File>)>,
}

impl JournalIter {
    fn new(dir: &Path) -> Result<Self> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).map_err(|e| Error::io(dir, e))? {
            let entry = entry.map_err(|e| Error::io(dir, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("events-") && name.ends_with(".jsonl") {
                files.push(entry.path());
            }
        }
        files.sort();
        let mut iter = Self { files: files.into(), current: None };
        iter.advance()?;
        Ok(iter)
    }

    /// Iterate the records of one day file.
    fn single(path: &Path) -> Result<Self> {
        let mut iter = Self { files: std::collections::VecDeque::from([path.to_path_buf()]), current: None };
        iter.advance()?;
        Ok(iter)
    }

    /// Open the next day file (in date order), or end the stream.
    fn advance(&mut self) -> Result<()> {
        self.current = match self.files.pop_front() {
            Some(p) => {
                let f = File::open(&p).map_err(|e| Error::io(&p, e))?;
                Some((p, BufReader::new(f)))
            }
            None => None,
        };
        Ok(())
    }
}

impl Iterator for JournalIter {
    type Item = Result<JournalEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let path_ref = {
                let c = self.current.as_mut()?;
                c.0.clone()
            };
            let reader = {
                let c = self.current.as_mut()?;
                &mut c.1
            };
            let mut header_buf = String::new();
            match reader.read_line(&mut header_buf) {
                Ok(0) => {
                    // End of this file; continue with the next day.
                    if let Err(err) = self.advance() {
                        return Some(Err(err));
                    }
                    continue;
                }
                Ok(_) => {}
                Err(err) => return Some(Err(Error::io(path_ref, err))),
            }
            let mut parts = header_buf.split(' ');
            if parts.next() != Some("mem-event") || parts.next() != Some("v1") {
                return Some(Err(Error::Journal(format!(
                    "{path_ref:?}: bad frame header"
                ))));
            }
            let len: usize = match parts.next().and_then(|s| s.trim().parse().ok()) {
                Some(l) => l,
                None => return Some(Err(Error::Journal(format!("{path_ref:?}: bad header length")))),
            };
            let mut body = vec![0u8; len];
            match reader.read_exact(&mut body) {
                Ok(()) => {}
                Err(err) => {
                    return Some(Err(Error::io(path_ref, err)));
                }
            }
            // Consume the trailing newline.
            let mut nl = [0u8; 1];
            let _ = reader.read_exact(&mut nl);
            match serde_json::from_slice::<JournalEvent>(&body) {
                Ok(event) => return Some(Ok(event)),
                Err(err) => return Some(Err(Error::Json(err))),
            }
        }
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Deduplicate a stream of events: prefer stable source ids; collapse only when
/// source identity + position match, never on content alone.
pub fn deduplicate(events: Vec<JournalEvent>) -> Vec<JournalEvent> {
    let mut seen_ids = HashMap::new();
    let mut seen_fallback = HashMap::new();
    let mut out = Vec::new();
    for event in events {
        let id = event.event_id.clone();
        let fallback = event.dedup_key();
        if seen_ids.contains_key(&id) || seen_fallback.contains_key(&fallback) {
            continue;
        }
        seen_ids.insert(id, ());
        seen_fallback.insert(fallback, ());
        out.push(event);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_then_read() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        let event = JournalEvent::new(
            "personal",
            "session_a",
            Role::User,
            Source::Chat {
                source_id: "msg_001".to_string(),
                occurred_at: None,
            },
            "Hello world",
            Redaction::None,
        );
        let stored = journal.append(event).unwrap();
        assert_eq!(stored.seq, 1);
        let all = journal.read_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].seq, 1);
    }

    #[test]
    fn recover_truncates_a_truncated_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = Journal::open(tmp.path()).unwrap();
        for i in 0..3 {
            let event = JournalEvent::new(
                "personal",
                "session_a",
                Role::User,
                Source::Chat {
                    source_id: format!("msg_{i}"),
                    occurred_at: None,
                },
                "Hello world",
                Redaction::None,
            );
            journal.append(event).unwrap();
        }
        // Truncate the last record's body on whatever day file the journal
        // actually used (it follows `ingested_at = Utc::now()`).
        let day = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let path = tmp.path().join(format!("events-{day}.jsonl"));
        let day_file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let written = day_file.metadata().unwrap().len();
        day_file.set_len(written - 5).unwrap();
        drop(day_file);

        let journal = Journal::open(tmp.path()).unwrap();
        let all = journal.read_all().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn bulk_writer_assigns_consecutive_seqs_without_rescanning() {
        let tmp = tempfile::tempdir().unwrap();
        // Seed 5 events via the slow path.
        {
            let j = Journal::open(tmp.path()).unwrap();
            for i in 0..5 {
                let e = JournalEvent::new(
                    "personal",
                    "s",
                    Role::User,
                    Source::Chat {
                        source_id: format!("m{i}"),
                        occurred_at: None,
                    },
                    "x",
                    Redaction::None,
                );
                j.append(e).unwrap();
            }
        }
        let bw = BulkWriter::open(tmp.path()).unwrap();
        assert_eq!(bw.next_seq(), 6);
        for i in 0..100 {
            let e = JournalEvent::new(
                "personal",
                "s",
                Role::User,
                Source::Chat {
                    source_id: format!("bulk_{i}"),
                    occurred_at: None,
                },
                "x",
                Redaction::None,
            );
            bw.append(e).unwrap();
        }
        bw.flush().unwrap();
        drop(bw);
        let j = Journal::open(tmp.path()).unwrap();
        let all = j.read_all().unwrap();
        assert_eq!(all.len(), 105);
        assert_eq!(all.last().unwrap().seq, 105);
    }

    #[test]
    fn bulk_writer_dedupes_against_existing_seqs() {
        let tmp = tempfile::tempdir().unwrap();
        let bw = BulkWriter::open(tmp.path()).unwrap();
        let mut events: Vec<JournalEvent> = Vec::new();
        for i in 0..50 {
            let e = JournalEvent::new(
                "personal",
                "s",
                Role::User,
                Source::Chat {
                    source_id: format!("d{i}"),
                    occurred_at: None,
                },
                "x",
                Redaction::None,
            );
            events.push(e);
        }
        let n = bw.append_many(events.clone()).unwrap();
        assert_eq!(n, 50);
        // Re-appending the same events writes nothing, in this writer and in
        // a fresh one opened over the same directory.
        let n2 = bw.append_many(events.clone()).unwrap();
        assert_eq!(n2, 0);
        bw.flush().unwrap();
        assert!(BulkWriter::open(tmp.path()).is_err(), "second writer must not open");
        drop(bw);
        let reopened = BulkWriter::open(tmp.path()).unwrap();
        assert_eq!(reopened.append_many(events).unwrap(), 0);
        let j = Journal::open(tmp.path()).unwrap();
        assert_eq!(j.read_all().unwrap().len(), 50);
    }

    #[test]
    fn redact_erases_only_the_named_events_and_keeps_order() {
        let tmp = tempfile::tempdir().unwrap();
        let j = Journal::open(tmp.path()).unwrap();
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut e = JournalEvent::new(
                "p",
                "s",
                Role::User,
                Source::Chat { source_id: format!("r{i}"), occurred_at: None },
                format!("secret number {i}"),
                Redaction::None,
            );
            e.set_project_from("/tmp/proj");
            ids.push(j.append(e).unwrap().event_id);
        }
        let target: std::collections::HashSet<String> = [ids[1].clone()].into();
        assert_eq!(j.redact(&target).unwrap(), 1);
        assert_eq!(j.redact(&target).unwrap(), 0);
        let all = j.read_all().unwrap();
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(all[1].content, ERASED);
        assert_eq!(all[1].redaction, Redaction::Full);
        assert_eq!(all[1].project(), Some("/tmp/proj"));
        assert_eq!(all[0].content, "secret number 0");
        assert!(!std::fs::read_to_string(tmp.path().join(
            std::fs::read_dir(tmp.path()).unwrap().flatten()
                .find(|e| e.file_name().to_string_lossy().starts_with("events-")).unwrap().file_name()
        )).unwrap().contains("secret number 1"));
    }

    #[test]
    fn day_files_are_read_in_date_order() {
        let tmp = tempfile::tempdir().unwrap();
        for (day, seq) in [("2026-01-01", 1u64), ("2026-01-02", 2), ("2026-01-03", 3)] {
            let mut e = JournalEvent::new(
                "p",
                "s",
                Role::User,
                Source::Chat { source_id: format!("d{seq}"), occurred_at: None },
                "x",
                Redaction::None,
            );
            e.seq = seq;
            let bytes = framed_record(&serde_json::to_vec(&e).unwrap());
            std::fs::write(tmp.path().join(format!("events-{day}.jsonl")), bytes).unwrap();
        }
        let j = Journal::open(tmp.path()).unwrap();
        let seqs: Vec<u64> = j.read_all().unwrap().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
    }

    #[test]
    fn bulk_writer_handles_two_day_files() {
        let tmp = tempfile::tempdir().unwrap();
        let bw = BulkWriter::open(tmp.path()).unwrap();
        let today = chrono::Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let tomorrow = today + chrono::Duration::days(1);
        let mut e1 = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat {
                source_id: "a".into(),
                occurred_at: Some(today),
            },
            "x",
            Redaction::None,
        );
        // JournalEvent::new always sets ingested_at to Utc::now(); override
        // explicitly so this test can target a known day.
        e1.ingested_at = today;
        let mut e2 = JournalEvent::new(
            "personal",
            "s",
            Role::User,
            Source::Chat {
                source_id: "b".into(),
                occurred_at: Some(tomorrow),
            },
            "x",
            Redaction::None,
        );
        e2.ingested_at = tomorrow;
        bw.append(e1).unwrap();
        bw.append(e2).unwrap();
        bw.flush().unwrap();

        // Two day files should now exist.
        let files: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("events-"))
            .collect();
        assert!(files.len() >= 2, "expected two day files, got {files:?}");
    }

    #[test]
    fn fsync_env_disables_sync() {
        // We can't truly observe fsync absence in a portable test, but we
        // can at least exercise the knob parsing.
        std::env::set_var("MEM_JOURNAL_FSYNC", "0");
        assert!(!fsync_enabled());
        std::env::remove_var("MEM_JOURNAL_FSYNC");
        assert!(fsync_enabled());
    }
}
