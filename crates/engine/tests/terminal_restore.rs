//! Terminal tabs across restarts: each tab's own command history and earlier output, and the
//! Claude Code session that ran in it resumed exactly (`claude --resume <id>`). Shells run with
//! a temporary `HOME`; `claude` is a fake that runs CodeCraft's session hook like Claude Code's
//! SessionStart hook would (the real CLI never runs in tests).
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;

const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo "fake-claude $*"
settings=; prev=; sid=; resume=
for a in "$@"; do
  case "$prev" in --settings) settings=$a;; --session-id) sid=$a;; --resume) resume=$a;; esac
  prev=$a
done
id=${resume:-${sid:-shell-session-1}}
mkdir -p "$HOME/transcripts"
: >"$HOME/transcripts/$id.jsonl"
if [ -n "$settings" ]; then
  cmd=$(sed -n 's/.*"command": *"\(.*\)".*/\1/p' "$settings")
  printf '{"session_id":"%s","transcript_path":"%s","source":"startup"}' "$id" "$HOME/transcripts/$id.jsonl" | eval "$cmd"
fi
echo "fake-claude ready $id"
exec sleep 60
"#;

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    iso: PathBuf,
    bin: PathBuf,
}

fn env() -> Env {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let (root, home, iso, bin) = (dir.join("proj"), dir.join("home"), dir.join("iso"), dir.join("bin"));
    for d in [&root, &home, &iso, &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(bin.join("claude"), FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    // The user's own startup files: only the fake `claude` and system tools on PATH.
    let profile = "export PATH=\"$FAKE_BIN:/usr/bin:/bin\"\nPS1='$ '\nPROMPT_COMMAND='history -a'\n";
    std::fs::write(home.join(".bash_profile"), profile).unwrap();
    std::fs::write(home.join(".bash_history"), "echo from-global-history\n").unwrap();
    let zshrc = "export PATH=\"$FAKE_BIN:/usr/bin:/bin\"\nPS1='%% '\nUSER_RC=yes\nHISTFILE=$HOME/.zsh_history\nSAVEHIST=100\nHISTSIZE=100\n";
    std::fs::write(home.join(".zshrc"), zshrc).unwrap();
    std::fs::write(home.join(".zsh_history"), "echo from-global-history\n").unwrap();
    Env { _tmp: tmp, root, home, iso, bin }
}

fn session(e: &Env, shell: &str) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&e.iso);
    s.session_store.fallback_dir = Some(e.iso.join("workspaces"));
    s.session_store.enable();
    s.terminal.shell = Some(shell.into());
    s.terminal.claude_program = Some(e.bin.join("claude"));
    s.terminal.env = vec![
        ("HOME".into(), e.home.display().to_string()),
        ("FAKE_BIN".into(), e.bin.display().to_string()),
        ("BASH_SILENCE_DEPRECATION_WARNING".into(), "1".into()),
    ];
    s.execute("file.open", &json!({ "path": e.root })).unwrap();
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(30) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    s
}

fn text(s: &mut Session, tab: u64) -> String {
    s.pump();
    let r = s.execute("terminal.read", &json!({ "tab": tab, "scrollback": true })).unwrap();
    r["text"].as_str().unwrap().to_string()
}

fn wait_text(s: &mut Session, tab: u64, needle: &str) -> String {
    let start = Instant::now();
    loop {
        let t = text(s, tab);
        if t.contains(needle) {
            return t;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "no `{needle}` in:\n{t}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_file(path: &Path, needle: &str) {
    let start = Instant::now();
    while !std::fs::read_to_string(path).is_ok_and(|t| t.contains(needle)) {
        assert!(start.elapsed() < Duration::from_secs(20), "no `{needle}` in {}", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn run(s: &mut Session, tab: u64, line: &str, expect: &str) -> String {
    s.execute("terminal.sendText", &json!({ "tab": tab, "text": format!("{line}\r") })).unwrap();
    wait_text(s, tab, expect)
}

fn tab_id(v: &Value) -> u64 {
    v["tab"].as_u64().unwrap()
}

/// The per-tab files of the only project in `iso`.
fn tab_files(e: &Env) -> PathBuf {
    let dir = e.iso.join("terminals");
    std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|d| d.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("proj-")))
        .unwrap()
}

#[test]
fn tabs_keep_their_history_output_and_claude_sessions() {
    if !Path::new("/bin/bash").is_file() {
        eprintln!("skipped: no /bin/bash");
        return;
    }
    let e = env();
    let mut s = session(&e, "/bin/bash");

    // A shell tab where the user started Claude Code themselves.
    let work = tab_id(&s.execute("terminal.new", &json!({ "title": "Work" })).unwrap());
    let out = run(&mut s, work, "claude --model fake", "fake-claude ready shell-session-1");
    assert!(out.contains("--settings ") && out.contains("--model fake"), "the shim added the hook:\n{out}");
    // A plain shell with its own history.
    let notes = tab_id(&s.execute("terminal.new", &Value::Null).unwrap());
    run(&mut s, notes, "echo note-$((1+2))", "note-3");
    run(&mut s, notes, "echo second-$((2+2))", "second-4");
    // A Claude Code tab.
    let r = s.execute("terminal.openClaude", &Value::Null).unwrap();
    let (agent, session_id) = (tab_id(&r), r["session"].as_str().unwrap().to_string());
    let out = wait_text(&mut s, agent, &format!("fake-claude ready {session_id}"));
    assert!(out.contains(&format!("--session-id {session_id}")), "{out}");
    // A tab the user closes: its files go.
    let gone = tab_id(&s.execute("terminal.new", &Value::Null).unwrap());
    run(&mut s, gone, "echo bye-$((3+3))", "bye-6");
    let files = tab_files(&e);
    let key = |s: &Session, id: u64| s.terminal.tabs.iter().find(|t| t.id == id).unwrap().key.clone();
    let gone_history = files.join(format!("{}.history", key(&s, gone)));
    wait_file(&gone_history, "bye-");
    s.execute("terminal.close", &json!({ "tab": gone })).unwrap();
    assert!(!gone_history.exists());

    // Each tab writes its own history file, without the others' commands or the user's.
    let notes_history = files.join(format!("{}.history", key(&s, notes)));
    wait_file(&notes_history, "echo second-");
    let h = std::fs::read_to_string(&notes_history).unwrap();
    assert!(!h.contains("claude") && !h.contains("from-global-history"), "{h}");
    assert!(std::fs::read_to_string(e.home.join(".bash_history")).unwrap().lines().count() == 1);

    s.execute("terminal.activate", &json!({ "tab": agent })).unwrap();
    s.shutdown();
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(e.root.join(".codecraft/workspace.json")).unwrap()).unwrap();
    let terms = saved["terminals"].as_array().unwrap();
    assert_eq!(terms.len(), 3, "{saved}");
    assert_eq!(
        (terms[0]["name"].as_str(), terms[0]["claude"]["session"].as_str()),
        (Some("Work"), Some("shell-session-1"))
    );
    assert!(terms[1]["claude"].is_null() && terms[1]["kind"].is_null());
    assert_eq!(
        (terms[2]["kind"].as_str(), terms[2]["claude"]["session"].as_str()),
        (Some("claudeCode"), Some(session_id.as_str()))
    );
    assert_eq!(saved["terminal_active"], 2);
    // Output is kept for the plain shell only.
    let outputs: Vec<String> = std::fs::read_dir(&files)
        .unwrap()
        .flatten()
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".output"))
        .collect();
    assert_eq!(outputs, vec![format!("{}.output", terms[1]["key"].as_str().unwrap())]);

    // Restart.
    let mut s = session(&e, "/bin/bash");
    let tabs: Vec<(u64, String)> = s.terminal.tabs.iter().map(|t| (t.id, t.title().to_string())).collect();
    assert_eq!(tabs.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(), ["Work", "Local", "Claude Code"]);
    assert_eq!(s.terminal.active, 2);
    // The shell tab resumes its Claude Code session (through the shim, so it is tracked again).
    let out = wait_text(&mut s, tabs[0].0, "fake-claude ready shell-session-1");
    assert!(out.contains("--resume shell-session-1") && out.contains("--settings "), "{out}");
    assert!(!out.contains("restored from the previous session"), "{out}");
    // The plain shell shows its earlier output and has its own history, then the user's.
    let notes = tabs[1].0;
    let out = wait_text(&mut s, notes, "restored from the previous session");
    assert!(out.contains("note-3") && out.contains("second-4"), "{out}");
    run(&mut s, notes, "echo \"hc=$(history | grep -v hc= | grep -c 'echo second-')\"", "hc=1");
    run(&mut s, notes, "echo \"gc=$(history | grep -v gc= | grep -c from-global-history)\"", "gc=1");
    run(&mut s, notes, "echo \"cc=$(history | grep -v cc= | grep -c 'claude --model')\"", "cc=0");
    // The Claude Code tab resumes its own session.
    let out = wait_text(&mut s, tabs[2].0, &format!("fake-claude ready {session_id}"));
    assert!(out.contains(&format!("--resume {session_id}")), "{out}");
    s.shutdown();

    // A session whose transcript is gone starts fresh instead of failing to resume.
    std::fs::remove_file(e.home.join(format!("transcripts/{session_id}.jsonl"))).unwrap();
    let mut s = session(&e, "/bin/bash");
    let agent = s.terminal.tabs[2].id;
    let out = wait_text(&mut s, agent, "fake-claude ready");
    assert!(out.contains("--session-id ") && !out.contains("--resume"), "{out}");
    s.shutdown();
}

#[test]
fn zsh_tabs_run_the_users_files_and_keep_their_own_history() {
    if !Path::new("/bin/zsh").is_file() {
        eprintln!("skipped: no /bin/zsh");
        return;
    }
    let e = env();
    let mut s = session(&e, "/bin/zsh");
    let tab = tab_id(&s.execute("terminal.new", &Value::Null).unwrap());
    let key = s.terminal.tabs[0].key.clone();
    let out = run(&mut s, tab, "echo \"rc=$USER_RC zd=${ZDOTDIR-unset} hf=${HISTFILE:t}\"", "rc=yes");
    assert!(out.contains(&format!("rc=yes zd=unset hf={key}.history")), "{out}");
    run(&mut s, tab, "echo \"g=$(fc -ln 1 | grep -v g= | grep -c from-global-history)\"", "g=1");
    run(&mut s, tab, "echo \"p=${path[1]:t:h}\"", "p=");
    let out = run(&mut s, tab, "echo \"shim=$(command -v claude)\"", "integration/bin/claude");
    assert!(out.contains("integration/bin/claude"), "{out}");
    run(&mut s, tab, "echo zsh-$((5*5))", "zsh-25");
    let history = tab_files(&e).join(format!("{key}.history"));
    wait_file(&history, "echo zsh-");
    assert!(!std::fs::read_to_string(e.home.join(".zsh_history")).unwrap().contains("zsh-"));
    s.shutdown();

    let mut s = session(&e, "/bin/zsh");
    let tab = s.terminal.tabs[0].id;
    assert_eq!(s.terminal.tabs[0].key, key);
    wait_text(&mut s, tab, "restored from the previous session");
    run(&mut s, tab, "echo \"h=$(fc -ln 1 | grep -v h= | grep -c 'echo zsh-')\"", "h=1");
    s.shutdown();
}

#[test]
fn a_saved_local_claude_shell_keeps_its_machine_in_a_remote_project() {
    if !Path::new("/bin/bash").is_file() { return; }
    let e = env();
    let mut s = session(&e, "/bin/bash");
    s.terminal.env.push(("PATH".into(), format!("{}:/usr/bin:/bin", e.bin.display())));
    s.remote.programs.ssh = e.bin.join("no-real-ssh").display().to_string();
    s.remote.auto_refresh = false;
    s.remote.project = Some(codecraft_engine::remote::RemoteProject {
        server: "fixture-only".into(), destination: "fixture-only".into(), remote: "~/project".into(),
        connection: codecraft_engine::remote::ConnectionState::Disconnected, error: None, pending_uploads: Default::default(),
    });
    let mut snapshot = codecraft_engine::session_store::snapshot(&s);
    snapshot.terminals = vec![codecraft_workspace::TerminalSnapshot {
        cwd: e.root.clone(),
        claude: Some(codecraft_workspace::ClaudeSnapshot { session: "shell-session-1".into(), transcript: None }),
        ..Default::default()
    }];
    codecraft_engine::session_store::restore(&mut s, &snapshot);
    let tab = s.terminal.tabs.first().expect("restored local Claude shell");
    assert!(!tab.remote);
    let id = tab.id;
    let output = wait_text(&mut s, id, "fake-claude ready shell-session-1");
    assert!(output.contains("--resume shell-session-1") && output.contains("--settings "));
    s.shutdown();
}
