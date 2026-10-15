//! The Worktrees / New Worktree / Create Pull Request dialogs and the GitHub repository list of
//! the Clone dialog. Thin: every action is a `vcs.*` command. (The merge editor is in
//! [`crate::merge_editor`].)

use egui::{Align, Layout, RichText, TextEdit, Ui, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::vcs::Dialog;

use crate::theme::Theme;

type Action = Option<(&'static str, Value)>;

/// The Clone dialog's list of the user's GitHub repositories; clicking one fills the URL.
pub fn github_repos(ui: &mut Ui, s: &Session, theme: &Theme, url: &mut String) -> Action {
    let mut action = None;
    ui.separator();
    if s.vcs.gh_path().is_none() {
        ui.add(
            egui::Label::new(RichText::new(codecraft_engine::vcs_git::github::GH_INSTALL_HINT).small().color(theme.warning))
                .wrap(),
        );
        return None;
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new("Your GitHub repositories").strong());
        if s.vcs.github_loading {
            ui.add(egui::Spinner::new().size(12.0));
        } else if ui.small_button(if s.vcs.github_repos.is_some() { "Reload" } else { "Load" }).clicked() {
            action = Some(("vcs.github.repositories", json!({ "fetch": true })));
        }
    });
    match &s.vcs.github_repos {
        Some(Ok(list)) => {
            egui::ScrollArea::vertical().id_salt("vcs-gh-repos").max_height(180.0).show(ui, |ui| {
                for r in list {
                    let sel = url.as_str() == r.url || url.as_str() == format!("{}.git", r.url);
                    let resp = ui.selectable_label(sel, &r.name_with_owner);
                    let resp = match r.description.as_deref().filter(|d| !d.is_empty()) {
                        Some(d) => resp.on_hover_text(d),
                        None => resp,
                    };
                    if resp.clicked() {
                        *url = format!("{}.git", r.url);
                    }
                }
            });
        }
        Some(Err(e)) => {
            ui.add(egui::Label::new(RichText::new(e).small().color(theme.error)).wrap());
        }
        None => {
            ui.label(RichText::new("Or type owner/repository in the URL field.").small().color(theme.text_muted));
        }
    }
    action
}

/// The work tree and pull request dialogs; returns (action, close).
pub fn more_dialogs(ui: &mut Ui, s: &Session, theme: &Theme, d: &mut Dialog, busy: bool, enter: bool) -> (Action, bool) {
    let mut action = None;
    let mut close = false;
    let error = |ui: &mut Ui| {
        if let Some(e) = &s.vcs.dialog_error {
            ui.add(egui::Label::new(RichText::new(e).color(theme.error).small()).wrap());
        }
    };
    let ok_cancel = |ui: &mut Ui, label: &str, enabled: bool| -> (bool, bool) {
        let mut r = (false, false);
        ui.add_space(6.0);
        ui.allocate_ui_with_layout(vec2(ui.available_width(), 24.0), Layout::right_to_left(Align::Center), |ui| {
            let b = egui::Button::new(RichText::new(label).strong()).fill(theme.accent);
            r.0 = ui.add_enabled(enabled, b).clicked();
            r.1 = ui.button("Cancel").clicked();
        });
        r
    };
    match d {
        Dialog::NewWorktree { path, branch, new_branch, base } => {
            egui::Grid::new("vcs-worktree").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Branch");
                ui.add(TextEdit::singleline(branch).hint_text("feature/x").desired_width(340.0));
                ui.end_row();
                ui.label("");
                ui.checkbox(new_branch, "Create a new branch");
                ui.end_row();
                if *new_branch {
                    ui.label("From");
                    ui.add(TextEdit::singleline(base).hint_text("HEAD").desired_width(340.0));
                    ui.end_row();
                }
                ui.label("Directory");
                ui.add(TextEdit::singleline(path).desired_width(340.0));
                ui.end_row();
            });
            error(ui);
            let can = !busy && !path.trim().is_empty() && !branch.trim().is_empty();
            let params = |open: bool| {
                json!({ "path": path.trim(), "branch": branch.trim(), "newBranch": *new_branch, "base": base.trim(), "open": open })
            };
            let mut open_after = false;
            ui.horizontal(|ui| {
                open_after = ui.add_enabled(can, egui::Button::new("Create and Open")).clicked();
            });
            let (ok, cancel) = ok_cancel(ui, "Create", can);
            if ok || open_after || (enter && can) {
                action = Some(("vcs.addWorktree", params(open_after)));
            }
            close |= cancel;
        }
        Dialog::Worktrees { selected } => {
            if s.vcs.worktrees_loading && s.vcs.worktrees.is_empty() {
                ui.add(egui::Spinner::new());
            }
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                egui::Grid::new("vcs-worktrees").num_columns(2).striped(true).show(ui, |ui| {
                    for w in &s.vcs.worktrees {
                        let sel = selected.as_ref() == Some(&w.path);
                        let label = w.path.display().to_string();
                        let color = if w.prunable { theme.text_muted } else { theme.text };
                        if ui.selectable_label(sel, RichText::new(label).color(color)).clicked() {
                            *selected = Some(w.path.clone());
                        }
                        let mut what = w.branch.clone().unwrap_or_else(|| {
                            w.head.as_deref().map(|h| h.get(..8).unwrap_or(h).to_string()).unwrap_or_default()
                        });
                        for (flag, text) in
                            [(w.main, "main"), (w.locked, "locked"), (w.prunable, "missing"), (w.detached, "detached")]
                        {
                            if flag {
                                what.push_str(&format!(" · {text}"));
                            }
                        }
                        ui.label(RichText::new(what).small().color(theme.text_muted));
                        ui.end_row();
                    }
                });
            });
            error(ui);
            let target = selected.clone().filter(|p| s.vcs.worktrees.iter().any(|w| &w.path == p));
            let is_main = target.as_ref().is_some_and(|p| s.vcs.worktrees.first().is_some_and(|w| &w.path == p));
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new("New Worktree…")).clicked() {
                    action = Some(("git.newWorktree", Value::Null));
                }
                if ui.add_enabled(target.is_some(), egui::Button::new("Open")).clicked() {
                    action = Some(("vcs.openWorktree", json!({ "path": target })));
                }
                let can_remove = !busy && target.is_some() && !is_main;
                if ui.add_enabled(can_remove, egui::Button::new("Remove")).clicked() {
                    action = Some(("vcs.removeWorktree", json!({ "path": target })));
                }
                if ui
                    .add_enabled(can_remove, egui::Button::new("Force Remove"))
                    .on_hover_text("Remove even with local changes")
                    .clicked()
                {
                    action = Some(("vcs.removeWorktree", json!({ "path": target, "force": true })));
                }
                if ui.button("Refresh").clicked() {
                    action = Some(("vcs.worktrees", json!({ "refresh": true })));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });
        }
        Dialog::CreatePullRequest { title, body, base, draft } => {
            let head = s.vcs.branch_name().unwrap_or("").to_string();
            ui.horizontal(|ui| {
                ui.label(RichText::new(&head).strong());
                ui.label("→");
                ui.add(TextEdit::singleline(base).hint_text("default branch").desired_width(200.0));
            });
            ui.add(TextEdit::singleline(title).hint_text("Title").desired_width(f32::INFINITY));
            ui.add(TextEdit::multiline(body).hint_text("Description").desired_rows(6).desired_width(f32::INFINITY));
            ui.checkbox(draft, "Draft");
            if s.vcs.github_remote().is_none() {
                ui.label(RichText::new("The repository has no GitHub remote.").small().color(theme.warning));
            }
            error(ui);
            let can = !busy && !title.trim().is_empty() && s.vcs.gh_path().is_some() && s.vcs.github_remote().is_some();
            let (ok, cancel) = ok_cancel(ui, "Create Pull Request", can);
            if ok {
                action = Some((
                    "vcs.createPullRequest",
                    json!({ "title": title.trim(), "body": body, "base": base.trim(), "draft": *draft, "open": true }),
                ));
            }
            close |= cancel;
        }
        _ => {}
    }
    (action, close)
}
