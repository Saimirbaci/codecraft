//! Background jobs: project loading (tree scan + `cargo metadata`), Find in Files, rustfmt.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, bounded};

use codecraft_cargo::CargoMetadata;
use codecraft_fs::{FileMatch, FileTree, FindOptions, ScanOptions};
use codecraft_text::search::SearchQuery;
use codecraft_workspace::{BackgroundTask, DocId, FindResults, NotificationLevel, Project, ToolWindowId};

use crate::Session;

struct ProjectLoad {
    root: PathBuf,
    tree: Result<FileTree, String>,
    cargo: Option<Result<CargoMetadata, String>>,
}

type FindJob = (Receiver<Result<Vec<FileMatch>, String>>, Arc<AtomicBool>);
type RustfmtJob = (Receiver<Result<String, String>>, DocId, u64);

#[derive(Default)]
pub(crate) struct Jobs {
    project: Option<Receiver<ProjectLoad>>,
    rescan: Option<Receiver<(PathBuf, Result<FileTree, String>)>>,
    find: Option<FindJob>,
    rustfmt: Option<RustfmtJob>,
}

impl Jobs {
    pub(crate) fn any_running(&self) -> bool {
        self.project.is_some() || self.rescan.is_some() || self.find.is_some() || self.rustfmt.is_some()
    }
}

const TASK_PROJECT: &str = "project-load";
const TASK_FIND: &str = "find-in-files";

impl Session {
    /// The project is still being scanned (or its Cargo metadata read): its run configurations
    /// and Cargo targets are not known yet.
    pub fn project_loading(&self) -> bool {
        self.jobs.project.is_some()
    }

    /// Open a directory as the project: scan it and read Cargo metadata in the background,
    /// then start rust-analyzer.
    pub fn open_project(&mut self, root: &Path) {
        let root = crate::normalize_path(root);
        self.close_project();
        self.ws.project = Some(Project::new(root.clone()));
        self.ws.note_recent_project(root.clone());
        crate::session_store::restore_on_open(self);
        crate::hot_exit::restore_on_open(self);
        self.ws.tasks.insert(
            TASK_PROJECT.into(),
            BackgroundTask {
                title: "Loading project".into(),
                message: Some(root.display().to_string()),
                percentage: None,
            },
        );
        let (tx, rx) = bounded(1);
        std::thread::spawn(move || {
            let tree = codecraft_fs::scan(&root, &ScanOptions::default()).map_err(|e| e.to_string());
            let cargo = if root.join("Cargo.toml").is_file() {
                Some(codecraft_cargo::metadata(&root, true).map_err(|e| e.to_string()))
            } else {
                None
            };
            let _ = tx.send(ProjectLoad { root, tree, cargo });
        });
        self.jobs.project = Some(rx);
        self.status = "Loading project…".into();
    }

    /// Close the project: tabs, runs, language server.
    pub fn close_project(&mut self) {
        crate::session_store::save_on_close(self);
        // The project's terminals close with it (its saved session brings them back).
        if self.ws.project.is_some() {
            crate::terminal::close_all(self);
        }
        // Hot exit keeps unsaved buffers for the next time instead of saving them.
        crate::hot_exit::quit_save(self);
        crate::runs::stop_all(self);
        self.stop_lsp();
        let tabs = self.ws.all_tabs();
        for t in tabs {
            self.ws.close_editor(t);
        }
        self.ws.project = None;
        self.ws.find = None;
        self.ws.runs.clear();
        self.ws.active_run = None;
        self.ws.clear_cargo_problems();
        self.run_configs.clear();
        self.selected_run_config = 0;
        crate::run_config_store::project_closed(self);
        self.popup = None;
        self.find_bar = None;
        self.jobs.project = None;
        self.ws.tasks.remove(TASK_PROJECT);
        self.stop_watching();
    }

    /// Rescan the project tree in the background (after file operations or focus regain).
    pub(crate) fn jobs_rescan(&mut self) {
        let Some(root) = self.ws.project.as_ref().map(|p| p.root.clone()) else { return };
        let (tx, rx) = bounded(1);
        std::thread::spawn(move || {
            let tree = codecraft_fs::scan(&root, &ScanOptions::default()).map_err(|e| e.to_string());
            let _ = tx.send((root, tree));
        });
        self.jobs.rescan = Some(rx);
    }

    /// Start Find in Files over the project (cancelling a previous search).
    pub(crate) fn start_find_in_files(&mut self, query: SearchQuery, mask: Option<String>) -> Result<(), String> {
        let root = self.project_root().ok_or("no project is open")?;
        let regex = query.to_regex().map_err(|e| e.to_string())?;
        if let Some((_, cancel)) = self.jobs.find.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let opts = FindOptions { file_mask: mask.filter(|m| !m.trim().is_empty()), ..FindOptions::default() };
        let (tx, rx) = bounded(1);
        let c = cancel.clone();
        std::thread::spawn(move || {
            let r = codecraft_fs::find_in_files(&root, &regex, &opts, &c).map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
        self.ws.find = Some(FindResults {
            title: format!("Find '{}'", query.pattern),
            query,
            searching: true,
            ..FindResults::default()
        });
        self.ws.layout.show(ToolWindowId::Find);
        self.ws
            .tasks
            .insert(TASK_FIND.into(), BackgroundTask { title: "Searching".into(), message: None, percentage: None });
        self.jobs.find = Some((rx, cancel));
        Ok(())
    }

    /// Reformat a document with rustfmt in the background.
    pub(crate) fn start_rustfmt(&mut self, doc: DocId) -> Result<(), String> {
        let od = self.ws.doc(doc).ok_or("no document")?;
        let text = od.doc.text();
        let revision = od.doc.revision();
        let edition = od
            .path
            .as_deref()
            .and_then(|p| self.ws.project.as_ref()?.cargo.as_ref()?.package_for_file(p).map(|pk| pk.edition.clone()))
            .unwrap_or_else(|| "2021".into());
        let cwd = self.project_root().unwrap_or_else(|| PathBuf::from("."));
        let (tx, rx) = bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(codecraft_cargo::rustfmt(&text, &edition, &cwd).map_err(|e| e.to_string()));
        });
        self.jobs.rustfmt = Some((rx, doc, revision));
        Ok(())
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    if let Some(load) = s.jobs.project.as_ref().and_then(|rx| rx.try_recv().ok()) {
        s.jobs.project = None;
        s.ws.tasks.remove(TASK_PROJECT);
        changed = true;
        on_project_loaded(s, load);
    }
    if let Some((root, tree)) = s.jobs.rescan.as_ref().and_then(|rx| rx.try_recv().ok()) {
        s.jobs.rescan = None;
        changed = true;
        if let (Some(p), Ok(tree)) = (&mut s.ws.project, tree)
            && p.root == root
        {
            p.tree = tree;
        }
    }
    if let Some(result) = s.jobs.find.as_ref().and_then(|(rx, _)| rx.try_recv().ok()) {
        s.jobs.find = None;
        s.ws.tasks.remove(TASK_FIND);
        changed = true;
        match result {
            Ok(matches) => {
                s.status = format!("{} match(es) found", matches.len());
                if let Some(f) = &mut s.ws.find {
                    f.matches = matches;
                    f.searching = false;
                }
            }
            Err(e) => {
                s.status = format!("Find in Files failed: {e}");
                if let Some(f) = &mut s.ws.find {
                    f.searching = false;
                }
            }
        }
    }
    if let Some(result) = s.jobs.rustfmt.as_ref().map(|(rx, d, r)| (rx.try_recv(), *d, *r))
        && let (Ok(res), doc, revision) = result
    {
        s.jobs.rustfmt = None;
        changed = true;
        apply_rustfmt(s, doc, revision, res);
    }
    changed
}

fn on_project_loaded(s: &mut Session, load: ProjectLoad) {
    let Some(project) = s.ws.project.as_mut().filter(|p| p.root == load.root) else { return };
    match load.tree {
        Ok(tree) => project.tree = tree,
        Err(e) => {
            let msg = format!("Cannot read {}: {e}", load.root.display());
            s.notify(NotificationLevel::Error, "Project", msg);
            return;
        }
    }
    let mut start_lsp = false;
    // Derived configurations are rebuilt from scratch here (QA M14: when the CMake model was
    // read before the scan finished, `cmake_cmds::refresh_run_configs` had already set them and
    // "CMake: Build" and "Run app" were listed twice).
    s.run_configs = Vec::new();
    s.selected_run_config = 0;
    match load.cargo {
        Some(Ok(meta)) => {
            s.run_configs = crate::runs::derive_configs(&meta);
            s.selected_run_config = 0;
            if let Some(p) = &mut s.ws.project {
                p.cargo = Some(meta);
                p.cargo_error = None;
            }
            start_lsp = true;
        }
        Some(Err(e)) => {
            if let Some(p) = &mut s.ws.project {
                p.cargo_error = Some(e.clone());
            }
            s.notify(NotificationLevel::Warning, "Cargo", format!("cargo metadata failed: {e}"));
            start_lsp = true;
        }
        None => {
            if let Some(p) = &mut s.ws.project {
                p.cargo_error = Some("No Cargo.toml in the project root".into());
            }
        }
    }
    s.start_watching(&load.root);
    // Run configurations of the other ecosystems in the project (CMake, Make, Zig, npm, Python).
    s.run_configs.extend(crate::run_configs::derive_polyglot(&load.root));
    // User configurations (Run › Edit Configurations…) come first.
    crate::run_config_store::project_loaded(s, &load.root);
    // Like CLion, a CMake project starts with its executable selected, not the build-only
    // configuration (QA M14: Debug and Profile of the default configuration ran `cmake --build`).
    let build_only = |c: &crate::RunConfig| c.command.as_ref().is_some_and(|cmd| cmd.build);
    if s.run_configs.get(s.selected_run_config).is_some_and(build_only)
        && let Some(i) = s.run_configs.iter().position(|c| !build_only(c) && c.name.starts_with("Run "))
    {
        s.selected_run_config = i;
    }
    s.status = format!("Opened {}", load.root.display());
    if start_lsp {
        s.start_lsp(&load.root);
    }
}

fn apply_rustfmt(s: &mut Session, doc: DocId, revision: u64, result: Result<String, String>) {
    match result {
        Ok(formatted) => {
            let editor = s.ws.editor_for_doc(doc);
            let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
            let Some(od) = s.ws.doc_mut(doc) else { return };
            if od.doc.revision() != revision {
                s.status = "Reformat skipped: the file changed while rustfmt ran".into();
                return;
            }
            // Only the changed lines are replaced, and carets keep their place (gap 15).
            let edit = crate::reformat::diff_edit(&od.doc, &formatted, &sels);
            if edit.is_noop() {
                s.status = "Reformatted with rustfmt (no changes)".into();
                return;
            }
            match od.doc.apply(&edit, &sels, codecraft_text::EditGroup::Standalone) {
                Ok(()) => {
                    s.status = "Reformatted with rustfmt".into();
                    if let Some(e) = editor {
                        s.ws.set_selections(e, edit.selections);
                    }
                }
                Err(e) => s.status = e.to_string(),
            }
        }
        Err(e) => s.notify(NotificationLevel::Error, "rustfmt", e),
    }
}
