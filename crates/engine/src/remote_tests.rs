//! Engine tests for SSH and remote (track S) with fake `ssh`/`rsync`/`scp` scripts: they log their
//! argv and emulate the remote host in a temp folder (its "home"), so no real host, key or
//! `~/.ssh` is ever touched.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Session;

pub(crate) struct Fake {
    pub dir: tempfile::TempDir,
    pub remote: PathBuf,
    pub project: PathBuf,
    pub home: PathBuf,
    pub log: PathBuf,
}

const SSH: &str = r#"#!/bin/sh
printf 'ssh %s\n' "$*" >> "$LOG"
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break;;
    -o|-p|-l|-i|-L|-O|-P) shift 2;;
    *) shift;;
  esac
done
dest="$1"; shift
[ "$dest" = "unreachable" ] && { echo "ssh: connect to host unreachable port 22: Connection refused" >&2; exit 255; }
cd "$REMOTE" || exit 1
HOME="$REMOTE" exec /bin/sh -c "$*"
"#;

/// Copies like rsync (no exclusions); dry runs print a canned itemize file.
const RSYNC: &str = r#"#!/bin/sh
printf 'rsync %s\n' "$*" >> "$LOG"
dry=0; from=""
for a in "$@"; do
  case "$a" in
    --dry-run) dry=1;;
    --files-from=*) from="${a#--files-from=}";;
  esac
done
eval "src=\${$(($# - 1))}"; eval "dst=\${$#}"
case "$src" in /*) dir=up;; *) dir=down;; esac
if [ $dry = 1 ] && [ -f "$FAKE/itemize-$dir" ]; then cat "$FAKE/itemize-$dir"; exit 0; fi
map() { case "$1" in /*) echo "$1";; *) p="${1#*:}"; case "$p" in /*) echo "$p";; *) echo "$REMOTE/$p";; esac;; esac; }
src=$(map "$src"); dst=$(map "$dst")
if [ $dry = 1 ]; then diff -qr "$src" "$dst"; exit 0; fi
if [ -n "$from" ]; then
  while IFS= read -r f; do [ -z "$f" ] && continue; mkdir -p "$(dirname "$dst/$f")"; cp "$src/$f" "$dst/$f"; done < "$from"
  exit 0
fi
case "$src" in */) mkdir -p "$dst"; cp -R "$src". "$dst";; *) cp "$src" "$dst";; esac
"#;

const SCP: &str = r#"#!/bin/sh
printf 'scp %s\n' "$*" >> "$LOG"
"#;

fn script(path: &Path, body: &str, fake: &Path, remote: &Path, log: &Path) {
    let text = format!(
        "#!/bin/sh\nLOG='{}'; REMOTE='{}'; FAKE='{}'\nPATH=\"$FAKE/bin:/usr/bin:/bin:/usr/sbin:/sbin\"; export PATH\n{}",
        log.display(),
        remote.display(),
        fake.display(),
        body.trim_start_matches("#!/bin/sh\n")
    );
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

pub(crate) fn fake() -> (Fake, Session) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let remote = root.join("remote-home");
    let project = root.join("project");
    let home = root.join("cc-home");
    let bin = root.join("bin");
    let log = root.join("argv.log");
    for d in [&remote, &project, &home, &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    script(&bin.join("ssh"), SSH, &root, &remote, &log);
    script(&bin.join("rsync"), RSYNC, &root, &remote, &log);
    script(&bin.join("scp"), SCP, &root, &remote, &log);
    if let Some(python) = codecraft_lsp::which("python3") {
        let setsid = bin.join("setsid");
        std::fs::write(&setsid, format!("#!{}\nimport os, sys\nos.setsid()\nos.execvp(sys.argv[1],sys.argv[1:])\n", python.display())).unwrap();
        std::fs::set_permissions(setsid, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(
        root.join("ssh_config"),
        "Host web web2\n  HostName web.example\n  User deploy\nHost *.internal\n  User ops\n",
    )
    .unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&home);
    s.remote.home_override = Some(home.clone());
    s.remote.ssh_config_override = Some(root.join("ssh_config"));
    s.remote.share_connections = false;
    s.remote.programs = crate::remote::Programs {
        ssh: bin.join("ssh").display().to_string(),
        rsync: bin.join("rsync").display().to_string(),
        scp: bin.join("scp").display().to_string(),
    };
    (Fake { dir, remote, project, home, log }, s)
}

impl Fake {
    pub(crate) fn argv(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn open_project(&self, s: &mut Session) {
        s.open_project(&self.project);
        pump_until(s, |s| !s.jobs_running());
    }
}

pub(crate) fn pump_until(s: &mut Session, done: impl Fn(&Session) -> bool) {
    let start = Instant::now();
    loop {
        s.pump();
        if done(s) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn settle(s: &mut Session) {
    pump_until(s, |s| !s.remote.busy());
}

fn run(s: &mut Session, id: &str, p: Value) -> Value {
    s.execute(id, &p).unwrap_or_else(|e| panic!("{id}: {e}"))
}

fn map_root(f: &Fake, s: &mut Session) {
    f.open_project(s);
    run(s, "remote.deployment.setMapping", json!({ "server": "web", "remote": "~/app", "excluded": "target; *.log" }));
}

#[test]
fn servers_come_from_the_config_and_ssh_json() {
    let (f, mut s) = fake();
    let v = run(&mut s, "remote.servers.list", json!({}));
    let names: Vec<&str> = v["servers"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["web", "web2"]);
    run(&mut s, "remote.servers.add", json!({ "name": "box", "host": "10.0.0.2", "port": "2222", "user": "me" }));
    assert!(f.home.join("ssh.json").is_file());
    let v = run(&mut s, "remote.servers.list", json!({}));
    assert_eq!(v["servers"][0]["name"], "box");
    assert_eq!(v["servers"][0]["port"], 2222);
    assert!(s.execute("remote.servers.add", &json!({ "name": "x", "host": "-oProxyCommand=sh" })).is_err());
    assert!(s.execute("remote.servers.remove", &json!({ "name": "web" })).is_err());
    run(&mut s, "remote.servers.remove", json!({ "name": "box" }));
    assert_eq!(s.remote.server_names(), vec!["web", "web2"]);
}

#[test]
fn test_connection_reports_success_and_failure() {
    let (_f, mut s) = fake();
    run(&mut s, "remote.servers.test", json!({ "server": "web" }));
    settle(&mut s);
    assert_eq!(s.remote.tests.get("web"), Some(&crate::remote::TestStatus::Ok("Connected".into())));
    run(&mut s, "remote.servers.test", json!({ "name": "u", "host": "unreachable" }));
    settle(&mut s);
    match s.remote.tests.get("u") {
        Some(crate::remote::TestStatus::Failed(e)) => assert!(e.contains("Connection refused"), "{e}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn upload_download_and_compare_through_the_mapping() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.project.join("src")).unwrap();
    std::fs::write(f.project.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(f.project.join("it's here.txt"), "quoted\n").unwrap();
    map_root(&f, &mut s);
    // A file: ssh with the content on stdin.
    run(&mut s, "tools.deployment.uploadToDefaultServer", json!({ "path": "src/main.rs" }));
    run(&mut s, "tools.deployment.uploadTo", json!({ "server": "web", "path": "it's here.txt" }));
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("app/src/main.rs")).unwrap(), "fn main() {}\n");
    assert_eq!(std::fs::read_to_string(f.remote.join("app/it's here.txt")).unwrap(), "quoted\n");
    assert!(f.argv().contains("BatchMode=yes"));
    assert!(s.remote.log.iter().all(|e| e.status == crate::remote::TransferStatus::Done), "{:?}", s.remote.log);
    // The project: rsync with the exclusions.
    run(&mut s, "tools.deployment.uploadToDefaultServer", json!({ "path": "" }));
    settle(&mut s);
    let argv = f.argv();
    assert!(argv.contains("--exclude=target") && argv.contains("--exclude=*.log"), "{argv}");
    assert!(argv.contains(&format!("{}/ web:app/", f.project.display())), "{argv}");
    // Download: the remote copy replaces the local file.
    std::fs::write(f.remote.join("app/src/main.rs"), "fn main() { remote() }\n").unwrap();
    run(&mut s, "tools.deployment.downloadFromDefaultServer", json!({ "path": "src/main.rs" }));
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.project.join("src/main.rs")).unwrap(), "fn main() { remote() }\n");
    // Compare: a diff of local vs deployed.
    std::fs::write(f.remote.join("app/src/main.rs"), "changed remotely\n").unwrap();
    run(&mut s, "tools.deployment.compareLocalFileWithDeployedVersion", json!({ "path": "src/main.rs" }));
    settle(&mut s);
    let cmp = s.misc.compare.clone().expect("compare view");
    assert_eq!(cmp.right, "changed remotely\n");
    assert_eq!(cmp.left, "fn main() { remote() }\n");
    // Unmapped or excluded paths are errors, not uploads.
    std::fs::create_dir_all(f.project.join("target")).unwrap();
    std::fs::write(f.project.join("target/x"), "x").unwrap();
    assert!(s.execute("tools.deployment.uploadTo", &json!({ "server": "web", "path": "target/x" })).is_err());
    assert!(s.execute("tools.deployment.uploadTo", &json!({ "server": "web2", "path": "src/main.rs" })).is_err());
}

#[test]
fn choosers_open_without_a_server() {
    let (f, mut s) = fake();
    map_root(&f, &mut s);
    run(&mut s, "remote.deployment.setMapping", json!({ "server": "web2", "remote": "/srv/app" }));
    let v = run(&mut s, "tools.deployment.uploadTo", json!({}));
    assert_eq!(v["picker"], true);
    let picker = s.remote.picker.clone().unwrap();
    assert_eq!(picker.command, "tools.deployment.uploadTo");
    assert_eq!(picker.servers, vec!["web", "web2"]);
    let v = run(&mut s, "tools.startSshSession", json!({}));
    assert_eq!(v["picker"], true);
    run(&mut s, "remote.dialog.close", json!({ "which": "picker" }));
    assert!(s.remote.picker.is_none());
    // The default server stays the first one mapped.
    let cfg = run(&mut s, "remote.deployment.get", json!({}));
    assert_eq!(cfg["defaultServer"], "web");
}

#[test]
fn automatic_upload_on_save() {
    let (f, mut s) = fake();
    std::fs::write(f.project.join("a.txt"), "one\n").unwrap();
    map_root(&f, &mut s);
    assert!(s.execute("tools.deployment.automaticUpload", &json!({ "on": true })).is_ok());
    let path = f.project.join("a.txt");
    s.open_file(&path).unwrap();
    let id = s.ws.docs().next().map(|d| d.id).unwrap();
    let od = s.ws.doc_mut(id).unwrap();
    od.doc.replace_all("zero one\n", &codecraft_text::SelectionSet::caret(0)).unwrap();
    s.save_all().unwrap();
    settle(&mut s);
    let up = std::fs::read_to_string(f.remote.join("app/a.txt")).unwrap();
    assert!(up.starts_with("zero"), "{up}");
    let v = run(&mut s, "tools.deployment.automaticUpload", json!({}));
    assert_eq!(v["autoUpload"], false);
}

#[test]
fn sync_preview_then_apply_selected_items() {
    let (f, mut s) = fake();
    std::fs::write(f.project.join("new.rs"), "new\n").unwrap();
    std::fs::write(f.project.join("skip.rs"), "skip\n").unwrap();
    std::fs::create_dir_all(f.remote.join("app")).unwrap();
    std::fs::write(f.remote.join("app/theirs.rs"), "theirs\n").unwrap();
    let root = f.dir.path().canonicalize().unwrap();
    std::fs::write(
        root.join("itemize-up"),
        ">f+++++++++ new.rs\n>f+++++++++ skip.rs\n*deleting   gone.rs\n.d..t...... ./\n",
    )
    .unwrap();
    std::fs::write(root.join("itemize-down"), ">f+++++++++ theirs.rs\ncd+++++++++ dir/\n").unwrap();
    map_root(&f, &mut s);
    run(&mut s, "remote.deployment.setOptions", json!({ "deleteTargetItems": true }));
    run(&mut s, "tools.deployment.syncLocalSubtreeWithDeployed", json!({}));
    settle(&mut s);
    let argv = f.argv();
    assert!(argv.contains("--dry-run") && argv.contains("--update") && argv.contains("--delete"), "{argv}");
    let preview = s.remote.sync_preview.clone().expect("preview");
    let items: Vec<(&str, codecraft_remote::Direction)> =
        preview.items.iter().map(|i| (i.path.as_str(), i.direction)).collect();
    use codecraft_remote::Direction::{Download, Upload};
    assert_eq!(items, vec![("new.rs", Upload), ("skip.rs", Upload), ("gone.rs", Upload), ("theirs.rs", Download)]);
    std::fs::write(f.remote.join("app/gone.rs"), "x").unwrap();
    run(&mut s, "remote.sync.toggle", json!({ "index": 1 }));
    run(&mut s, "remote.sync.apply", json!({}));
    settle(&mut s);
    assert!(f.remote.join("app/new.rs").is_file());
    assert!(!f.remote.join("app/skip.rs").exists());
    assert!(!f.remote.join("app/gone.rs").exists());
    assert_eq!(std::fs::read_to_string(f.project.join("theirs.rs")).unwrap(), "theirs\n");
    assert!(f.argv().contains("--files-from="));
    assert!(s.remote.sync_preview.is_none());
}

#[test]
fn scp_is_the_fallback_without_rsync() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.project.join("d")).unwrap();
    std::fs::write(f.project.join("d/x"), "x").unwrap();
    s.remote.programs.rsync = f.dir.path().join("bin/no-rsync").display().to_string();
    map_root(&f, &mut s);
    run(&mut s, "tools.deployment.uploadToDefaultServer", json!({ "path": "d" }));
    settle(&mut s);
    let argv = f.argv();
    assert!(argv.contains("scp ") && argv.contains(&format!("{}/d/. web:app/d/", f.project.display())), "{argv}");
    assert!(s.remote.log.last().unwrap().summary.contains("scp"));
}

#[test]
fn remote_host_browse_open_edit_and_manage() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.remote.join("site/sub dir")).unwrap();
    std::fs::write(f.remote.join("site/index.html"), "<h1>hi</h1>\n").unwrap();
    run(&mut s, "tools.deployment.browseRemoteHost", json!({ "server": "web", "path": "~/site" }));
    settle(&mut s);
    let b = s.remote.browser.clone();
    assert_eq!(b.path, f.remote.join("site").display().to_string(), "pwd normalizes the path");
    let names: Vec<&str> = b.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["sub dir", "index.html"], "{:?}", b.error);
    // Open a file: downloaded to the cache and opened; saving uploads it back.
    run(&mut s, "remote.host.open", json!({ "name": "index.html" }));
    settle(&mut s);
    let local = s.ws.active().and_then(|(_, d)| d.path.clone()).expect("opened");
    assert!(local.starts_with(&f.home));
    std::fs::write(&local, "<h1>edited</h1>\n").unwrap();
    crate::remote::after_save(&mut s, std::slice::from_ref(&local));
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("site/index.html")).unwrap(), "<h1>edited</h1>\n");
    // New folder, rename, delete (asks first).
    run(&mut s, "remote.host.mkdir", json!({ "name": "new one" }));
    settle(&mut s);
    assert!(f.remote.join("site/new one").is_dir());
    run(&mut s, "remote.host.rename", json!({ "name": "new one", "newName": "renamed" }));
    settle(&mut s);
    assert!(f.remote.join("site/renamed").is_dir());
    let v = run(&mut s, "remote.host.delete", json!({ "name": "renamed" }));
    assert_eq!(v["confirm"], true);
    assert!(f.remote.join("site/renamed").is_dir());
    run(&mut s, "remote.host.delete", json!({ "name": "renamed", "confirm": true }));
    settle(&mut s);
    assert!(!f.remote.join("site/renamed").exists());
    assert!(s.remote.browser.entries.iter().all(|e| e.name != "renamed"));
    assert!(s.execute("remote.host.rename", &json!({ "name": "index.html", "newName": "../x" })).is_err());
    // Into a folder and back up.
    run(&mut s, "remote.host.open", json!({ "name": "sub dir" }));
    settle(&mut s);
    assert!(s.remote.browser.path.ends_with("sub dir"));
    run(&mut s, "remote.host.up", json!({}));
    settle(&mut s);
    assert!(s.remote.browser.path.ends_with("site"));
}

#[test]
fn ssh_session_opens_a_terminal_without_batch_mode() {
    let (f, mut s) = fake();
    let v = run(&mut s, "tools.startSshSession", json!({ "server": "web" }));
    assert!(v["tab"].is_u64());
    let start = Instant::now();
    while !f.argv().contains("-- web") && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let argv = f.argv();
    assert!(argv.contains("-- web"), "{argv}");
    assert!(!argv.contains("BatchMode"), "{argv}");
}

#[test]
fn remote_development_mirrors_and_opens_the_project() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.remote.join("proj/src")).unwrap();
    std::fs::write(f.remote.join("proj/Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
    std::fs::write(f.remote.join("proj/src/main.rs"), "fn main() {}\n").unwrap();
    let v = run(&mut s, "file.remoteDevelopment", json!({}));
    assert_eq!(v["dialog"], true);
    assert_eq!(s.remote.remote_dev.as_ref().map(|d| d.server.as_str()), Some("web"));
    assert!(s.execute("file.remoteDevelopment", &json!({ "server": "web", "path": "~" })).is_err());
    let opened = run(&mut s, "file.remoteDevelopment", json!({ "server": "web", "path": "~/proj" }));
    settle(&mut s);
    let root = PathBuf::from(opened["local"].as_str().unwrap());
    assert!(s.ws.project.is_none(), "opening remote must preserve the current project window");
    assert!(s.effects.iter().any(|e| matches!(e, crate::Effect::OpenProjectWindow(p) if p == &root)));
    s.open_project(&root);
    settle(&mut s);
    assert_eq!(s.remote.project.as_ref().unwrap().connection, crate::remote::ConnectionState::Connected);
    assert!(root.starts_with(f.home.join("remote")));
    assert_eq!(std::fs::read_to_string(root.join("src/main.rs")).unwrap(), "fn main() {}\n");
    let cfg = crate::deployment::load(&root).unwrap();
    assert!(cfg.auto_upload);
    assert_eq!(cfg.default_server.as_deref(), Some("web"));
    assert_eq!(cfg.server("web").and_then(|d| d.remote_for("src")).as_deref(), Some("~/proj/src"));
    let runs = std::fs::read_to_string(root.join(crate::run_config_store::FILE)).unwrap();
    assert!(runs.contains("\"ssh\"") && runs.contains("Run on web"), "{runs}");
    // Sync from Remote brings remote changes.
    std::fs::write(f.remote.join("proj/src/main.rs"), "fn main() { 2 }\n").unwrap();
    run(&mut s, "remote.syncFromRemote", json!({}));
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(root.join("src/main.rs")).unwrap(), "fn main() { 2 }\n");
}

#[test]
fn ssh_run_target_syncs_then_runs_in_the_mapped_folder() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.project.join("scripts")).unwrap();
    std::fs::write(f.project.join("run.sh"), "echo \"$MODE|$1|$(pwd)\" > \"$HOME/ran.txt\"\n").unwrap();
    map_root(&f, &mut s);
    let target = crate::ssh_target::SshTarget { server: "web".into(), ..Default::default() };
    let mut cmd = crate::runs::ProcessCommand::new("/bin/sh", &[], f.project.join("scripts"));
    cmd.args = vec![f.project.join("run.sh").display().to_string(), "it's".into()];
    cmd.env = vec![("MODE".into(), "a b".into())];
    let w = crate::ssh_target::wrap(&mut s, &target, cmd.clone()).unwrap();
    assert_eq!(w.program, "sh");
    let line = &w.args[1];
    assert!(line.contains("rsync") && line.contains("--exclude=target"), "{line}");
    assert!(line.contains("exec ") && line.contains("../run.sh"), "{line}");
    // Run the wrapped command for real against the fake host.
    let out = std::process::Command::new("sh").args(&w.args).current_dir(&w.cwd).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(f.remote.join("app/run.sh").is_file());
    let ran = std::fs::read_to_string(f.remote.join("ran.txt")).unwrap();
    assert_eq!(ran.trim(), format!("a b|it's|{}", f.remote.join("app/scripts").display()));
    // Without a mapping or a remote folder the target explains what to do.
    let other = crate::ssh_target::SshTarget { server: "web2".into(), ..Default::default() };
    assert!(crate::ssh_target::wrap(&mut s, &other, cmd.clone()).is_err());
    let explicit = crate::ssh_target::SshTarget {
        server: "web2".into(),
        remote_dir: "/srv/x".into(),
        sync: false,
        ..Default::default()
    };
    let w = crate::ssh_target::wrap(&mut s, &explicit, cmd).unwrap();
    assert!(w.args.last().unwrap().contains("cd /srv/x/scripts"), "{:?}", w.args);
    assert!(w.args.iter().any(|a| a.contains("--")));
}

#[test]
fn file_transfer_log_and_clear() {
    let (f, mut s) = fake();
    std::fs::write(f.project.join("a.txt"), "a").unwrap();
    map_root(&f, &mut s);
    run(&mut s, "remote.servers.add", json!({ "name": "down", "host": "unreachable" }));
    run(&mut s, "remote.deployment.setMapping", json!({ "server": "down", "remote": "~/x" }));
    run(&mut s, "tools.deployment.uploadToDefaultServer", json!({ "path": "a.txt" }));
    run(&mut s, "tools.deployment.uploadTo", json!({ "server": "down", "path": "a.txt" }));
    settle(&mut s);
    assert_eq!(s.remote.log.len(), 2);
    assert_eq!(s.remote.log[0].status, crate::remote::TransferStatus::Done);
    match &s.remote.log[1].status {
        crate::remote::TransferStatus::Failed(e) => assert!(e.contains("Connection refused"), "{e}"),
        other => panic!("{other:?}"),
    }
    assert!(s.execute("tools.deployment.uploadTo", &json!({ "server": "web", "path": "missing.txt" })).is_err());
    run(&mut s, "remote.fileTransfer.clear", json!({}));
    assert!(s.remote.log.is_empty());
}

#[test]
fn data_sources_can_use_an_ssh_tunnel() {
    let (f, mut s) = fake();
    f.open_project(&mut s);
    s.db.set_secret_store(std::sync::Arc::new(codecraft_db::MemoryStore::default()));
    run(&mut s, "db.addDataSource", json!({ "kind": "postgres", "name": "PG", "host": "db.internal" }));
    let id = s.db.sources.first().map(|d| d.config.id.clone()).unwrap();
    assert!(s.execute("remote.dataSource.setTunnel", &json!({ "dataSource": id, "server": "nope" })).is_err());
    run(&mut s, "remote.dataSource.setTunnel", json!({ "dataSource": id, "server": "web" }));
    assert_eq!(s.db.source(&id).and_then(|d| d.config.ssh_tunnel.clone()).as_deref(), Some("web"));
    let saved = std::fs::read_to_string(f.project.join(".codecraft/datasources.json")).unwrap();
    assert!(saved.contains("\"ssh_tunnel\": \"web\""), "{saved}");
    // The tunnel is planned from the same servers.
    s.pump();
    let plan = s.db.tunnels.plan(&s.db.source(&id).unwrap().config).unwrap().unwrap();
    assert!(plan.args(41000).unwrap().contains(&"127.0.0.1:41000:db.internal:5432".to_string()));
    run(&mut s, "remote.dataSource.setTunnel", json!({ "dataSource": id, "server": null }));
    assert_eq!(s.db.source(&id).and_then(|d| d.config.ssh_tunnel.clone()), None);
}

#[test]
fn remote_reopen_refreshes_incrementally_and_preserves_conflicts() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.remote.join("proj")).unwrap();
    std::fs::write(f.remote.join("proj/a"), "one").unwrap();
    let response = run(&mut s, "file.remoteDevelopment", json!({"server": "web", "path": "~/proj"}));
    settle(&mut s);
    let root = PathBuf::from(response["local"].as_str().unwrap());
    std::fs::write(root.join("a"), "local edit").unwrap();
    std::fs::write(f.remote.join("proj/a"), "remote edit").unwrap();
    std::fs::write(f.remote.join("proj/b"), "new remote file").unwrap();
    let again = run(&mut s, "file.remoteDevelopment", json!({"server": "web", "path": "~/proj"}));
    settle(&mut s);
    assert_eq!(again["local"], response["local"]);
    assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "local edit");
    assert_eq!(std::fs::read_to_string(root.join("b")).unwrap(), "new remote file");
    assert!(s.ws.notifications.iter().any(|n| n.title == "Remote sync conflicts"));
    let argv = f.argv();
    assert!(
        argv.contains("--checksum") && argv.contains("--max-size=52428800") && argv.contains("--filter=:- .gitignore"),
        "{argv}"
    );
}

#[test]
fn remote_project_disconnect_pauses_saves_and_reopen_restores_the_marker() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.remote.join("proj")).unwrap();
    std::fs::write(f.remote.join("proj/a"), "one").unwrap();
    let response = run(&mut s, "file.remoteDevelopment", json!({"server": "web", "path": "~/proj"}));
    settle(&mut s);
    let root = PathBuf::from(response["local"].as_str().unwrap());
    s.open_project(&root);
    settle(&mut s);
    assert_eq!(run(&mut s, "remote.project.status", json!({}))["project"]["destination"], "deploy@web.example");
    run(&mut s, "remote.project.disconnect", json!({}));
    std::fs::write(root.join("a"), "offline edit").unwrap();
    crate::remote::after_save(&mut s, &[root.join("a")]);
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/a")).unwrap(), "one");
    assert_eq!(s.remote.project.as_ref().unwrap().pending_uploads.len(), 1);
    assert!(s.execute("remote.syncFromRemote", &json!({})).is_err());
    run(&mut s, "remote.project.reconnect", json!({}));
    settle(&mut s);
    s.close_project();
    assert!(s.remote.project.is_none());
    s.open_project(&root);
    settle(&mut s);
    assert!(s.remote.project.is_some());
}

#[test]
fn remote_project_folder_picker_reuses_remote_host_listing() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.remote.join("projects/app")).unwrap();
    run(&mut s, "file.remoteDevelopment", json!({}));
    run(&mut s, "tools.deployment.browseRemoteHost", json!({"server": "web", "path": "~/projects"}));
    settle(&mut s);
    run(&mut s, "remote.host.open", json!({"name": "app"}));
    settle(&mut s);
    run(&mut s, "remote.project.chooseFolder", json!({}));
    assert_eq!(s.remote.remote_dev.as_ref().unwrap().path, f.remote.join("projects/app").display().to_string());
}

#[test]
fn remote_project_requires_rsync_instead_of_unbounded_scp_fallback() {
    let (f, mut s) = fake();
    s.remote.programs.rsync = f.dir.path().join("absent-rsync").display().to_string();
    let error = s.execute("file.remoteDevelopment", &json!({"server": "web", "path": "~/proj"})).unwrap_err();
    assert!(error.to_string().contains("require rsync"));
    assert!(!s.remote.busy());
}

#[test]
fn checked_uploads_preserve_remote_edits_and_resume_after_offline_save() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.remote.join("proj")).unwrap();
    std::fs::write(f.remote.join("proj/a"), "one").unwrap();
    let response = run(&mut s, "file.remoteDevelopment", json!({"server": "web", "path": "~/proj"}));
    settle(&mut s);
    let root = PathBuf::from(response["local"].as_str().unwrap());
    s.open_project(&root);
    settle(&mut s);
    std::fs::write(root.join("a"), "second").unwrap();
    crate::remote::after_save(&mut s, &[root.join("a")]);
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/a")).unwrap(), "second");
    // Advancing the baseline after a successful upload permits repeated saves.
    std::fs::write(root.join("a"), "third").unwrap();
    crate::remote::after_save(&mut s, &[root.join("a")]);
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/a")).unwrap(), "third");
    run(&mut s, "remote.project.disconnect", json!({}));
    std::fs::write(root.join("a"), "offline").unwrap();
    crate::remote::after_save(&mut s, &[root.join("a")]);
    // A restart also restores the pending queue from this mirror's own metadata.
    s.close_project();
    s.open_project(&root);
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/a")).unwrap(), "offline");
    std::fs::write(f.remote.join("proj/a"), "remote edit").unwrap();
    std::fs::write(root.join("a"), "local edit").unwrap();
    crate::remote::after_save(&mut s, &[root.join("a")]);
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/a")).unwrap(), "remote edit");
    assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "local edit");
    let project = s.remote.project.as_ref().unwrap();
    assert_eq!(project.connection, crate::remote::ConnectionState::Connected);
    assert_eq!(project.pending_uploads.len(), 1);
    assert!(project.error.as_ref().unwrap().contains("Remote file changed"));
    assert_eq!(s.remote.conflicts, vec![PathBuf::from("a")]);
    run(&mut s, "remote.project.resolve", json!({"path":"a", "action":"compare"}));
    assert_eq!(s.misc.compare.as_ref().unwrap().right, "remote edit");
    run(&mut s, "remote.project.resolve", json!({"path":"a", "action":"keepMine"})); settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/a")).unwrap(), "local edit");
    assert!(s.remote.project.as_ref().unwrap().pending_uploads.is_empty());
    assert!(s.remote.conflicts.is_empty());
    std::fs::write(f.remote.join("proj/a"), "next remote").unwrap();
    std::fs::write(root.join("a"), "another local edit").unwrap();
    crate::remote::after_save(&mut s, &[root.join("a")]); settle(&mut s);
    run(&mut s, "remote.project.resolve", json!({"path":"a", "action":"takeRemote"})); settle(&mut s);
    assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "next remote");
    let pending: Vec<String> = serde_json::from_str(&std::fs::read_to_string(root.join(".codecraft/remote-pending.json")).unwrap()).unwrap();
    assert!(pending.is_empty());
}

#[test]
fn mirror_worker_lock_keeps_unsaved_buffers_until_save_can_retry() {
    let (f, mut s) = fake();
    std::fs::create_dir_all(f.remote.join("proj")).unwrap();
    std::fs::write(f.remote.join("proj/a.txt"), "one").unwrap();
    let response = run(&mut s, "file.remoteDevelopment", json!({"server": "web", "path": "~/proj"}));
    settle(&mut s);
    let root = PathBuf::from(response["local"].as_str().unwrap());
    s.open_project(&root);
    settle(&mut s);
    s.open_file(&root.join("a.txt")).unwrap();
    let id = s.ws.active().unwrap().1.id;
    s.ws.doc_mut(id).unwrap().doc.replace_all("unsaved", &codecraft_text::SelectionSet::caret(0)).unwrap();
    let lock = crate::remote::mirror_lock(&root);
    let guard = lock.lock().unwrap();
    assert!(s.save_all().unwrap_err().to_string().contains("Remote transfer in progress"));
    assert!(s.ws.doc(id).unwrap().doc.is_modified());
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one");
    drop(guard);
    s.save_all().unwrap();
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/a.txt")).unwrap(), "unsaved");
}

#[test]
fn real_rsync_over_fake_transport_filters_and_refreshes_the_snapshot() {
    let Some(rsync) = codecraft_lsp::which("rsync") else { return };
    let Some(python) = codecraft_lsp::which("python3") else { return };
    let (f, mut s) = fake();
    let script = format!(
        "#!{}\nimport sys, subprocess\nargs=sys.argv[1:]\ni=args.index('-e'); del args[i:i+2]\nremote={}\nfor i in (-2,-1):\n if args[i].startswith('web:'): args[i]=remote+'/'+args[i][4:]\nsys.exit(subprocess.run([{}]+args).returncode)\n",
        python.display(),
        serde_json::to_string(&f.remote.display().to_string()).unwrap(),
        serde_json::to_string(&rsync.display().to_string()).unwrap()
    );
    let program = f.dir.path().join("local-rsync");
    std::fs::write(&program, script).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.remote.programs.rsync = program.display().to_string();
    let remote = f.remote.join("proj");
    for dir in ["src", "target", "node_modules", ".git/objects", "cache", "empty"] {
        std::fs::create_dir_all(remote.join(dir)).unwrap();
    }
    for file in ["src/main.txt", "target/a", "node_modules/a", ".git/objects/a", "cache/a", "ignored.tmp"] {
        std::fs::write(remote.join(file), "one").unwrap();
    }
    std::fs::write(remote.join(".gitignore"), "cache/\n*.tmp\n").unwrap();
    std::fs::File::create(remote.join("large")).unwrap().set_len(crate::remote::MAX_OPEN_BYTES + 1).unwrap();
    let response = run(&mut s, "file.remoteDevelopment", json!({"server": "web", "path": "~/proj"}));
    settle(&mut s);
    let root = PathBuf::from(response["local"].as_str().unwrap());
    assert_eq!(std::fs::read_to_string(root.join("src/main.txt")).unwrap(), "one");
    assert!(root.join("empty").is_dir());
    for excluded in ["target", "node_modules", ".git", "cache", "ignored.tmp", "large"] {
        assert!(!root.join(excluded).exists(), "{excluded} was not filtered");
    }
    s.open_project(&root);
    settle(&mut s);
    std::fs::write(remote.join("src/main.txt"), "two").unwrap();
    run(&mut s, "remote.syncFromRemote", json!({}));
    settle(&mut s);
    assert_eq!(std::fs::read_to_string(root.join("src/main.txt")).unwrap(), "two");
    std::fs::remove_file(remote.join("src/main.txt")).unwrap();
    run(&mut s, "remote.syncFromRemote", json!({}));
    settle(&mut s);
    assert!(!root.join("src/main.txt").exists());
}

fn open_mirror(f: &Fake, s: &mut Session) -> PathBuf {
    std::fs::create_dir_all(f.remote.join("proj")).unwrap();
    std::fs::write(f.remote.join("proj/a.txt"), "one").unwrap();
    let response = run(s, "file.remoteDevelopment", json!({"server":"web", "path":"~/proj"}));
    settle(s);
    let root = PathBuf::from(response["local"].as_str().unwrap());
    s.open_project(&root);
    settle(s);
    root
}

#[test]
fn auto_refresh_preserves_dirty_buffers_reloads_clean_files_and_resumes_after_reconnect() {
    let (f, mut s) = fake();
    let root = open_mirror(&f, &mut s);
    s.open_file(&root.join("a.txt")).unwrap();
    let id = s.ws.active().unwrap().1.id;
    run(&mut s, "editor.type", json!({"text":"unsaved"}));
    let unsaved = s.ws.doc(id).unwrap().doc.text();
    std::fs::write(f.remote.join("proj/a.txt"), "remote").unwrap();
    std::fs::write(f.remote.join("proj/b.txt"), "clean remote file").unwrap();
    s.remote.window_focused = false;
    s.remote.last_refresh = Instant::now() - Duration::from_secs(4);
    s.pump();
    assert!(!s.remote.busy());
    assert!(!root.join("b.txt").exists());
    s.remote.window_focused = true;
    s.remote.last_refresh = Instant::now() - Duration::from_secs(4);
    s.pump(); settle(&mut s);
    assert_eq!(s.ws.doc(id).unwrap().doc.text(), unsaved);
    assert!(s.ws.doc(id).unwrap().doc.is_modified());
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one");
    assert_eq!(s.remote.conflicts, vec![PathBuf::from("a.txt")]);
    assert!(root.join("b.txt").is_file());
    run(&mut s, "remote.project.resolve", json!({"path":"a.txt", "action":"compare"}));
    assert_eq!(s.misc.compare.as_ref().unwrap().right, "remote");
    run(&mut s, "remote.project.resolve", json!({"path":"a.txt", "action":"takeRemote"}));
    assert_eq!(s.ws.doc(id).unwrap().doc.text(), "remote");
    assert!(!s.ws.doc(id).unwrap().doc.is_modified());
    assert!(s.remote.conflicts.is_empty());
    run(&mut s, "remote.project.autoRefresh", json!({"paused":true}));
    assert!(crate::deployment::load(&root).unwrap().pause_remote_refresh);
    let count = f.argv().lines().count();
    s.remote.last_refresh = Instant::now() - Duration::from_secs(20);
    s.pump(); assert_eq!(f.argv().lines().count(), count);
    run(&mut s, "remote.project.autoRefresh", json!({"paused":false}));
    run(&mut s, "remote.project.disconnect", json!({}));
    s.remote.last_refresh = Instant::now() - Duration::from_secs(20);
    s.pump(); assert!(!s.remote.busy());
    run(&mut s, "remote.project.reconnect", json!({})); settle(&mut s);
    std::fs::write(f.remote.join("proj/a.txt"), "after reconnect").unwrap();
    s.remote.last_refresh = Instant::now() - Duration::from_secs(4);
    s.pump(); settle(&mut s);
    assert_eq!(s.ws.doc(id).unwrap().doc.text(), "after reconnect");
}

#[test]
fn remote_tree_create_rename_delete_and_changed_remote_refusal() {
    let (f, mut s) = fake();
    let root = open_mirror(&f, &mut s);
    let folder = root.join("new-folder");
    // Use the same engine hook used by File/New and resource edits.
    run(&mut s, "file.new.directory", json!({"path":folder})); settle(&mut s);
    assert!(f.remote.join("proj/new-folder").is_dir());
    let created = root.join("created");
    run(&mut s, "file.new.file", json!({"path":created})); settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/created")).unwrap(), "");
    let renamed = root.join("renamed");
    run(&mut s, "refactor.moveFile", json!({"path":created, "to":renamed})); settle(&mut s);
    assert!(!f.remote.join("proj/created").exists()); assert!(f.remote.join("proj/renamed").is_file());
    std::fs::write(f.remote.join("proj/renamed"), "someone else").unwrap();
    std::fs::remove_file(&renamed).unwrap();
    crate::remote::tree_operation(&mut s, &renamed, None, false, false).unwrap(); settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/renamed")).unwrap(), "someone else");
    assert!(!renamed.exists());
    assert!(s.remote.log.iter().any(|e| matches!(&e.status, crate::remote::TransferStatus::Failed(error) if error.contains("Remote tree changed"))));
    std::fs::remove_dir(&folder).unwrap();
    crate::remote::tree_operation(&mut s, &folder, None, true, false).unwrap(); settle(&mut s);
    assert!(!f.remote.join("proj/new-folder").exists());
}

#[test]
fn keep_mine_checks_the_reviewed_snapshot_and_merge_uses_the_three_way_view() {
    let (f, mut s) = fake();
    let root = open_mirror(&f, &mut s);
    std::fs::write(root.join("a.txt"), "mine").unwrap();
    std::fs::write(f.remote.join("proj/a.txt"), "theirs").unwrap();
    run(&mut s, "remote.syncFromRemote", json!({})); settle(&mut s);
    run(&mut s, "remote.project.resolve", json!({"path":"a.txt", "action":"merge"}));
    let merge = s.vcs.merge.as_ref().unwrap();
    assert_eq!((&*merge.base, &*merge.ours, &*merge.theirs), ("one", "mine", "theirs"));
    std::fs::write(f.remote.join("proj/a.txt"), "intermediate remote").unwrap();
    run(&mut s, "remote.syncFromRemote", json!({})); settle(&mut s);
    s.remote.conflicts.clear(); // A refresh or another resolution can clear the list while this view remains open.
    assert!(s.execute("vcs.merge.apply", &json!({"allowUnresolved":true})).unwrap_err().to_string().contains("snapshot changed"));
    s.remote.conflicts.push(PathBuf::from("a.txt"));
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "mine");
    s.vcs.merge = None;
    // A new remote edit after review must still refuse Keep Mine's upload.
    std::fs::write(f.remote.join("proj/a.txt"), "newer remote").unwrap();
    run(&mut s, "remote.project.resolve", json!({"path":"a.txt", "action":"keepMine"})); settle(&mut s);
    assert_eq!(std::fs::read_to_string(f.remote.join("proj/a.txt")).unwrap(), "newer remote");
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "mine");
    assert!(!s.remote.project.as_ref().unwrap().pending_uploads.is_empty());
    let (incoming, _) = crate::remote::mirror_paths(&root).unwrap();
    std::fs::create_dir_all(incoming.join("folder")).unwrap();
    std::fs::write(root.join("folder"), "preserve this file").unwrap();
    s.remote.conflicts.push(PathBuf::from("folder"));
    assert!(s.execute("remote.project.resolve", &json!({"path":"folder", "action":"takeRemote"})).unwrap_err().to_string().contains("Directory conflicts"));
    assert_eq!(std::fs::read_to_string(root.join("folder")).unwrap(), "preserve this file");
}

#[test]
fn remote_external_sources_are_fetched_and_opened_read_only_and_targets_default_to_ssh() {
    use codecraft_lsp::path_mapper::{Direction, RootMapper};
    let (f, mut s) = fake();
    let root = open_mirror(&f, &mut s);
    let outside = f.remote.join("toolchain/lib.rs");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap(); std::fs::write(&outside, "pub fn source() {}\n").unwrap();
    let cache = crate::remote::external_cache(&s).unwrap();
    let mapper = RootMapper::new(&root.to_string_lossy(), &f.remote.join("proj").to_string_lossy()).with_external_cache(&cache.to_string_lossy());
    let cached = PathBuf::from(mapper.map_path(&outside.to_string_lossy(), Direction::ToClient).unwrap());
    assert_eq!(crate::remote::map_output_path(&s, &outside), cached);
    assert_eq!(crate::remote::map_output_path(&s, &f.remote.join("proj/../toolchain/lib.rs")), cached);
    let printed = format!("{}:2:3: error", f.remote.join("proj/a.txt").display());
    assert_eq!(crate::remote::map_output_line(&s, &printed), format!("{}:2:3: error", root.join("a.txt").display()));
    let sibling = format!("{}-other/a.txt:2:3", f.remote.join("proj").display());
    assert_eq!(crate::remote::map_output_line(&s, &sibling), sibling);
    s.navigate_to_position(&cached, 0, 4).unwrap(); settle(&mut s);
    assert_eq!(std::fs::read_to_string(&cached).unwrap(), "pub fn source() {}\n");
    assert!(s.ws.active().unwrap().1.read_only);
    assert!(s.execute("editor.type", &json!({"text":"bad"})).is_err());
    let unicode = f.remote.join("toolchain/unicode.rs");
    std::fs::write(&unicode, "🦀x\n").unwrap();
    let cached_unicode = PathBuf::from(mapper.map_path(&unicode.to_string_lossy(), Direction::ToClient).unwrap());
    assert!(crate::remote::fetch_external_lsp(&mut s, &cached_unicode, 0, 2).unwrap()); settle(&mut s);
    assert_eq!(s.ws.active().unwrap().0.selections.primary().head, 1);
    let report = run(&mut s, "refactor.preview.open", json!({"apply":true,"edit":{"documentChanges":[{"kind":"delete", "uri":codecraft_lsp::path_to_uri(&cached_unicode).unwrap()}]}}));
    assert_eq!(report["applied"], 0);
    assert!(report["errors"][0].as_str().unwrap().contains("read-only"));
    assert!(cached_unicode.is_file());
    run(&mut s, "runConfig.create", json!({"name":"Remote default", "kind":"command", "command":"echo hello"}));
    assert!(matches!(s.run_store.user.first().unwrap().target, crate::run_targets::RunTarget::Ssh(_)));
    let target = crate::target_exec::effective(&s, &crate::run_targets::RunTarget::Local);
    assert!(matches!(target, crate::run_targets::RunTarget::Ssh(t) if t.server == "web" && !t.sync));
    for id in ["run.run", "run.testsInFile", "run.debug", "run.profile", "cargo.check", "cargo.clippy"] {
        assert!(matches!(crate::target_exec::scope(&mut s, id, &json!({})).unwrap(), Some(crate::run_targets::RunTarget::Ssh(_))));
        assert_eq!(crate::target_exec::scope(&mut s, id, &json!({"target":"local"})).unwrap(), Some(crate::run_targets::RunTarget::Local));
    }
}

#[test]
fn remote_terminal_defaults_to_project_directory_and_restores_a_shell_tab() {
    let (f, mut s) = fake();
    let _root = open_mirror(&f, &mut s);
    s.terminal.env.push(("HOME".into(), f.home.display().to_string()));
    run(&mut s, "terminal.newRemote", json!({}));
    let start = Instant::now();
    while !f.argv().contains("-tt") && start.elapsed() < Duration::from_secs(5) { s.pump(); std::thread::sleep(Duration::from_millis(10)); }
    let argv = f.argv();
    assert!(argv.contains("-tt") && argv.contains("/proj"), "{argv}");
    assert!(s.terminal.tabs.first().unwrap().command.is_none());
    let (saved, selected) = crate::terminal::snapshot(&s);
    assert_eq!(saved.len(), 1);
    crate::terminal::close_all(&mut s);
    assert_eq!(crate::terminal::restore(&mut s, &saved, selected), 1);
    assert!(s.terminal.tabs.first().unwrap().command.is_none());
    s.shutdown();
}

#[test]
fn stopping_remote_run_terminates_the_detached_remote_child_tree() {
    if codecraft_lsp::which("python3").is_none() { return; }
    for action in ["stop", "disconnect", "close", "quit"] {
    let (f, mut s) = fake();
    let root = open_mirror(&f, &mut s);
    let script = "sh -c 'echo $$ > child.pid; while :; do echo tick >> heartbeat; sleep 0.03; done' & wait";
    let cmd = crate::runs::ProcessCommand::new("sh", &["-c", script], &root);
    let target = crate::target_exec::effective(&s, &crate::run_targets::RunTarget::Local);
    let tab = crate::runs::start_on_target(&mut s, &cmd, "Remote child", &target).unwrap();
    let heartbeat = f.remote.join("proj/heartbeat");
    let deadline = Instant::now() + Duration::from_secs(8);
    while !heartbeat.is_file() { s.pump(); assert!(Instant::now() < deadline, "{}", f.argv()); std::thread::sleep(Duration::from_millis(10)); }
    match action {
        "stop" => { assert!(crate::runs::stop(&mut s, tab)); assert!(crate::runs::stop(&mut s, tab)); },
        "disconnect" => { run(&mut s, "remote.project.disconnect", json!({})); },
        "close" => s.close_project(),
        _ => { s.shutdown(); s.shutdown(); },
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    while s.runs.any_running() { s.pump(); assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(10)); }
    std::thread::sleep(Duration::from_millis(300));
    let bytes = std::fs::metadata(&heartbeat).unwrap().len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), bytes, "remote descendant is still writing after {action}");
    }
}

#[test]
fn inotify_stream_drives_refresh_without_repeated_transfers_and_pause_stops_it() {
    let Some(python) = codecraft_lsp::which("python3") else { return };
    let (f, mut s) = fake();
    let watcher = f.dir.path().join("bin/inotifywait");
    std::fs::write(&watcher, format!("#!{}\nimport os,time,sys\nwhile True:\n if os.path.exists('.event'):\n  os.remove('.event');print('changed',flush=True)\n time.sleep(.02)\n", python.display())).unwrap();
    std::fs::set_permissions(&watcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let root = open_mirror(&f, &mut s);
    pump_until(&mut s, |s| s.remote.auto_refresh_mode() == "inotify");
    s.remote.last_refresh = Instant::now() - Duration::from_secs(4); s.pump(); settle(&mut s);
    let count = f.argv().lines().filter(|l| l.starts_with("rsync ")).count();
    s.remote.last_refresh = Instant::now() - Duration::from_secs(4); s.pump(); settle(&mut s);
    assert_eq!(f.argv().lines().filter(|l| l.starts_with("rsync ")).count(), count);
    std::fs::write(f.remote.join("proj/a.txt"), "inotify update").unwrap();
    std::fs::write(f.remote.join("proj/.event"), "notify").unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        s.remote.last_refresh = Instant::now() - Duration::from_secs(4); s.pump();
        if std::fs::read_to_string(root.join("a.txt")).unwrap() == "inotify update" { break; }
        assert!(Instant::now() < deadline, "inotify event was not applied: {}", f.argv());
        std::thread::sleep(Duration::from_millis(20));
    }
    settle(&mut s);
    run(&mut s, "remote.project.autoRefresh", json!({"paused":true})); s.pump();
    assert_eq!(s.remote.auto_refresh_mode(), "paused");
}

#[test]
fn remote_language_servers_initialize_on_the_host_and_all_restart_on_reconnect() {
    use codecraft_lsp::ServerKind;
    let Some(python) = codecraft_lsp::which("python3") else { return };
    let (f, mut s) = fake();
    let mock = r#"
import sys,json,os
with open(os.path.join(os.environ['HOME'],'server-starts'),'a') as f: f.write(sys.argv[0]+'\n')
if sys.argv[0].endswith('clangd') and os.path.exists(os.path.join(os.environ['HOME'],'missing-clangd')):
 print('Install clangd on the remote host',file=sys.stderr);sys.exit(127)
def send(msg):
 b=json.dumps(msg).encode();sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(b)).encode()+b);sys.stdout.buffer.flush()
while True:
 headers={}
 while True:
  line=sys.stdin.buffer.readline()
  if not line: sys.exit(0)
  if line==b'\r\n': break
  k,v=line.decode().split(':',1);headers[k.lower()]=v.strip()
 msg=json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
 method=msg.get('method')
 if method=='initialize':
  with open(os.path.join(os.environ['HOME'],'initializations'),'a') as f: f.write(json.dumps(msg['params'])+'\n')
  send({'jsonrpc':'2.0','id':msg['id'],'result':{'capabilities':{'textDocumentSync':2}}})
 elif method=='shutdown': send({'jsonrpc':'2.0','id':msg['id'],'result':None})
 elif method=='exit': sys.exit(0)
"#;
    for program in ["rust-analyzer", "clangd", "basedpyright-langserver"] {
        let path = f.dir.path().join("bin").join(program);
        std::fs::write(&path, format!("#!{}\n{mock}", python.display())).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let root = open_mirror(&f, &mut s);
    for (name, text) in [("main.rs", "fn main() {}"), ("main.cpp", "int main() {}"), ("main.py", "x = 1")] {
        std::fs::write(root.join(name), text).unwrap();
        s.open_file(&root.join(name)).unwrap();
    }
    s.lsp_enabled = true;
    run(&mut s, "remote.project.reconnect", json!({}));
    let kinds = [ServerKind::RustAnalyzer, ServerKind::Clangd, ServerKind::Python];
    pump_until(&mut s, |s| kinds.iter().all(|k| matches!(s.server_status(*k), crate::LspStatus::Ready)));
    let starts = std::fs::read_to_string(f.remote.join("server-starts")).unwrap().lines().count();
    assert_eq!(starts, 3);
    let initializations = std::fs::read_to_string(f.remote.join("initializations")).unwrap();
    let remote_uri = codecraft_lsp::path_to_uri(&f.remote.join("proj")).unwrap();
    for line in initializations.lines() {
        let params: Value = serde_json::from_str(line).unwrap();
        assert_eq!(params["rootUri"], json!(remote_uri));
        assert!(params["processId"].is_null());
    }
    assert!(f.argv().contains("ssh -T"));
    run(&mut s, "remote.project.disconnect", json!({}));
    assert!(s.servers.is_empty());
    run(&mut s, "remote.project.reconnect", json!({}));
    pump_until(&mut s, |s| kinds.iter().all(|k| matches!(s.server_status(*k), crate::LspStatus::Ready)));
    assert_eq!(std::fs::read_to_string(f.remote.join("server-starts")).unwrap().lines().count(), 6);
    std::fs::remove_file(f.dir.path().join("bin/basedpyright-langserver")).unwrap();
    let pyright = f.dir.path().join("bin/pyright-langserver");
    std::fs::write(&pyright, format!("#!{}\n{mock}", python.display())).unwrap();
    std::fs::set_permissions(pyright, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(f.remote.join("missing-clangd"), "missing").unwrap();
    run(&mut s, "remote.project.disconnect", json!({}));
    run(&mut s, "remote.project.reconnect", json!({}));
    pump_until(&mut s, |s| matches!(s.server_status(ServerKind::Python), crate::LspStatus::Ready)
        && matches!(s.server_status(ServerKind::Clangd), crate::LspStatus::Stopped(message) if message.contains("Install clangd")));
    assert!(std::fs::read_to_string(f.remote.join("server-starts")).unwrap().lines().any(|line| line.ends_with("/pyright-langserver")));
    s.shutdown();
}

#[test]
fn repository_workers_coordinate_with_remote_mirror_commands() {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    let Some(git) = codecraft_lsp::which("git") else { return };
    let (f, mut s) = fake();
    let root = open_mirror(&f, &mut s);
    let status = std::process::Command::new(&git).args(["init", "-q"]).arg(&root).status().unwrap();
    if !status.success() { return; }
    s.vcs.repo = codecraft_vcs::discover(&codecraft_vcs::Git::new(root.clone())).unwrap();
    assert!(s.vcs.repo.is_some());
    let started = Arc::new(AtomicBool::new(false));
    let worker_started = started.clone();
    crate::vcs::simple_op(&mut s, "Mirror coordination regression", false, move |_| {
        worker_started.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_millis(150));
        Err(codecraft_vcs::GitError::Cancelled)
    }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !started.load(Ordering::Acquire) { assert!(Instant::now() < deadline); std::thread::sleep(Duration::from_millis(5)); }
    assert!(s.execute("remote.project.status", &json!({})).unwrap_err().to_string().contains("mirror"));
    pump_until(&mut s, |s| !s.vcs.busy());
    assert!(s.execute("remote.project.status", &json!({})).is_ok());
}

#[test]
fn remote_cargo_check_clippy_and_rebuild_run_in_the_host_directory() {
    let (f, mut s) = fake();
    let root = open_mirror(&f, &mut s);
    let cargo = f.dir.path().join("bin/cargo");
    std::fs::write(&cargo, "#!/bin/sh\nprintf '%s|%s\\n' \"$(pwd)\" \"$*\" >> \"$HOME/cargo-commands\"\n").unwrap();
    std::fs::set_permissions(cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    for command in [codecraft_cargo::CargoCommand::Check, codecraft_cargo::CargoCommand::Clippy] {
        crate::runs::start(&mut s, codecraft_cargo::CargoInvocation::new(command, &root), "Remote diagnostics", true).unwrap();
        pump_until(&mut s, |s| !s.runs.any_running());
    }
    crate::runs::start_chain(&mut s,
        codecraft_cargo::CargoInvocation::new(codecraft_cargo::CargoCommand::Clean, &root),
        Some(codecraft_cargo::CargoInvocation::new(codecraft_cargo::CargoCommand::Build, &root)), "Remote rebuild", true).unwrap();
    pump_until(&mut s, |s| !s.runs.any_running());
    let commands = std::fs::read_to_string(f.remote.join("cargo-commands")).unwrap();
    for command in ["check", "clippy", "clean", "build"] {
        assert!(commands.lines().any(|line| line.starts_with(&format!("{}|{command}", f.remote.join("proj").display()))), "{commands}");
    }
}
