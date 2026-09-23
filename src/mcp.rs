//! MCP over stdio.
//!
//! One JSON-RPC message per line on stdin. Responses are one JSON object
//! per line on stdout. Diagnostics go to stderr. The tools read the same
//! journal files as `mem search`.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::ops;

const PROTOCOL: &str = "2024-11-05";

pub fn serve_stdio(root: &Path, scope_id: &str, session_id: &str) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| Error::io(Path::new("stdin"), e))?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                write_message(&mut stdout, &error_response(None, -32700, &err.to_string()))?;
                continue;
            }
        };
        if let Some(response) = handle_message(root, scope_id, session_id, &message) {
            write_message(&mut stdout, &response)?;
        }
    }
    Ok(())
}

fn write_message(stdout: &mut impl Write, message: &Value) -> Result<()> {
    writeln!(stdout, "{}", serde_json::to_string(message)?)
        .map_err(|e| Error::io(Path::new("stdout"), e))?;
    stdout
        .flush()
        .map_err(|e| Error::io(Path::new("stdout"), e))?;
    Ok(())
}

pub(crate) fn handle_message(
    root: &Path,
    scope_id: &str,
    session_id: &str,
    message: &Value,
) -> Option<Value> {
    let method = message.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let id = message.get("id").cloned();
    id.as_ref()?;
    let id = id.unwrap();
    let params = message.get("params").cloned().unwrap_or(json!({}));
    match method {
        "initialize" => Some(result(id, initialize_result(&params))),
        "tools/list" => Some(result(id, json!({"tools": tools()}))),
        "tools/call" => Some(result(id, call_tool(root, scope_id, session_id, &params))),
        "ping" => Some(result(id, json!({}))),
        _ => Some(error_response(
            Some(id),
            -32601,
            &format!("method not found: {method}"),
        )),
    }
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL);
    let version = if matches!(
        requested,
        "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25"
    ) {
        requested
    } else {
        PROTOCOL
    };
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "mem", "version": env!("CARGO_PKG_VERSION")},
    })
}

fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "memory_search",
            "description": "Search this memory: curated facts (fact_* ids) and raw session events (journal:* ids). Rows are reranked by JEV or the local model when available. Pass project to get only one project's memory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to look for, in plain words."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 20, "description": "Hits to return. Default 8."},
                    "rerank": {"type": "boolean", "description": "Set false to keep the lexical order. Default true."},
                    "project": {"type": "string", "description": "Only this project's memory (folder name or root path)."}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "memory_status",
            "description": "Journal size and projects, consolidation lag, fact counts, pending changes, and the active reranker and extractor.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "memory_read",
            "description": "Read a fact, an entity's current facts, or a journal event, by the id a search returned. Forgotten or erased facts read as not found.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string", "description": "fact_*, an entity id (p_, o_, w_, pref_), or journal:<event>#<seq>."}},
                "required": ["id"]
            }
        }),
        json!({
            "name": "memory_history",
            "description": "Every fact of an entity with its status (current, superseded, retracted). Forgotten or erased facts are left out.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }
        }),
        json!({
            "name": "tasks_read",
            "description": "Read the versioned task list stored in this memory root.",
            "inputSchema": {"type": "object", "properties": {}}
        }),
        json!({
            "name": "tasks_update",
            "description": "Append a task title if expected_version matches the file. A stale version conflicts and changes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "expected_version": {"type": "integer", "minimum": 0}
                },
                "required": ["title", "expected_version"]
            }
        }),
        json!({
            "name": "memory_request_change",
            "description": "Change memory now: remember a fact, correct one (the old fact is superseded), or forget one (never returned again). Validated and published atomically; repeating a request_id does not apply it twice.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["remember", "correct", "forget"]},
                    "request_id": {"type": "string", "description": "Unique per change; letters, digits, _ and -."},
                    "entity_id": {"type": "string", "description": "p_<person>, o_<org>, w_<project>, or pref_<name>."},
                    "statement": {"type": "string", "description": "The fact as one sentence (remember, correct)."},
                    "fact_id": {"type": "string", "description": "Id for the new fact (remember, correct). Generated when omitted."},
                    "target_fact_id": {"type": "string", "description": "Fact to correct or forget."},
                    "reason": {"type": "string", "description": "Why, recorded with a forget."}
                },
                "required": ["kind", "request_id", "entity_id"]
            }
        }),
    ]
}

fn call_tool(root: &Path, scope_id: &str, session_id: &str, params: &Value) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let known = [
        "memory_search",
        "memory_status",
        "memory_read",
        "memory_history",
        "tasks_read",
        "tasks_update",
        "memory_request_change",
    ];
    if !known.contains(&name) {
        return json!({
            "content": [{"type": "text", "text": format!("unknown tool: {name}")}],
            "isError": true,
        });
    }
    let result = ops::execute(root, scope_id, session_id, name, &args);
    let text = result.payload.to_string();
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": result.error.is_some() || result.conflict || result.not_found,
    })
}

fn result(id: Value, value: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": value})
}

fn error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Journal, JournalEvent, Redaction, Role, Source};

    #[test]
    fn initialize_and_search_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        journal
            .append(JournalEvent::new(
                "personal",
                "s",
                Role::User,
                Source::Chat {
                    source_id: "claude:1".into(),
                    occurred_at: None,
                },
                "agensis ships channels on the desktop",
                Redaction::None,
            ))
            .unwrap();

        let init = handle_message(
            tmp.path(),
            "personal",
            "default",
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
        )
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "mem");
        assert_eq!(init["result"]["protocolVersion"], "2024-11-05");

        let listed = handle_message(
            tmp.path(),
            "personal",
            "default",
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        )
        .unwrap();
        let names: Vec<&str> = listed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"memory_search"));

        let called = handle_message(
            tmp.path(),
            "personal",
            "default",
            &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"agensis","limit":3}}}),
        )
        .unwrap();
        let text = called["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("channels"));
        assert_eq!(called["result"]["isError"], false);
    }
}
