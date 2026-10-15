//! GitHub in the Git tool window and dialogs: the Pull Requests tab (filters, list, details,
//! changed files with their diff, read-only review comments, checkout, merge with a method) and
//! the Share Project on GitHub / Create Gist dialogs. Thin: every action is a `vcs.pr.*` /
//! `vcs.github.*` command; the state is `Session::vcs.prs`.

use egui::{Align, FontId, Layout, RichText, TextEdit, Ui, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::vcs::Dialog;
use codecraft_engine::vcs_git::github::{GH_INSTALL_HINT, MergeMethod};

use crate::theme::Theme;

type Action = Option<(&'static str, Value)>;

/// A unified patch with coloured lines.
pub fn patch_view(ui: &mut Ui, theme: &Theme, salt: &str, patch: &str, max_height: f32) {
    let font = FontId::monospace(12.0);
    let row_h = ui.fonts_mut(|f| f.row_height(&font));
    let lines: Vec<&str> = patch.lines().collect();
    egui::ScrollArea::both().id_salt(salt).max_height(max_height.max(60.0)).auto_shrink([false, true]).show_rows(
        ui,
        row_h,
        lines.len(),
        |ui, range| {
            for line in lines.get(range).unwrap_or_default() {
                let (color, bg) = if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
                    (theme.text_muted, None)
                } else if line.starts_with('+') {
                    (theme.diff_added, Some(theme.diff_added_bg))
                } else if line.starts_with('-') {
                    (theme.diff_removed, Some(theme.diff_removed_bg))
                } else if line.starts_with("@@") {
                    (theme.accent, None)
                } else {
                    (theme.text_editor, None)
                };
                let mut text = RichText::new(*line).font(font.clone()).color(color);
                if let Some(bg) = bg {
                    text = text.background_color(bg);
                }
                ui.add(egui::Label::new(text).extend());
            }
        },
    );
}

fn state_color(theme: &Theme, state: &str) -> egui::Color32 {
    match state {
        "OPEN" => theme.success,
        "MERGED" => theme.accent,
        "CLOSED" => theme.error,
        _ => theme.text_muted,
    }
}

/// The Git tool window's Pull Requests tab.
pub fn show(ui: &mut Ui, s: &mut Session, theme: &Theme, cmd: &mut Action) {
    if s.vcs.gh_path().is_none() {
        ui.add(egui::Label::new(RichText::new(GH_INSTALL_HINT).color(theme.warning)).wrap());
        return;
    }
    if s.vcs.github_remote().is_none() {
        ui.label(RichText::new("The repository has no GitHub remote.").color(theme.text_muted));
        return;
    }
    let fetch = |s: &Session| {
        let f = &s.vcs.prs.filter;
        json!({ "state": f.state, "author": f.author, "label": f.label, "search": f.search, "fetch": true })
    };
    ui.horizontal(|ui| {
        let before = s.vcs.prs.filter.state.clone();
        egui::ComboBox::from_id_salt("vcs-pr-state").selected_text(before.as_str()).width(90.0).show_ui(ui, |ui| {
            for st in ["open", "closed", "merged", "all"] {
                ui.selectable_value(&mut s.vcs.prs.filter.state, st.to_string(), st);
            }
        });
        let mut enter = false;
        for (hint, label) in [("Author", 0), ("Label", 1), ("Search", 2)] {
            let f = &mut s.vcs.prs.filter;
            let field = match label {
                0 => &mut f.author,
                1 => &mut f.label,
                _ => &mut f.search,
            };
            let resp = ui.add(TextEdit::singleline(field).hint_text(hint).desired_width(110.0));
            let name = format!("Filter by {}", hint.to_lowercase());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, &name));
            enter |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        }
        if ui.button("Refresh").clicked() || enter || before != s.vcs.prs.filter.state {
            *cmd = Some(("vcs.pr.list", fetch(s)));
        }
        if s.vcs.prs.busy() {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    if let Some(e) = &s.vcs.prs.error {
        ui.add(egui::Label::new(RichText::new(e).color(theme.error).small()).wrap());
    }
    ui.separator();
    egui::Panel::left("vcs-pr-list").resizable(true).default_size(320.0).size_range(180.0..=900.0).show(ui, |ui| {
        if s.vcs.prs.loaded && s.vcs.prs.list.is_empty() {
            ui.label(RichText::new("No pull requests").color(theme.text_muted));
        }
        egui::ScrollArea::vertical().id_salt("vcs-pr-rows").auto_shrink([false, false]).show(ui, |ui| {
            for pr in &s.vcs.prs.list {
                let sel = s.vcs.prs.selected == Some(pr.number);
                let resp = ui.add(
                    egui::Button::selectable(sel, RichText::new(format!("#{} {}", pr.number, pr.title)).strong())
                        .frame_when_inactive(false),
                );
                if resp.clicked() {
                    *cmd = Some(("vcs.pr.select", json!({ "number": pr.number })));
                }
                let mut meta = format!("{} · {} → {}", pr.author.login, pr.head_ref_name, pr.base_ref_name);
                if pr.is_draft {
                    meta.push_str(" · draft");
                }
                for l in &pr.labels {
                    meta.push_str(&format!(" · {}", l.name));
                }
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new(&pr.state).small().color(state_color(theme, &pr.state)));
                    ui.label(RichText::new(meta).small().color(theme.text_muted));
                });
                ui.add_space(2.0);
            }
        });
    });
    details(ui, s, theme, cmd);
}

fn details(ui: &mut Ui, s: &mut Session, theme: &Theme, cmd: &mut Action) {
    let Some(number) = s.vcs.prs.selected else {
        ui.label(RichText::new("Select a pull request").color(theme.text_muted));
        return;
    };
    if let Some(e) = &s.vcs.prs.details_error {
        ui.add(egui::Label::new(RichText::new(e).color(theme.error).small()).wrap());
    }
    let Some(d) = s.vcs.prs.details.clone() else {
        ui.add(egui::Spinner::new());
        return;
    };
    let busy = s.vcs.op.is_some();
    egui::ScrollArea::vertical().id_salt("vcs-pr-details").auto_shrink([false, false]).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("#{} {}", d.number, d.title)).strong().size(14.0));
            ui.label(RichText::new(&d.state).small().color(state_color(theme, &d.state)));
            if d.is_draft {
                ui.label(RichText::new("draft").small().color(theme.text_muted));
            }
        });
        ui.label(
            RichText::new(format!(
                "{} wants to merge {} into {} · +{} −{}{}",
                d.author.login,
                d.head_ref_name,
                d.base_ref_name,
                d.additions,
                d.deletions,
                if d.mergeable.is_empty() { String::new() } else { format!(" · {}", d.mergeable.to_lowercase()) }
            ))
            .small()
            .color(theme.text_muted),
        );
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("Checkout")).on_hover_text("gh pr checkout").clicked() {
                *cmd = Some(("vcs.pr.checkout", json!({ "number": number })));
            }
            if ui.button("Open in Browser").clicked() {
                *cmd = Some(("vcs.pr.openInBrowser", json!({ "number": number })));
            }
            ui.separator();
            let method = s.vcs.prs.merge_method;
            egui::ComboBox::from_id_salt("vcs-pr-merge-method").selected_text(method.label()).show_ui(ui, |ui| {
                for m in [MergeMethod::Merge, MergeMethod::Squash, MergeMethod::Rebase] {
                    ui.selectable_value(&mut s.vcs.prs.merge_method, m, m.label());
                }
            });
            ui.checkbox(&mut s.vcs.prs.delete_branch, "Delete branch");
            let open = d.state == "OPEN";
            if ui.add_enabled(open && !busy, egui::Button::new("Merge")).clicked() {
                let m = match s.vcs.prs.merge_method {
                    MergeMethod::Merge => "merge",
                    MergeMethod::Squash => "squash",
                    MergeMethod::Rebase => "rebase",
                };
                *cmd = Some((
                    "vcs.pr.merge",
                    json!({ "number": number, "method": m, "deleteBranch": s.vcs.prs.delete_branch }),
                ));
            }
        });
        if !d.labels.is_empty() {
            let names: Vec<&str> = d.labels.iter().map(|l| l.name.as_str()).collect();
            ui.label(RichText::new(format!("Labels: {}", names.join(", "))).small());
        }
        if !d.body.trim().is_empty() {
            ui.add_space(4.0);
            ui.add(egui::Label::new(RichText::new(&d.body)).wrap());
        }
        if !d.reviews.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new("Reviews").strong());
            for r in &d.reviews {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&r.author.login).strong().small());
                    ui.label(RichText::new(r.state.to_lowercase().replace('_', " ")).small().color(theme.text_muted));
                    if !r.body.trim().is_empty() {
                        ui.label(RichText::new(&r.body).small());
                    }
                });
            }
        }
        for c in &d.comments {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&c.author.login).strong().small());
                ui.label(RichText::new(&c.body).small());
            });
        }
        ui.separator();
        ui.label(RichText::new(format!("Files ({})", s.vcs.prs.diff.len().max(d.files.len()))).strong());
        if s.vcs.prs.diff_loading {
            ui.add(egui::Spinner::new().size(12.0));
        }
        for (path, _) in s.vcs.prs.diff.clone() {
            let sel = s.vcs.prs.selected_file.as_deref() == Some(path.as_str());
            let stats = d.files.iter().find(|f| f.path == path).map(|f| format!("  +{} −{}", f.additions, f.deletions));
            let n_comments = s.vcs.prs.comments_on(&path).count();
            let mut label = path.clone();
            if let Some(st) = stats {
                label.push_str(&st);
            }
            if n_comments > 0 {
                label.push_str(&format!("  · {n_comments} comment{}", if n_comments == 1 { "" } else { "s" }));
            }
            if ui.add(egui::Button::selectable(sel, label).frame_when_inactive(false)).clicked() {
                *cmd = Some(("vcs.pr.selectFile", json!({ "path": path })));
            }
        }
        if let Some(path) = s.vcs.prs.selected_file.clone() {
            ui.separator();
            ui.label(RichText::new(&path).strong().small());
            if let Some(p) = s.vcs.prs.file_patch() {
                patch_view(ui, theme, "vcs-pr-patch", p, 360.0);
            }
            let comments: Vec<_> = s.vcs.prs.comments_on(&path).cloned().collect();
            if !comments.is_empty() {
                ui.label(RichText::new("Review comments").strong().small());
            }
            for c in comments {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    let line =
                        c.line.or(c.original_line).map(|l| format!("line {l}")).unwrap_or_else(|| "outdated".into());
                    ui.label(RichText::new(format!("{} · {line}", c.user.login)).small().color(theme.text_muted));
                    ui.add(egui::Label::new(RichText::new(&c.body).small()).wrap());
                });
            }
            if let Some(e) = &s.vcs.prs.comments_error {
                ui.label(RichText::new(e).small().color(theme.error));
            }
        }
    });
}

/// The Share Project on GitHub and Create Gist dialogs; returns (action, close).
pub fn dialogs(ui: &mut Ui, s: &Session, theme: &Theme, d: &mut Dialog, busy: bool, enter: bool) -> (Action, bool) {
    let mut action = None;
    let mut close = false;
    let has_gh = s.vcs.gh_path().is_some();
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
        Dialog::ShareOnGitHub { name, description, private, remote } => {
            egui::Grid::new("vcs-share").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Repository name");
                let resp = ui.add(TextEdit::singleline(name).desired_width(300.0));
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Repository name"));
                ui.end_row();
                ui.label("Remote");
                ui.add(TextEdit::singleline(remote).desired_width(300.0));
                ui.end_row();
                ui.label("Description");
                ui.add(TextEdit::multiline(description).desired_rows(2).desired_width(300.0));
                ui.end_row();
            });
            ui.checkbox(private, "Private");
            if s.vcs.repo.is_none() {
                ui.label(
                    RichText::new("The project will become a Git repository with a first commit of all its files.")
                        .small()
                        .color(theme.text_muted),
                );
            }
            error(ui);
            let can = has_gh && !busy && !name.trim().is_empty();
            let (ok, cancel) = ok_cancel(ui, "Share", can);
            if ok || (enter && can) {
                action = Some((
                    "vcs.github.share",
                    json!({ "name": name.trim(), "description": description, "private": *private, "remote": remote.trim() }),
                ));
            }
            close |= cancel;
        }
        Dialog::CreateGist { path, description, public } => {
            ui.label(RichText::new(path.as_str()).strong());
            ui.add(TextEdit::singleline(description).hint_text("Description").desired_width(f32::INFINITY));
            ui.checkbox(public, "Public (otherwise secret)");
            error(ui);
            let can = has_gh && !busy;
            let (ok, cancel) = ok_cancel(ui, "Create Gist", can);
            if ok || (enter && can) {
                action = Some((
                    "vcs.github.createGist",
                    json!({ "path": path, "description": description, "public": *public, "open": true }),
                ));
            }
            close |= cancel;
        }
        _ => {}
    }
    (action, close)
}
