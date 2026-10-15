//! Nothing lost on restart (M13 track SR) through the real shell: quitting with unsaved changes
//! does not ask and does not write the file; the next start brings the buffer back with a
//! "restored unsaved changes" banner (Compare with Disk opens the diff view, Dismiss hides it);
//! a file changed on disk meanwhile gets the warning banner and Discard goes back to the disk
//! text. Settings › System Settings has the switch. Set `CODECRAFT_SNAPSHOT_DIR` to also write
//! PNGs. Nothing touches the real config directory or home.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use eframe::App as _;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::Popup;
use codecraft_engine::hot_exit::{self, Conflict};
use codecraft_ui_egui::{App, PersistedState};

fn harness(persisted: PersistedState, cfg: PathBuf) -> Harness<'static, App> {
    Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .build_eframe(move |_| App::restoring(persisted.clone(), cfg.clone()))
}

fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    let image = h.render().unwrap();
    image.save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

struct Env {
    _tmp: tempfile::TempDir,
    cfg: PathBuf,
    home: PathBuf,
    proj: PathBuf,
    file: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let proj = root.join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    let file = proj.join("src/main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    Env { cfg: root.join("cfg"), home: root.join("home"), proj, file, _tmp: tmp }
}

/// A first run: open the project and the file, type, quit. Returns the state to start from.
fn first_run(e: &Env, typed: &str) -> PersistedState {
    let mut h = harness(PersistedState::default(), e.cfg.clone());
    h.run_steps(2);
    {
        let s = &mut h.state_mut().session;
        s.isolate_home(&e.home);
        s.execute("file.open", &json!({ "path": e.proj })).unwrap();
        s.execute("file.open", &json!({ "path": e.file })).unwrap();
        s.execute("editor.type", &json!({ "text": typed })).unwrap();
    }
    h.run_steps(3);
    let state = h.state_mut().capture_state();
    // Quit: no question, the file is left alone.
    h.state_mut().on_exit();
    state
}

fn second_run(e: &Env, state: PersistedState) -> Harness<'static, App> {
    let mut h = harness(state, e.cfg.clone());
    h.run_steps(3);
    h.state_mut().session.isolate_home(&e.home);
    h.run_steps(1);
    h
}

#[test]
fn quitting_keeps_the_buffer_and_the_next_start_shows_the_banner() {
    let e = env();
    let state = first_run(&e, "// unsaved\n");
    assert_eq!(std::fs::read_to_string(&e.file).unwrap(), "fn main() {}\n", "quit did not write the file");
    let kept = e.proj.join(hot_exit::DIR);
    assert!(std::fs::read_dir(&kept).unwrap().any(|f| f.unwrap().path().extension().is_some_and(|x| x == "json")));

    let mut h = second_run(&e, state);
    let doc = h.state().session.ws.doc_by_path(&e.file).unwrap();
    assert_eq!(h.state().session.ws.doc(doc).unwrap().doc.text(), "// unsaved\nfn main() {}\n");
    assert!(h.query_by_label_contains("Restored unsaved changes from the last session").is_some());
    snapshot(&mut h, "hot-exit-banner");

    h.get_by_label("Compare with Disk").click();
    h.run_steps(3);
    let cmp = h.state().session.misc.compare.clone().unwrap();
    assert_eq!((cmp.left.as_str(), cmp.right.as_str()), ("fn main() {}\n", "// unsaved\nfn main() {}\n"));
    snapshot(&mut h, "hot-exit-compare");
    h.state_mut().session.misc.compare = None;
    h.run_steps(2);

    h.get_by_label("Dismiss").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Restored unsaved changes").is_none(), "dismissed");
    assert!(h.state().session.ws.doc(doc).unwrap().doc.is_modified(), "the changes stay");
    h.state_mut().on_exit();
}

#[test]
fn a_file_changed_meanwhile_gets_the_warning_banner_and_discard_goes_back_to_disk() {
    let e = env();
    let state = first_run(&e, "// mine\n");
    std::fs::write(&e.file, "fn other() {}\n").unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&e.file).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(5)).unwrap();
    drop(f);

    let mut h = second_run(&e, state);
    let doc = h.state().session.ws.doc_by_path(&e.file).unwrap();
    assert_eq!(h.state().session.hot_exit.restored[&doc].conflict, Conflict::ChangedOnDisk);
    assert!(h.query_by_label_contains("the file changed on disk since").is_some());
    assert!(h.query_by_label("Keep My Changes").is_some());
    snapshot(&mut h, "hot-exit-conflict");
    h.get_by_label("Discard").click();
    h.run_steps(3);
    let s = &h.state().session;
    assert_eq!(s.ws.doc(doc).unwrap().doc.text(), "fn other() {}\n");
    assert!(!s.ws.doc(doc).unwrap().doc.is_modified());
    assert!(h.query_by_label_contains("changed on disk since").is_none());
    h.state_mut().on_exit();
    assert_eq!(std::fs::read_to_string(&e.file).unwrap(), "fn other() {}\n");
}

#[test]
fn system_settings_has_the_hot_exit_switch() {
    let e = env();
    let mut h = harness(PersistedState::default(), e.cfg.clone());
    h.run_steps(2);
    let page = codecraft_engine::settings::PAGE_SYSTEM;
    h.state_mut().session.execute("app.settings", &json!({ "page": page })).unwrap();
    h.run_steps(2);
    assert!(matches!(h.state().session.popup, Some(Popup::Settings)));
    assert!(h.query_by_label_contains("Keep unsaved changes on exit").is_some());
    snapshot(&mut h, "hot-exit-setting");
    assert!(h.state().session.ws.settings.hot_exit, "on by default");
}
