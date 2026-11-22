//! Terminal tabs of a remote (SSH) project across restarts: each tab's own command history kept
//! on the host, and the Claude Code session that ran in it resumed there. `ssh` is a fake that
//! runs the remote command locally with the "host's" own `HOME`; `claude` is a fake that runs
//! CodeCraft's session hook like Claude Code's SessionStart hook would.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::remote::{ConnectionState, RemoteProject};

const FAKE_CLAUDE: &str = r#"#!/bin/sh
echo "fake-claude $*"
settings=; prev=; resume=
for a in "$@"; do
  case "$prev" in --settings) settings=$a;; --resume) resume=$a;; esac
  prev=$a
done
id=${resume:-sess-remote-1}
if [ -n "$settings" ]; then
  cmd=$(sed -n 's/.*"command": *"\(.*\)".*/\1/p' "$settings")
  printf '{"session_id":"%s","cwd":"%s","source":"startup"}' "$id" "$PWD" | eval "$cmd"
fi
echo "fake-claude ready $id"
exec sleep 60
"#;

/// `ssh [options] -- dest '<command>'`: the command runs here with the host's `HOME`.
const FAKE_SSH: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break;;
    -o|-p|-l|-i|-L|-O|-P) shift 2;;
    *) shift;;
  esac
done
shift
HOME="$RHOME" SHELL=/bin/bash PATH="$RBIN:/usr/bin:/bin" BASH_SILENCE_DEPRECATION_WARNING=1 exec /bin/sh -c "$*"
"#;

struct Env {
    _tmp: tempfile::TempDir,
    project: PathBuf,
    rhome: PathBuf,
    iso: PathBuf,
    bin: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().canonicalize().unwrap();
    let (project, rhome, iso, bin) = (dir.join("proj"), dir.join("rhome"), dir.join("iso"), dir.join("bin"));
    for d in [&project, &rhome.join("app"), &iso, &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    let script = |name: &str, body: &str| {
        let text = body.replacen('\n', &format!("\nRHOME='{}'; RBIN='{}'\n", rhome.display(), bin.display()), 1);
        std::fs::write(bin.join(name), text).unwrap();
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    script("ssh", FAKE_SSH);
    script("claude", FAKE_CLAUDE);
    // The host's own startup file: history is written after every command.
    std::fs::write(rhome.join(".bash_profile"), "PS1='$ '\nPROMPT_COMMAND='history -a'\n").unwrap();
    std::fs::write(rhome.join(".bash_history"), "echo from-hosts-global-history\n").unwrap();
    std::fs::write(iso.join("ssh_config"), "Host web\n  HostName web.example\n  User deploy\n").unwrap();
    Env { _tmp: tmp, project, rhome, iso, bin }
}

fn session(e: &Env) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&e.iso);
    s.remote.share_connections = false;
    s.remote.auto_refresh = false;
    s.remote.programs.ssh = e.bin.join("ssh").display().to_string();
    s.terminal.remote_poll_every = Duration::ZERO;
    s.terminal.env = vec![("BASH_SILENCE_DEPRECATION_WARNING".into(), "1".into())];
    s.execute("file.open", &json!({ "path": e.project })).unwrap();
    let start = Instant::now();
    while s.jobs_running() && start.elapsed() < Duration::from_secs(30) {
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    s.execute("remote.deployment.setMapping", &json!({ "server": "web", "remote": "~/app" })).unwrap();
    s.remote.project = Some(RemoteProject {
        server: "web".into(),
        destination: "web".into(),
        remote: "~/app".into(),
        connection: ConnectionState::Connected,
        error: None,
        pending_uploads: Default::default(),
    });
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

fn wait_until(s: &mut Session, what: &str, cond: impl Fn(&mut Session) -> bool) {
    let start = Instant::now();
    while !cond(s) {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        s.pump();
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

fn key(s: &Session, id: u64) -> String {
    s.terminal.tabs.iter().find(|t| t.id == id).unwrap().key.clone()
}

fn host_files(e: &Env) -> PathBuf {
    e.rhome.join(".codecraft/terminals")
}

#[test]
fn remote_tabs_keep_their_history_on_the_host_and_resume_claude_there() {
    if !Path::new("/bin/bash").is_file() {
        eprintln!("skipped: no /bin/bash");
        return;
    }
    let e = env();
    let mut s = session(&e);

    // A tab where the user started Claude Code on the host.
    let work = tab_id(&s.execute("terminal.new", &json!({ "title": "Work" })).unwrap());
    assert!(s.terminal.tabs.iter().find(|t| t.id == work).unwrap().remote, "a shell on the host");
    let out = run(&mut s, work, "cd ~/app && claude --model fake", "fake-claude ready sess-remote-1");
    assert!(out.contains("--settings ") && out.contains("--model fake"), "the host's shim added the hook:\n{out}");
    // A plain tab with its own history, and one the user closes.
    let notes = tab_id(&s.execute("terminal.new", &json!({ "title": "Notes" })).unwrap());
    run(&mut s, notes, "echo note-$((1+2))", "note-3");
    run(&mut s, notes, "echo second-$((2+2))", "second-4");
    let gone = tab_id(&s.execute("terminal.new", &json!({ "title": "Gone" })).unwrap());
    run(&mut s, gone, "echo bye-$((3+3))", "bye-6");
    let (work_key, notes_key, gone_key) = (key(&s, work), key(&s, notes), key(&s, gone));

    // The files are on the host, one history per tab, without the host's global one.
    let files = host_files(&e);
    wait_until(&mut s, "the history files", |_| {
        std::fs::read_to_string(files.join(format!("{notes_key}.history"))).is_ok_and(|h| h.contains("echo second-"))
            && files.join(format!("{gone_key}.history")).is_file()
    });
    let history = std::fs::read_to_string(files.join(format!("{notes_key}.history"))).unwrap();
    assert!(!history.contains("claude") && !history.contains("from-hosts-global-history"), "{history}");
    assert!(std::fs::read_to_string(e.rhome.join(".bash_history")).unwrap().lines().count() == 1);

    // Closing a tab removes its files on the host.
    s.execute("terminal.close", &json!({ "tab": gone })).unwrap();
    wait_until(&mut s, "the closed tab's files to go", |_| !files.join(format!("{gone_key}.history")).exists());
    assert!(files.join(format!("{notes_key}.history")).is_file());

    // The poll reads the Claude Code session back: the snapshot keeps it, with the directory
    // it ran in, and says the tab is on the host.
    wait_until(&mut s, "the session record", |s| {
        codecraft_engine::terminal::snapshot(s).0.iter().any(|t| t.claude.is_some())
    });
    let snapshot = codecraft_engine::session_store::snapshot(&s);
    let (tabs, _) = codecraft_engine::terminal::snapshot(&s);
    assert_eq!(tabs.len(), 2, "{tabs:?}");
    assert!(tabs.iter().all(|t| t.remote));
    let claude = tabs.iter().find(|t| t.key.as_deref() == Some(work_key.as_str())).unwrap();
    assert_eq!(claude.claude.as_ref().map(|c| c.session.as_str()), Some("sess-remote-1"));
    assert_eq!(claude.claude.as_ref().and_then(|c| c.transcript.as_ref()), None);
    assert_eq!(claude.cwd, e.rhome.join("app"), "the directory Claude Code ran in on the host");
    assert!(tabs.iter().find(|t| t.key.as_deref() == Some(notes_key.as_str())).unwrap().claude.is_none());
    assert_eq!(snapshot.terminals, tabs);
    // The final save keeps the plain tab's output for the next start.
    codecraft_engine::session_store::save_now(&mut s).unwrap();
    s.shutdown();

    // Restart: the same host, the same tabs.
    let mut s = session(&e);
    codecraft_engine::session_store::restore(&mut s, &snapshot);
    let titles: Vec<String> = s.terminal.tabs.iter().map(|t| t.title().to_string()).collect();
    assert_eq!(titles.len(), 2, "{titles:?}");
    assert!(s.terminal.tabs.iter().all(|t| t.remote), "restored on the host, not locally");
    let (work, notes) = (s.terminal.tabs[0].id, s.terminal.tabs[1].id);
    // The tab resumes its session on the host (through the shim, so it is tracked again), in
    // the directory it ran in.
    let out = wait_text(&mut s, work, "fake-claude ready sess-remote-1");
    assert!(out.contains("--resume sess-remote-1") && out.contains("--settings "), "{out}");
    // The plain tab shows its earlier output and has its own history, then the host's.
    let out = wait_text(&mut s, notes, "restored from the previous session");
    assert!(out.contains("note-3") && out.contains("second-4"), "{out}");
    run(&mut s, notes, "echo \"hc=$(history | grep -v hc= | grep -c 'echo second-')\"", "hc=1");
    run(&mut s, notes, "echo \"gc=$(history | grep -v gc= | grep -c from-hosts-global-history)\"", "gc=1");
    run(&mut s, notes, "echo \"cc=$(history | grep -v cc= | grep -c 'claude --model')\"", "cc=0");
    s.shutdown();
}
