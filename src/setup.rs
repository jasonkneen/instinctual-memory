//! `mem setup`: wire memory into the agents so they use it without asking.
//!
//! Claude Code: `SessionStart` / `UserPromptSubmit` / `SessionEnd` hooks in
//! `~/.claude/settings.json` (see [`crate::hook`]), the `mem` MCP server at
//! user scope, and the `/mem` skill. Codex: the `mem` MCP server in
//! `~/.codex/config.toml` (Codex also reads the AGENTS.md that
//! `mem writeback` maintains). Every edited file is backed up first, and
//! running setup again changes nothing. `--remove` undoes it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use crate::error::{Error, Result};

/// The `/mem` skill, shipped inside the binary.
const SKILL: &str = include_str!("../skills/mem/SKILL.md");

/// (hook event, `mem hook` argument, timeout seconds)
const HOOKS: &[(&str, &str, u64)] = &[
    ("SessionStart", "start", 10),
    ("UserPromptSubmit", "prompt", 10),
    ("SessionEnd", "end", 10),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Claude,
    Codex,
    All,
}

/// Install (or with `remove`, uninstall) and return what was done.
pub fn run(target: Target, remove: bool) -> Result<Vec<String>> {
    let home = home()?;
    let mem = mem_command();
    let mut done = Vec::new();
    if matches!(target, Target::Claude | Target::All) {
        claude_hooks(&home, &mem, remove, &mut done)?;
        claude_mcp(&mem, remove, &mut done);
        claude_skill(&home, remove, &mut done)?;
    }
    if matches!(target, Target::Codex | Target::All) {
        codex_mcp(&home, &mem, remove, &mut done)?;
    }
    Ok(done)
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Usage("HOME is not set".into()))
}

/// How agents should call `mem`: the `mem` on PATH if there is one (a
/// symlink keeps following new builds), else this executable.
fn mem_command() -> String {
    let on_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|dir| dir.join("mem")).find(|p| p.is_file())
    });
    on_path
        .or_else(|| std::env::current_exe().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "mem".into())
}

fn is_mem_hook(command: &str, arg: &str) -> bool {
    command.contains("mem") && command.trim_end().ends_with(&format!(" hook {arg}"))
}

fn backup(path: &Path) -> Result<()> {
    if path.exists() {
        let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
        let copy = path.with_extension(format!("{}.mem-backup-{stamp}", path.extension().and_then(|e| e.to_str()).unwrap_or("")));
        std::fs::copy(path, &copy).map_err(|e| Error::io(path, e))?;
    }
    Ok(())
}

fn claude_hooks(home: &Path, mem: &str, remove: bool, done: &mut Vec<String>) -> Result<()> {
    let path = home.join(".claude").join("settings.json");
    let mut settings: Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| Error::Usage(format!("{} is not valid JSON ({e}); fix it and rerun", path.display())))?,
        Err(_) => json!({}),
    };
    if !settings.is_object() {
        return Err(Error::Usage(format!("{} is not a JSON object", path.display())));
    }
    if !settings["hooks"].is_object() {
        settings["hooks"] = json!({});
    }
    let mut changed = false;
    for (event, arg, timeout) in HOOKS {
        let groups = settings["hooks"][*event].as_array().cloned().unwrap_or_default();
        let present = groups.iter().any(|g| {
            g["hooks"].as_array().is_some_and(|hs| {
                hs.iter().any(|h| h["command"].as_str().is_some_and(|c| is_mem_hook(c, arg)))
            })
        });
        if remove {
            if present {
                let kept: Vec<Value> = groups
                    .into_iter()
                    .filter(|g| {
                        !g["hooks"].as_array().is_some_and(|hs| {
                            hs.iter().any(|h| h["command"].as_str().is_some_and(|c| is_mem_hook(c, arg)))
                        })
                    })
                    .collect();
                settings["hooks"][*event] = Value::Array(kept);
                done.push(format!("Claude Code: removed the {event} hook"));
                changed = true;
            }
        } else if !present {
            let mut groups = groups;
            groups.push(json!({"hooks": [{
                "type": "command",
                "command": format!("'{mem}' hook {arg}"),
                "timeout": timeout,
            }]}));
            settings["hooks"][*event] = Value::Array(groups);
            done.push(format!("Claude Code: added the {event} hook (mem hook {arg})"));
            changed = true;
        }
    }
    if changed {
        backup(&path)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        }
        let body = serde_json::to_string_pretty(&settings)? + "\n";
        std::fs::write(&path, body).map_err(|e| Error::io(&path, e))?;
    } else if !remove {
        done.push("Claude Code: hooks already installed".into());
    }
    Ok(())
}

fn claude_mcp(mem: &str, remove: bool, done: &mut Vec<String>) {
    let quiet = |args: &[&str]| {
        Command::new("claude")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if Command::new("claude").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        done.push("Claude Code: `claude` not on PATH; MCP server not registered".into());
        return;
    }
    let present = quiet(&["mcp", "get", "mem"]);
    if remove {
        if present && quiet(&["mcp", "remove", "-s", "user", "mem"]) {
            done.push("Claude Code: removed the mem MCP server".into());
        }
    } else if present {
        done.push("Claude Code: mem MCP server already registered".into());
    } else if quiet(&["mcp", "add", "-s", "user", "mem", "--", mem, "serve", "--stdio"]) {
        done.push("Claude Code: registered the mem MCP server (user scope)".into());
    } else {
        done.push(format!("Claude Code: could not register MCP; run `claude mcp add -s user mem -- {mem} serve --stdio`"));
    }
}

fn claude_skill(home: &Path, remove: bool, done: &mut Vec<String>) -> Result<()> {
    let dir = home.join(".claude").join("skills").join("mem");
    let file = dir.join("SKILL.md");
    if remove {
        // Only remove a copy setup wrote, never a link to a checkout.
        let is_link = std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink());
        if !is_link && std::fs::read_to_string(&file).is_ok_and(|s| s == SKILL) {
            std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
            done.push("Claude Code: removed the /mem skill".into());
        }
        return Ok(());
    }
    if file.exists() {
        done.push("Claude Code: /mem skill already present".into());
        return Ok(());
    }
    std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    std::fs::write(&file, SKILL).map_err(|e| Error::io(&file, e))?;
    done.push("Claude Code: installed the /mem skill".into());
    Ok(())
}

const CODEX_HEADER: &str = "[mcp_servers.mem]";

fn codex_mcp(home: &Path, mem: &str, remove: bool, done: &mut Vec<String>) -> Result<()> {
    let path = home.join(".codex").join("config.toml");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let present = current.lines().any(|l| l.trim() == CODEX_HEADER);
    if remove {
        if present {
            backup(&path)?;
            let mut out = Vec::new();
            let mut skipping = false;
            for line in current.lines() {
                let t = line.trim();
                if t == CODEX_HEADER || t.starts_with("[mcp_servers.mem.") {
                    skipping = true;
                    continue;
                }
                if skipping && t.starts_with('[') {
                    skipping = false;
                }
                if !skipping {
                    out.push(line);
                }
            }
            std::fs::write(&path, out.join("\n") + "\n").map_err(|e| Error::io(&path, e))?;
            done.push("Codex: removed the mem MCP server".into());
        }
        return Ok(());
    }
    if present {
        done.push("Codex: mem MCP server already configured".into());
        return Ok(());
    }
    if !home.join(".codex").is_dir() {
        done.push("Codex: ~/.codex not found; skipped".into());
        return Ok(());
    }
    backup(&path)?;
    let sep = if current.is_empty() || current.ends_with("\n\n") {
        ""
    } else if current.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    let block = format!("{CODEX_HEADER}\ncommand = {mem:?}\nargs = [\"serve\", \"--stdio\"]\n");
    std::fs::write(&path, format!("{current}{sep}{block}")).map_err(|e| Error::io(&path, e))?;
    done.push("Codex: added the mem MCP server to ~/.codex/config.toml".into());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hooks_install_once_and_remove_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let other = json!({"theme": "dark", "hooks": {"UserPromptSubmit": [{"hooks": [{"type": "command", "command": "other-tool"}]}]}});
        std::fs::write(home.join(".claude/settings.json"), other.to_string()).unwrap();

        let mut done = Vec::new();
        claude_hooks(home, "/bin/mem", false, &mut done).unwrap();
        claude_hooks(home, "/bin/mem", false, &mut done).unwrap();
        let s: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".claude/settings.json")).unwrap()).unwrap();
        assert_eq!(s["theme"], "dark");
        let prompt = s["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(prompt.len(), 2, "other tool's hook kept, mem added once");
        assert_eq!(prompt[1]["hooks"][0]["command"], "'/bin/mem' hook prompt");
        assert!(s["hooks"]["SessionStart"].is_array() && s["hooks"]["SessionEnd"].is_array());

        claude_hooks(home, "/bin/mem", true, &mut done).unwrap();
        let s: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".claude/settings.json")).unwrap()).unwrap();
        assert_eq!(s["hooks"]["UserPromptSubmit"].as_array().unwrap().len(), 1);
        assert_eq!(s["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "other-tool");
    }

    #[test]
    fn codex_block_added_once_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(home.join(".codex/config.toml"), "model = \"x\"\n\n[mcp_servers.other]\ncommand = \"o\"\n").unwrap();
        let mut done = Vec::new();
        codex_mcp(home, "/bin/mem", false, &mut done).unwrap();
        codex_mcp(home, "/bin/mem", false, &mut done).unwrap();
        let cfg = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert_eq!(cfg.matches(CODEX_HEADER).count(), 1);
        assert!(cfg.contains("[mcp_servers.other]"));
        codex_mcp(home, "/bin/mem", true, &mut done).unwrap();
        let cfg = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
        assert!(!cfg.contains(CODEX_HEADER));
        assert!(cfg.contains("[mcp_servers.other]\ncommand = \"o\""));
    }
}
