//! Terminal commands end to end: tabs, input, reading, copy/paste, clear, Run in Terminal, Open
//! Claude Code (with a fake `claude`; the real CLI never runs in tests) and process cleanup.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::workspace::ToolWindowId;
use codecraft_engine::{CommandError, Effect, Session, terminal};

fn session(root: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.execute("file.open", &json!({ "path": root })).unwrap();
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(30) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    s
}

fn read(s: &mut Session, tab: u64) -> Value {
    s.execute("terminal.read", &json!({ "tab": tab, "scrollback": true })).unwrap()
}

fn wait_text(s: &mut Session, tab: u64, needle: &str) -> Value {
    let start = Instant::now();
    loop {
        s.pump();
        let r = read(s, tab);
        if r["text"].as_str().unwrap().contains(needle) {
            return r;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "no `{needle}` in:\n{}", r["text"].as_str().unwrap());
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| {
            let stat = String::from_utf8_lossy(&o.stdout).trim().to_string();
            !stat.is_empty() && !stat.starts_with('Z')
        })
        .unwrap_or(false)
}

fn wait_dead(pid: u32) {
    let start = Instant::now();
    while alive(pid) {
        assert!(start.elapsed() < Duration::from_secs(5), "process {pid} survived");
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn tab_id(v: &Value) -> u64 {
    v["tab"].as_u64().unwrap()
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

#[test]
fn tabs_input_and_output() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mut s = session(&root);
    assert!(s.execute("terminal.sendText", &json!({ "text": "x" })).is_err(), "no terminal yet");

    let a = tab_id(&s.execute("terminal.new", &Value::Null).unwrap());
    assert!(s.ws.layout.is_visible(ToolWindowId::Terminal), "opening a tab shows the window");
    assert!(s.terminal.focus_requested);
    s.execute("terminal.sendText", &json!({ "text": "echo \"hello-$((40+2)) $TERM_PROGRAM\"; pwd -P\r" })).unwrap();
    let r = wait_text(&mut s, a, "hello-42 CodeCraft");
    assert!(r["text"].as_str().unwrap().contains(&root.display().to_string()), "starts in the project root");
    assert_eq!(r["alive"], true);
    assert_eq!(r["title"], "Local");

    // A second tab, named like RustRover's; next/previous cycle.
    let b = tab_id(&s.execute("terminal.new", &json!({ "title": "  " })).unwrap());
    let list = s.execute("terminal.list", &Value::Null).unwrap();
    assert_eq!(list[1]["title"], "Local (2)");
    assert_eq!(list[1]["active"], true);
    s.execute("terminal.next", &Value::Null).unwrap();
    assert_eq!(s.terminal.active_tab().unwrap().id, a);
    s.execute("terminal.previous", &Value::Null).unwrap();
    assert_eq!(s.terminal.active_tab().unwrap().id, b);
    s.execute("terminal.activate", &json!({ "tab": a })).unwrap();
    assert_eq!(s.terminal.active_tab().unwrap().id, a);
    s.execute("terminal.rename", &json!({ "tab": b, "title": "Build" })).unwrap();
    assert_eq!(s.execute("terminal.list", &Value::Null).unwrap()[1]["title"], "Build");

    // Keys: Ctrl+C interrupts a running program.
    s.execute("terminal.sendText", &json!({ "tab": b, "text": "sleep 100\r" })).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    s.execute("terminal.sendKey", &json!({ "tab": b, "key": "Ctrl+C" })).unwrap();
    s.execute("terminal.sendText", &json!({ "tab": b, "text": "echo after-$((1+1))\r" })).unwrap();
    let r = wait_text(&mut s, b, "after-2");
    // `after-2` arrived well before the 100 s sleep would have ended.
    assert!(r["text"].as_str().unwrap().contains("^C"), "{}", r["text"]);
    assert!(matches!(s.execute("terminal.sendKey", &json!({ "key": "Hyper+Q" })), Err(CommandError::BadParams(_))));
    assert!(matches!(s.execute("terminal.close", &json!({ "tab": 999 })), Err(CommandError::BadParams(_))));

    // Paste (bracketing is up to the program; /bin/sh just reads it).
    s.execute("terminal.paste", &json!({ "tab": a, "text": "echo pasted-$((2*3))\n" })).unwrap();
    wait_text(&mut s, a, "pasted-6");

    // Copy the selection; Cmd+K clear.
    s.execute("terminal.activate", &json!({ "tab": a })).unwrap();
    s.execute("terminal.selectAll", &Value::Null).unwrap();
    let copied = s.execute("terminal.copy", &Value::Null).unwrap();
    assert!(copied["text"].as_str().unwrap().contains("hello-42"));
    assert!(s.effects.iter().any(|e| matches!(e, Effect::CopyToClipboard(t) if t.contains("pasted-6"))));
    s.execute("terminal.clear", &Value::Null).unwrap();
    let r = read(&mut s, a);
    assert!(!r["text"].as_str().unwrap().contains("hello-42"), "{}", r["text"]);
    assert_eq!(r["history"], 0);
    s.execute("terminal.resize", &json!({ "cols": 100, "rows": 30 })).unwrap();
    let r = read(&mut s, a);
    assert_eq!((r["cols"].as_u64(), r["rows"].as_u64()), (Some(100), Some(30)));

    // Exit status.
    s.execute("terminal.sendText", &json!({ "tab": a, "text": "exit 7\r" })).unwrap();
    let start = Instant::now();
    while read(&mut s, a)["alive"] == true {
        s.pump();
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(read(&mut s, a)["exitCode"], 7);
    assert!(s.execute("terminal.sendText", &json!({ "tab": a, "text": "x" })).is_err(), "the process has exited");

    // Closing the last tab hides the window; every process is gone.
    let pids: Vec<u32> = s.terminal.tabs.iter().filter_map(|t| t.term.pid()).collect();
    s.execute("terminal.close", &json!({ "tab": a })).unwrap();
    s.execute("terminal.close", &Value::Null).unwrap();
    assert!(s.terminal.tabs.is_empty());
    assert!(!s.ws.layout.is_visible(ToolWindowId::Terminal));
    assert!(terminal::wait_closed(&mut s, Duration::from_secs(10)));
    for pid in pids {
        wait_dead(pid);
    }
    s.shutdown();
}

#[test]
fn run_in_terminal_and_commands() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    let mut s = session(&root);
    let t = tab_id(
        &s.execute(
            "terminal.runInTerminal",
            &json!({ "command": "echo ran-$((3*3)) in $(basename $PWD)", "cwd": root.join("sub") }),
        )
        .unwrap(),
    );
    wait_text(&mut s, t, "ran-9 in sub");
    assert!(s.terminal.tabs[0].alive(), "the shell stays open after the command");
    // A program with args runs directly (no shell), and its tab is named after it.
    let p = tab_id(&s.execute("terminal.new", &json!({ "command": "/bin/echo", "args": ["direct", "args"] })).unwrap());
    wait_text(&mut s, p, "direct args");
    assert_eq!(s.terminal.tabs[1].title(), "echo");
    assert!(s.execute("terminal.new", &json!({ "args": ["x"] })).is_err());
    assert!(s.execute("terminal.new", &json!({ "cwd": root.join("missing") })).is_err());
    assert!(s.execute("terminal.runInTerminal", &json!({ "command": " " })).is_err());
    // Scrolling.
    s.execute(
        "terminal.sendText",
        &json!({ "tab": t, "text": "i=0; while [ $i -lt 60 ]; do echo row$i; i=$((i+1)); done\r" }),
    )
    .unwrap();
    wait_text(&mut s, t, "row59");
    s.execute("terminal.activate", &json!({ "tab": t })).unwrap();
    let top = s.execute("terminal.scroll", &json!({ "to": "top" })).unwrap();
    assert!(top["offset"].as_u64().unwrap() > 0);
    let back = s.execute("terminal.scroll", &json!({ "lines": -5 })).unwrap();
    assert_eq!(back["offset"].as_u64().unwrap(), top["offset"].as_u64().unwrap() - 5);
    assert_eq!(s.execute("terminal.scroll", &json!({ "offset": 0 })).unwrap()["offset"], 0);
    assert!(s.execute("terminal.scroll", &json!({ "to": "sideways" })).is_err());
    let meta = s.execute("terminal.toggleAltAsMeta", &Value::Null).unwrap();
    assert_eq!(meta["enabled"], true);
    assert!(s.execute("terminal.setShell", &json!({ "path": "/no/such/shell" })).is_err());
    s.execute("terminal.setShell", &json!({ "path": "/bin/sh" })).unwrap();

    // Shutdown kills every process.
    let pids: Vec<u32> = s.terminal.tabs.iter().filter_map(|t| t.term.pid()).collect();
    s.shutdown();
    for pid in pids {
        wait_dead(pid);
    }
}

#[test]
fn renamed_tabs_keep_their_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let mut s = session(&root);
    let a = tab_id(&s.execute("terminal.new", &Value::Null).unwrap());
    let title = |s: &mut Session| {
        s.pump();
        s.execute("terminal.list", &Value::Null).unwrap()[0]["title"].clone()
    };
    let set_osc = |s: &mut Session, osc: &str, marker: &str| {
        let text = format!("printf '\\033]0;{osc}\\007'; echo {marker}-$((1+1))\r");
        s.execute("terminal.sendText", &json!({ "tab": a, "text": text })).unwrap();
        wait_text(s, a, &format!("{marker}-2"));
    };

    // Unnamed: the program's title shows.
    set_osc(&mut s, "vim notes", "one");
    assert_eq!(title(&mut s), "vim notes");

    // Renamed: the name sticks when the program sets a title again.
    let r = s.execute("terminal.rename", &json!({ "tab": a, "title": "  Deploy staging  " })).unwrap();
    assert_eq!((r["title"].as_str(), r["renamed"].as_bool()), (Some("Deploy staging"), Some(true)));
    set_osc(&mut s, "zsh: ~/project", "two");
    assert_eq!(title(&mut s), "Deploy staging");

    // A blank name goes back to the automatic one, and the program's title shows again.
    let r = s.execute("terminal.rename", &json!({ "tab": a, "title": " " })).unwrap();
    assert_eq!(r["renamed"], false);
    assert_eq!(s.terminal.tabs[0].name, "Local");
    assert_eq!(title(&mut s), "zsh: ~/project");

    // A title given to `terminal.new` is the user's name too.
    let b = tab_id(&s.execute("terminal.new", &json!({ "title": "Migrations" })).unwrap());
    assert!(s.terminal.tabs[1].renamed);

    // No `title`: the UI edits the name in place.
    assert_eq!(s.terminal.rename_requested, None);
    s.execute("terminal.rename", &json!({ "tab": b })).unwrap();
    assert_eq!(s.terminal.rename_requested, Some(b));
    assert!(matches!(s.execute("terminal.rename", &json!({ "title": 3 })), Err(CommandError::BadParams(_))));
}

#[test]
fn tab_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = session(tmp.path());
    for _ in 0..terminal::MAX_TABS {
        s.execute("terminal.new", &json!({ "command": "/bin/sleep", "args": ["30"] })).unwrap();
    }
    let err = s.execute("terminal.new", &Value::Null).unwrap_err();
    assert!(err.to_string().contains("too many"), "{err}");
    s.shutdown();
}

#[test]
fn open_claude_code_connects_to_the_ide_server() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(&root).unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    // Never the real CLI: without an explicit program a headless session refuses.
    let err = s.execute("terminal.openClaude", &Value::Null).unwrap_err();
    assert!(err.to_string().contains("not started"), "{err}");

    s.ide.set_lock_dir(Some(tmp.path().join("locks")));
    s.execute("file.open", &json!({ "path": root })).unwrap();
    let start = Instant::now();
    while !s.ide.is_running() {
        s.pump();
        assert!(start.elapsed() < Duration::from_secs(20), "the IDE server did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let fake = script(
        tmp.path(),
        "claude",
        "echo \"fake-claude port=$CLAUDE_CODE_SSE_PORT force=$FORCE_CODE_TERMINAL nested=${CLAUDECODE:-no} dir=$(basename $PWD)\"; sleep 30",
    );
    s.terminal.claude_program = Some(fake.clone());
    let r = s.execute("terminal.openClaude", &Value::Null).unwrap();
    let port = s.ide.port().unwrap();
    assert_eq!(r["ideServer"], port);
    let tab = tab_id(&r);
    let out = wait_text(&mut s, tab, "fake-claude");
    assert!(
        out["text"].as_str().unwrap().contains(&format!("port={port} force=true nested=no dir=proj")),
        "{}",
        out["text"]
    );
    assert_eq!(out["title"], "Claude Code");
    s.terminal.claude_program = Some(tmp.path().join("missing"));
    assert!(s.execute("terminal.openClaude", &Value::Null).is_err());
    let pid = s.terminal.tabs[0].term.pid().unwrap();
    s.shutdown();
    wait_dead(pid);
}

#[test]
fn links_resolve_against_the_tab_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let mut s = session(&root);
    s.execute("terminal.new", &Value::Null).unwrap();
    assert_eq!(s.terminal.resolve_path(0, "src/main.rs", None), Some(root.join("src/main.rs")));
    assert_eq!(s.terminal.resolve_path(0, "./src/../src/main.rs", None), Some(root.join("src/main.rs")));
    assert_eq!(s.terminal.resolve_path(0, "src/nope.rs", None), None);
    assert_eq!(s.terminal.resolve_path(9, "main.rs", Some(&root.join("src"))), Some(root.join("src/main.rs")));
    s.shutdown();
}
