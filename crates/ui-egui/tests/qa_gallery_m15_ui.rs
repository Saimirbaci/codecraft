//! M15 QA (track QB): the gallery's second half. What `qa_gallery_ui.rs` leaves out because it
//! starts processes or needs a repository — the Terminal, the Python Console, the Rust REPL, a
//! real Git repository (log, commit with changelists, shelf, the merge editor on a real
//! conflict), the remote and Docker windows (fake `ssh` and `docker`) — and the M15 additions:
//! analysis underlines and the Alt+Enter popup, Settings › Python, two debug sessions' tabs, the
//! profiler's time-range selection and Memory view, Connect an External Agent. Painted at
//! 1280×800 and 900×700 in the dark and light themes; PNGs only with `CODECRAFT_QA_RENDER_DIR`
//! (never committed). Every part skips cleanly when its tool is missing (git, python3, debugpy,
//! ruff; the Rust REPL uses a stand-in when evcxr is missing).
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use egui::{Key, Modifiers};
#[path = "support/glyphs.rs"]
mod glyphs;

use egui_kittest::Harness;
use serde_json::{Value, json};

use codecraft_engine::debug::DebugStatus;
use codecraft_engine::inspections::Tool;
use codecraft_engine::services::ServicesSection;
use codecraft_engine::vcs::{CommitTab, GitTab};
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

const SIZES: [(f32, f32); 2] = [(1280.0, 800.0), (900.0, 700.0)];
const THEMES: [&str; 2] = ["Dark", "Light"];
const CMD: Modifiers = Modifiers { alt: false, ctrl: false, shift: false, mac_cmd: true, command: true };
const ALT: Modifiers = Modifiers { alt: true, ctrl: false, shift: false, mac_cmd: false, command: false };

fn render_dir() -> Option<PathBuf> {
    std::env::var_os("CODECRAFT_QA_RENDER_DIR").map(PathBuf::from)
}

fn settle(h: &mut Harness<'_, App>, what: &str, secs: u64, mut done: impl FnMut(&App) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        h.run_steps(1);
        if done(h.state()) {
            h.run_steps(2);
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("gallery m15: gave up waiting for {what}: {}", h.state().session.status);
    false
}

/// Paint the current state in every size and theme (PNGs only with `CODECRAFT_QA_RENDER_DIR`).
fn shoot(h: &mut Harness<'_, App>, name: &str) {
    let dir = render_dir();
    for (w, ht) in SIZES {
        for theme in THEMES {
            h.state_mut().session.ws.settings.theme = theme.to_string();
            h.set_size(egui::vec2(w, ht));
            h.run_steps(4);
            if w == SIZES[0].0 && theme == THEMES[0] {
                for (text, missing) in glyphs::missing_glyphs(h) {
                    eprintln!("tofu in {name}: {missing:?} in {text:?}");
                }
            }
            if let Some(dir) = &dir {
                let _ = std::fs::create_dir_all(dir);
                let file = dir.join(format!("{name}-{}x{}-{}.png", w as u32, ht as u32, theme.to_lowercase()));
                h.render().unwrap().save(file).unwrap();
            }
        }
    }
    h.state_mut().session.ws.settings.theme = "Dark".into();
    h.set_size(egui::vec2(SIZES[0].0, SIZES[0].1));
    h.run_steps(2);
}

fn has(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success())
}

fn which(name: &str) -> Option<PathBuf> {
    let out = Command::new("/usr/bin/which").arg(name).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim())).filter(|p| p.is_file())
}

fn script(path: &Path, body: &str) -> PathBuf {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_path_buf()
}

fn write(root: &Path, files: &[(&str, &str)]) {
    for (rel, text) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

fn harness(root: &Path, home: &Path, setup: impl FnOnce(&mut App) + Send + 'static) -> Harness<'static, App> {
    let (root, home) = (root.to_path_buf(), home.to_path_buf());
    let mut h = Harness::builder().with_size(egui::vec2(SIZES[0].0, SIZES[0].1)).build_eframe(move |_| {
        let mut app = App::ephemeral(Some(root));
        app.session.isolate_home(&home);
        app.session.ws.settings.keymap = "macOS".into();
        // A plain shell without the user's startup files; never the user's Docker.
        app.session.terminal.shell = Some("/bin/sh".into());
        app.session.docker.program = home.join("no-docker").display().to_string();
        app.session.docker.config_dir = Some(home.join("docker-config"));
        setup(&mut app);
        app
    });
    settle(&mut h, "the project", 60, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h
}

fn exec(h: &mut Harness<'_, App>, id: &str, params: Value) -> Option<Value> {
    match h.state_mut().session.execute(id, &params) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("gallery m15: {id} failed: {e}");
            None
        }
    }
}

fn only(h: &mut Harness<'_, App>, tw: ToolWindowId) {
    for other in ToolWindowId::ALL {
        h.state_mut().session.ws.layout.hide(other);
    }
    h.state_mut().session.ws.layout.show(tw);
    h.run_steps(2);
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    (dir, base)
}

// ---------------------------------------------------------------- consoles

const APP_PY: &str = "import os
import json


def load(path):
    with open(path) as f:
        return json.load(f)


def total(items):
    return sum(i[\"price\"] * i[\"qty\"] for i in items)
";

/// The Terminal, the Python Console and the Rust REPL.
#[test]
fn gallery_consoles() {
    let (_dir, base) = tmp();
    let root = base.join("shop");
    write(&root, &[("app.py", APP_PY), ("README.md", "# shop\n"), ("src/main.rs", "fn main() {}\n")]);
    let evcxr = which("evcxr").unwrap_or_else(|| {
        // A stand-in that answers like evcxr (its prompt and the value of an expression).
        script(
            &base.join("bin/evcxr"),
            "#!/bin/sh\nprintf 'Welcome to evcxr. For help, type :help\\n>> '\nwhile IFS= read -r line; do case \"$line\" in *+*) printf '42\\n\\302\\221>> ';; *) printf '\\302\\221>> ';; esac; done\n",
        )
    });
    let mut h = harness(&root, &base.join("home"), move |app| app.session.rust.repl.program = Some(evcxr));
    exec(&mut h, "file.open", json!({ "path": root.join("app.py") }));
    h.run_steps(3);

    // Terminal: a couple of commands and their output.
    exec(&mut h, "terminal.new", json!({}));
    settle(&mut h, "the shell", 30, |a| a.session.terminal.active_tab().is_some_and(|t| t.screen.text().contains('$')));
    for line in
        ["echo \"hello from $(basename \"$PWD\")\"", "ls -la", "printf '\\033[31mred\\033[0m \\033[1mbold\\033[0m\\n'"]
    {
        h.event(egui::Event::Text(line.into()));
        h.run_steps(1);
        h.key_press(Key::Enter);
        h.run_steps(2);
    }
    settle(&mut h, "the output", 30, |a| {
        a.session.terminal.active_tab().is_some_and(|t| t.screen.text().contains("bold"))
    });
    let first = h.state().session.terminal.active_tab().map(|t| t.id);
    exec(&mut h, "terminal.new", json!({}));
    settle(&mut h, "the second tab", 30, |a| a.session.terminal.tabs.len() == 2);
    if let Some(id) = first {
        exec(&mut h, "terminal.activate", json!({ "tab": id }));
    }
    only(&mut h, ToolWindowId::Terminal);
    shoot(&mut h, "11-terminal");

    // Python Console: a result, an error and the Variables view.
    if has("python3", &["--version"]) {
        h.state_mut().session.ws.layout.remember_size(ToolWindowId::PythonConsole, 360.0);
        exec(&mut h, "tools.pythonOrDebugConsole", json!({}));
        if settle(&mut h, "the console", 60, |a| a.session.python.console.ready) {
            for code in [
                "items = [{'price': 3, 'qty': 2}, {'price': 5, 'qty': 1}]",
                "sum(i['price'] * i['qty'] for i in items)",
                "names = ['apple', 'pear', 'plum']\nprices = {n: len(n) for n in names}",
                "1 / 0",
            ] {
                exec(&mut h, "python.console.execute", json!({ "code": code }));
                settle(&mut h, "the result", 30, |a| a.session.python.console.running.is_none());
            }
            only(&mut h, ToolWindowId::PythonConsole);
            shoot(&mut h, "12-python-console");
        }
    }

    // Rust REPL.
    h.state_mut().session.ws.layout.remember_size(ToolWindowId::RustRepl, 320.0);
    exec(&mut h, "tools.rust.rustRepl", json!({}));
    if settle(&mut h, "the REPL", 120, |a| a.session.rust.repl.ready) {
        exec(&mut h, "rust.repl.execute", json!({ "code": "40 + 2" }));
        settle(&mut h, "the REPL's answer", 120, |a| a.session.rust.repl.last_status.is_some());
        only(&mut h, ToolWindowId::RustRepl);
        shoot(&mut h, "13-rust-repl");
    }
    h.state_mut().session.shutdown();
}

// ---------------------------------------------------------------- Git

fn git_env(home: &Path) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), home.display().to_string()),
        ("GIT_CONFIG_GLOBAL".into(), home.join(".gitconfig").display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_ALLOW_PROTOCOL".into(), "file".into()),
    ]
}

fn git(dir: &Path, home: &Path, args: &[&str]) {
    let out = Command::new("git").args(args).current_dir(dir).envs(git_env(home)).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A repository with a few commits on two branches, a conflict-ready `feature` branch and
/// uncommitted changes in two files plus an unversioned one.
fn repo(base: &Path) -> (PathBuf, PathBuf) {
    let home = base.join("githome");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = Ada Lovelace\n\temail = ada@example.com\n[commit]\n\tgpgsign = false\n",
    )
    .unwrap();
    let root = base.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &home, &["init", "-q", "-b", "main"]);
    write(
        &root,
        &[
            ("Cargo.toml", "[package]\nname = \"repo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
            ("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n"),
        ],
    );
    git(&root, &home, &["add", "-A"]);
    git(&root, &home, &["commit", "-qm", "Initial commit"]);
    write(&root, &[("src/lib.rs", "pub fn price(n: u32) -> u32 {\n    n * 2\n}\n")]);
    git(&root, &home, &["add", "-A"]);
    git(&root, &home, &["commit", "-qm", "Add the price function"]);
    git(&root, &home, &["checkout", "-qb", "feature"]);
    write(&root, &[("src/lib.rs", "pub fn price(n: u32) -> u32 {\n    n * 3 // feature pricing\n}\n")]);
    git(&root, &home, &["commit", "-qam", "Feature: triple the price"]);
    git(&root, &home, &["checkout", "-q", "main"]);
    write(&root, &[("src/lib.rs", "pub fn price(n: u32) -> u32 {\n    n * 2 + 1 // handling fee\n}\n")]);
    git(&root, &home, &["commit", "-qam", "Add a handling fee"]);
    git(&root, &home, &["tag", "v0.1.0"]);
    (root, home)
}

/// The Git tool window's log, the Commit tool window with two changelists, the shelf, and the
/// merge editor on a real conflict.
#[test]
fn gallery_git() {
    if !has("git", &["--version"]) {
        eprintln!("skipping: git is missing");
        return;
    }
    let (_dir, base) = tmp();
    let (root, home) = repo(&base);
    let genv = git_env(&home);
    let mut h = harness(&root, &base.join("home"), move |app| app.session.vcs.set_git_env(genv));
    settle(&mut h, "the repository", 60, |a| {
        a.session.vcs.repo.is_some() && a.session.vcs.status.branch.head.is_some()
    });

    // The log with a commit selected.
    exec(&mut h, "git.showGitLog", json!({}));
    settle(&mut h, "the log", 60, |a| a.session.vcs.log.loaded && !a.session.vcs.log.loading);
    if let Some(hash) = h.state().session.vcs.log.commits.get(1).map(|c| c.hash.clone()) {
        exec(&mut h, "vcs.selectCommit", json!({ "hash": hash }));
        settle(&mut h, "the details", 30, |a| {
            a.session.vcs.log.details.is_some() && !a.session.vcs.log.details_loading
        });
    }
    only(&mut h, ToolWindowId::Git);
    h.state_mut().session.vcs.git_tab = GitTab::Log;
    shoot(&mut h, "14-git-log");

    // Uncommitted changes in two changelists, one file selected for its diff preview.
    write(
        &root,
        &[
            ("src/main.rs", "fn main() {\n    println!(\"hello, {}\", repo::price(2));\n}\n"),
            ("Cargo.toml", "[package]\nname = \"repo\"\nversion = \"0.2.0\"\nedition = \"2021\"\n"),
            ("notes.md", "todo\n"),
        ],
    );
    exec(&mut h, "vcs.refresh", json!({}));
    settle(&mut h, "the changes", 30, |a| a.session.vcs.status.files.len() == 3);
    exec(
        &mut h,
        "vcs.changelist.new",
        json!({ "name": "Release", "comment": "version bump", "files": ["Cargo.toml"] }),
    );
    exec(&mut h, "git.commit", json!({}));
    exec(&mut h, "vcs.setCommitMessage", json!({ "message": "Print the price\n\nUses the library's price()." }));
    exec(&mut h, "vcs.selectChange", json!({ "path": "src/main.rs" }));
    settle(&mut h, "the preview", 30, |a| a.session.vcs.preview.as_ref().is_some_and(|p| !p.loading));
    only(&mut h, ToolWindowId::Commit);
    shoot(&mut h, "15-commit-changelists");

    // Shelve the Release list; the Shelf tab.
    exec(&mut h, "vcs.shelve", json!({ "name": "Version bump", "files": ["Cargo.toml"] }));
    settle(&mut h, "the shelf", 30, |a| a.session.vcs.op.is_none() && !a.session.vcs.shelves.is_empty());
    h.state_mut().session.vcs.commit_tab = CommitTab::Shelf;
    shoot(&mut h, "15-commit-shelf");
    h.state_mut().session.vcs.commit_tab = CommitTab::Changes;

    // Commit the rest, then merge `feature`: a conflict in src/lib.rs.
    let _ = Command::new("git").args(["add", "-A"]).current_dir(&root).envs(git_env(&home)).output();
    let _ =
        Command::new("git").args(["commit", "-qm", "Print the price"]).current_dir(&root).envs(git_env(&home)).output();
    let _ = Command::new("git").args(["merge", "feature"]).current_dir(&root).envs(git_env(&home)).output();
    exec(&mut h, "vcs.refresh", json!({ "full": true }));
    if settle(&mut h, "the conflict", 30, |a| !a.session.vcs.conflicts().is_empty()) {
        only(&mut h, ToolWindowId::Commit);
        shoot(&mut h, "16-commit-conflict");
        exec(&mut h, "vcs.mergeView", json!({ "path": "src/lib.rs" }));
        if settle(&mut h, "the merge editor", 30, |a| a.session.vcs.merge.is_some()) {
            shoot(&mut h, "16-merge-editor");
        }
    }
    h.state_mut().session.shutdown();
}

// ---------------------------------------------------------------- remote and Docker

const DOCKER_PS: &str = r#"{"Command":"\"./server\"","ID":"c1a2b3","Image":"shop-web:1.4","Labels":"com.docker.compose.project=shop,com.docker.compose.service=web","Names":"shop-web-1","Ports":"0.0.0.0:8080->80/tcp","State":"running","Status":"Up 3 hours","CreatedAt":"2026-10-15 09:12:00 +0000 UTC"}
{"Command":"\"docker-entrypoint.s…\"","ID":"d4e5f6","Image":"postgres:16","Labels":"com.docker.compose.project=shop,com.docker.compose.service=db","Names":"shop-db-1","Ports":"5432/tcp","State":"running","Status":"Up 3 hours (healthy)"}
{"Command":"\"sh\"","ID":"a9b8c7","Image":"alpine:3.20","Labels":"","Names":"scratch","State":"exited","Status":"Exited (0) 2 days ago"}"#;
const DOCKER_IMAGES: &str = r#"{"ID":"i1","Repository":"shop-web","Tag":"1.4","Size":"84.2MB","CreatedSince":"3 hours ago"}
{"ID":"i2","Repository":"postgres","Tag":"16","Size":"431MB","CreatedSince":"2 weeks ago"}"#;

fn fake_docker(dir: &Path) -> PathBuf {
    script(
        &dir.join("docker"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"--context\" ]; then shift 2; fi\ncase \"$1\" in\n\
             context) printf '%s\\n' '{{\"Current\":true,\"Description\":\"Docker Desktop\",\"DockerEndpoint\":\"unix:///var/run/docker.sock\",\"Error\":\"\",\"Name\":\"desktop-linux\"}}' ;;\n\
             ps) if [ \"$3\" = \"--filter\" ]; then :; else printf '%s\\n' '{DOCKER_PS}'; fi ;;\n\
             images) printf '%s\\n' '{DOCKER_IMAGES}' ;;\n\
             volume) printf '%s\\n' '{{\"Driver\":\"local\",\"Name\":\"shop_pgdata\"}}' ;;\n\
             network) printf '%s\\n' '{{\"Driver\":\"bridge\",\"ID\":\"n1\",\"Name\":\"shop_default\",\"Scope\":\"local\"}}' ;;\n\
             logs) printf '2026-10-15 listening on :80\\nGET /items 200 3ms\\n'; exec sleep 30 ;;\n\
             inspect) echo '[{{\"Id\":\"c1a2b3\",\"State\":{{\"Running\":true}}}}]' ;;\n\
             *) echo \"$@\" ;;\nesac\n"
        ),
    )
}

/// Remote Host, File Transfer and Deployment › Configuration with a fake `ssh`; Services ›
/// Docker with a fake `docker`.
#[test]
fn gallery_remote_and_docker() {
    let (_dir, base) = tmp();
    let (remote, project, bin) = (base.join("remote"), base.join("project"), base.join("bin"));
    write(&project, &[("index.html", "<p>hi</p>\n"), ("app/main.py", "print('hi')\n")]);
    write(
        &remote,
        &[("www/index.html", "<p>old</p>\n"), ("www/assets/site.css", "body{}\n"), ("www/logs/access.log", "GET /\n")],
    );
    script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in --) shift; break;; -o|-p|-l|-i|-L|-O) shift 2;; *) shift;; esac; done\n\
             shift\ncd '{r}' || exit 1\nHOME='{r}' exec /bin/sh -c \"$*\"\n",
            r = remote.display()
        ),
    );
    let docker = fake_docker(&bin);
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("ssh_config"),
        "Host web\n  HostName web.example\n  User deploy\nHost db\n  HostName 10.0.0.5\n",
    )
    .unwrap();
    let ssh = bin.join("ssh").display().to_string();
    let (no_rsync, no_scp) = (bin.join("no-rsync").display().to_string(), bin.join("no-scp").display().to_string());
    let mut h = harness(&project, &home, move |app| {
        let s = &mut app.session;
        s.lsp_enabled = false;
        s.remote.share_connections = false;
        s.remote.programs.ssh = ssh;
        s.remote.programs.rsync = no_rsync;
        s.remote.programs.scp = no_scp;
    });

    // Deployment › Configuration with the hosts from the (isolated) ssh config.
    exec(&mut h, "tools.deployment.configuration", json!({}));
    h.run_steps(3);
    if let Some(d) = h.state_mut().session.remote.dialog.as_mut() {
        d.mapping.remote = "~/www".into();
    }
    shoot(&mut h, "17-deployment-configuration");
    exec(&mut h, "remote.deployment.setMapping", json!({ "server": "web", "local": "", "remote": "~/www" }));
    exec(&mut h, "remote.dialog.close", json!({}));
    h.run_steps(3);

    // Remote Host, browsing ~/www.
    exec(&mut h, "remote.host.browse", json!({ "server": "web", "path": "~/www" }));
    settle(&mut h, "the listing", 30, |a| !a.session.remote.busy());
    only(&mut h, ToolWindowId::RemoteHost);
    shoot(&mut h, "17-remote-host");

    // File Transfer after an upload.
    exec(&mut h, "tools.deployment.uploadTo", json!({ "server": "web", "path": "index.html" }));
    exec(&mut h, "tools.deployment.uploadToDefaultServer", json!({ "path": "app/main.py" }));
    settle(&mut h, "the uploads", 30, |a| !a.session.remote.busy());
    only(&mut h, ToolWindowId::FileTransfer);
    shoot(&mut h, "17-file-transfer");

    // Services › Docker.
    h.state_mut().session.docker.program = docker.display().to_string();
    h.state_mut().session.services.section = ServicesSection::Docker;
    exec(&mut h, "docker.refresh", json!({}));
    settle(&mut h, "the containers", 30, |a| a.session.docker.loaded && !a.session.docker.loading);
    only(&mut h, ToolWindowId::Services);
    shoot(&mut h, "18-services-docker");
    h.state_mut().session.shutdown();
}

// ---------------------------------------------------------------- M15

const LINT_PY: &str = "import os
import sys


def greet(name):
    message = \"hello \" + name
    return message


print(greet(sys.argv[0]))
";

/// Analysis underlines and the Alt+Enter popup (ruff on save), Settings › Python, two debug
/// sessions, the profiler's time range and Memory view, Connect an External Agent.
#[test]
fn gallery_m15() {
    let (_dir, base) = tmp();
    let root = base.join("m15");
    write(
        &root,
        &[
            ("lint.py", LINT_PY),
            (
                "server.py",
                "def handle(request):\n    reply = request.upper()\n    return reply\n\n\nprint('server', handle('ping'))\n",
            ),
            ("client.py", "def send(n):\n    total = n * 2\n    return total\n\n\nprint('client', send(21))\n"),
            (
                "grow.py",
                "def build(n):\n    return [bytes(2000) for _ in range(n)]\n\n\nkeep = build(4000)\nmore = build(8000)\ndel more\nprint(len(keep))\n",
            ),
        ],
    );
    let trace = root.join("trace.xml");
    std::fs::write(&trace, include_str!("../../profile/tests/fixtures/xctrace-time-profile.xml")).unwrap();
    let mut h = harness(&root, &base.join("home"), |app| {
        app.session.analysis.editor.on_save.debounce = Duration::ZERO;
    });
    let lint = root.join("lint.py");
    exec(&mut h, "file.open", json!({ "path": lint }));
    only(&mut h, ToolWindowId::Project);

    // Ruff on save: the underline, then Alt+Enter on it.
    if let Some(ruff) = which("ruff") {
        h.state_mut().session.inspections.programs.insert(Tool::Ruff, ruff);
        // A user's edit, then Cmd+S.
        let end = h.state().session.ws.active().map(|(_, d)| d.doc.len_chars()).unwrap_or(0);
        exec(&mut h, "editor.setSelection", json!({ "anchor": end }));
        exec(&mut h, "editor.type", json!({ "text": "print(greet('qa'))\n" }));
        h.run_steps(2);
        h.key_press_modifiers(CMD, Key::S);
        let ok = settle(&mut h, "ruff on save", 60, |a| {
            !a.session.analysis.editor.on_save.busy() && !a.session.analysis.editor.of(&lint).is_empty()
        });
        if ok {
            exec(&mut h, "editor.setSelection", json!({ "anchor": 8 }));
            exec(&mut h, "view.errorDescription", json!({}));
            shoot(&mut h, "19-analysis-underline");
            h.key_press_modifiers(ALT, Key::Enter);
            h.run_steps(3);
            shoot(&mut h, "19-analysis-alt-enter");
            h.key_press(Key::Escape);
            h.run_steps(2);
            h.state_mut().session.popup = None;
        }
    }

    // Settings › Languages & Frameworks › Python.
    exec(&mut h, "app.settings", json!({ "page": codecraft_engine::settings::PAGE_PYTHON }));
    h.run_steps(3);
    shoot(&mut h, "20-settings-python");
    let _ = h.state_mut().session.execute("settings.dialog.cancel", &Value::Null);
    h.run_steps(3);

    // Two debug sessions, both paused.
    if codecraft_engine::dap::discover::find_debugpy(None).is_ok() {
        for f in ["server.py", "client.py"] {
            exec(&mut h, "debug.breakpoint.set", json!({ "path": root.join(f), "line": 3 }));
        }
        exec(&mut h, "debug.start", json!({ "path": root.join("server.py") }));
        settle(&mut h, "the server's stop", 120, |a| a.session.debug.is_stopped() && !a.session.debug.settling());
        exec(&mut h, "debug.start", json!({ "path": root.join("client.py") }));
        let both = settle(&mut h, "both stops", 120, |a| {
            let d = &a.session.debug;
            d.sessions().iter().filter(|x| matches!(x.status, DebugStatus::Stopped { .. })).count() == 2
                && !d.settling()
        });
        if both {
            only(&mut h, ToolWindowId::Debug);
            shoot(&mut h, "21-debug-two-sessions");
        }
        exec(&mut h, "debug.stopAll", json!({}));
        settle(&mut h, "the sessions to end", 60, |a| !a.session.debug.busy());
    }

    // The profiler: a time range on the threads timeline, then the flame graph of that range.
    exec(&mut h, "run.openProfilerSnapshot", json!({ "path": trace }));
    settle(&mut h, "the trace", 60, |a| a.session.profiler.snapshot.is_some() && a.session.profiler.loading.is_none());
    let duration = h.state().session.profiler.snapshot.as_ref().and_then(|s| s.profile.duration_ns).unwrap_or(0);
    let ms = duration as f64 / 1e6;
    exec(&mut h, "profiler.selectRange", json!({ "startMs": ms * 0.25, "endMs": ms * 0.7 }));
    settle(&mut h, "the range", 60, |a| {
        a.session.profiler.loading.is_none()
            && a.session.profiler.snapshot.as_ref().is_some_and(|s| s.analysis.range.is_some())
    });
    exec(&mut h, "profiler.selectTab", json!({ "tab": "timeline" }));
    only(&mut h, ToolWindowId::Profiler);
    shoot(&mut h, "22-profiler-timeline-range");
    exec(&mut h, "profiler.selectTab", json!({ "tab": "methodList" }));
    shoot(&mut h, "22-profiler-methods-range");

    // Python memory over time (the real tracemalloc helper).
    if has("python3", &["--version"]) {
        exec(&mut h, "profiler.record", json!({ "command": "python3 grow.py", "kind": "memory", "interval": 0.01 }));
        let ok = settle(&mut h, "the memory profile", 120, |a| {
            !a.session.profiler.busy()
                && a.session.profiler.snapshot.as_ref().is_some_and(|s| s.profile.memory.is_some())
        });
        if ok {
            exec(&mut h, "profiler.selectTab", json!({ "tab": "memory" }));
            only(&mut h, ToolWindowId::Profiler);
            shoot(&mut h, "23-profiler-memory");
        }
    }

    // Connect an External Agent (Agents › ⇄).
    if let Some(v) = exec(&mut h, "agent.connectExternal", json!({})) {
        h.state_mut().ui.agent.connect = Some(v);
        only(&mut h, ToolWindowId::Project);
        shoot(&mut h, "24-connect-external-agent");
        h.state_mut().ui.agent.connect = None;
    }
    h.state_mut().session.shutdown();
}
