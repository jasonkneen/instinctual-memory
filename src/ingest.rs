//! Source adapters.
//!
//! V1 ships four reference adapters. Each adapter reads the supported export
//! format, normalises records to [`crate::journal::JournalEvent`], and the
//! caller appends them through the durable journal. Adapters never publish
//! memory; they only populate the source journal.
//!
//! - [`ChatExportAdapter`] — JSONL chat exports (one event per line).
//! - [`CalendarAdapter`] — iCalendar (.ics) feeds.
//! - [`VoiceAdapter`] — whisper/vtt transcripts.
//! - [`IdeHistoryAdapter`] — JSON IDE history files.
//!
//! Custom adapters implement the [`Adapter`] trait.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

use crate::error::{Error, Result};
use crate::journal::{JournalEvent, Redaction, Role, Source};

/// Open `path` for buffered line reading. Reads in 64 KiB chunks instead of
/// slurping the whole file into memory.
fn buffered_lines(path: &Path) -> Result<BufReader<File>> {
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    Ok(BufReader::with_capacity(64 * 1024, file))
}

/// Pick the first non-empty text-like field from a JSON record. Order is:
/// `text`, `message`, `content`, `body`, `stem`, `question.stem`,
/// `question`, `prompt`, `input`, `query`. Returns an empty string when
/// none of them are present or all are empty — caller treats that as
/// "skip this record".
pub fn extract_text_field(value: &serde_json::Value) -> String {
    const FIELDS: &[&str] = &[
        "text",
        "message",
        "content",
        "body",
        "stem",
        "prompt",
        "input",
        "query",
    ];
    if let Some(s) = value.as_str() {
        return s.to_string();
    }
    for field in FIELDS {
        if let Some(v) = value.get(*field) {
            if let Some(s) = v.as_str() {
                let s = s.trim();
                if !s.is_empty() {
                    return s.to_string();
                }
            }
            // Recurse one level for `content` / `message` being a struct or
            // array. Handles Anthropic-style `content: [{type, text}]`.
            if matches!(*field, "content" | "message") {
                let inner = extract_text_field(v);
                if !inner.trim().is_empty() {
                    return inner;
                }
            }
        }
    }
    // Nested `question` object: prefer `stem`, then concatenate `choices`.
    if let Some(q) = value.get("question") {
        if let Some(stem) = q.get("stem").and_then(|v| v.as_str()) {
            let stem = stem.trim();
            if !stem.is_empty() {
                let mut out = stem.to_string();
                if let Some(choices) = q.get("choices").and_then(|v| v.as_array()) {
                    let labels: Vec<String> = choices
                        .iter()
                        .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                        .map(|s| s.to_string())
                        .collect();
                    if !labels.is_empty() {
                        out.push('\n');
                        out.push_str(&labels.join("\n"));
                    }
                }
                return out;
            }
        }
    }
    // Last resort: stringify the whole record. Gives us a usable haystack
    // even when the schema is fully custom.
    serde_json::to_string(value).unwrap_or_default()
}

/// Common adapter interface.
pub trait Adapter {
    fn name(&self) -> &'static str;
    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>>;
}

/// Outcome of running an adapter against one file.
#[derive(Debug, Clone, Default)]
pub struct IngestReport {
    pub events: Vec<JournalEvent>,
    pub skipped: usize,
}

impl IngestReport {
    pub fn into_events_and_skipped(self) -> (Vec<JournalEvent>, usize) {
        (self.events, self.skipped)
    }
}

/// JSONL chat export adapter. Each line is a JSON object with at least:
/// - `id` (string): stable message id from the export.
/// - `role` (string): `user` | `assistant` | `system` | `tool` | `note`.
/// - `text` (string): message text.
/// - `ts` (string, optional): ISO-8601 timestamp.
///
/// Bad lines (malformed JSON, missing required field, unknown role) are
/// skipped with a warning on stderr; the rest of the file is processed.
/// Use [`ChatExportAdapter::read_strict`] to fail on the first bad line.
pub struct ChatExportAdapter;

impl ChatExportAdapter {
    /// Read the file strictly: any malformed line returns an error.
    pub fn read_strict(
        &self,
        path: &Path,
        scope_id: &str,
        session_id: &str,
    ) -> Result<Vec<JournalEvent>> {
        let reader = buffered_lines(path)?;
        let mut events = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(err) => return Err(Error::io(path, err)),
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line)
                .map_err(|e| Error::InvalidAdapter(format!("line {}: {e}", i + 1)))?;
            let id = value
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::InvalidAdapter(format!("line {}: missing id", i + 1)))?
                .to_string();
            let role_str = value
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let role = parse_role(role_str).ok_or_else(|| {
                Error::InvalidAdapter(format!("line {}: unknown role {role_str}", i + 1))
            })?;
            match self.build_event(scope_id, session_id, role, &value, id) {
                Some(e) => events.push(e),
                None => {
                    return Err(Error::InvalidAdapter(format!(
                        "line {}: no text content",
                        i + 1
                    )))
                }
            }
        }
        Ok(events)
    }

    fn build_event(
        &self,
        scope_id: &str,
        session_id: &str,
        role: Role,
        value: &serde_json::Value,
        id: String,
    ) -> Option<JournalEvent> {
        // Try every conventional text field, in order, before giving up.
        // This makes the adapter work for chat exports, QA datasets,
        // annotation dumps, and anything else shaped like JSONL.
        let text = extract_text_field(value);
        if text.trim().is_empty() {
            return None;
        }
        let occurred_at = value
            .get("ts")
            .and_then(|v| v.as_str())
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| Some(d.with_timezone(&Utc)))
            .unwrap_or(None);
        let mut event = JournalEvent::new(
            scope_id,
            session_id,
            role,
            Source::Chat {
                source_id: id,
                occurred_at,
            },
            text,
            Redaction::None,
        );
        if let Some(metadata) = value.get("metadata") {
            event.metadata = metadata.clone();
        } else {
            // Persist the original record (minus the text we extracted) as
            // metadata so nothing is lost — fields like `choices`, `answer`,
            // `labels`, etc. survive for downstream consumers.
            let mut meta = serde_json::Map::new();
            if let Some(obj) = value.as_object() {
                for (k, v) in obj {
                    if k == "text" || k == "ts" || k == "id" || k == "role" || k == "metadata" {
                        continue;
                    }
                    meta.insert(k.clone(), v.clone());
                }
            }
            event.metadata = serde_json::Value::Object(meta);
        }
        Some(event)
    }
}

impl Adapter for ChatExportAdapter {
    fn name(&self) -> &'static str {
        "chat_export"
    }

    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>> {
        self.read_lenient(path, scope_id, session_id)
    }
}

impl ChatExportAdapter {
    /// Lenient variant: skip bad lines with a warning, return the rest.
    pub fn read_lenient(
        &self,
        path: &Path,
        scope_id: &str,
        session_id: &str,
    ) -> Result<Vec<JournalEvent>> {
        let reader = buffered_lines(path)?;
        let mut events = Vec::new();
        let mut skipped = 0usize;
        for (i, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(err) => {
                    eprintln!("{}:{}: skipped (bad json: {err})", path.display(), i + 1);
                    skipped += 1;
                    continue;
                }
            };
            let Some(id) = value.get("id").and_then(|v| v.as_str()) else {
                eprintln!("{}:{}: skipped (missing id)", path.display(), i + 1);
                skipped += 1;
                continue;
            };
            let role_str = value
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");
            let Some(role) = parse_role(role_str) else {
                eprintln!(
                    "{}:{}: skipped (unknown role {role_str})",
                    path.display(),
                    i + 1
                );
                skipped += 1;
                continue;
            };
            match self.build_event(
                scope_id,
                session_id,
                role,
                &value,
                id.to_string(),
            ) {
                Some(e) => events.push(e),
                None => {
                    // Skip silently — empty records are common in mixed
                    // datasets. Don't burn a `skipped` slot on them.
                    continue;
                }
            }
        }
        if skipped > 0 {
            eprintln!(
                "{}: skipped {skipped} line(s); ingested {}",
                path.display(),
                events.len()
            );
        }
        Ok(events)
    }
}

/// Claude Code session adapter. Each line is a JSON object with `type` in
/// {`user`, `assistant`} (plus many event types we ignore), a `timestamp`,
/// a `sessionId`, and `message.{role,content}` where content may be a
/// string or an array of `{type, text}` parts.
pub struct ClaudeAdapter;

impl Adapter for ClaudeAdapter {
    fn name(&self) -> &'static str {
        "claude_code"
    }

    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>> {
        let reader = buffered_lines(path)?;
        let mut events = Vec::new();
        for (line_no, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(kind) = value.get("type").and_then(|v| v.as_str()) else {
                continue;
            };
            if !matches!(kind, "user" | "assistant") {
                continue;
            }
            let role = match kind {
                "user" => Role::User,
                _ => Role::Assistant,
            };
            let Some(message) = value.get("message") else { continue };
            let text = extract_text_parts(message.get("content"));
            if text.trim().is_empty() {
                continue;
            }
            let occurred_at = value
                .get("timestamp")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));
            let Some(session_uuid) = value.get("sessionId").and_then(|v| v.as_str()) else {
                continue;
            };
            // One id per message: the line's own uuid, else its line number.
            let message_key = value
                .get("uuid")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| format!("line{line_no}"));
            let mut event = JournalEvent::new(
                scope_id.to_string(),
                session_id.to_string(),
                role,
                Source::Chat {
                    source_id: format!("claude:{session_uuid}:{message_key}"),
                    occurred_at,
                },
                text,
                Redaction::None,
            );
            if let Some(cwd) = value.get("cwd").and_then(|v| v.as_str()) {
                event.set_project_from(cwd);
            }
            events.push(event);
        }
        Ok(events)
    }
}

/// Codex adapter. Each line is a JSON object whose `type` is usually
/// `session_meta`, `event_msg`, or `response_item`. Messages live inside
/// `response_item` items where `payload.type` is `message` and `payload.role`
/// is `user` or `assistant`. Assistant items with a non-final `channel` are
/// dropped (intermediate commentary). User items may include an
/// `<environment_context>` block that is removed before storage.
pub struct CodexAdapter;

impl Adapter for CodexAdapter {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>> {
        let reader = buffered_lines(path)?;
        let mut events = Vec::new();
        let mut session_uuid: Option<String> = None;
        let mut cwd: Option<String> = None;
        // Forked rollouts reuse the parent's session id, so the rollout file
        // name is part of each message id.
        let rollout = path.file_stem().and_then(|s| s.to_str()).unwrap_or("rollout").to_string();
        for (line_no, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match value.get("type").and_then(|v| v.as_str()) {
                Some("session_meta") => {
                    // Subagent rollouts hold prompts one agent wrote to another, not
                    // the user's words. Claude subagent transcripts are skipped the same way.
                    if crate::ingest::is_codex_subagent(&value) {
                        return Ok(Vec::new());
                    }
                    session_uuid = value
                        .get("payload")
                        .and_then(|p| p.get("id"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    cwd = value["payload"]["cwd"].as_str().map(str::to_string);
                }
                Some("response_item") => {
                    let Some(payload) = value.get("payload") else { continue };
                    if payload.get("type").and_then(|v| v.as_str()) != Some("message") {
                        continue;
                    }
                    let Some(role_str) =
                        payload.get("role").and_then(|v| v.as_str())
                    else {
                        continue;
                    };
                    let role_enum = match role_str {
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        _ => continue,
                    };
                    if role_enum == Role::Assistant {
                        if let Some(channel) =
                            payload.get("channel").and_then(|v| v.as_str())
                        {
                            if channel != "final" {
                                continue;
                            }
                        }
                    }
                    let mut text = extract_text_parts(payload.get("content"));
                    if role_enum == Role::User
                        && text.contains("</environment_context>")
                    {
                        if let Some(pos) = text.rfind("</environment_context>") {
                            text = text[pos + "</environment_context>".len()..].to_string();
                        }
                    }
                    if text.trim().is_empty() {
                        continue;
                    }
                    let occurred_at = value
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.with_timezone(&Utc));
                    let sid = session_uuid.clone().unwrap_or_else(|| session_id.to_string());
                    let mut event = JournalEvent::new(
                        scope_id.to_string(),
                        session_id.to_string(),
                        role_enum,
                        Source::Chat {
                            source_id: format!("codex:{sid}:{rollout}:line{line_no}"),
                            occurred_at,
                        },
                        text,
                        Redaction::None,
                    );
                    if let Some(cwd) = &cwd {
                        event.set_project_from(cwd);
                    }
                    events.push(event);
                }
                _ => {}
            }
        }
        Ok(events)
    }
}

/// True when a Codex `session_meta` record describes a spawned subagent.
pub(crate) fn is_codex_subagent(meta: &serde_json::Value) -> bool {
    let payload = &meta["payload"];
    payload["thread_source"].as_str() == Some("subagent") || payload["source"].get("subagent").is_some()
}

/// pi / omp session adapter. `omp` is a pi fork and both write the same
/// JSONL: an optional `title` line, then a `session` header carrying
/// `version` and the session's `cwd`, then a tree of entries where a
/// `message` entry holds an `AgentMessage`. Only the user's and the
/// assistant's text is ingested: thinking, tool calls, tool results,
/// extension-injected `custom` messages, and compaction or branch
/// summaries are skipped, like the other transcript adapters.
pub struct PiAdapter;

impl Adapter for PiAdapter {
    fn name(&self) -> &'static str {
        "pi"
    }

    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>> {
        // The source prefix keeps pi and omp events (and the two harnesses)
        // apart even when a session is cloned between them.
        let harness = pi_harness(path);
        let reader = buffered_lines(path)?;
        let mut events = Vec::new();
        let mut session_uuid: Option<String> = None;
        let mut cwd: Option<String> = None;
        for (line_no, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => continue,
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match value.get("type").and_then(|v| v.as_str()) {
                Some("session") => {
                    session_uuid = value.get("id").and_then(|v| v.as_str()).map(str::to_string);
                    cwd = value.get("cwd").and_then(|v| v.as_str()).map(str::to_string);
                }
                Some("message") => {
                    let Some(message) = value.get("message") else { continue };
                    let role = match message.get("role").and_then(|v| v.as_str()) {
                        Some("user") => Role::User,
                        Some("assistant") => Role::Assistant,
                        // toolResult / custom / bashExecution / summaries:
                        // not the user's or the assistant's own words.
                        _ => continue,
                    };
                    let text = extract_pi_text(message.get("content"));
                    if text.trim().is_empty() {
                        continue;
                    }
                    let occurred_at = value
                        .get("timestamp")
                        .and_then(|v| v.as_str())
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|d| d.with_timezone(&Utc))
                        .or_else(|| {
                            message
                                .get("timestamp")
                                .and_then(|v| v.as_i64())
                                .and_then(DateTime::from_timestamp_millis)
                        });
                    let uuid = session_uuid.clone().unwrap_or_else(|| session_id.to_string());
                    // Every entry has its own 8-char id; the line number is
                    // the fallback for records written without one.
                    let entry_key = value
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("line{line_no}"));
                    let mut event = JournalEvent::new(
                        scope_id.to_string(),
                        session_id.to_string(),
                        role,
                        Source::Chat {
                            source_id: format!("{harness}:{uuid}:{entry_key}"),
                            occurred_at,
                        },
                        text,
                        Redaction::None,
                    );
                    if let Some(cwd) = &cwd {
                        event.set_project_from(cwd);
                    }
                    events.push(event);
                }
                _ => {}
            }
        }
        Ok(events)
    }
}

/// Which harness wrote a pi-format session: `omp` for `~/.omp/...`, else `pi`.
fn pi_harness(path: &Path) -> &'static str {
    if path.components().any(|c| c.as_os_str() == ".omp") {
        "omp"
    } else {
        "pi"
    }
}

/// The working directory a pi/omp session ran in, read from its `session`
/// header without parsing the messages. `None` when the file is not a pi
/// session or the header has no `cwd`.
pub(crate) fn pi_session_cwd(path: &Path) -> Option<String> {
    let reader = buffered_lines(path).ok()?;
    for line in reader.lines().map_while(|l| l.ok()) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("type").and_then(|v| v.as_str()) == Some("session") {
            return value.get("cwd").and_then(|v| v.as_str()).map(str::to_string);
        }
    }
    None
}

/// Text of a pi/omp message: a plain string, or the `text` blocks of a
/// content array (thinking and tool-call blocks are dropped).
fn extract_pi_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.replace('\u{0}', ""),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter(|p| p.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn extract_text_parts(value: Option<&serde_json::Value>) -> String {
    let Some(v) = value else {
        return String::new();
    };
    match v {
        serde_json::Value::String(s) => s.replace('\u{0}', "").to_string(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                let kind = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if matches!(kind, "text" | "input_text" | "output_text") {
                    p.get("text")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Markdown adapter for agent project-memory files: AGENTS.md, CLAUDE.md,
/// CODEX.md, README.md, and any other `.md` file dropped into the
/// project. The whole file becomes one journal event with the filename as
/// the source id and the relative path captured in metadata; the content
/// is the full text. Consolidation is expected to extract structured
/// facts from this material in turn operations.
pub struct MarkdownAdapter;

impl Adapter for MarkdownAdapter {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let trimmed = raw.trim_start_matches('\u{feff}').to_string();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        // Path plus content hash: an edited file is a new event, an unchanged
        // one is skipped on re-ingest.
        let digest = crate::journal::sha256_hex(trimmed.as_bytes());
        let source_id = format!("md:{}:{}", path.display(), &digest[..16]);
        let mut metadata = serde_json::Map::new();
        metadata.insert(
            "file_name".into(),
            serde_json::Value::String(file_name.clone()),
        );
        metadata.insert(
            "kind".into(),
            serde_json::Value::String(agent_memory_kind(&file_name).to_string()),
        );
        if let Some(parent) = path.parent() {
            metadata.insert(
                "rel_path".into(),
                serde_json::Value::String(
                    parent
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string(),
                ),
            );
        }
        let mut event = JournalEvent::new(
            scope_id.to_string(),
            session_id.to_string(),
            Role::Note,
            Source::IdeHistory {
                source_id,
                position: 0,
                occurred_at: None,
            },
            trimmed,
            Redaction::None,
        );
        event.metadata = serde_json::Value::Object(metadata);
        if let Some(parent) = path.canonicalize().ok().as_deref().and_then(Path::parent) {
            event.set_project_from(&parent.display().to_string());
        }
        Ok(vec![event])
    }
}

fn agent_memory_kind(file_name: &str) -> &'static str {
    let lower = file_name.to_ascii_lowercase();
    match lower.as_str() {
        "agents.md" => "agents",
        "claude.md" => "claude",
        "codex.md" | "codex.local.md" => "codex",
        "readme.md" => "readme",
        "copilot-instructions.md" => "copilot",
        "windsurfrules.md" | ".windsurfrules" => "windsurf",
        _ => "project_markdown",
    }
}

pub struct CalendarAdapter;

impl Adapter for CalendarAdapter {
    fn name(&self) -> &'static str {
        "calendar"
    }

    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let mut events = Vec::new();
        let mut position = 0u64;
        let mut current: Option<(String, Option<DateTime<Utc>>, String)> = None;
        for line in raw.lines() {
            let line = line.trim_end();
            if line.eq_ignore_ascii_case("BEGIN:VEVENT") {
                current = None;
                continue;
            }
            if line.eq_ignore_ascii_case("END:VEVENT") {
                if let Some((uid, when, summary)) = current.take() {
                    let event = JournalEvent::new(
                        scope_id,
                        session_id,
                        Role::Note,
                        Source::Calendar {
                            source_id: uid,
                            position,
                            occurred_at: when,
                        },
                        summary,
                        Redaction::None,
                    );
                    events.push(event);
                    position += 1;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("UID:") {
                if let Some(slot) = current.as_mut() {
                    slot.0 = rest.trim().to_string();
                } else {
                    current = Some((rest.trim().to_string(), None, String::new()));
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("DTSTART:") {
                let parsed = parse_ics_datetime(rest.trim());
                if let Some(slot) = current.as_mut() {
                    slot.1 = parsed;
                } else {
                    current = Some((String::new(), parsed, String::new()));
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("SUMMARY:") {
                if let Some(slot) = current.as_mut() {
                    slot.2 = rest.to_string();
                } else {
                    current = Some((String::new(), None, rest.to_string()));
                }
            }
        }
        Ok(events)
    }
}

fn parse_ics_datetime(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ") {
        return Some(Utc.from_utc_datetime(&naive));
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y%m%d") {
        let naive = date.and_hms_opt(0, 0, 0)?;
        return Some(Utc.from_utc_datetime(&naive));
    }
    None
}

/// Voice / VTT transcript adapter. Each cue becomes a journal event.
pub struct VoiceAdapter;

impl Adapter for VoiceAdapter {
    fn name(&self) -> &'static str {
        "voice"
    }

    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let mut events = Vec::new();
        let mut position = 0u64;
        let mut current_text = String::new();
        let mut current_when: Option<DateTime<Utc>> = None;
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with("WEBVTT") || line.starts_with("NOTE") {
                continue;
            }
            if line.contains("-->") {
                let start = line.split("-->").next().unwrap_or("").trim();
                current_when = parse_vtt_time(start);
                continue;
            }
            if line.chars().all(|c| c.is_ascii_digit()) && !current_text.is_empty() {
                continue;
            }
            if !current_text.is_empty() {
                current_text.push(' ');
            }
            current_text.push_str(line);
            let event = JournalEvent::new(
                scope_id,
                session_id,
                Role::Note,
                Source::Voice {
                    source_id: path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("voice")
                        .to_string(),
                    position,
                    occurred_at: current_when,
                },
                current_text.clone(),
                Redaction::None,
            );
            events.push(event);
            position += 1;
            current_text.clear();
        }
        Ok(events)
    }
}

fn parse_vtt_time(s: &str) -> Option<DateTime<Utc>> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let sec_parts: Vec<&str> = parts[2].split('.').collect();
    let s: u32 = sec_parts[0].parse().ok()?;
    let ms: u32 = sec_parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
    let total = chrono::Duration::hours(h as i64)
        + chrono::Duration::minutes(m as i64)
        + chrono::Duration::seconds(s as i64)
        + chrono::Duration::milliseconds(ms as i64);
    Some(Utc::now() + total)
}

/// IDE history adapter. JSON shape: `{ "events": [ { "id", "ts", "command", "result" } ] }`.
pub struct IdeHistoryAdapter;

impl Adapter for IdeHistoryAdapter {
    fn name(&self) -> &'static str {
        "ide_history"
    }

    fn read(&self, path: &Path, scope_id: &str, session_id: &str) -> Result<Vec<JournalEvent>> {
        let raw = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| Error::InvalidAdapter(e.to_string()))?;
        let mut events = Vec::new();
        let source_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("ide")
            .to_string();
        for (i, item) in value
            .get("events")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
        {
            let id = item
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| format!("{source_name}_{i}"));
            let when = item
                .get("ts")
                .and_then(|v| v.as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&Utc));
            let command = item
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let result = item.get("result").cloned().unwrap_or(serde_json::Value::Null);
            let mut event = JournalEvent::new(
                scope_id,
                session_id,
                Role::Tool,
                Source::IdeHistory {
                    source_id: id,
                    position: i as u64,
                    occurred_at: when,
                },
                command,
                Redaction::None,
            );
            event.metadata = serde_json::json!({ "result": result });
            events.push(event);
        }
        Ok(events)
    }
}

fn parse_role(s: &str) -> Option<Role> {
    match s {
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "tool" => Some(Role::Tool),
        "system" => Some(Role::System),
        "note" => Some(Role::Note),
        _ => None,
    }
}

/// Choose an adapter based on file extension.
pub fn adapter_for(path: &Path) -> Result<Box<dyn Adapter>> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    if name.ends_with(".ide.json") {
        return Ok(Box::new(IdeHistoryAdapter));
    }
    match ext.as_str() {
        "jsonl" | "json" => Ok(match sniff_jsonl(path) {
            JsonlKind::Claude => Box::new(ClaudeAdapter),
            JsonlKind::Codex => Box::new(CodexAdapter),
            JsonlKind::Pi => Box::new(PiAdapter),
            JsonlKind::ChatExport => Box::new(ChatExportAdapter),
        }),
        "ics" => Ok(Box::new(CalendarAdapter)),
        "vtt" | "srt" => Ok(Box::new(VoiceAdapter)),
        "md" | "markdown" => Ok(Box::new(MarkdownAdapter)),
        _ => Err(Error::InvalidAdapter(format!(
            "no adapter for extension {ext}"
        ))),
    }
}

enum JsonlKind {
    Claude,
    Codex,
    Pi,
    ChatExport,
}

/// Tell Claude Code transcripts and Codex rollouts apart from generic chat
/// exports by the first records that parse.
fn sniff_jsonl(path: &Path) -> JsonlKind {
    let Ok(reader) = buffered_lines(path) else {
        return JsonlKind::ChatExport;
    };
    for line in reader.lines().map_while(|l| l.ok()).take(50) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let kind = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if value.get("sessionId").is_some() && !kind.is_empty() {
            return JsonlKind::Claude;
        }
        if value.get("payload").is_some()
            && matches!(kind, "session_meta" | "response_item" | "event_msg" | "turn_context")
        {
            return JsonlKind::Codex;
        }
        // pi / omp: a `session` header, or a tree entry carrying a message.
        if kind == "session" && value.get("version").is_some() && value.get("cwd").is_some() {
            return JsonlKind::Pi;
        }
        if kind == "message" && value.get("parentId").is_some() && value.get("message").is_some() {
            return JsonlKind::Pi;
        }
        if value.get("role").is_some() || value.get("text").is_some() {
            return JsonlKind::ChatExport;
        }
    }
    JsonlKind::ChatExport
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_routes_by_content() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("a.jsonl");
        std::fs::write(&claude, r#"{"type":"user","sessionId":"S","message":{"content":"hi"}}"#).unwrap();
        let codex = dir.path().join("b.jsonl");
        std::fs::write(&codex, r#"{"type":"session_meta","payload":{"id":"C"}}"#).unwrap();
        let pi = dir.path().join("c.jsonl");
        std::fs::write(&pi, r#"{"type":"session","version":3,"id":"P","cwd":"/tmp/p"}"#).unwrap();
        let pi_msg = dir.path().join("c-msg.jsonl");
        std::fs::write(&pi_msg, r#"{"type":"message","id":"e1","parentId":null,"message":{"role":"user","content":"hi"}}"#).unwrap();
        let chat = dir.path().join("d.jsonl");
        std::fs::write(&chat, r#"{"id":"m1","role":"user","text":"hi"}"#).unwrap();
        let ide = dir.path().join("x.ide.json");
        std::fs::write(&ide, r#"{"events":[]}"#).unwrap();
        assert_eq!(adapter_for(&claude).unwrap().name(), "claude_code");
        assert_eq!(adapter_for(&codex).unwrap().name(), "codex");
        assert_eq!(adapter_for(&pi).unwrap().name(), "pi");
        assert_eq!(adapter_for(&pi_msg).unwrap().name(), "pi");
        assert_eq!(adapter_for(&chat).unwrap().name(), "chat_export");
        assert_eq!(adapter_for(&ide).unwrap().name(), "ide_history");
    }

    #[test]
    fn pi_adapter_reads_text_only_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pi.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"title","v":1,"title":"Work"}"#, "\n",
                r#"{"type":"session","version":3,"id":"S","timestamp":"2026-01-01T00:00:00Z","cwd":"/tmp/proj"}"#, "\n",
                r#"{"type":"model_change","id":"m","parentId":null}"#, "\n",
                r#"{"type":"message","id":"u1","parentId":"m","timestamp":"2026-01-01T00:00:01Z","message":{"role":"user","content":[{"type":"text","text":"we deploy with fly"}]}}"#, "\n",
                r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"text","text":"Noted."},{"type":"toolCall","name":"bash","arguments":{"command":"ls"}}],"timestamp":1785019122997}}"#, "\n",
                r#"{"type":"message","id":"t1","parentId":"a1","message":{"role":"toolResult","content":[{"type":"text","text":"tool output"}]}}"#, "\n",
                r#"{"type":"message","id":"c1","parentId":"a1","message":{"role":"custom","customType":"x","content":"injected"}}"#, "\n",
            ),
        )
        .unwrap();
        let events = PiAdapter.read(&path, "p", "s").unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].role, Role::User);
        assert_eq!(events[0].content, "we deploy with fly");
        assert_eq!(events[0].project(), Some("/tmp/proj"));
        assert_eq!(events[1].role, Role::Assistant);
        assert_eq!(events[1].content, "Noted.");
        assert!(events[0].source.event_id().contains("pi:S:u1"));

        // The same file read twice yields the same stable ids, so the
        // journal skips the second write.
        let again = PiAdapter.read(&path, "p", "s").unwrap();
        assert_eq!(events[0].event_id, again[0].event_id);
        let journal_dir = dir.path().join("journal");
        let writer = crate::journal::BulkWriter::open(&journal_dir).unwrap();
        assert_eq!(writer.append_many(events).unwrap(), 2);
        drop(writer);
        let writer = crate::journal::BulkWriter::open(&journal_dir).unwrap();
        assert_eq!(writer.append_many(again).unwrap(), 0);
    }

    #[test]
    fn pi_and_omp_sessions_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let body = concat!(
            r#"{"type":"session","version":3,"id":"S","cwd":"/tmp/p"}"#, "\n",
            r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":"hi"}}"#, "\n",
        );
        let pi = dir.path().join("pi.jsonl");
        std::fs::write(&pi, body).unwrap();
        let omp_dir = dir.path().join(".omp").join("agent").join("sessions");
        std::fs::create_dir_all(&omp_dir).unwrap();
        let omp = omp_dir.join("omp.jsonl");
        std::fs::write(&omp, body).unwrap();
        let a = PiAdapter.read(&pi, "p", "s").unwrap();
        let b = PiAdapter.read(&omp, "p", "s").unwrap();
        assert!(a[0].source.event_id().contains("pi:S:u1"));
        assert!(b[0].source.event_id().contains("omp:S:u1"));
        assert_ne!(a[0].event_id, b[0].event_id);
    }

    #[test]
    fn chat_adapter_parses_minimal_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chat.jsonl");
        std::fs::write(
            &path,
            "{\"id\":\"m1\",\"role\":\"user\",\"text\":\"hello\",\"ts\":\"2026-01-01T00:00:00Z\"}\n",
        )
        .unwrap();
        let events = ChatExportAdapter
            .read(&path, "personal", "session")
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role, Role::User);
    }

    #[test]
    fn ics_adapter_extracts_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cal.ics");
        std::fs::write(
            &path,
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:abc\nDTSTART:20260101T120000Z\nSUMMARY:Standup\nEND:VEVENT\nEND:VCALENDAR\n",
        )
        .unwrap();
        let events = CalendarAdapter.read(&path, "personal", "session").unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].content.contains("Standup"));
    }

    #[test]
    fn vtt_adapter_extracts_cues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.vtt");
        std::fs::write(
            &path,
            "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\nHello world\n\n00:00:02.000 --> 00:00:04.000\nSecond cue\n",
        )
        .unwrap();
        let events = VoiceAdapter.read(&path, "personal", "session").unwrap();
        assert!(!events.is_empty());
    }

    #[test]
    fn claude_and_codex_give_each_message_its_own_id_and_reingest_adds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("s.jsonl");
        std::fs::write(
            &claude,
            concat!(
                r#"{"type":"user","uuid":"u1","sessionId":"S","message":{"role":"user","content":"first"}}"#, "\n",
                r#"{"type":"assistant","uuid":"u2","sessionId":"S","message":{"role":"assistant","content":"second"}}"#, "\n",
            ),
        )
        .unwrap();
        let codex = dir.path().join("rollout-x.jsonl");
        std::fs::write(
            &codex,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"C"}}"#, "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"one"}}"#, "\n",
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":"two"}}"#, "\n",
            ),
        )
        .unwrap();
        let mut events = ClaudeAdapter.read(&claude, "p", "s").unwrap();
        events.extend(CodexAdapter.read(&codex, "p", "s").unwrap());
        // A forked rollout that reuses the session id keeps its own ids.
        let fork = dir.path().join("rollout-y.jsonl");
        std::fs::copy(&codex, &fork).unwrap();
        events.extend(CodexAdapter.read(&fork, "p", "s").unwrap());
        let ids: std::collections::HashSet<_> = events.iter().map(|e| e.event_id.clone()).collect();
        assert_eq!(ids.len(), 6, "{ids:?}");

        let journal_dir = dir.path().join("journal");
        let writer = crate::journal::BulkWriter::open(&journal_dir).unwrap();
        assert_eq!(writer.append_many(events.clone()).unwrap(), 6);
        drop(writer);
        let writer = crate::journal::BulkWriter::open(&journal_dir).unwrap();
        assert_eq!(writer.append_many(events).unwrap(), 0);
    }

    #[test]
    fn markdown_id_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "# A\nrun tests\n").unwrap();
        let a = MarkdownAdapter.read(&path, "p", "s").unwrap();
        let again = MarkdownAdapter.read(&path, "p", "s").unwrap();
        std::fs::write(&path, "# A\nrun tests before pushing\n").unwrap();
        let b = MarkdownAdapter.read(&path, "p", "s").unwrap();
        assert_eq!(a[0].event_id, again[0].event_id);
        assert_ne!(a[0].event_id, b[0].event_id);
    }
}
