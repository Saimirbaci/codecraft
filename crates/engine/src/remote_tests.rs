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
if [ $dry = 1 ]; then cat "$FAKE/itemize-$dir" 2>/dev/null; exit 0; fi
map() { case "$1" in /*) echo "$1";; *) p="${1#*:}"; case "$p" in /*) echo "$p";; *) echo "$REMOTE/$p";; esac;; esac; }
src=$(map "$src"); dst=$(map "$dst")
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
        "#!/bin/sh\nLOG='{}'; REMOTE='{}'; FAKE='{}'\n{}",
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
    std::fs::write(
        root.join("ssh_config"),
        "Host web web2\n  HostName web.example\n  User deploy\nHost *.internal\n  User ops\n",
    )
    .unwrap();
    let mut s = Session::new();
    s.lsp_enabled = false;
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
    run(&mut s, "file.remoteDevelopment", json!({ "server": "web", "path": "~/proj" }));
    pump_until(&mut s, |s| s.ws.project.is_some() && !s.remote.busy());
    let root = s.ws.project.as_ref().unwrap().root.clone();
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
    let explicit = crate::ssh_target::SshTarget { server: "web2".into(), remote_dir: "/srv/x".into(), sync: false };
    let w = crate::ssh_target::wrap(&mut s, &explicit, cmd).unwrap();
    assert!(w.args.last().unwrap().contains("cd /srv/x/scripts"), "{:?}", w.args);
    assert!(w.args.contains(&"--".to_string()));
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
