//! `codecraft <path>` handed to the running app (see `codecraft_handoff`): where each path goes.
//!
//! - A folder opens as a project in a **new window**, unless that project is already open (that
//!   window is focused instead), the main window has no project yet (it takes it), or the request
//!   asked to reuse the current window (`-r`).
//! - A file opens in the window whose project contains it (the deepest one), else in the focused
//!   window; `file:line[:col]` also moves the caret.
//! - A path that does not exist is reported in the focused window's status bar.
//! - No paths just brings the app to the front.
//!
//! [`plan`] is pure (apart from asking the file system what a path is) so the routing is tested
//! without windows; `App::apply_open_request` carries the actions out.

use std::path::{Path, PathBuf};

use codecraft_handoff::Target;

/// A window: the main one, or the `n`th project window beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    Main,
    Window(usize),
}

/// The open windows' projects, as the planner sees them.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    /// The main window's project root, if it has one.
    pub main: Option<PathBuf>,
    /// Each project window's root, by index (`None`: closed).
    pub windows: Vec<Option<PathBuf>>,
    /// The window that has the focus.
    pub focused: Slot,
}

impl Layout {
    fn roots(&self) -> impl Iterator<Item = (Slot, &Path)> {
        self.main
            .iter()
            .map(|p| (Slot::Main, p.as_path()))
            .chain(self.windows.iter().enumerate().filter_map(|(i, p)| Some((Slot::Window(i), p.as_deref()?))))
    }

    /// The window whose project is exactly `path`.
    fn with_root(&self, path: &Path) -> Option<Slot> {
        self.roots().find(|(_, root)| *root == path).map(|(slot, _)| slot)
    }

    /// The window whose project holds `path` (the deepest root when several do).
    fn containing(&self, path: &Path) -> Option<Slot> {
        self.roots()
            .filter(|(_, root)| path.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
            .map(|(slot, _)| slot)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Open the folder as the project of this window (`file.open`) and focus it.
    OpenFolderIn(Slot, PathBuf),
    /// Open the folder in a new project window.
    OpenWindow(PathBuf),
    /// Open the file in this window, move the caret, focus the window.
    OpenFile { slot: Slot, path: PathBuf, line: Option<usize>, column: Option<usize> },
    /// Bring this window to the front.
    Focus(Slot),
    /// Tell the user the path is not there.
    Missing(PathBuf),
}

/// What to do for one command-line target.
pub(crate) fn plan(target: &Target, reuse_window: bool, layout: &Layout) -> Action {
    let path = codecraft_engine::normalize_path(&target.path);
    if path.is_dir() {
        return match layout.with_root(&path) {
            Some(slot) => Action::Focus(slot),
            None if reuse_window => Action::OpenFolderIn(layout.focused, path),
            None if layout.main.is_none() => Action::OpenFolderIn(Slot::Main, path),
            None => Action::OpenWindow(path),
        };
    }
    if path.is_file() {
        let slot = layout.containing(&path).unwrap_or(layout.focused);
        return Action::OpenFile { slot, path, line: target.line, column: target.column };
    }
    Action::Missing(target.path.clone())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn target(path: &Path) -> Target {
        Target { path: path.to_path_buf(), line: None, column: None }
    }

    fn layout(main: Option<&Path>, windows: &[Option<&Path>], focused: Slot) -> Layout {
        Layout {
            main: main.map(codecraft_engine::normalize_path),
            windows: windows.iter().map(|w| w.map(codecraft_engine::normalize_path)).collect(),
            focused,
        }
    }

    #[test]
    fn a_folder_gets_a_new_window_by_default() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let l = layout(Some(a.path()), &[], Slot::Main);
        let want = codecraft_engine::normalize_path(b.path());
        assert_eq!(plan(&target(b.path()), false, &l), Action::OpenWindow(want));
    }

    #[test]
    fn an_open_folder_is_focused_not_opened_again() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let l = layout(Some(a.path()), &[None, Some(b.path())], Slot::Main);
        assert_eq!(plan(&target(a.path()), false, &l), Action::Focus(Slot::Main));
        assert_eq!(plan(&target(b.path()), false, &l), Action::Focus(Slot::Window(1)));
        // Also with -r.
        assert_eq!(plan(&target(b.path()), true, &l), Action::Focus(Slot::Window(1)));
    }

    #[test]
    fn an_empty_main_window_takes_the_first_folder() {
        let b = tempfile::tempdir().unwrap();
        let l = layout(None, &[], Slot::Main);
        let want = codecraft_engine::normalize_path(b.path());
        assert_eq!(plan(&target(b.path()), false, &l), Action::OpenFolderIn(Slot::Main, want));
    }

    #[test]
    fn reuse_window_opens_the_folder_in_the_focused_window() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let c = tempfile::tempdir().unwrap();
        let l = layout(Some(a.path()), &[Some(b.path())], Slot::Window(0));
        let want = codecraft_engine::normalize_path(c.path());
        assert_eq!(plan(&target(c.path()), true, &l), Action::OpenFolderIn(Slot::Window(0), want));
    }

    #[test]
    fn a_file_goes_to_the_window_with_its_project() {
        let a = tempfile::tempdir().unwrap();
        let sub = a.path().join("crates/x");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("lib.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        let other = tempfile::tempdir().unwrap();
        // Two windows hold the file's path: the deeper project wins.
        let l = layout(Some(a.path()), &[Some(&sub), Some(other.path())], Slot::Window(1));
        let t = Target { path: file.clone(), line: Some(4), column: Some(2) };
        assert_eq!(
            plan(&t, false, &l),
            Action::OpenFile {
                slot: Slot::Window(0),
                path: codecraft_engine::normalize_path(&file),
                line: Some(4),
                column: Some(2)
            }
        );
    }

    #[test]
    fn a_file_outside_every_project_opens_in_the_focused_window() {
        let a = tempfile::tempdir().unwrap();
        let loose = tempfile::tempdir().unwrap();
        let file = loose.path().join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        let l = layout(Some(a.path()), &[Some(a.path())], Slot::Window(0));
        assert!(matches!(plan(&target(&file), false, &l), Action::OpenFile { slot: Slot::Window(0), .. }));
        let none = layout(None, &[], Slot::Main);
        assert!(matches!(plan(&target(&file), false, &none), Action::OpenFile { slot: Slot::Main, .. }));
    }

    #[test]
    fn a_missing_path_is_reported() {
        let a = tempfile::tempdir().unwrap();
        let missing = a.path().join("nope");
        let l = layout(Some(a.path()), &[], Slot::Main);
        assert_eq!(plan(&target(&missing), false, &l), Action::Missing(missing));
    }

    fn request(targets: &[(&Path, Option<usize>)], reuse_window: bool) -> codecraft_handoff::OpenRequest {
        codecraft_handoff::OpenRequest {
            targets: targets.iter().map(|(p, l)| Target { path: p.to_path_buf(), line: *l, column: None }).collect(),
            reuse_window,
        }
    }

    #[test]
    fn the_app_opens_folders_in_windows_and_files_at_their_line() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let file = b.path().join("notes.txt");
        std::fs::write(&file, "one\ntwo\nthree\nfour\n").unwrap();
        let ctx = egui::Context::default();
        let mut app = crate::App::ephemeral(None);
        app.session.lsp_enabled = false;

        // The empty main window takes the first folder; the second gets its own window.
        app.apply_open_request(&ctx, &request(&[(a.path(), None)], false));
        assert_eq!(app.project_window_count(), 1);
        assert_eq!(app.session.project_root().map(|p| codecraft_engine::normalize_path(&p)), Some(codecraft_engine::normalize_path(a.path())));
        app.apply_open_request(&ctx, &request(&[(b.path(), None)], false));
        assert_eq!(app.project_window_count(), 2);

        // The same folders again only focus their windows.
        app.apply_open_request(&ctx, &request(&[(a.path(), None), (b.path(), None)], false));
        assert_eq!(app.project_window_count(), 2);

        // A file in the second project opens there, with the caret on its line.
        app.apply_open_request(&ctx, &request(&[(&file, Some(3))], false));
        let s = app.project_window_session(0).unwrap();
        let (editor, doc) = s.ws.active().unwrap();
        assert_eq!(doc.path.as_deref().map(codecraft_engine::normalize_path), Some(codecraft_engine::normalize_path(&file)));
        assert_eq!(doc.doc.offset_to_position(editor.selections.primary().head).line, 2);
        assert!(app.session.ws.active().is_none(), "the main window was not touched");

        // A path that is not there is reported, and nothing else changes.
        app.apply_open_request(&ctx, &request(&[(&a.path().join("missing"), None)], false));
        assert!(app.session.status.contains("does not exist"), "{}", app.session.status);
        assert_eq!(app.project_window_count(), 2);

        // -r puts a new folder into the current (main) window instead of a third one.
        let c = tempfile::tempdir().unwrap();
        app.apply_open_request(&ctx, &request(&[(c.path(), None)], true));
        assert_eq!(app.project_window_count(), 2);
        assert_eq!(app.session.project_root().map(|p| codecraft_engine::normalize_path(&p)), Some(codecraft_engine::normalize_path(c.path())));
    }
}
