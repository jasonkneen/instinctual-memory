//! End-to-end: drive the `mem` binary through ingest, consolidate, search,
//! change, read, forget, erase, writeback, and status on a fresh root.
//! Offline: reranking is off and consolidation uses the rules extractor.

use std::path::Path;

use assert_cmd::Command;
use serde_json::Value;

fn mem(root: &Path) -> Command {
    let mut cmd = Command::cargo_bin("mem").unwrap();
    cmd.arg("--root").arg(root);
    cmd.env("MEM_RERANK", "off");
    cmd.env("MEM_LLM_API_KEY", "");
    cmd.env("OPENROUTER_API_KEY", "");
    cmd.env("MEM_JEV_DOTENV_PATH", root.join("no.env"));
    cmd
}

fn json(out: &[u8]) -> Value {
    serde_json::from_slice(out).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(out)))
}

#[test]
fn full_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("mem");
    let project = tmp.path().join("shop");
    std::fs::create_dir_all(project.join(".git")).unwrap();

    // A Claude Code transcript and an AGENTS.md for the project.
    let transcript = tmp.path().join("session.jsonl");
    let cwd = project.display().to_string();
    std::fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"user","uuid":"u1","sessionId":"S1","cwd":cwd,
                "message":{"role":"user","content":"Hello, my name is Priya and I live in Leeds."}}),
            serde_json::json!({"type":"assistant","uuid":"u2","sessionId":"S1","cwd":cwd,
                "message":{"role":"assistant","content":"Nice to meet you."}}),
        ),
    )
    .unwrap();
    std::fs::write(project.join("AGENTS.md"), "# Shop\n\nRun the checkout tests before merging.\n").unwrap();

    mem(&root).arg("init").assert().success();
    mem(&root).arg("ingest").arg(&transcript).arg(project.join("AGENTS.md")).assert().success();
    // Ingesting again adds nothing.
    let again = mem(&root).arg("ingest").arg(&transcript).assert().success();
    assert!(String::from_utf8_lossy(&again.get_output().stdout).contains("0 new events"));

    let status = json(&mem(&root).arg("status").assert().success().get_output().stdout);
    assert_eq!(status["journal"]["events"], 3);
    assert_eq!(status["journal"]["projects"][0], "shop");
    assert_eq!(status["unconsolidated_events"], 3);

    mem(&root).arg("consolidate").assert().success();
    let status = json(&mem(&root).arg("status").assert().success().get_output().stdout);
    assert_eq!(status["unconsolidated_events"], 0);
    assert!(status["active_facts"].as_u64().unwrap() >= 2);

    // Search finds the consolidated fact and the raw event, per project too.
    let found = json(&mem(&root).args(["search", "Priya", "--no-rerank"]).assert().success().get_output().stdout);
    let hits = found["hits"].as_array().unwrap();
    assert!(hits.iter().any(|h| h["statement"].as_str().unwrap().contains("Priya")));
    let scoped = json(
        &mem(&root)
            .args(["search", "checkout tests", "--no-rerank", "--project", "shop"])
            .assert()
            .success()
            .get_output()
            .stdout,
    );
    assert!(!scoped["hits"].as_array().unwrap().is_empty());
    let other = json(
        &mem(&root)
            .args(["search", "checkout tests", "--no-rerank", "--project", "elsewhere"])
            .assert()
            .success()
            .get_output()
            .stdout,
    );
    assert!(other["hits"].as_array().unwrap().is_empty());

    // Immediate change, read, forget: a forgotten fact no longer reads.
    mem(&root)
        .args([
            "change", "remember", "--entity", "w_shop", "--fact-id", "fact_payments",
            "--predicate", "payments", "--statement", "Payments go through Stripe.",
        ])
        .assert()
        .success();
    let read = json(&mem(&root).args(["read", "fact_payments"]).assert().success().get_output().stdout);
    assert_eq!(read["statement"], "Payments go through Stripe.");
    mem(&root)
        .args(["change", "forget", "--entity", "w_shop", "--target", "fact_payments"])
        .assert()
        .success();
    mem(&root).args(["read", "fact_payments"]).assert().failure();

    // Erase removes the name fact everywhere, including history.
    let facts = json(&mem(&root).args(["read", "p_priya"]).assert().success().get_output().stdout);
    let name_fact = facts["fact_id"].as_str().unwrap().to_string();
    let erased = json(&mem(&root).args(["erase", &name_fact]).assert().success().get_output().stdout);
    assert_eq!(erased["facts_erased"][0], name_fact.as_str());
    mem(&root).args(["read", &name_fact]).assert().failure();
    let log = std::process::Command::new("git")
        .args(["--git-dir"])
        .arg(root.join("memory.git"))
        .args(["log", "-p", "memory"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&log.stdout).contains("name is Priya"));

    // Writeback keeps hand-written text and adds the managed block.
    mem(&root)
        .args(["writeback", "--to"])
        .arg(project.join("AGENTS.md"))
        .assert()
        .success();
    let agents = std::fs::read_to_string(project.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("# Shop\n\nRun the checkout tests before merging.\n"));
    assert!(agents.contains("<!-- mem:begin"));

    mem(&root).arg("index").assert().success();
    mem(&root).args(["read", "nonexistent_id"]).assert().failure();
}

#[test]
fn stores_resolve_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    let home_dir = tmp.path().join("homedir");
    let mem_home = tmp.path().join("home/.mem");
    let project = tmp.path().join("proj");
    let plain = tmp.path().join("plain");
    let legacy = tmp.path().join("legacy");
    let explicit = tmp.path().join("explicit");
    std::fs::create_dir_all(&home_dir).unwrap();
    std::fs::create_dir_all(project.join(".git/info")).unwrap();
    std::fs::create_dir_all(project.join("sub")).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::create_dir_all(&legacy).unwrap();
    let run = |dir: &Path, args: &[&str]| {
        let mut cmd = Command::cargo_bin("mem").unwrap();
        cmd.current_dir(dir)
            .env("HOME", &home_dir)
            .env("MEM_HOME", &mem_home)
            .env_remove("MEM_ROOT")
            .args(args);
        cmd
    };
    let same_path = |got: &Value, expect: &Path| {
        let got = got.as_str().unwrap();
        assert_eq!(
            std::fs::canonicalize(got).unwrap(),
            std::fs::canonicalize(expect).unwrap(),
            "{got} vs {}",
            expect.display()
        );
    };

    // No store here, above, or global: say so like git does. Twice, same stderr.
    let fatal = "fatal: not a mem store (or any of the parent directories): .mem";
    let hint = "hint: run `mem init` for a store in this project, or `mem init --global` for ~/.mem";
    let err1 = run(&plain, &["status"]).assert().failure().get_output().stderr.clone();
    let err2 = run(&plain, &["status"]).assert().failure().get_output().stderr.clone();
    assert_eq!(err1, err2);
    let text = String::from_utf8_lossy(&err1);
    assert!(text.contains(fatal), "{text}");
    assert!(text.contains(hint), "{text}");

    // `init` in a project subfolder makes a local store at the project root.
    let created = run(&project.join("sub"), &["init"]).assert().success().get_output().stdout.clone();
    assert!(String::from_utf8_lossy(&created).contains("Initialised local store"));
    assert!(project.join(".mem/memory.git").is_dir());
    let exclude = std::fs::read_to_string(project.join(".git/info/exclude")).unwrap();
    assert!(exclude.lines().any(|l| l == "/.mem/"));
    let status = json(&run(&project.join("sub"), &["status"]).assert().success().get_output().stdout);
    assert_eq!(status["store"], "local");
    same_path(&status["root"], &project.join(".mem"));

    // `mem index` republishes INDEX.md, then says it is already current.
    let indexed = run(&project, &["index"]).assert().success().get_output().stdout.clone();
    assert!(String::from_utf8_lossy(&indexed).contains("INDEX.md published"));
    let again = run(&project, &["index"]).assert().success().get_output().stdout.clone();
    assert!(String::from_utf8_lossy(&again).contains("INDEX.md already current"));

    // Re-running init never wipes a store, and says so.
    let marker = project.join(".mem/journal/keep");
    std::fs::write(&marker, "x").unwrap();
    let repeat = run(&project, &["init"]).assert().success().get_output().stdout.clone();
    let repeat_again = run(&project, &["init"]).assert().success().get_output().stdout.clone();
    assert_eq!(repeat, repeat_again);
    assert!(String::from_utf8_lossy(&repeat).contains("already initialised"));
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
    assert!(project.join(".mem/memory.git").is_dir());

    // A legacy `mem/` directory that holds memory.git is the local store.
    let legacy_store = legacy.join("mem");
    run(&legacy, &["--root"]).arg(&legacy_store).arg("init").assert().success();
    let status = json(&run(&legacy, &["status"]).assert().success().get_output().stdout);
    assert_eq!(status["store"], "local");
    same_path(&status["root"], &legacy_store);

    // `$MEM_ROOT` wins over the global store; `--root` wins over `$MEM_ROOT`.
    run(&plain, &["init", "--global"]).assert().success();
    assert!(!home_dir.join(".mem").exists(), "MEM_HOME must override ~/.mem");
    let mut via_env = Command::cargo_bin("mem").unwrap();
    via_env
        .current_dir(&plain)
        .env("HOME", &home_dir)
        .env("MEM_HOME", &mem_home)
        .env("MEM_ROOT", project.join(".mem"))
        .arg("status");
    let status = json(&via_env.assert().success().get_output().stdout);
    assert_eq!(status["store"], "MEM_ROOT");
    same_path(&status["root"], &project.join(".mem"));

    run(&plain, &["--root"]).arg(&explicit).arg("init").assert().success();
    let mut via_flag = Command::cargo_bin("mem").unwrap();
    via_flag
        .current_dir(&project)
        .env("HOME", &home_dir)
        .env("MEM_HOME", &mem_home)
        .env("MEM_ROOT", project.join(".mem"))
        .arg("--root")
        .arg(&explicit)
        .arg("status");
    let status = json(&via_flag.assert().success().get_output().stdout);
    assert_eq!(status["store"], "explicit (--root)");
    same_path(&status["root"], &explicit);

    // Outside the project, with a global store, the global one is used.
    let status = json(&run(&plain, &["status"]).assert().success().get_output().stdout);
    assert_eq!(status["store"], "global");
    same_path(&status["root"], &mem_home);
    let status = json(&run(&project, &["--global", "status"]).assert().success().get_output().stdout);
    assert_eq!(status["store"], "global");
    same_path(&status["root"], &mem_home);
}

#[test]
fn writeback_local_store_includes_every_entity_and_skips_file_facts() {
    let tmp = tempfile::tempdir().unwrap();
    let home_dir = tmp.path().join("homedir");
    let mem_home = tmp.path().join("mem-home");
    let project = tmp.path().join("shop");
    std::fs::create_dir_all(&home_dir).unwrap();
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::write(project.join("AGENTS.md"), "# Shop\n\nHand written rule.\n").unwrap();
    let mem_in = |dir: &Path| {
        let mut cmd = Command::cargo_bin("mem").unwrap();
        cmd.current_dir(dir)
            .env("HOME", &home_dir)
            .env("MEM_HOME", &mem_home)
            .env_remove("MEM_ROOT")
            .env("MEM_RERANK", "off");
        cmd
    };
    mem_in(&project).arg("init").assert().success();
    let root = project.join(".mem");
    let remember = |request: &str, entity: &str, fact_id: &str, statement: &str, source: &str| {
        let repo = mem::repo::GitRepo::open(root.join("memory.git")).unwrap();
        let now = chrono::Utc::now();
        mem::change::apply(
            &repo,
            mem::change::ChangeRequest {
                kind: mem::change::ChangeKind::Remember,
                request_id: request.into(),
                scope_id: "personal".into(),
                session_id: "s".into(),
                source_event_id: source.into(),
                entity_id: entity.into(),
                fact: Some(mem::fact::Fact {
                    id: fact_id.into(),
                    predicate: "note".into(),
                    statement: statement.into(),
                    kind: mem::fact::FactKind::ExplicitAssertion,
                    status: mem::fact::FactStatus::Active,
                    observed_at: now,
                    valid_from: None,
                    valid_from_precision: None,
                    valid_to: None,
                    expires_at: None,
                    review_after: None,
                    supersedes: vec![],
                    sources: vec![mem::fact::SourceRef {
                        event_id: source.into(),
                        role: mem::fact::Role::Note,
                        evidence: statement.into(),
                    }],
                    visibility: mem::fact::Visibility::Private,
                }),
                target_fact_id: None,
                reason: None,
                proposed_at: now,
            },
        )
        .unwrap();
    };
    remember("req_fly", "w_shop", "fact_fly", "Deploys go to Fly with fly deploy.", "evt_chat_user");
    remember("req_ada", "p_ada", "fact_ada", "Ada reviews the release checklist.", "evt_chat_ada");
    remember(
        "req_file",
        "w_shop",
        "fact_file",
        "Run the checkout tests before merging.",
        "evt_ide_md:/shop/AGENTS.md:abcd_0",
    );

    mem_in(&project)
        .args(["writeback", "--to"])
        .arg(project.join("AGENTS.md"))
        .assert()
        .success();
    let agents = std::fs::read_to_string(project.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("# Shop\n\nHand written rule.\n"), "{agents}");
    assert_eq!(agents.matches("<!-- mem:begin").count(), 1, "{agents}");
    assert!(agents.contains("Deploys go to Fly with fly deploy."), "{agents}");
    assert!(agents.contains("Ada reviews the release checklist."), "{agents}");
    assert!(!agents.contains("Run the checkout tests before merging."), "{agents}");
    assert_eq!(agents.matches("## Project").count(), 1, "{agents}");
    assert_eq!(agents.matches("## People").count(), 1, "{agents}");

    mem_in(&project)
        .args(["writeback", "--include-file-facts", "--to"])
        .arg(project.join("AGENTS.md"))
        .assert()
        .success();
    let agents = std::fs::read_to_string(project.join("AGENTS.md")).unwrap();
    assert!(agents.starts_with("# Shop\n\nHand written rule.\n"), "{agents}");
    assert_eq!(agents.matches("<!-- mem:begin").count(), 1, "{agents}");
    assert!(agents.contains("Run the checkout tests before merging."), "{agents}");
    assert!(agents.contains("Deploys go to Fly with fly deploy."), "{agents}");

    // A shared store (`--root`) writes the project's entity, not every entity.
    mem_in(&project)
        .arg("--root")
        .arg(&root)
        .args(["writeback", "--to"])
        .arg(project.join("NOTES.md"))
        .assert()
        .success();
    let notes = std::fs::read_to_string(project.join("NOTES.md")).unwrap();
    assert!(notes.contains("Deploys go to Fly with fly deploy."), "{notes}");
    assert!(!notes.contains("Ada reviews the release checklist."), "{notes}");
    assert!(!notes.contains("Run the checkout tests before merging."), "{notes}");
}

#[test]
fn hooks_inject_relevant_memory_and_stay_silent_otherwise() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("shop");
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(project.join(".git/info")).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    let home = tmp.path().join("home/.mem");
    let mem_in = |dir: &Path| {
        let mut cmd = Command::cargo_bin("mem").unwrap();
        cmd.current_dir(dir).env("MEM_HOME", &home).env_remove("MEM_ROOT").env("MEM_RERANK", "off");
        cmd
    };
    mem_in(&project).arg("init").assert().success();
    mem_in(&project)
        .args(["change", "remember", "--entity", "w_shop", "--predicate", "deploy",
               "--statement", "The shop backend deploys to Fly with fly deploy."])
        .assert()
        .success();
    mem_in(&project)
        .args(["change", "remember", "--entity", "pref_user", "--predicate", "style",
               "--statement", "User wants small commits with clear messages."])
        .assert()
        .success();

    let hook = |dir: &Path, event: &str, payload: serde_json::Value| {
        let out = mem_in(dir).args(["hook", event]).write_stdin(payload.to_string()).assert().success();
        String::from_utf8_lossy(&out.get_output().stdout).to_string()
    };
    let cwd = project.display().to_string();

    let start = hook(&project, "start", serde_json::json!({"cwd": cwd, "hook_event_name": "SessionStart"}));
    let start: Value = serde_json::from_str(&start).unwrap();
    let text = start["hookSpecificOutput"]["additionalContext"].as_str().unwrap();
    assert!(text.contains("small commits"), "{text}");

    let prompt = hook(&project, "prompt", serde_json::json!({"cwd": cwd, "prompt": "how does the backend deploy?"}));
    let prompt: Value = serde_json::from_str(&prompt).unwrap();
    assert_eq!(prompt["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(prompt["hookSpecificOutput"]["additionalContext"].as_str().unwrap().contains("fly deploy"));

    // Unrelated prompt, short prompt, slash command: nothing injected.
    for p in ["rename the header component please", "ok", "/help me now please"] {
        assert!(hook(&project, "prompt", serde_json::json!({"cwd": cwd, "prompt": p})).trim().is_empty(), "{p}");
    }
    // No store anywhere: silent success, not an error.
    let none = hook(&elsewhere, "prompt", serde_json::json!({"cwd": elsewhere.display().to_string(), "prompt": "how does the backend deploy?"}));
    assert!(none.trim().is_empty());
    // Plain (pi/omp) output is bare text, without the Claude envelope.
    let plain = mem_in(&project)
        .args(["hook", "prompt", "--plain", "--cwd"])
        .arg(&project)
        .args(["--prompt", "how does the backend deploy?"])
        .assert()
        .success();
    let plain = String::from_utf8_lossy(&plain.get_output().stdout).to_string();
    assert!(plain.contains("fly deploy"), "{plain}");
    assert!(!plain.trim_start().starts_with('{'), "{plain}");
    // Garbage payload: still silent success.
    mem_in(&project).args(["hook", "prompt"]).write_stdin("not json").assert().success();
}

#[test]
fn backfills_pi_and_omp_sessions_and_runs_operations() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = tmp.path().join("mem");
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(home.join(".pi/agent/sessions/--x--")).unwrap();
    std::fs::create_dir_all(home.join(".omp/agent/sessions/--x--")).unwrap();
    let cwd = project.display().to_string();
    let session = |harness: &str| {
        [
            serde_json::json!({"type":"title","v":1,"title":"Work"}),
            serde_json::json!({"type":"session","version":3,"id":format!("{harness}-s"),"cwd":cwd}),
            serde_json::json!({"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"We deploy with fly"}]}}),
            serde_json::json!({"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":[{"type":"thinking","thinking":"x"},{"type":"text","text":"Noted."},{"type":"toolCall","name":"bash","arguments":{}}]}}),
        ]
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n"
    };
    std::fs::write(home.join(".pi/agent/sessions/--x--/pi.jsonl"), session("pi")).unwrap();
    std::fs::write(home.join(".omp/agent/sessions/--x--/omp.jsonl"), session("omp")).unwrap();
    // pi/omp keep advisor and worker sidecars in a sibling directory named
    // after the session. They are agent-to-agent and must be skipped.
    let sidecar_dir = home.join(".pi/agent/sessions/--x--/pi-s");
    std::fs::create_dir_all(&sidecar_dir).unwrap();
    std::fs::write(sidecar_dir.join("__advisor.jsonl"), session("advisor")).unwrap();
    std::fs::write(sidecar_dir.join("worker.jsonl"), session("worker")).unwrap();

    mem(&root).arg("init").assert().success();
    Command::cargo_bin("mem")
        .unwrap()
        .env("HOME", &home)
        .env("MEM_PI_AGENT_DIR", home.join(".pi/agent"))
        .env("MEM_OMP_AGENT_DIR", home.join(".omp/agent"))
        .env("MEM_RERANK", "off")
        .arg("--root")
        .arg(&root)
        .args(["backfill-local", "--project"])
        .arg(&project)
        .assert()
        .success();

    let status = json(&mem(&root).arg("status").assert().success().get_output().stdout);
    // Thinking and tool calls are dropped: two text messages per session.
    // The two sidecar files in the sibling directory are skipped.
    assert_eq!(status["journal"]["events"], 4);
    assert_eq!(status["journal"]["projects"][0], "proj");

    // The hidden `mem op` runs the same operations as the MCP server.
    let found = json(
        &mem(&root)
            .args(["op", "memory_search", "--args", r#"{"query":"deploy","rerank":false}"#])
            .assert()
            .success()
            .get_output()
            .stdout,
    );
    assert!(!found["hits"].as_array().unwrap().is_empty());

    // Re-running the backfill adds nothing (stable ids for both harnesses).
    let again = Command::cargo_bin("mem")
        .unwrap()
        .env("HOME", &home)
        .env("MEM_PI_AGENT_DIR", home.join(".pi/agent"))
        .env("MEM_OMP_AGENT_DIR", home.join(".omp/agent"))
        .env("MEM_RERANK", "off")
        .arg("--root")
        .arg(&root)
        .args(["backfill-local", "--project"])
        .arg(&project)
        .assert()
        .success();
    let status = json(&mem(&root).arg("status").assert().success().get_output().stdout);
    assert_eq!(status["journal"]["events"], 4, "{}", String::from_utf8_lossy(&again.get_output().stdout));
}
