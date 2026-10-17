//! M16 track RT in the UI: Settings › Python › Remote Interpreters lists the interpreters a fake
//! SSH host has (the fake `ssh` runs the probe locally, in a temp "remote home"), clicking one
//! lists its packages; Run › Manage Targets… has the Add a target form.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_ui_egui::App;

const SSH: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    --) shift; break;;
    -N) exec sleep 3600;;
    -o|-p|-l|-i|-L|-O|-P) shift 2;;
    *) shift;;
  esac
done
shift
cd "$REMOTE" || exit 1
HOME="$REMOTE" exec /bin/sh -c "$*"
"#;

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

#[test]
fn remote_interpreters_on_the_python_page_and_the_add_target_form() {
    if std::process::Command::new("python3").arg("-c").arg("pass").output().map_or(true, |o| !o.status.success()) {
        eprintln!("skipping: no python3");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    let remote = base.join("remote-home");
    let home = base.join("home");
    for d in [&root, &remote.join("app"), &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(root.join("main.py"), "print(1)\n").unwrap();
    std::fs::write(home.join("ssh_config"), "Host web\n  HostName web.example\n").unwrap();
    let ssh = base.join("ssh");
    std::fs::write(&ssh, SSH.replace("#!/bin/sh\n", &format!("#!/bin/sh\nREMOTE='{}'\n", remote.display()))).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 900.0)).build_eframe({
        let root = root.clone();
        let ssh = ssh.display().to_string();
        move |_| {
            let mut app = App::ephemeral(Some(root));
            app.session.isolate_home(&home);
            app.session.remote.share_connections = false;
            app.session.remote.programs.ssh = ssh;
            app
        }
    });
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    let s = &mut h.state_mut().session;
    s.execute("remote.deployment.setMapping", &json!({ "server": "web", "remote": "~/app" })).unwrap();
    s.execute("app.settings", &json!({ "page": codecraft_engine::settings::PAGE_PYTHON })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Remote Interpreters (SSH / Docker)").is_some());
    assert!(h.query_all_by_label_contains("Interpreters on an SSH host").next().is_some());
    h.state_mut().session.execute("python.remoteInterpreters.detect", &json!({ "target": "web" })).unwrap();
    settle(&mut h, "detection", |a| !a.session.targets.python.busy());
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("Project folder there:").next().is_some(), "the probe reported the folder: {:?}", h.state().session.targets.python.selected_list());
    let found = h.query_all_by_label_contains("Python 3.").count();
    assert!(found > 0, "interpreters listed: {:?}", h.state().session.targets.python.selected_list());
    h.query_all_by_label_contains("Python 3.").next().unwrap().click();
    h.run_steps(2);
    settle(&mut h, "packages", |a| !a.session.targets.python.busy());
    h.run_steps(3);
    assert!(h.query_all_by_label_contains(" packages").next().is_some(), "{:?}", h.state().session.targets.python.selected_packages());

    // Manage Targets has the Add a target form.
    h.state_mut().session.execute("settings.dialog.cancel", &json!({})).ok();
    h.state_mut().session.execute("run.manageTargets", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Add a target").is_some());
    assert!(h.query_by_label("Add Target").is_some());
}
