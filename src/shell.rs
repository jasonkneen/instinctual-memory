//! Line-oriented memory shell.
//!
//! Lines are memory commands. They are never passed to the host shell, and
//! nothing here writes a command-history file.

use std::io::{BufRead, Write};
use std::path::Path;

use serde_json::json;

use crate::error::Result;
use crate::ops::{self, OpResult};

pub fn run(
    root: &Path,
    scope_id: &str,
    session_id: &str,
    input: impl BufRead,
    mut output: impl Write,
) -> Result<()> {
    for line in input.lines() {
        let line = line.map_err(|e| crate::error::Error::io(Path::new("stdin"), e))?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "quit" || line == "exit" {
            break;
        }
        if line == "help" || line == "?" {
            write!(output, "{HELP}").map_err(|e| crate::error::Error::io(Path::new("stdout"), e))?;
            continue;
        }
        if rejected(line) {
            writeln!(output, "rejected").map_err(|e| crate::error::Error::io(Path::new("stdout"), e))?;
            continue;
        }
        dispatch(root, scope_id, session_id, line, &mut output)?;
    }
    Ok(())
}

const HELP: &str = "\
search <query>                                  facts and journal events
read <id>                                       fact, entity, or journal:<event>#<seq>
history <id>                                    every fact of an entity with its status
status                                          the memory root this shell reads
change remember <entity> <request-id> <fact-id> <statement...>
change correct <entity> <request-id> <old-fact-id> <new-fact-id> <statement...>
change forget <entity> <request-id> <fact-id>
tasks read | tasks update <expected-version> <title...>
help | quit
";

fn rejected(line: &str) -> bool {
    if line.contains(';') || line.contains('|') || line.contains('`') || line.contains("$(") {
        return true;
    }
    let command = line.split_whitespace().next().unwrap_or("");
    matches!(
        command,
        "echo" | "sh" | "bash" | "zsh" | "touch" | "rm" | "cat" | "ls" | "cd"
    )
}

fn dispatch(
    root: &Path,
    scope_id: &str,
    session_id: &str,
    line: &str,
    output: &mut impl Write,
) -> Result<()> {
    let mut parts = line.split_whitespace();
    let command = parts.next().unwrap_or("");
    let result = match command {
        "search" => {
            let query = line["search".len()..].trim();
            ops::execute(root, scope_id, session_id, "memory_search", &json!({"query": query}))
        }
        "read" => ops::execute(
            root,
            scope_id,
            session_id,
            "memory_read",
            &json!({"id": parts.next().unwrap_or("")}),
        ),
        "history" => ops::execute(
            root,
            scope_id,
            session_id,
            "memory_history",
            &json!({"id": parts.next().unwrap_or("")}),
        ),
        "status" => ops::execute(root, scope_id, session_id, "memory_status", &json!({})),
        "change" => change_line(root, scope_id, session_id, line),
        "tasks" => tasks_line(root, scope_id, session_id, line),
        _ => OpResult {
            payload: json!({"error": "unknown command"}),
            progress: json!({}),
            conflict: false,
            not_found: false,
            error: Some("unknown command".into()),
        },
    };
    render(command, &result, output)
}

fn change_line(root: &Path, scope_id: &str, session_id: &str, line: &str) -> OpResult {
    let mut parts = line.split_whitespace();
    let _ = parts.next();
    let kind = parts.next().unwrap_or("");
    let entity_id = parts.next().unwrap_or("");
    let request_id = parts.next().unwrap_or("");
    let (fact_id, target, statement) = if kind == "correct" {
        let target = parts.next().unwrap_or("").to_string();
        let fact_id = parts.next().unwrap_or("").to_string();
        let statement = parts.collect::<Vec<_>>().join(" ");
        (fact_id, Some(target), statement)
    } else if kind == "forget" {
        (String::new(), parts.next().map(str::to_string), String::new())
    } else {
        let fact_id = parts.next().unwrap_or("").to_string();
        let statement = parts.collect::<Vec<_>>().join(" ");
        (fact_id, None, statement)
    };
    ops::execute(
        root,
        scope_id,
        session_id,
        "memory_request_change",
        &json!({
            "kind": kind,
            "entity_id": entity_id,
            "request_id": request_id,
            "fact_id": fact_id,
            "target_fact_id": target,
            "statement": statement,
        }),
    )
}

fn tasks_line(root: &Path, scope_id: &str, session_id: &str, line: &str) -> OpResult {
    let mut parts = line.split_whitespace();
    let _ = parts.next();
    match parts.next().unwrap_or("read") {
        "read" => ops::execute(root, scope_id, session_id, "tasks_read", &json!({})),
        "update" => {
            let version = parts.next().unwrap_or("0");
            let title = parts.collect::<Vec<_>>().join(" ");
            ops::execute(
                root,
                scope_id,
                session_id,
                "tasks_update",
                &json!({"expected_version": version.parse::<u64>().unwrap_or(0), "title": title}),
            )
        }
        _ => ops::execute(root, scope_id, session_id, "tasks_read", &json!({})),
    }
}

fn render(command: &str, result: &OpResult, output: &mut impl Write) -> Result<()> {
    if result.not_found {
        writeln!(output, "not found").map_err(|e| crate::error::Error::io(std::path::Path::new("stdout"), e))?;
        return Ok(());
    }
    if let Some(err) = &result.error {
        writeln!(output, "error: {err}").map_err(|e| crate::error::Error::io(std::path::Path::new("stdout"), e))?;
        return Ok(());
    }
    match command {
        "search" => {
            let hits = result.payload.get("hits").and_then(|v| v.as_array());
            if hits.map(|h| h.is_empty()).unwrap_or(true) {
                writeln!(output, "(no hits)").ok();
            }
            for hit in hits.into_iter().flatten() {
                let id = hit.get("fact_id").and_then(|v| v.as_str()).unwrap_or("");
                let statement = hit.get("statement").and_then(|v| v.as_str()).unwrap_or("");
                writeln!(output, "{id}").ok();
                writeln!(output, "{statement}").ok();
            }
        }
        "read" => {
            let statement = result
                .payload
                .get("statement")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            writeln!(output, "{statement}").ok();
        }
        "history" => {
            if let Some(records) = result.payload.get("records").and_then(|v| v.as_array()) {
                for record in records {
                    let label = record.get("label").and_then(|v| v.as_str()).unwrap_or("");
                    let statement = record.get("statement").and_then(|v| v.as_str()).unwrap_or("");
                    writeln!(output, "{label} {statement}").ok();
                }
            }
        }
        _ => {
            writeln!(output, "{}", result.payload).ok();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Journal, JournalEvent, Redaction, Role, Source};
    use crate::repo::GitRepo;
    use std::io::Cursor;

    #[test]
    fn shell_remember_then_forget_hides_the_fact() {
        let tmp = tempfile::tempdir().unwrap();
        GitRepo::create(tmp.path()).unwrap();
        let script = "change remember p_sam r1 fact_sam_city Sam lives in York.\n\
                      read fact_sam_city\n\
                      change forget p_sam r2 fact_sam_city\n\
                      read fact_sam_city\nquit\n";
        let mut out = Vec::new();
        run(tmp.path(), "personal", "default", std::io::Cursor::new(script.as_bytes().to_vec()), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Sam lives in York."), "{text}");
        assert_eq!(text.lines().last(), Some("not found"), "{text}");
    }

    #[test]
    fn shell_search_then_read_and_rejects_host_shell() {
        let phrase = "amber-dock-2208 in the shell";
        let tmp = tempfile::tempdir().unwrap();
        GitRepo::create(tmp.path()).unwrap();
        let journal = Journal::open(tmp.path().join("journal")).unwrap();
        journal
            .append(JournalEvent::new(
                "personal",
                "s",
                Role::User,
                Source::Chat {
                    source_id: "shell".into(),
                    occurred_at: None,
                },
                phrase,
                Redaction::None,
            ))
            .unwrap();
        let found = crate::ops::execute(
            tmp.path(),
            "personal",
            "default",
            "memory_search",
            &json!({"query": phrase}),
        );
        let id = found.payload["hits"][0]["fact_id"].as_str().unwrap();
        let script = format!("search {phrase}\nread {id}\nquit\n");
        let mut out = Vec::new();
        run(
            tmp.path(),
            "personal",
            "default",
            Cursor::new(script.into_bytes()),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches(phrase).count(), 2, "{text}");
        eprintln!("checked {phrase}");
        let mut rejected_out = Vec::new();
        run(
            tmp.path(),
            "personal",
            "default",
            Cursor::new(b"echo pwned\nsearch x; touch nowhere\nexit\n".to_vec()),
            &mut rejected_out,
        )
        .unwrap();
        let rejected = String::from_utf8(rejected_out).unwrap();
        assert!(rejected.contains("rejected"), "{rejected}");
        assert!(!rejected.contains("pwned"));
    }
}
