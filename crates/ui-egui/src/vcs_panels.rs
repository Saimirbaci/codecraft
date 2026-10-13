//! The Commit and Git tool windows, the branch widget and its popup, the Git dialogs, and the VCS
//! decorations painted by the editor (gutter change markers, annotations, conflict blocks), the
//! project tree and the editor tabs. OWNED BY the Git track.
//!
//! All state the control channel may need lives in `Session::vcs`; this module keeps only view
//! state ([`VcsUi`]) and turns clicks into `git.*` / `vcs.*` commands.

use std::path::Path;
use std::sync::Arc;

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Id, Key, Layout, Margin, Pos2, Rect, RichText, Sense, Stroke,
    TextEdit, Ui, pos2, vec2,
};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::vcs::{CommitTab, ConsoleKind, Dialog, FileDiff, GitTab};
use codecraft_engine::vcs_git::lines::LineChange;
use codecraft_engine::vcs_git::ops::ResetMode;
use codecraft_engine::vcs_git::{Blame, Commit, FileState, FileStatus, GraphRow, LineChangeKind, RefKind, RefLabel};
use codecraft_workspace::DocId;

use crate::diff_view::{self, DiffOptions};
use crate::theme::Theme;
use crate::{UiState, run_command};

/// View state of the Git UI (not needed by agents).
#[derive(Default)]
pub struct VcsUi {
    /// Diffs side by side (else unified).
    pub side_by_side: bool,
    /// The branches popup's open state last frame (synced with `Session::vcs.branches_popup`).
    popup_open_last: bool,
    branch_filter: String,
    /// Where the hunk popup opens (the clicked marker).
    pub hunk_anchor: Option<Pos2>,
    hunk_shown_last: bool,
    /// Hard reset needs an extra confirmation.
    confirm_hard: bool,
    log_filter: Option<String>,
    log_filter_changed: Option<std::time::Instant>,
}

// ---------------------------------------------------------------- colours

/// The colour of a file in the given VCS state.
pub fn state_color(theme: &Theme, state: FileState) -> Color32 {
    match state {
        FileState::Modified | FileState::Renamed => theme.vcs_modified,
        FileState::Added => theme.vcs_added,
        FileState::Deleted => theme.vcs_deleted,
        FileState::Untracked => theme.vcs_untracked,
        FileState::Ignored => theme.vcs_ignored,
        FileState::Conflicted => theme.vcs_conflict,
    }
}

/// The colour of a file's name (editor tabs, Project tree), if it has a VCS state.
pub fn file_color(s: &Session, theme: &Theme, path: &Path) -> Option<Color32> {
    s.vcs.file_state(path).map(|st| state_color(theme, st))
}

/// The colour of a Project tree row: files by state, directories when ignored or when they
/// contain changes.
pub fn tree_color(s: &Session, theme: &Theme, path: &Path, is_dir: bool) -> Option<Color32> {
    if is_dir { s.vcs.dir_state(path).map(|st| state_color(theme, st)) } else { file_color(s, theme, path) }
}

fn lane_color(theme: &Theme, lane: usize) -> Color32 {
    let palette =
        [theme.accent, theme.success, theme.warning, theme.constant, theme.function, theme.keyword, theme.string];
    palette[lane % palette.len()]
}

// ---------------------------------------------------------------- dates

/// `YYYY-MM-DD` of a Unix time (UTC; civil-from-days).
fn date(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// "5 minutes ago", "Yesterday", or the date.
fn when(ts: i64) -> String {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(ts);
    let ago = now.saturating_sub(ts);
    match ago {
        i64::MIN..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", ago / 60),
        3600..=86_399 => format!("{} h ago", ago / 3600),
        86_400..=172_799 => "Yesterday".into(),
        _ => date(ts),
    }
}

fn file_name(rel: &str) -> (&str, &str) {
    match rel.rsplit_once('/') {
        Some((dir, name)) => (name, dir),
        None => (rel, ""),
    }
}

// ---------------------------------------------------------------- toolbar branch widget

/// The branch widget of the main toolbar (New UI): the current branch with ↑↓ counts; clicking
/// opens the branches popup. Also draws the Git dialogs, which float above the window.
pub fn branch_widget(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    overlays(ui.ctx(), s, st);
    if s.vcs.repo.is_none() {
        return;
    }
    let theme = st.theme.clone();
    let b = &s.vcs.status.branch;
    let mut text = s.vcs.head_label();
    if b.ahead > 0 {
        text.push_str(&format!(" ↑{}", b.ahead));
    }
    if b.behind > 0 {
        text.push_str(&format!(" ↓{}", b.behind));
    }
    if s.vcs.state != codecraft_engine::vcs_git::RepoState::Normal {
        text.push_str(&format!(" ({})", s.vcs.state.label()));
    }
    text.push_str(" ▾");
    let tip = match &b.upstream {
        Some(u) => format!("Git Branch: {} → {u}", s.vcs.head_label()),
        None => format!("Git Branch: {} (no upstream)", s.vcs.head_label()),
    };
    let (icon_rect, _) = ui.allocate_exact_size(vec2(14.0, 18.0), Sense::hover());
    paint_branch_icon(ui.painter(), icon_rect, theme.text_muted);
    let resp = ui.add(egui::Button::new(RichText::new(text).strong()).frame(false)).on_hover_text(tip);
    let id = egui::Popup::default_response_id(&resp);
    if s.vcs.branches_popup != st.vcs.popup_open_last {
        if s.vcs.branches_popup {
            egui::Popup::open_id(ui.ctx(), id);
        } else {
            egui::Popup::close_id(ui.ctx(), id);
        }
    }
    egui::Popup::menu(&resp)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(340.0)
        .show(|ui| branches_menu(ui, s, st, &theme));
    let open = egui::Popup::is_id_open(ui.ctx(), id);
    s.vcs.branches_popup = open;
    st.vcs.popup_open_last = open;
}

/// A small branch glyph (two commits and a fork), painted so it never depends on the font.
fn paint_branch_icon(painter: &egui::Painter, r: Rect, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    let (x0, x1) = (r.left() + 4.0, r.left() + 11.0);
    let (top, mid, bottom) = (r.center().y - 6.0, r.center().y - 1.0, r.center().y + 6.0);
    painter.line_segment([pos2(x0, top), pos2(x0, bottom)], stroke);
    painter.line_segment([pos2(x1, top + 2.0), pos2(x1, mid)], stroke);
    painter.line_segment([pos2(x1, mid), pos2(x0, bottom - 2.0)], stroke);
    painter.circle_filled(pos2(x0, bottom), 2.0, color);
    painter.circle_filled(pos2(x1, top + 1.0), 2.0, color);
    painter.circle_filled(pos2(x0, top), 2.0, color);
}

fn branches_menu(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme) {
    let mut cmd: Option<(&'static str, Value)> = None;
    ui.add(
        TextEdit::singleline(&mut st.vcs.branch_filter)
            .hint_text("Search for branches and actions")
            .desired_width(320.0),
    );
    let shortcut = |id: &str| crate::keymap::shortcut_label(id).unwrap_or_default();
    for (id, label) in [
        ("git.updateProject", "Update Project…"),
        ("git.commit", "Commit…"),
        ("git.push", "Push…"),
        ("git.newBranch", "New Branch…"),
        ("git.fetch", "Fetch"),
    ] {
        let enabled = s.enabled(id).is_ok();
        let button = egui::Button::new(label).shortcut_text(RichText::new(shortcut(id)).color(theme.text_muted));
        if ui.add_enabled(enabled, button).clicked() {
            cmd = Some((id, Value::Null));
            ui.close();
        }
    }
    let conflicted = s.vcs.state != codecraft_engine::vcs_git::RepoState::Normal || !s.vcs.conflicts().is_empty();
    if conflicted && ui.button(RichText::new("Resolve Conflicts…").color(theme.vcs_conflict)).clicked() {
        cmd = Some(("vcs.showConflicts", Value::Null));
        ui.close();
    }
    let filter = st.vcs.branch_filter.to_lowercase();
    let current = s.vcs.branch_name().unwrap_or("").to_string();
    for remote in [false, true] {
        let list: Vec<_> = s
            .vcs
            .branches
            .iter()
            .filter(|b| b.remote == remote && (filter.is_empty() || b.name.to_lowercase().contains(&filter)))
            .cloned()
            .collect();
        if list.is_empty() {
            continue;
        }
        ui.separator();
        ui.label(RichText::new(if remote { "Remote" } else { "Local" }).color(theme.text_muted).small());
        egui::ScrollArea::vertical().id_salt(("vcs-branches", remote)).max_height(260.0).show(ui, |ui| {
            for b in list {
                let mut label = if b.current { format!("✓ {}", b.name) } else { format!("   {}", b.name) };
                if b.ahead > 0 {
                    label.push_str(&format!("  ↑{}", b.ahead));
                }
                if b.behind > 0 {
                    label.push_str(&format!("  ↓{}", b.behind));
                }
                if b.gone {
                    label.push_str("  (gone)");
                }
                ui.menu_button(label, |ui| {
                    let name = b.name.clone();
                    if !b.current && ui.button("Checkout").clicked() {
                        cmd = Some(("vcs.checkout", json!({ "branch": name, "remote": remote })));
                        ui.close();
                    }
                    if ui.button(format!("New Branch from '{name}'…")).clicked() {
                        cmd = Some(("git.newBranch", json!({ "from": name })));
                        ui.close();
                    }
                    if !b.current && !current.is_empty() {
                        if ui.button(format!("Merge '{name}' into '{current}'")).clicked() {
                            cmd = Some(("vcs.merge", json!({ "branch": name })));
                            ui.close();
                        }
                        if ui.button(format!("Rebase '{current}' onto '{name}'")).clicked() {
                            cmd = Some(("vcs.rebase", json!({ "onto": name })));
                            ui.close();
                        }
                    }
                    if b.current && ui.button("Push…").clicked() {
                        cmd = Some(("git.push", Value::Null));
                        ui.close();
                    }
                    ui.separator();
                    if !remote && ui.button("Rename…").clicked() {
                        s.vcs.dialog = Some(Dialog::RenameBranch { old: name.clone(), name: name.clone() });
                        s.vcs.dialog_error = None;
                        ui.close();
                    }
                    if !b.current && ui.button(RichText::new("Delete").color(theme.error)).clicked() {
                        s.vcs.dialog = Some(Dialog::DeleteBranch { name: name.clone(), remote, force: false });
                        s.vcs.dialog_error = None;
                        ui.close();
                    }
                });
            }
        });
    }
    if let Some((id, params)) = cmd {
        s.vcs.branches_popup = false;
        run_command(s, st, id, &params);
    }
}

// ---------------------------------------------------------------- Commit tool window

pub fn show(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    if s.vcs.repo.is_none() {
        not_a_repo(ui, s, st);
        return;
    }
    let mut cmd: Option<(&'static str, Value)> = None;
    ui.horizontal(|ui| {
        let stashes = s.vcs.stashes.len();
        for (tab, label) in
            [(CommitTab::Changes, "Changes".to_string()), (CommitTab::Stashes, format!("Stashes ({stashes})"))]
        {
            if ui.selectable_label(s.vcs.commit_tab == tab, label).clicked() {
                s.vcs.commit_tab = tab;
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon(ui, &theme, "⟳", "Refresh") {
                cmd = Some(("vcs.refresh", Value::Null));
            }
            if ui.small_button("Rollback…").on_hover_text("Roll back the checked changes").clicked() {
                cmd = Some(("git.uncommittedChanges.rollback", Value::Null));
            }
            if s.vcs.op.is_some() {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
    });
    ui.separator();
    match s.vcs.commit_tab {
        CommitTab::Changes => changes_view(ui, s, st, &theme, &mut cmd),
        CommitTab::Stashes => stashes_view(ui, s, &theme, &mut cmd),
    }
    if let Some((id, params)) = cmd {
        run_command(s, st, id, &params);
    }
}

fn not_a_repo(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let msg = s.vcs.unavailable.clone().unwrap_or_else(|| "The project is not under Git version control.".into());
    ui.label(RichText::new(msg).color(st.theme.text_muted));
    if ui.button("Clone Repository…").clicked() {
        run_command(s, st, "git.clone", &Value::Null);
    }
}

fn icon(ui: &mut Ui, theme: &Theme, text: &str, tip: &str) -> bool {
    ui.add(egui::Button::new(RichText::new(text).color(theme.text).size(14.0)).frame(false))
        .on_hover_text(tip)
        .clicked()
}

fn changes_view(
    ui: &mut Ui,
    s: &mut Session,
    st: &mut UiState,
    theme: &Theme,
    cmd: &mut Option<(&'static str, Value)>,
) {
    // The commit box at the bottom (message, Amend, buttons).
    egui::Panel::bottom("vcs-commit-box")
        .resizable(false)
        .frame(Frame::new().inner_margin(Margin::symmetric(0, 4)))
        .show(ui, |ui| commit_box(ui, s, st, theme, cmd));
    // The selected change's diff.
    if let Some(preview) = s.vcs.preview.clone() {
        egui::Panel::bottom("vcs-change-preview")
            .resizable(true)
            .default_size(ui.available_height() * 0.45)
            .size_range(80.0..=1200.0)
            .frame(Frame::new().inner_margin(Margin::symmetric(0, 4)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&preview.path).strong().size(12.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon(ui, theme, "×", "Close the diff") {
                            s.vcs.preview = None;
                            s.vcs.commit.selected = None;
                        }
                        if ui.selectable_label(st.vcs.side_by_side, RichText::new("Side by side").small()).clicked() {
                            st.vcs.side_by_side = !st.vcs.side_by_side;
                        }
                        if ui.small_button("Diff Window").on_hover_text("Open in a diff window").clicked() {
                            *cmd = Some(("vcs.compare", json!({ "path": preview.path, "rev": "HEAD" })));
                        }
                    });
                });
                file_diff_view(ui, st, theme, "vcs-preview", &preview, ui.available_height());
            });
    }
    let changes: Vec<FileStatus> = s.vcs.changes().into_iter().cloned().collect();
    let unversioned: Vec<FileStatus> = s.vcs.unversioned().into_iter().cloned().collect();
    egui::ScrollArea::vertical().id_salt("vcs-changes").auto_shrink([false, false]).show(ui, |ui| {
        let conflicts: Vec<&FileStatus> = changes.iter().filter(|f| f.conflict.is_some()).collect();
        if !conflicts.is_empty() || s.vcs.state != codecraft_engine::vcs_git::RepoState::Normal {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Merge Conflicts ({})", conflicts.len())).color(theme.vcs_conflict).strong(),
                );
                if ui.small_button("Resolve…").clicked() {
                    *cmd = Some(("vcs.showConflicts", Value::Null));
                }
            });
        }
        if changes.is_empty() && unversioned.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No changes").color(theme.text_muted));
        }
        group(ui, s, st, theme, "Changes", &changes, cmd);
        group(ui, s, st, theme, "Unversioned Files", &unversioned, cmd);
        if s.vcs.status.truncated {
            ui.label(RichText::new("Too many changes: the list is cut.").color(theme.warning).small());
        }
    });
}

fn group(
    ui: &mut Ui,
    s: &mut Session,
    st: &mut UiState,
    theme: &Theme,
    title: &str,
    files: &[FileStatus],
    cmd: &mut Option<(&'static str, Value)>,
) {
    if files.is_empty() {
        return;
    }
    let included = files.iter().filter(|f| s.vcs.is_included(f)).count();
    let id = Id::new(("vcs-group", title));
    let mut open = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(true);
    ui.horizontal(|ui| {
        if ui.add(egui::Button::new(RichText::new(if open { "⏷" } else { "⏵" }).size(10.0)).frame(false)).clicked()
        {
            open = !open;
        }
        let mut all = included == files.len();
        let resp = ui.add(egui::Checkbox::without_text(&mut all).indeterminate(included > 0 && included < files.len()));
        let label = format!("Include {title}");
        resp.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, included == files.len(), &label)
        });
        if resp.changed() {
            for f in files {
                s.vcs.commit.excluded.remove(&f.path);
                if f.untracked {
                    if all {
                        s.vcs.commit.included_untracked.insert(f.path.clone());
                    } else {
                        s.vcs.commit.included_untracked.remove(&f.path);
                    }
                } else if !all {
                    s.vcs.commit.excluded.insert(f.path.clone());
                }
            }
        }
        ui.label(RichText::new(title).strong());
        ui.label(
            RichText::new(format!("{} file{}", files.len(), if files.len() == 1 { "" } else { "s" }))
                .color(theme.text_muted)
                .small(),
        );
    });
    ui.data_mut(|d| d.insert_temp(id, open));
    if !open {
        return;
    }
    for f in files {
        let selected = s.vcs.commit.selected.as_deref() == Some(f.path.as_str());
        let (name, dir) = file_name(&f.path);
        let color = state_color(theme, f.state());
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            let mut inc = s.vcs.is_included(f);
            let resp = ui.add(egui::Checkbox::without_text(&mut inc));
            let label = format!("Include {}", f.path);
            resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, inc, &label));
            if resp.changed() {
                *cmd = Some(("vcs.include", json!({ "path": f.path, "included": inc })));
            }
            let mut text = egui::text::LayoutJob::default();
            text.append(
                name,
                0.0,
                egui::TextFormat { font_id: FontId::proportional(13.0), color, ..Default::default() },
            );
            if let Some(orig) = &f.orig_path {
                text.append(
                    &format!("  ← {orig}"),
                    0.0,
                    egui::TextFormat {
                        font_id: FontId::proportional(12.0),
                        color: theme.text_muted,
                        ..Default::default()
                    },
                );
            }
            if !dir.is_empty() {
                text.append(
                    &format!("  {dir}"),
                    0.0,
                    egui::TextFormat {
                        font_id: FontId::proportional(12.0),
                        color: theme.text_muted,
                        ..Default::default()
                    },
                );
            }
            let row = ui
                .add(egui::Button::selectable(selected, text).frame_when_inactive(false))
                .on_hover_text(f.state().label());
            if row.clicked() {
                *cmd = Some(("vcs.selectChange", json!({ "path": f.path })));
            }
            if row.double_clicked() {
                *cmd = Some(("vcs.compare", json!({ "path": f.path, "rev": "HEAD" })));
            }
            row.context_menu(|ui| {
                if ui.button("Show Diff").clicked() {
                    *cmd = Some(("vcs.compare", json!({ "path": f.path, "rev": "HEAD" })));
                    ui.close();
                }
                if ui.button("Jump to Source").clicked() {
                    if let Some(abs) = s.vcs.abs(&f.path) {
                        *cmd = Some(("file.open", json!({ "path": abs })));
                    }
                    ui.close();
                }
                if !f.untracked && ui.button("Rollback…").clicked() {
                    *cmd = Some(("git.uncommittedChanges.rollback", json!({ "files": [f.path] })));
                    ui.close();
                }
                if f.conflict.is_some() && ui.button("Resolve Conflicts…").clicked() {
                    *cmd = Some(("vcs.showConflicts", Value::Null));
                    ui.close();
                }
            });
        });
    }
    let _ = st;
}

fn commit_box(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme, cmd: &mut Option<(&'static str, Value)>) {
    ui.horizontal(|ui| {
        let mut amend = s.vcs.commit.amend;
        if ui.checkbox(&mut amend, "Amend").on_hover_text("Amend the last commit").changed() {
            *cmd = Some(("vcs.setAmend", json!({ "amend": amend })));
        }
        ui.checkbox(&mut s.vcs.commit.sign_off, "Sign-off");
    });
    let id = Id::new("vcs-commit-message");
    // Cmd+Enter commits (and must not insert a newline).
    let focused = ui.memory(|m| m.has_focus(id));
    if focused && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::Enter)) {
        *cmd = Some(("vcs.commit", Value::Null));
    }
    let resp = ui.add(
        TextEdit::multiline(&mut s.vcs.commit.message)
            .id(id)
            .hint_text("Commit Message")
            .desired_rows(4)
            .desired_width(f32::INFINITY)
            .font(FontId::monospace(13.0)),
    );
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Commit Message"));
    if s.vcs.focus_message {
        resp.request_focus();
        s.vcs.focus_message = false;
    }
    let busy = s.vcs.op.clone();
    ui.horizontal(|ui| {
        let can = busy.is_none() && (!s.vcs.commit.message.trim().is_empty() || s.vcs.commit.amend);
        let label = if s.vcs.commit.amend { "Amend Commit" } else { "Commit" };
        if ui
            .add_enabled(can, egui::Button::new(RichText::new(label).strong()).fill(theme.accent))
            .on_hover_text("⌘↩")
            .clicked()
        {
            *cmd = Some(("vcs.commit", Value::Null));
        }
        if ui.add_enabled(can, egui::Button::new("Commit and Push…")).clicked() {
            *cmd = Some(("vcs.commit", json!({ "push": true })));
        }
        let generating = s.vcs.generating;
        let provider = codecraft_engine::commit_ai::Provider::from_id(&s.ws.settings.commit_ai_provider)
            .map_or("the language model", |p| p.label());
        if ui
            .add_enabled(!generating && !s.vcs.included_paths().is_empty(), egui::Button::new("Generate"))
            .on_hover_text(format!("Write the commit message with {provider} (choose it in Settings)"))
            .clicked()
        {
            *cmd = Some(("vcs.generateMessage", Value::Null));
        }
        if generating {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("Generating…").color(theme.text_muted).small());
        }
        if let Some(op) = &busy {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new(&op.label).color(theme.text_muted).small());
        }
    });
    let _ = st;
}

fn stashes_view(ui: &mut Ui, s: &mut Session, theme: &Theme, cmd: &mut Option<(&'static str, Value)>) {
    ui.horizontal(|ui| {
        if ui.button("Stash Changes…").clicked() {
            *cmd = Some(("git.uncommittedChanges.stashChanges", Value::Null));
        }
    });
    if s.vcs.stashes.is_empty() {
        ui.label(RichText::new("No stashes").color(theme.text_muted));
        return;
    }
    egui::ScrollArea::vertical().id_salt("vcs-stashes").auto_shrink([false, false]).show(ui, |ui| {
        for st in s.vcs.stashes.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(st.reference()).monospace().color(theme.text_muted));
                ui.label(&st.message);
                ui.label(RichText::new(when(st.time)).color(theme.text_muted).small());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Drop").clicked() {
                        *cmd = Some(("vcs.dropStash", json!({ "index": st.index })));
                    }
                    if ui.small_button("Pop").clicked() {
                        *cmd = Some(("vcs.unstash", json!({ "index": st.index, "pop": true })));
                    }
                    if ui.small_button("Apply").clicked() {
                        *cmd = Some(("vcs.unstash", json!({ "index": st.index, "pop": false })));
                    }
                });
            });
        }
    });
}

/// A file diff (or why there is none).
fn file_diff_view(ui: &mut Ui, st: &UiState, theme: &Theme, salt: &str, d: &FileDiff, height: f32) {
    if d.loading {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new("Loading…").color(theme.text_muted));
        });
        return;
    }
    if let Some(note) = &d.note {
        ui.label(RichText::new(note).color(theme.text_muted));
        if d.old.is_empty() && d.new.is_empty() {
            return;
        }
    }
    let font = FontId::monospace(12.0);
    let opts = DiffOptions { side_by_side: st.vcs.side_by_side, context: 3, max_height: height.max(60.0) };
    diff_view::show(ui, theme, &font, (salt, &d.path), &d.old, &d.new, &opts);
}

// ---------------------------------------------------------------- Git tool window

/// The Git tool window: Log and Console tabs.
pub fn show_git(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    if s.vcs.repo.is_none() {
        not_a_repo(ui, s, st);
        return;
    }
    let mut cmd: Option<(&'static str, Value)> = None;
    ui.horizontal(|ui| {
        for (tab, label) in [(GitTab::Log, "Log"), (GitTab::Console, "Console")] {
            if ui.selectable_label(s.vcs.git_tab == tab, label).clicked() {
                s.vcs.git_tab = tab;
                if tab == GitTab::Log && !s.vcs.log.loaded {
                    cmd = Some(("vcs.log", json!({})));
                }
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(op) = s.vcs.op.clone() {
                if ui.button("Cancel").on_hover_text("Stop the running Git command").clicked() {
                    cmd = Some(("vcs.cancel", Value::Null));
                }
                ui.label(RichText::new(&op.label).color(theme.text_muted));
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
    });
    match s.vcs.git_tab {
        GitTab::Log => log_view(ui, s, st, &theme, &mut cmd),
        GitTab::Console => console_view(ui, s, &theme),
    }
    if let Some((id, params)) = cmd {
        run_command(s, st, id, &params);
    }
}

fn console_view(ui: &mut Ui, s: &mut Session, theme: &Theme) {
    ui.horizontal(|ui| {
        if ui.small_button("Clear").clicked() {
            s.vcs.console.clear();
        }
    });
    let font = FontId::monospace(12.0);
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let n = s.vcs.console.len();
    egui::ScrollArea::both().id_salt("vcs-console").auto_shrink([false, false]).stick_to_bottom(true).show_rows(
        ui,
        row_h,
        n,
        |ui, range| {
            for e in s.vcs.console.get(range).unwrap_or_default() {
                let (text, color) = match e.kind {
                    ConsoleKind::Command => (format!("› {}", e.text), theme.link),
                    ConsoleKind::Output => (e.text.clone(), theme.text_editor),
                    ConsoleKind::Error => (e.text.clone(), theme.error),
                    ConsoleKind::Info => (e.text.clone(), theme.console_system),
                };
                ui.add(egui::Label::new(RichText::new(text).font(font.clone()).color(color)).extend());
            }
        },
    );
}

fn ref_badge(ui: &Ui, painter: &egui::Painter, x: f32, cy: f32, r: &RefLabel, theme: &Theme) -> f32 {
    let (color, prefix) = match r.kind {
        RefKind::Tag => (theme.warning, "🏷 "),
        RefKind::RemoteBranch => (theme.constant, ""),
        RefKind::Head | RefKind::LocalBranch => (if r.current { theme.success } else { theme.vcs_modified }, ""),
        RefKind::Other => (theme.text_muted, ""),
    };
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(format!("{prefix}{}", r.name), FontId::proportional(11.0), color));
    let rect = Rect::from_min_size(pos2(x, cy - galley.size().y / 2.0 - 1.0), galley.size() + vec2(8.0, 2.0));
    painter.rect(
        rect,
        CornerRadius::same(3),
        color.gamma_multiply(0.15),
        Stroke::new(1.0, color.gamma_multiply(0.6)),
        egui::StrokeKind::Inside,
    );
    painter.galley(pos2(x + 4.0, rect.top() + 1.0), galley, color);
    rect.width() + 4.0
}

fn paint_graph(painter: &egui::Painter, rect: Rect, row: &GraphRow, theme: &Theme) {
    const LANE_W: f32 = 12.0;
    let x = |lane: usize| rect.left() + 8.0 + lane as f32 * LANE_W;
    let (top, mid, bottom) = (rect.top(), rect.center().y, rect.bottom());
    for (a, b) in &row.top {
        painter.line_segment([pos2(x(*a), top), pos2(x(*b), mid)], Stroke::new(1.5, lane_color(theme, *a)));
    }
    for (a, b) in &row.bottom {
        painter.line_segment([pos2(x(*a), mid), pos2(x(*b), bottom)], Stroke::new(1.5, lane_color(theme, *b)));
    }
    painter.circle(pos2(x(row.lane), mid), 3.5, lane_color(theme, row.lane), Stroke::new(1.0, theme.editor_bg));
}

fn log_view(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme, cmd: &mut Option<(&'static str, Value)>) {
    // Filters.
    ui.horizontal(|ui| {
        let text = st.vcs.log_filter.get_or_insert_with(|| s.vcs.log.text.clone());
        let resp = ui.add(TextEdit::singleline(text).hint_text("Text or hash").desired_width(220.0));
        if resp.changed() {
            st.vcs.log_filter_changed = Some(std::time::Instant::now());
        }
        let branch_label = s.vcs.log.branch.clone().unwrap_or_else(|| "All branches".into());
        let mut chosen = s.vcs.log.branch.clone();
        egui::ComboBox::from_id_salt("vcs-log-branch")
            .selected_text(format!("Branch: {branch_label}"))
            .width(180.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut chosen, None, "All branches");
                if s.vcs.status.branch.oid.is_some() {
                    ui.selectable_value(&mut chosen, Some("HEAD".to_string()), "HEAD");
                }
                for b in &s.vcs.branches {
                    ui.selectable_value(&mut chosen, Some(b.name.clone()), &b.name);
                }
            });
        if chosen != s.vcs.log.branch {
            *cmd = Some(("vcs.log", json!({ "branch": chosen })));
        }
        if let Some(p) = s.vcs.log.path.clone()
            && ui.button(format!("Path: {p} ×")).on_hover_text("Clear the path filter").clicked()
        {
            *cmd = Some(("vcs.log", json!({ "path": null })));
        }
        if icon(ui, theme, "⟳", "Refresh") {
            *cmd = Some(("vcs.log", json!({})));
        }
        if s.vcs.log.loading {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    // Apply the text filter once typing pauses.
    if let (Some(t), Some(at)) = (&st.vcs.log_filter, st.vcs.log_filter_changed) {
        if at.elapsed() > std::time::Duration::from_millis(300) {
            *cmd = Some(("vcs.log", json!({ "text": t })));
            st.vcs.log_filter_changed = None;
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
    if let Some(e) = &s.vcs.log.error {
        ui.label(RichText::new(e).color(theme.error).small());
    }
    // Details on the right.
    if s.vcs.log.selected.is_some() {
        egui::Panel::right("vcs-log-details")
            .resizable(true)
            .default_size(ui.available_width() * 0.42)
            .size_range(200.0..=1400.0)
            .frame(Frame::new().inner_margin(Margin::symmetric(6, 2)))
            .show(ui, |ui| details_view(ui, s, st, theme, cmd));
    }
    // The commit list.
    let row_h = 22.0;
    let commits: Arc<Vec<Commit>> = Arc::new(s.vcs.log.commits.clone());
    let graph = s.vcs.log.graph.clone();
    let lanes = graph.iter().map(|g| g.width).max().unwrap_or(1).min(12);
    let graph_w = 10.0 + lanes as f32 * 12.0;
    let n = commits.len();
    if n == 0 && !s.vcs.log.loading {
        ui.label(RichText::new("No commits").color(theme.text_muted));
        return;
    }
    egui::ScrollArea::vertical().id_salt("vcs-log").auto_shrink([false, false]).show_rows(ui, row_h, n, |ui, range| {
        if range.end + 20 >= n && s.vcs.log.has_more && !s.vcs.log.loading {
            *cmd = Some(("vcs.log.more", Value::Null));
        }
        for i in range {
            let (Some(c), Some(g)) = (commits.get(i), graph.get(i)) else { continue };
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
            let selected = s.vcs.log.selected.as_deref() == Some(c.hash.as_str());
            let painter = ui.painter_at(rect);
            if selected {
                painter.rect_filled(rect, CornerRadius::ZERO, theme.list_selection);
            } else if resp.hovered() {
                painter.rect_filled(rect, CornerRadius::ZERO, theme.hover);
            }
            paint_graph(&painter, Rect::from_min_size(rect.min, vec2(graph_w, row_h)), g, theme);
            let date_w = 90.0;
            let author_w = 130.0;
            let right = rect.right();
            let cy = rect.center().y;
            let mut x = rect.left() + graph_w + 4.0;
            for r in &c.refs {
                x += ref_badge(ui, &painter, x, cy, r, theme);
            }
            let subject_clip =
                Rect::from_min_max(pos2(x, rect.top()), pos2(right - date_w - author_w - 8.0, rect.bottom()));
            ui.painter().with_clip_rect(subject_clip).text(
                pos2(x, cy),
                Align2::LEFT_CENTER,
                &c.subject,
                FontId::proportional(13.0),
                theme.text,
            );
            let author_clip = Rect::from_min_max(
                pos2(right - date_w - author_w, rect.top()),
                pos2(right - date_w - 6.0, rect.bottom()),
            );
            ui.painter().with_clip_rect(author_clip).text(
                pos2(author_clip.left(), cy),
                Align2::LEFT_CENTER,
                &c.author,
                FontId::proportional(12.0),
                theme.text_muted,
            );
            painter.text(
                pos2(right - 4.0, cy),
                Align2::RIGHT_CENTER,
                when(c.time),
                FontId::proportional(12.0),
                theme.text_muted,
            );
            let resp = resp.on_hover_text(format!("{}  {}\n{} <{}>", c.short(), c.subject, c.author, c.email));
            if resp.clicked() {
                *cmd = Some(("vcs.selectCommit", json!({ "hash": c.hash })));
            }
            resp.context_menu(|ui| {
                if ui.button("Copy Revision Number").clicked() {
                    ui.ctx().copy_text(c.hash.clone());
                    ui.close();
                }
                if ui.button("New Branch…").clicked() {
                    *cmd = Some(("git.newBranch", json!({ "from": c.hash })));
                    ui.close();
                }
                if ui.button("New Tag…").clicked() {
                    *cmd = Some(("git.newTag", json!({ "rev": c.hash })));
                    ui.close();
                }
                if ui.button("Reset Current Branch to Here…").clicked() {
                    *cmd = Some(("git.resetHead", json!({ "rev": c.hash })));
                    ui.close();
                }
            });
        }
    });
}

fn details_view(
    ui: &mut Ui,
    s: &mut Session,
    st: &mut UiState,
    theme: &Theme,
    cmd: &mut Option<(&'static str, Value)>,
) {
    let Some(d) = s.vcs.log.details.clone() else {
        if s.vcs.log.details_loading {
            ui.add(egui::Spinner::new().size(12.0));
        }
        return;
    };
    let files_h = (ui.available_height() * 0.35).max(80.0);
    egui::ScrollArea::vertical().id_salt("vcs-commit-message").max_height(ui.available_height() * 0.3).show(ui, |ui| {
        let mut lines = d.message.lines();
        ui.label(RichText::new(lines.next().unwrap_or_default()).strong());
        let body: Vec<&str> = lines.collect();
        if !body.is_empty() {
            ui.label(body.join("\n").trim());
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            let short = d.hash.get(..8).unwrap_or(&d.hash).to_string();
            if ui.link(RichText::new(&short).monospace()).on_hover_text("Copy the full hash").clicked() {
                ui.ctx().copy_text(d.hash.clone());
            }
            ui.label(RichText::new(format!("{} <{}>", d.author, d.email)).color(theme.text_muted).small());
            ui.label(RichText::new(format!("on {}", date(d.time))).color(theme.text_muted).small());
        });
        if d.committer != d.author {
            ui.label(RichText::new(format!("committed by {}", d.committer)).color(theme.text_muted).small());
        }
    });
    ui.separator();
    ui.label(
        RichText::new(format!("{} changed file{}", d.files.len(), if d.files.len() == 1 { "" } else { "s" }))
            .small()
            .color(theme.text_muted),
    );
    egui::ScrollArea::vertical().id_salt("vcs-commit-files").max_height(files_h).show(ui, |ui| {
        for f in &d.files {
            let selected = s.vcs.log.selected_file.as_deref() == Some(f.path.as_str());
            let (name, dir) = file_name(&f.path);
            let text = RichText::new(if dir.is_empty() { name.to_string() } else { format!("{name}  {dir}") })
                .color(state_color(theme, f.state));
            if ui.add(egui::Button::selectable(selected, text).frame_when_inactive(false)).clicked() {
                *cmd = Some(("vcs.selectCommitFile", json!({ "path": f.path })));
            }
        }
    });
    if let Some(diff) = s.vcs.log.file_diff.clone() {
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new(&diff.path).small().strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.selectable_label(st.vcs.side_by_side, RichText::new("Side by side").small()).clicked() {
                    st.vcs.side_by_side = !st.vcs.side_by_side;
                }
            });
        });
        file_diff_view(ui, st, theme, "vcs-commit-diff", &diff, ui.available_height());
    }
}

// ---------------------------------------------------------------- floating: dialogs, compare, hunk

/// Dialogs, the compare window and the hunk popup (drawn every frame).
pub fn overlays(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    dialogs(ctx, s, st);
    compare_window(ctx, s, st);
    hunk_popup(ctx, s, st);
    if s.vcs.busy() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn compare_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(d) = s.vcs.compare.clone() else { return };
    let theme = st.theme.clone();
    let mut open = true;
    egui::Window::new(format!(
        "{}: {} ↔ {}",
        d.path,
        if d.old_title.is_empty() { "…" } else { &d.old_title },
        d.new_title
    ))
    .id(Id::new("vcs-compare"))
    .open(&mut open)
    .collapsible(false)
    .resizable(true)
    .default_size([900.0, 560.0])
    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
    .show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.checkbox(&mut st.vcs.side_by_side, "Side by side");
        });
        let h = ui.available_height();
        file_diff_view(ui, st, &theme, "vcs-compare", &d, h);
    });
    if !open {
        s.vcs.compare = None;
    }
}

fn hunk_popup(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(h) = s.vcs.hunk.clone() else {
        st.vcs.hunk_shown_last = false;
        return;
    };
    let theme = st.theme.clone();
    let pos = st.vcs.hunk_anchor.unwrap_or(pos2(200.0, 200.0));
    let mut cmd: Option<(&'static str, Value)> = None;
    let mut close = false;
    let area = egui::Area::new(Id::new("vcs-hunk"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos + vec2(8.0, 4.0))
        .show(ctx, |ui| {
            Frame::popup(ui.style()).fill(theme.popup_bg).show(ui, |ui| {
                ui.set_width(520.0);
                ui.horizontal(|ui| {
                    let what = match h.change.kind {
                        LineChangeKind::Added => "Added lines",
                        LineChangeKind::Modified => "Modified lines",
                        LineChangeKind::Deleted => "Deleted lines",
                    };
                    ui.label(RichText::new(what).strong().small());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icon(ui, &theme, "×", "Close (Esc)") {
                            close = true;
                        }
                        if ui.small_button("Show Diff").clicked() {
                            if let Some(path) = s.ws.doc(h.doc).and_then(|d| d.path.clone()) {
                                cmd = Some(("vcs.compare", json!({ "path": path, "rev": "HEAD" })));
                            }
                            close = true;
                        }
                        if ui.small_button("Rollback").on_hover_text("Rollback Lines (⌥⌘Z)").clicked() {
                            cmd = Some(("vcs.rollbackHunk", Value::Null));
                        }
                    });
                });
                let old = if h.old_text.is_empty() && h.change.kind == LineChangeKind::Added {
                    String::new()
                } else {
                    format!("{}\n", h.old_text)
                };
                let new = if h.new_text.is_empty() && h.change.kind == LineChangeKind::Deleted {
                    String::new()
                } else {
                    format!("{}\n", h.new_text)
                };
                let opts = DiffOptions { side_by_side: false, context: 0, max_height: 260.0 };
                diff_view::show(
                    ui,
                    &theme,
                    &FontId::monospace(12.0),
                    ("vcs-hunk", h.doc.0, h.change.start),
                    &old,
                    &new,
                    &opts,
                );
            });
        });
    let clicked_outside = ctx.input(|i| i.pointer.any_click())
        && ctx.input(|i| i.pointer.interact_pos()).is_some_and(|p| !area.response.rect.contains(p));
    if ctx.input(|i| i.key_pressed(Key::Escape)) || (st.vcs.hunk_shown_last && clicked_outside && cmd.is_none()) {
        close = true;
    }
    st.vcs.hunk_shown_last = !close;
    if close {
        s.vcs.hunk = None;
    }
    if let Some((id, params)) = cmd {
        // The command acts on the hunk's document even if another editor is active.
        run_command(s, st, id, &params);
    }
}

fn dialog_error(ui: &mut Ui, s: &Session, theme: &Theme) {
    if let Some(e) = &s.vcs.dialog_error {
        ui.add(egui::Label::new(RichText::new(e).color(theme.error).small()).wrap());
    }
}

/// OK / Cancel row; returns (ok, cancel).
fn buttons(ui: &mut Ui, ok_label: &str, ok_enabled: bool, danger: bool, theme: &Theme) -> (bool, bool) {
    let mut ok = false;
    let mut cancel = false;
    ui.add_space(6.0);
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 24.0), Layout::right_to_left(Align::Center), |ui| {
        let fill = if danger { theme.error } else { theme.accent };
        if ui.add_enabled(ok_enabled, egui::Button::new(RichText::new(ok_label).strong()).fill(fill)).clicked() {
            ok = true;
        }
        if ui.button("Cancel").clicked() {
            cancel = true;
        }
    });
    (ok, cancel)
}

fn branch_names(s: &Session, include_current: bool) -> Vec<String> {
    s.vcs.branches.iter().filter(|b| include_current || !b.current).map(|b| b.name.clone()).collect()
}

fn dialogs(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(mut d) = s.vcs.dialog.clone() else {
        st.vcs.confirm_hard = false;
        return;
    };
    let theme = st.theme.clone();
    let mut open = true;
    let mut action: Option<(&'static str, Value)> = None;
    let mut close = false;
    let busy = s.vcs.op.clone();
    let title = match &d {
        Dialog::Push { remote, .. } => format!("Push Commits to {remote}"),
        Dialog::Pull { .. } => "Pull to the Current Branch".into(),
        Dialog::NewBranch { .. } => "Create New Branch".into(),
        Dialog::RenameBranch { old, .. } => format!("Rename Branch '{old}'"),
        Dialog::DeleteBranch { .. } => "Delete Branch".into(),
        Dialog::NewTag { .. } => "Create New Tag".into(),
        Dialog::Reset { .. } => "Git Reset".into(),
        Dialog::ChooseBranch { rebase: false, .. } => "Merge into the Current Branch".into(),
        Dialog::ChooseBranch { rebase: true, .. } => "Rebase the Current Branch".into(),
        Dialog::Stash { .. } => "Stash Changes".into(),
        Dialog::Unstash { .. } => "Unstash Changes".into(),
        Dialog::Rollback { .. } => "Rollback Changes".into(),
        Dialog::CreatePatch { .. } => "Create Patch".into(),
        Dialog::ApplyPatch { .. } => "Apply Patch".into(),
        Dialog::Remotes { .. } => "Git Remotes".into(),
        Dialog::Clone { .. } => "Clone Repository".into(),
        Dialog::Conflicts { .. } => "Conflicts".into(),
        Dialog::Compare { branch: true, .. } => "Compare with Branch".into(),
        Dialog::Compare { .. } => "Compare with Revision".into(),
    };
    let field_w = 360.0;
    egui::Window::new(title)
        .id(Id::new("vcs-dialog"))
        .open(&mut open)
        .collapsible(false)
        .resizable(matches!(d, Dialog::Conflicts { .. } | Dialog::Push { .. } | Dialog::ApplyPatch { .. }))
        .default_width(480.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .show(ctx, |ui| {
            let enter = ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
            match &mut d {
                Dialog::Push { remote, branch, remote_branch, set_upstream, force, commits } => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(branch.as_str()).strong());
                        ui.label("→");
                        egui::ComboBox::from_id_salt("vcs-push-remote").selected_text(remote.as_str()).show_ui(
                            ui,
                            |ui| {
                                for r in &s.vcs.remotes {
                                    ui.selectable_value(remote, r.name.clone(), &r.name);
                                }
                            },
                        );
                        ui.label(":");
                        ui.add(TextEdit::singleline(remote_branch).desired_width(160.0));
                    });
                    ui.separator();
                    match commits {
                        None => {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                        Some(list) if list.is_empty() => {
                            ui.label(RichText::new("Nothing to push").color(theme.text_muted));
                        }
                        Some(list) => {
                            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                                for c in list.iter() {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(c.short()).monospace().color(theme.text_muted));
                                        ui.label(&c.subject);
                                        ui.label(RichText::new(&c.author).small().color(theme.text_muted));
                                    });
                                }
                            });
                        }
                    }
                    ui.checkbox(set_upstream, "Set upstream (track the remote branch)");
                    ui.checkbox(force, "Force push (with lease)");
                    dialog_error(ui, s, &theme);
                    let (ok, cancel) = buttons(
                        ui,
                        if *force { "Force Push" } else { "Push" },
                        busy.is_none() && !remote.is_empty(),
                        *force,
                        &theme,
                    );
                    if ok {
                        action = Some(("vcs.push", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::Pull { remote, branch, rebase } => {
                    egui::Grid::new("vcs-pull").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("Remote");
                        egui::ComboBox::from_id_salt("vcs-pull-remote").selected_text(remote.as_str()).show_ui(
                            ui,
                            |ui| {
                                for r in &s.vcs.remotes {
                                    ui.selectable_value(remote, r.name.clone(), &r.name);
                                }
                            },
                        );
                        ui.end_row();
                        ui.label("Branch");
                        ui.add(TextEdit::singleline(branch).desired_width(field_w));
                        ui.end_row();
                    });
                    ui.checkbox(rebase, "Rebase instead of merge");
                    dialog_error(ui, s, &theme);
                    let (ok, cancel) = buttons(ui, "Pull", busy.is_none(), false, &theme);
                    if ok || enter {
                        action = Some(("vcs.pull", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::NewBranch { name, from, checkout } => {
                    if let Some(f) = from {
                        ui.label(RichText::new(format!("From {f}")).color(theme.text_muted));
                    }
                    let r = ui.add(TextEdit::singleline(name).hint_text("Branch name").desired_width(field_w));
                    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "New branch name"));
                    if !r.has_focus() && name.is_empty() {
                        r.request_focus();
                    }
                    ui.checkbox(checkout, "Checkout branch");
                    dialog_error(ui, s, &theme);
                    let valid = codecraft_engine::vcs_git::valid_ref_name(name.trim());
                    let (ok, cancel) = buttons(ui, "Create", valid && busy.is_none(), false, &theme);
                    if ok || (enter && valid) {
                        action = Some(("vcs.newBranch", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::RenameBranch { name, .. } => {
                    let r = ui.add(TextEdit::singleline(name).desired_width(field_w));
                    if !r.has_focus() && name.is_empty() {
                        r.request_focus();
                    }
                    dialog_error(ui, s, &theme);
                    let valid = codecraft_engine::vcs_git::valid_ref_name(name.trim());
                    let (ok, cancel) = buttons(ui, "Rename", valid && busy.is_none(), false, &theme);
                    if ok || (enter && valid) {
                        action = Some(("vcs.renameBranch", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::DeleteBranch { name, remote, force } => {
                    let where_ = if *remote { " on its remote" } else { "" };
                    ui.label(format!("Delete branch '{name}'{where_}?"));
                    if !*remote {
                        ui.checkbox(force, "Delete even if it is not merged");
                    }
                    dialog_error(ui, s, &theme);
                    let (ok, cancel) = buttons(ui, "Delete", busy.is_none(), true, &theme);
                    if ok {
                        action = Some(("vcs.deleteBranch", json!({ "name": name, "remote": remote, "force": force })));
                        close = true;
                    }
                    close |= cancel;
                }
                Dialog::NewTag { name, rev, message } => {
                    egui::Grid::new("vcs-tag").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("Tag name");
                        let r = ui.add(TextEdit::singleline(name).desired_width(field_w));
                        if !r.has_focus() && name.is_empty() {
                            r.request_focus();
                        }
                        ui.end_row();
                        ui.label("Commit");
                        ui.add(TextEdit::singleline(rev).desired_width(field_w));
                        ui.end_row();
                        ui.label("Message");
                        ui.add(
                            TextEdit::multiline(message)
                                .hint_text("optional: an annotated tag")
                                .desired_rows(2)
                                .desired_width(field_w),
                        );
                        ui.end_row();
                    });
                    dialog_error(ui, s, &theme);
                    let valid = codecraft_engine::vcs_git::valid_ref_name(name.trim());
                    let (ok, cancel) = buttons(ui, "Create Tag", valid && busy.is_none(), false, &theme);
                    if ok {
                        action = Some(("vcs.newTag", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::Reset { rev, mode } => {
                    ui.horizontal(|ui| {
                        ui.label("Reset the current branch to");
                        ui.add(TextEdit::singleline(rev).desired_width(220.0));
                    });
                    for (m, label, help) in [
                        (ResetMode::Soft, "Soft", "Keep all changes; they stay staged."),
                        (ResetMode::Mixed, "Mixed", "Keep all changes, unstaged."),
                        (ResetMode::Hard, "Hard", "Discard all local changes (they cannot be recovered)."),
                        (ResetMode::Keep, "Keep", "Keep local changes; stop if they would be overwritten."),
                    ] {
                        ui.radio_value(mode, m, RichText::new(label).strong());
                        ui.label(RichText::new(help).small().color(theme.text_muted));
                    }
                    let hard = *mode == ResetMode::Hard;
                    if hard {
                        ui.checkbox(
                            &mut st.vcs.confirm_hard,
                            RichText::new("I understand local changes will be lost").color(theme.error),
                        );
                    }
                    dialog_error(ui, s, &theme);
                    let can = busy.is_none() && !rev.trim().is_empty() && (!hard || st.vcs.confirm_hard);
                    let (ok, cancel) = buttons(ui, "Reset", can, hard, &theme);
                    if ok {
                        action = Some(("vcs.reset", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::ChooseBranch { rebase, branch } => {
                    let names = branch_names(s, false);
                    let label = if *rebase { "Rebase onto" } else { "Merge" };
                    ui.horizontal(|ui| {
                        ui.label(label);
                        egui::ComboBox::from_id_salt("vcs-choose-branch")
                            .selected_text(if branch.is_empty() {
                                "Choose a branch".to_string()
                            } else {
                                branch.clone()
                            })
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                for n in &names {
                                    ui.selectable_value(branch, n.clone(), n);
                                }
                            });
                    });
                    dialog_error(ui, s, &theme);
                    let (ok, cancel) = buttons(
                        ui,
                        if *rebase { "Rebase" } else { "Merge" },
                        !branch.is_empty() && busy.is_none(),
                        false,
                        &theme,
                    );
                    if ok {
                        action = Some(if *rebase {
                            ("vcs.rebase", json!({ "onto": branch }))
                        } else {
                            ("vcs.merge", json!({ "branch": branch }))
                        });
                    }
                    close |= cancel;
                }
                Dialog::Stash { message, include_untracked } => {
                    ui.add(TextEdit::singleline(message).hint_text("Message").desired_width(field_w));
                    ui.checkbox(include_untracked, "Include unversioned files");
                    dialog_error(ui, s, &theme);
                    let (ok, cancel) = buttons(ui, "Create Stash", busy.is_none(), false, &theme);
                    if ok || enter {
                        action = Some(("vcs.stash", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::Unstash { selected, pop, reinstate_index } => {
                    if s.vcs.stashes.is_empty() {
                        ui.label(RichText::new("No stashes").color(theme.text_muted));
                    }
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        for stsh in &s.vcs.stashes {
                            let text = format!("{}  {}  ({})", stsh.reference(), stsh.message, when(stsh.time));
                            if ui.selectable_label(*selected == Some(stsh.index), text).clicked() {
                                *selected = Some(stsh.index);
                            }
                        }
                    });
                    ui.checkbox(pop, "Pop stash (drop it after applying)");
                    ui.checkbox(reinstate_index, "Reinstate index");
                    dialog_error(ui, s, &theme);
                    ui.horizontal(|ui| {
                        if let Some(i) = *selected
                            && ui.add_enabled(busy.is_none(), egui::Button::new("Drop")).clicked()
                        {
                            action = Some(("vcs.dropStash", json!({ "index": i })));
                        }
                    });
                    let (ok, cancel) = buttons(
                        ui,
                        if *pop { "Pop Stash" } else { "Apply Stash" },
                        selected.is_some() && busy.is_none(),
                        false,
                        &theme,
                    );
                    if ok {
                        action = Some(("vcs.unstash", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::Rollback { files, delete_added } => {
                    ui.label(format!(
                        "Roll back {} file{} to HEAD?",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" }
                    ));
                    egui::ScrollArea::vertical().max_height(200.0).show(ui, |ui| {
                        for f in files.iter() {
                            let color =
                                s.vcs.status.file(f).map(|x| state_color(&theme, x.state())).unwrap_or(theme.text);
                            ui.label(RichText::new(f).color(color));
                        }
                    });
                    if files.iter().any(|f| s.vcs.status.file(f).is_some_and(|x| x.state() == FileState::Added)) {
                        ui.checkbox(delete_added, "Delete local copies of added files");
                    }
                    dialog_error(ui, s, &theme);
                    let (ok, cancel) = buttons(ui, "Rollback", busy.is_none() && !files.is_empty(), true, &theme);
                    if ok {
                        action = Some(("vcs.rollback", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::CreatePatch { path, files, to_clipboard } => {
                    ui.label(format!("{} file{} with changes", files.len(), if files.len() == 1 { "" } else { "s" }));
                    ui.checkbox(to_clipboard, "Copy the patch to the clipboard");
                    if !*to_clipboard {
                        ui.add(TextEdit::singleline(path).hint_text("Patch file").desired_width(field_w));
                    }
                    dialog_error(ui, s, &theme);
                    let (ok, cancel) = buttons(ui, "Create", busy.is_none() && !files.is_empty(), false, &theme);
                    if ok {
                        action = Some(("vcs.createPatch", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::ApplyPatch { path, text } => {
                    ui.add(TextEdit::singleline(path).hint_text("Patch file path").desired_width(field_w));
                    ui.label(RichText::new("or paste the patch:").small().color(theme.text_muted));
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        ui.add(
                            TextEdit::multiline(text)
                                .font(FontId::monospace(12.0))
                                .desired_rows(8)
                                .desired_width(f32::INFINITY),
                        );
                    });
                    dialog_error(ui, s, &theme);
                    let can = busy.is_none() && (!path.trim().is_empty() || !text.trim().is_empty());
                    let (ok, cancel) = buttons(ui, "Apply", can, false, &theme);
                    if ok {
                        action = Some(("vcs.applyPatch", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::Remotes { name, url } => {
                    for r in s.vcs.remotes.clone() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&r.name).strong());
                            ui.label(RichText::new(&r.fetch_url).color(theme.text_muted));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.add_enabled(busy.is_none(), egui::Button::new("Remove")).clicked() {
                                    action = Some(("vcs.removeRemote", json!({ "name": r.name })));
                                }
                            });
                        });
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.add(TextEdit::singleline(name).hint_text("Name").desired_width(90.0));
                        ui.add(TextEdit::singleline(url).hint_text("URL").desired_width(260.0));
                        let can = busy.is_none() && !name.trim().is_empty() && !url.trim().is_empty();
                        if ui.add_enabled(can, egui::Button::new("Add")).clicked() {
                            action = Some(("vcs.addRemote", json!({ "name": name.trim(), "url": url.trim() })));
                        }
                    });
                    dialog_error(ui, s, &theme);
                    ui.allocate_ui_with_layout(
                        vec2(ui.available_width(), 24.0),
                        Layout::right_to_left(Align::Center),
                        |ui| {
                            if ui.button("Close").clicked() {
                                close = true;
                            }
                        },
                    );
                }
                Dialog::Clone { url, directory } => {
                    egui::Grid::new("vcs-clone").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("URL");
                        let r =
                            ui.add(TextEdit::singleline(url).hint_text("https://… or git@…").desired_width(field_w));
                        if !r.has_focus() && url.is_empty() {
                            r.request_focus();
                        }
                        ui.end_row();
                        ui.label("Directory");
                        ui.add(TextEdit::singleline(directory).desired_width(field_w));
                        ui.end_row();
                    });
                    if let Some(op) = &busy {
                        ui.horizontal(|ui| {
                            ui.add(egui::Spinner::new().size(12.0));
                            let last = s.vcs.console.last().map(|c| c.text.clone()).unwrap_or_default();
                            ui.label(RichText::new(format!("{}: {last}", op.label)).small().color(theme.text_muted));
                            if ui.button("Cancel").clicked() {
                                action = Some(("vcs.cancel", Value::Null));
                            }
                        });
                    }
                    dialog_error(ui, s, &theme);
                    let can = busy.is_none() && !url.trim().is_empty() && !directory.trim().is_empty();
                    let (ok, cancel) = buttons(ui, "Clone", can, false, &theme);
                    if ok {
                        action = Some(("vcs.clone", Value::Null));
                    }
                    close |= cancel;
                }
                Dialog::Conflicts { selected } => {
                    let conflicts: Vec<FileStatus> = s.vcs.conflicts().into_iter().cloned().collect();
                    if s.vcs.state != codecraft_engine::vcs_git::RepoState::Normal {
                        ui.label(RichText::new(format!("{} in progress", s.vcs.state.label())).color(theme.warning));
                    }
                    if conflicts.is_empty() {
                        ui.label(RichText::new("All conflicts are resolved.").color(theme.success));
                    }
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        egui::Grid::new("vcs-conflicts").num_columns(2).striped(true).show(ui, |ui| {
                            for f in &conflicts {
                                let sel = selected.as_deref() == Some(f.path.as_str());
                                if ui.selectable_label(sel, RichText::new(&f.path).color(theme.vcs_conflict)).clicked()
                                {
                                    *selected = Some(f.path.clone());
                                }
                                ui.label(
                                    RichText::new(f.conflict.map(|k| k.label()).unwrap_or(""))
                                        .small()
                                        .color(theme.text_muted),
                                );
                                ui.end_row();
                            }
                        });
                    });
                    let target = selected
                        .clone()
                        .filter(|p| conflicts.iter().any(|f| &f.path == p))
                        .or_else(|| conflicts.first().map(|f| f.path.clone()));
                    if selected.is_none() {
                        selected.clone_from(&target);
                    }
                    ui.horizontal(|ui| {
                        let can = busy.is_none() && target.is_some();
                        let p = json!({ "path": target });
                        if ui.add_enabled(can, egui::Button::new("Accept Yours")).clicked() {
                            action = Some(("vcs.acceptYours", p.clone()));
                        }
                        if ui.add_enabled(can, egui::Button::new("Accept Theirs")).clicked() {
                            action = Some(("vcs.acceptTheirs", p.clone()));
                        }
                        if ui
                            .add_enabled(can, egui::Button::new("Merge…"))
                            .on_hover_text("Open the file with its conflict markers")
                            .clicked()
                        {
                            action = Some(("vcs.mergeConflict", p.clone()));
                        }
                        if ui.add_enabled(can, egui::Button::new("Mark Resolved")).clicked() {
                            action = Some(("vcs.markResolved", p));
                        }
                    });
                    dialog_error(ui, s, &theme);
                    ui.separator();
                    ui.horizontal(|ui| {
                        if s.vcs.state != codecraft_engine::vcs_git::RepoState::Normal {
                            if ui
                                .add_enabled(
                                    busy.is_none(),
                                    egui::Button::new(format!("Abort {}", s.vcs.state.label())),
                                )
                                .clicked()
                            {
                                action = Some(("vcs.abortOperation", Value::Null));
                            }
                            let can = busy.is_none() && conflicts.is_empty();
                            if ui
                                .add_enabled(
                                    can,
                                    egui::Button::new(
                                        RichText::new(format!("Continue {}", s.vcs.state.label())).strong(),
                                    ),
                                )
                                .clicked()
                            {
                                action = Some(("vcs.continueOperation", Value::Null));
                            }
                        }
                        ui.allocate_ui_with_layout(
                            vec2(ui.available_width(), 24.0),
                            Layout::right_to_left(Align::Center),
                            |ui| {
                                if ui.button("Close").clicked() {
                                    close = true;
                                }
                            },
                        );
                    });
                }
                Dialog::Compare { path, rev, branch } => {
                    ui.label(RichText::new(path.as_str()).strong());
                    if *branch {
                        let names = branch_names(s, false);
                        egui::ComboBox::from_id_salt("vcs-compare-branch")
                            .selected_text(rev.as_str())
                            .width(260.0)
                            .show_ui(ui, |ui| {
                                for n in &names {
                                    ui.selectable_value(rev, n.clone(), n);
                                }
                            });
                    } else {
                        ui.add(
                            TextEdit::singleline(rev).hint_text("Revision (hash, tag, HEAD~1…)").desired_width(field_w),
                        );
                    }
                    let (ok, cancel) = buttons(ui, "Compare", !rev.trim().is_empty(), false, &theme);
                    if ok || enter {
                        action = Some(("vcs.compare", json!({ "path": path, "rev": rev.trim() })));
                    }
                    close |= cancel;
                }
            }
        });
    // Write the edited form back unless something replaced the dialog meanwhile.
    if s.vcs.dialog.as_ref().map(std::mem::discriminant) == Some(std::mem::discriminant(&d)) {
        s.vcs.dialog = Some(d);
    }
    if !open || close {
        s.vcs.dialog = None;
        s.vcs.dialog_error = None;
    }
    if let Some((id, params)) = action {
        run_command(s, st, id, &params);
    }
}

// ---------------------------------------------------------------- editor decorations

/// What the editor paints for version control in one document (collected once per frame).
#[derive(Default)]
pub struct GutterVcs {
    changes: Vec<LineChange>,
    blame: Option<Arc<Blame>>,
    /// Conflict blocks: (first line, last line inclusive, 0 = marker line, 1 = ours, 2 = theirs).
    conflicts: Vec<(usize, usize, u8)>,
}

impl GutterVcs {
    pub fn collect(s: &Session, doc: DocId) -> Self {
        let changes = s.vcs.markers(doc).to_vec();
        let blame = s.vcs.blame_for(doc).and_then(|b| b.blame.clone());
        let conflicted =
            s.ws.doc(doc)
                .and_then(|d| d.path.as_deref())
                .is_some_and(|p| s.vcs.file_state(p) == Some(FileState::Conflicted));
        let conflicts =
            if conflicted { s.ws.doc(doc).map(|d| conflict_blocks(&d.doc)).unwrap_or_default() } else { Vec::new() };
        Self { changes, blame, conflicts }
    }

    /// Width of the annotations column (0 when Annotate is off).
    pub fn annotation_width(&self, char_w: f32) -> f32 {
        if self.blame.is_some() { (char_w * 0.92 * 32.0).round() } else { 0.0 }
    }

    /// Background of a line inside a conflict block.
    pub fn line_bg(&self, line: usize, theme: &Theme) -> Option<Color32> {
        self.conflicts.iter().find(|(a, b, _)| (*a..=*b).contains(&line)).map(|(_, _, k)| match k {
            0 => theme.diff_removed_bg,
            1 => theme.diff_added_bg,
            _ => theme.diff_modified_bg,
        })
    }

    /// Whether a change marker covers the line (clicking it opens the hunk popup).
    pub fn has_change(&self, line: usize) -> bool {
        self.changes.iter().any(|c| c.covers(line))
    }

    /// The commit annotated on a line.
    pub fn annotation(&self, line: usize) -> Option<(String, String)> {
        let c = self.blame.as_ref()?.line(line)?;
        if c.uncommitted {
            return None;
        }
        let tip = format!("{}\n{} <{}>, {}\n\n{}", c.short(), c.author, c.email, date(c.time), c.summary);
        Some((c.hash.clone(), tip))
    }

    /// Paint the gutter decorations of one line: the annotation in `ann` and the change bar at
    /// the gutter's right edge (just left of the text).
    #[allow(clippy::too_many_arguments)]
    pub fn paint_line(
        &self,
        painter: &egui::Painter,
        theme: &Theme,
        line: usize,
        y: f32,
        line_h: f32,
        gutter: Rect,
        ann: Rect,
        font: &FontId,
    ) {
        if let Some(b) = &self.blame
            && ann.width() > 0.0
            && let Some(c) = b.line(line)
            && !c.uncommitted
        {
            let first_of_run = line == 0 || b.lines.get(line - 1) != b.lines.get(line);
            let color = if first_of_run { theme.text_muted } else { theme.text_disabled };
            let author: String = c.author.chars().take(12).collect();
            let text = format!("{} {} {}", date(c.time), author, c.short().get(..7).unwrap_or(c.short()));
            painter.with_clip_rect(ann).text(
                pos2(ann.left() + 4.0, y + line_h * 0.5),
                Align2::LEFT_CENTER,
                text,
                font.clone(),
                color,
            );
        }
        let x1 = gutter.right() - 4.0;
        for c in &self.changes {
            match c.kind {
                LineChangeKind::Deleted if c.start == line => {
                    let ty = y;
                    let tri = vec![pos2(x1 - 1.0, ty - 4.0), pos2(x1 + 4.0, ty), pos2(x1 - 1.0, ty + 4.0)];
                    painter.add(egui::Shape::convex_polygon(tri, theme.vcs_gutter_deleted, Stroke::NONE));
                }
                LineChangeKind::Added | LineChangeKind::Modified if (c.start..c.end).contains(&line) => {
                    let color = if c.kind == LineChangeKind::Added {
                        theme.vcs_gutter_added
                    } else {
                        theme.vcs_gutter_modified
                    };
                    let r = Rect::from_min_max(pos2(x1, y), pos2(x1 + 3.0, y + line_h));
                    painter.rect_filled(r, CornerRadius::ZERO, color);
                }
                _ => {}
            }
        }
    }
}

/// Conflict marker blocks of a document (`<<<<<<<` … `|||||||` … `=======` … `>>>>>>>`).
fn conflict_blocks(doc: &codecraft_engine::text::Document) -> Vec<(usize, usize, u8)> {
    let mut out = Vec::new();
    let mut open: Option<(usize, u8)> = None;
    for line in 0..doc.len_lines().min(200_000) {
        let Some(text) = doc.line_text(line) else { continue };
        let marker = ["<<<<<<<", "|||||||", "=======", ">>>>>>>"].iter().position(|m| text.starts_with(m));
        match (marker, open) {
            (Some(0), _) => {
                out.push((line, line, 0));
                open = Some((line + 1, 1));
            }
            (Some(1 | 2), Some((start, kind))) => {
                if line > start {
                    out.push((start, line - 1, kind));
                }
                out.push((line, line, 0));
                open = Some((line + 1, if marker == Some(2) { 2 } else { 0 }));
            }
            (Some(3), Some((start, kind))) => {
                if line > start {
                    out.push((start, line - 1, kind));
                }
                out.push((line, line, 0));
                open = None;
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_767_323_045), "2026-01-02");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(file_name("a/b/c.rs"), ("c.rs", "a/b"));
        assert_eq!(file_name("c.rs"), ("c.rs", ""));
    }

    #[test]
    fn conflict_blocks_are_found() {
        let doc =
            codecraft_engine::text::Document::from_text("a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> other\nb\n");
        let blocks = conflict_blocks(&doc);
        assert_eq!(blocks, vec![(1, 1, 0), (2, 2, 1), (3, 3, 0), (4, 4, 2), (5, 5, 0)]);
        let doc = codecraft_engine::text::Document::from_text("=======\n>>>>>>> x\n");
        assert!(conflict_blocks(&doc).is_empty());
    }
}
