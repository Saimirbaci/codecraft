//! The three-way merge window (Resolve Conflicts › Merge…), the Worktrees / New Worktree /
//! Create Pull Request dialogs and the GitHub repository list of the Clone dialog. Thin: every
//! action is a `vcs.*` command; the merge state lives in `Session::vcs.merge`.

use egui::{Align, Align2, FontId, Frame, Id, Layout, RichText, TextEdit, Ui, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::vcs::Dialog;
use codecraft_engine::vcs_git::merge3::{ChunkKind, Resolution};

use crate::theme::Theme;
use crate::{UiState, run_command};

type Action = Option<(&'static str, Value)>;

fn kind_label(k: ChunkKind) -> &'static str {
    match k {
        ChunkKind::Ours => "Changed in yours",
        ChunkKind::Theirs => "Changed in theirs",
        ChunkKind::Same => "Same change on both sides",
        ChunkKind::Conflict => "Conflict",
    }
}

fn resolution_label(r: Resolution) -> &'static str {
    match r {
        Resolution::Ours => "yours accepted",
        Resolution::Theirs => "theirs accepted",
        Resolution::OursThenTheirs => "both accepted (yours first)",
        Resolution::TheirsThenOurs => "both accepted (theirs first)",
        Resolution::Base => "ignored (base kept)",
    }
}

/// One version's lines of a chunk in a fixed-width column.
fn text_column(ui: &mut Ui, theme: &Theme, title: &str, text: &str, fill: egui::Color32, width: f32) {
    ui.vertical(|ui| {
        ui.set_width(width);
        ui.label(RichText::new(title).small().color(theme.text_muted));
        Frame::new().fill(fill).inner_margin(4.0).show(ui, |ui| {
            ui.set_width(width - 8.0);
            let shown = if text.is_empty() { "(no lines)" } else { text.strip_suffix('\n').unwrap_or(text) };
            let color = if text.is_empty() { theme.text_muted } else { theme.text_editor };
            ui.add(egui::Label::new(RichText::new(shown).font(FontId::monospace(12.0)).color(color)).wrap());
        });
    });
}

pub fn merge_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(view) = s.vcs.merge.clone() else { return };
    let theme = st.theme.clone();
    let mut open = true;
    let mut action: Action = None;
    let (result, unresolved) = view.result();
    let title = format!("Merge Revisions for {}", view.path);
    crate::windows::resizable(egui::Window::new(title).default_size([1000.0, 640.0]), st, "vcs-merge")
        .id(Id::new("vcs-merge"))
        .open(&mut open)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.button("Apply Non-Conflicting Changes").clicked() {
                    action = Some(("vcs.merge.applyNonConflicting", Value::Null));
                }
                if ui.button("Accept Left").on_hover_text("Resolve every conflict with yours").clicked() {
                    action = Some(("vcs.merge.resolveAll", json!({ "resolution": "ours" })));
                }
                if ui.button("Accept Right").on_hover_text("Resolve every conflict with theirs").clicked() {
                    action = Some(("vcs.merge.resolveAll", json!({ "resolution": "theirs" })));
                }
                if ui.button("▲").on_hover_text("Previous conflict").clicked() {
                    action = Some(("vcs.merge.select", json!({ "next": false })));
                }
                if ui.button("▼").on_hover_text("Next conflict").clicked() {
                    action = Some(("vcs.merge.select", json!({ "next": true })));
                }
                let summary = if unresolved == 0 {
                    RichText::new("All changes are resolved").color(theme.success)
                } else {
                    RichText::new(format!("{unresolved} of {} conflicts unresolved", view.conflicts()))
                        .color(theme.vcs_conflict)
                };
                ui.label(summary);
            });
            ui.separator();
            let col_w = ((ui.available_width() - 40.0) / 3.0).max(120.0);
            let body_h = (ui.available_height() - 210.0).max(120.0);
            egui::ScrollArea::vertical().id_salt("vcs-merge-chunks").max_height(body_h).show(ui, |ui| {
                if view.chunks.is_empty() {
                    ui.label(RichText::new("The versions don't differ.").color(theme.text_muted));
                }
                for (i, c) in view.chunks.iter().enumerate() {
                    let Some((base, ours, theirs)) = view.texts(i) else { continue };
                    let res = view.resolutions.get(i).copied().flatten();
                    let selected = view.selected == i;
                    let stroke_color = if selected { theme.accent } else { theme.border };
                    let r = Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, stroke_color)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let color = if c.kind == ChunkKind::Conflict { theme.vcs_conflict } else { theme.text_muted };
                            ui.label(RichText::new(format!("#{} {}", i + 1, kind_label(c.kind))).strong().color(color));
                            match res {
                                Some(r) => ui.label(RichText::new(resolution_label(r)).small().color(theme.success)),
                                None => ui.label(RichText::new("unresolved").small().color(theme.warning)),
                            };
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let mut set = |ui: &mut Ui, label: &str, tip: &str, value: &str| {
                                    if ui.small_button(label).on_hover_text(tip).clicked() {
                                        action =
                                            Some(("vcs.merge.resolve", json!({ "chunk": i, "resolution": value })));
                                    }
                                };
                                set(ui, "Reset", "Mark unresolved", "none");
                                set(ui, "Ignore", "Keep the base lines", "base");
                                set(ui, "Both ⇄", "Accept theirs, then yours", "theirsThenOurs");
                                set(ui, "Both", "Accept yours, then theirs", "both");
                                set(ui, "« Accept Right", "Accept theirs", "theirs");
                                set(ui, "Accept Left »", "Accept yours", "ours");
                            });
                        });
                        ui.horizontal_top(|ui| {
                            let fill = |changed: bool| if changed { theme.diff_modified_bg } else { theme.editor_bg };
                            let ours_changed = c.kind != ChunkKind::Theirs;
                            let theirs_changed = c.kind != ChunkKind::Ours;
                            text_column(ui, &theme, "Yours", &ours, fill(ours_changed), col_w);
                            text_column(ui, &theme, "Base", &base, theme.editor_bg, col_w);
                            text_column(ui, &theme, "Theirs", &theirs, fill(theirs_changed), col_w);
                        });
                    });
                    if r.response.interact(egui::Sense::click()).clicked() {
                        action = Some(("vcs.merge.select", json!({ "chunk": i })));
                    }
                    ui.add_space(4.0);
                }
            });
            ui.separator();
            egui::CollapsingHeader::new("Result").default_open(true).show(ui, |ui| {
                egui::ScrollArea::vertical().id_salt("vcs-merge-result").max_height(130.0).show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(&result).font(FontId::monospace(12.0)).color(theme.text_editor))
                            .wrap(),
                    );
                });
            });
            if let Some(e) = &s.vcs.dialog_error {
                ui.add(egui::Label::new(RichText::new(e).color(theme.error).small()).wrap());
            }
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                let apply = egui::Button::new(RichText::new("Apply").strong()).fill(theme.accent);
                if ui
                    .add_enabled(unresolved == 0 && s.vcs.op.is_none(), apply)
                    .on_disabled_hover_text("Resolve every conflict first")
                    .clicked()
                {
                    action = Some(("vcs.merge.apply", Value::Null));
                }
                if ui.button("Cancel").clicked() {
                    action = Some(("vcs.merge.close", Value::Null));
                }
                if unresolved > 0
                    && ui
                        .button("Save with Markers")
                        .on_hover_text("Write the file with conflict markers; it stays conflicted")
                        .clicked()
                {
                    action = Some(("vcs.merge.apply", json!({ "allowUnresolved": true })));
                }
            });
        });
    if !open {
        action = Some(("vcs.merge.close", Value::Null));
    }
    if let Some((id, params)) = action {
        run_command(s, st, id, &params);
    }
}

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
