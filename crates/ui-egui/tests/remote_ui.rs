//! Track S's windows driven headlessly with a fake `ssh` that runs the remote command in a temp
//! folder: the server chooser, Deployment › Configuration…, File Transfer, Remote Host (with the
//! delete confirmation) and the sync preview. No real host, key or `~/.ssh` is touched.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

struct Fake {
    _dir: tempfile::TempDir,
    root: PathBuf,
    remote: PathBuf,
    project: PathBuf,
    home: PathBuf,
    bin: PathBuf,
}

fn fake() -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (remote, project, home, bin) = (root.join("remote"), root.join("project"), root.join("home"), root.join("bin"));
    for d in [&remote, &project, &home, &bin] {
        std::fs::create_dir_all(d).unwrap();
    }
    let ssh = format!(
        "#!/bin/sh\nwhile [ $# -gt 0 ]; do case \"$1\" in --) shift; break;; -o|-p|-l|-i|-L|-O) shift 2;; *) shift;; esac; done\n\
         shift\ncd '{r}' || exit 1\nHOME='{r}' exec /bin/sh -c \"$*\"\n",
        r = remote.display()
    );
    std::fs::write(bin.join("ssh"), ssh).unwrap();
    std::fs::set_permissions(bin.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join("ssh_config"), "Host web\n  HostName web.example\nHost db\n").unwrap();
    std::fs::write(project.join("a.txt"), "hello\n").unwrap();
    Fake { _dir: dir, root, remote, project, home, bin }
}

fn harness(f: &Fake) -> Harness<'static, App> {
    let project = f.project.clone();
    let mut h = Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .build_eframe(move |_| App::ephemeral(Some(project.clone())));
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    let s = &mut h.state_mut().session;
    s.lsp_enabled = false;
    s.remote.home_override = Some(f.home.clone());
    s.remote.ssh_config_override = Some(f.root.join("ssh_config"));
    s.remote.share_connections = false;
    s.remote.programs.ssh = f.bin.join("ssh").display().to_string();
    s.remote.programs.rsync = f.bin.join("no-rsync").display().to_string();
    s.remote.programs.scp = f.bin.join("no-scp").display().to_string();
    h
}

fn exists(p: &Path) -> bool {
    p.exists()
}

#[test]
fn configuration_dialog_saves_a_mapping_and_uploads_show_in_file_transfer() {
    let f = fake();
    let mut h = harness(&f);
    h.state_mut().session.execute("tools.deployment.configuration", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("web").next().is_some());
    // Fill the mapping form (the UI edits the dialog's fields in place) and save.
    {
        let d = h.state_mut().session.remote.dialog.as_mut().unwrap();
        assert_eq!(d.selected.as_deref(), Some("web"));
        d.mapping.remote = "~/site".into();
    }
    h.run_steps(2);
    h.get_by_label("Save Mapping").click();
    h.run_steps(3);
    let cfg = codecraft_engine::deployment::load(&f.project).unwrap();
    assert_eq!(cfg.default_server.as_deref(), Some("web"));
    assert_eq!(cfg.server("web").unwrap().mappings[0].remote, "~/site");
    h.state_mut().session.execute("remote.dialog.close", &json!({})).unwrap();

    // Upload: the File Transfer window lists it.
    h.state_mut().session.execute("tools.deployment.uploadToDefaultServer", &json!({ "path": "a.txt" })).unwrap();
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::FileTransfer));
    settle(&mut h, "upload", |a| !a.session.remote.busy());
    h.run_steps(3);
    assert!(exists(&f.remote.join("site/a.txt")));
    assert!(h.query_all_by_label_contains("Upload a.txt").next().is_some());
    h.get_by_label("Clear").click();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("No transfers yet").next().is_some());
}

#[test]
fn server_chooser_then_remote_host_with_delete_confirmation() {
    let f = fake();
    std::fs::create_dir_all(f.remote.join("www/old")).unwrap();
    std::fs::write(f.remote.join("www/index.html"), "<p>hi</p>").unwrap();
    let mut h = harness(&f);
    // No default server: Browse Remote Host asks which one.
    h.state_mut().session.execute("tools.deployment.browseRemoteHost", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Choose a server").is_some());
    h.get_by_label("web").click();
    h.run_steps(3);
    assert!(h.state().session.remote.picker.is_none());
    settle(&mut h, "listing", |a| !a.session.remote.busy());
    h.state_mut().session.execute("remote.host.browse", &json!({ "server": "web", "path": "~/www" })).unwrap();
    settle(&mut h, "listing", |a| !a.session.remote.busy());
    h.run_steps(3);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::RemoteHost));
    assert!(h.query_all_by_label_contains("📄 index.html").next().is_some());
    assert!(h.query_all_by_label_contains("📁 old").next().is_some());
    // Delete asks first; Cancel keeps the folder, Delete removes it.
    h.state_mut().session.execute("remote.host.delete", &json!({ "name": "old" })).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("cannot be undone").next().is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(h.state().session.remote.confirm.is_none());
    assert!(exists(&f.remote.join("www/old")));
    h.state_mut().session.execute("remote.host.delete", &json!({ "name": "old" })).unwrap();
    h.run_steps(3);
    h.get_by_label("Delete").click();
    settle(&mut h, "delete", |a| !a.session.remote.busy());
    assert!(!exists(&f.remote.join("www/old")));
}

#[test]
fn remote_development_dialog_and_sync_preview_render() {
    let f = fake();
    let mut h = harness(&f);
    h.state_mut().session.execute("file.remoteDevelopment", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Open Project in New Window").is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(h.state().session.remote.remote_dev.is_none());
    // A sync preview (as the engine builds it from rsync's itemized output).
    h.state_mut().session.remote.sync_preview = Some(codecraft_engine::remote::SyncPreview {
        server: "web".into(),
        local: f.project.clone(),
        remote: "~/site".into(),
        items: vec![codecraft_engine::remote::SyncItem {
            path: "a.txt".into(),
            direction: codecraft_engine::remote::Direction::Upload,
            change: codecraft_engine::remote::Change::Transfer,
            is_dir: false,
            selected: true,
        }],
        excludes: Vec::new(),
    });
    h.run_steps(3);
    assert!(h.query_by_label("Synchronize Selected").is_some());
    assert!(h.query_all_by_label_contains("upload").next().is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(h.state().session.remote.sync_preview.is_none());
}

#[test]
fn remote_development_browses_folders_in_the_dialog() {
    let f = fake();
    std::fs::create_dir_all(f.remote.join("projects/app")).unwrap();
    let mut h = harness(&f);
    h.state_mut().session.execute("file.remoteDevelopment", &json!({})).unwrap();
    h.state_mut().session.remote.remote_dev.as_mut().unwrap().path = "~/projects".into();
    h.run_steps(3);
    h.get_by_label("Browse Folders…").click_accesskit();
    settle(&mut h, "folders", |app| !app.session.remote.busy());
    h.run_steps(3);
    assert!(h.query_by_label("Use This Folder").is_some());
    h.get_by_label("Use This Folder").click_accesskit();
    h.run_steps(3);
    assert_eq!(
        h.state().session.remote.remote_dev.as_ref().unwrap().path,
        f.remote.join("projects").display().to_string()
    );
}

#[test]
fn remote_status_exposes_connection_and_reconnect_action() {
    let f = fake();
    std::fs::create_dir_all(f.remote.join("projects/app")).unwrap();
    let mut h = harness(&f);
    h.state_mut().session.remote.auto_refresh = false;
    h.state_mut().session.remote.project = Some(codecraft_engine::remote::RemoteProject {
        server: "web".into(),
        destination: "deploy@web.example".into(),
        remote: "~/projects/app".into(),
        connection: codecraft_engine::remote::ConnectionState::Disconnected,
        error: None,
        pending_uploads: Default::default(),
    });
    h.run_steps(3);
    h.get_by_label("SSH deploy@web.example · disconnected").click_accesskit();
    h.run_steps(3);
    assert!(h.query_by_label("Reconnect").is_some());
    h.get_by_label("Reconnect").click_accesskit();
    settle(&mut h, "reconnect", |app| !app.session.remote.busy());
    assert_eq!(
        h.state().session.remote.project.as_ref().unwrap().connection,
        codecraft_engine::remote::ConnectionState::Connected
    );
}

#[test]
fn remote_conflict_controls_compare_and_take_incoming_and_pause_refresh() {
    use codecraft_engine::remote::{ConnectionState, RemoteProject};
    let f = fake();
    let mut h = harness(&f);
    let incoming = f.root.join(".snapshots/project");
    let baseline = f.root.join(".baselines/project");
    std::fs::create_dir_all(&incoming).unwrap();
    std::fs::create_dir_all(&baseline).unwrap();
    std::fs::write(incoming.join("a.txt"), "incoming\n").unwrap();
    std::fs::write(baseline.join("a.txt"), "base\n").unwrap();
    {
        let s = &mut h.state_mut().session;
        s.remote.auto_refresh = false;
        s.remote.project = Some(RemoteProject { server: "web".into(), destination: "web.example".into(),
            remote: "~/project".into(), connection: ConnectionState::Connected, error: None, pending_uploads: Default::default() });
        s.remote.conflicts = vec![PathBuf::from("a.txt")];
        s.execute("remote.project.conflicts", &json!({})).unwrap();
        s.ws.layout.show(ToolWindowId::FileTransfer);
    }
    h.run_steps(3);
    for label in ["Compare", "Keep Mine", "Take Remote", "Merge", "Resume Auto-refresh"] {
        assert!(h.query_all_by_label(label).next().is_some(), "missing {label}");
    }
    h.get_by_label("Compare").click(); h.run_steps(2);
    assert_eq!(h.state().session.misc.compare.as_ref().unwrap().right, "incoming\n");
    h.state_mut().session.execute("compare.close", &json!({})).unwrap();
    h.run_steps(2);
    h.get_by_label("Take Remote").click(); h.run_steps(3);
    assert_eq!(std::fs::read_to_string(f.project.join("a.txt")).unwrap(), "incoming\n");
    assert!(h.state().session.remote.conflicts.is_empty());
    h.get_by_label("Resume Auto-refresh").click(); h.run_steps(2);
    assert!(h.state().session.remote.auto_refresh);
    assert!(!codecraft_engine::deployment::load(&f.project).unwrap().pause_remote_refresh);
    h.get_by_label("Pause Auto-refresh").click(); h.run_steps(2);
    assert!(!h.state().session.remote.auto_refresh);
}
