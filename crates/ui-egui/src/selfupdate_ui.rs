//! Self-update UI renders engine state; actions are commands.
use crate::{UiState, run_command};
use codecraft_engine::Session;
use serde_json::{Value, json};
pub(crate) fn show(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if let Some(commit) = s.selfupdate.revert_confirm.clone() {
        egui::Window::new("Revert your accepted change?").collapsible(false).show(ctx, |ui| {
            ui.label(format!(
                "Create a new git revert commit for {commit} on codecraft-user. Uncommitted files block the operation."
            ));
            if ui.button("Revert change").clicked() {
                run_command(s, st, "selfupdate.revertChange", &json!({"commit":commit,"confirm":true}));
            }
            if ui.button("Cancel revert").clicked() {
                s.selfupdate.revert_confirm = None;
            }
        });
    }
    if let Some(id) = s.selfupdate.developer_payload.clone() {
        egui::Window::new("Built CodeCraft payload").show(ctx, |ui| {
            ui.label("This developer launch has no stable launcher. Your built payload is ready to open.");
            if ui.button("Open built payload").clicked() {
                run_command(s, st, "selfupdate.openPayload", &json!({"id":id}));
            }
            if ui.button("Close").clicked() {
                s.selfupdate.developer_payload = None;
            }
        });
    }
    if let Some(notice) = s.selfupdate.updated_notice.clone() {
        egui::Window::new("CodeCraft updated").show(ctx, |ui| {
            ui.label(notice);
            if ui.button("Roll Back").clicked() {
                run_command(s, st, "version.rollback", &Value::Null);
                s.selfupdate.updated_notice = None;
            }
            if ui.button("Dismiss update").clicked() {
                s.selfupdate.updated_notice = None;
            }
        });
    }
    if let Some(c) = s.selfupdate.confirm.clone() {
        egui::Window::new("Build CodeCraft from feedback?").collapsible(false).show(ctx, |ui| {
            ui.label(format!(
                "{} file(s) changed. Version: {} + source commit + next counter.",
                c.files.len(),
                codecraft_engine::about::VERSION
            ));
            egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                for path in &c.files {
                    ui.label(path.display().to_string());
                }
            });
            ui.label("The app will close and reopen; your tabs, terminals and Claude sessions are restored.");
            ui.horizontal(|ui| {
                for (label, mode) in [("Build & Restart", "restart"), ("Build only", "build"), ("Cancel", "cancel")] {
                    if ui.button(label).clicked() {
                        run_command(s, st, "feedback.acceptAndBuild", &json!({"job":c.job,"confirm":true,"mode":mode}));
                    }
                }
            });
        });
    }
    if let Some(id) = s.selfupdate.restart_confirm.clone() {
        egui::Window::new("Restart CodeCraft?").collapsible(false).show(ctx, |ui| {
            ui.label(format!("Restart into {id}"));
            ui.label("The app will close and reopen; your tabs, terminals and Claude sessions are restored.");
            if s.ws.docs().any(|d| d.doc.is_modified()) {
                ui.label("Unsaved files will be saved before restarting. A save failure cancels the restart.");
            }
            ui.horizontal(|ui| {
                if ui.button("Save & Restart").clicked() {
                    run_command(s, st, "selfupdate.restartInto", &json!({"id":id,"confirm":true}));
                }
                if s.ws.docs().any(|d| d.doc.is_modified()) && ui.button("Discard & Restart").clicked() {
                    run_command(s, st, "selfupdate.restartInto", &json!({"id":id,"confirm":true,"discard":true}));
                }
                if ui.button("Cancel").clicked() {
                    s.selfupdate.restart_confirm = None;
                }
            });
        });
    }
    if let Some(notice) = s.selfupdate.rollback_notice.clone() {
        egui::Window::new("CodeCraft update rolled back").show(ctx, |ui| {
            ui.label(notice.lines().next().unwrap_or_default());
            egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                ui.monospace(codecraft_engine::selfupdate::log_tail(
                    codecraft_engine::selfupdate::crash_log(&notice),
                    8192,
                ));
            });
            let meta = s
                .versions
                .entries
                .iter()
                .find(|m| {
                    m.reason.is_some()
                        && notice.split_whitespace().nth(2).map(|id| id.trim_end_matches(':')) == Some(m.id.as_str())
                })
                .cloned();
            if let Some(meta) = meta {
                if let Some(job) = meta.feedback_job
                    && ui.button("Send this failure to the agent").clicked()
                {
                    run_command(
                        s,
                        st,
                        "selfupdate.sendFailure",
                        &json!({"job":job,"session":meta.feedback_session,"confirm":true}),
                    );
                }
                if let Some(commit) = meta.user_commit {
                    ui.label("Reverting creates a new commit in your managed source.");
                    if ui.button("Revert my change").clicked() {
                        run_command(s, st, "selfupdate.revertChange", &json!({"commit":commit}));
                    }
                }
            }
            if ui.button("Dismiss").clicked() {
                s.selfupdate.rollback_notice = None;
            }
        });
    }
}
pub(crate) fn settings(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, cmds: &mut Vec<(&'static str, Value)>) {
    if !s.selfupdate.loaded {
        cmds.push(("selfupdate.status", Value::Null));
        ui.spinner();
        return;
    }
    let mut draft = st.selfupdate_settings.take().unwrap_or_else(|| s.selfupdate.settings.clone());
    ui.heading("Self-update");
    ui.label("Repository URL (credentials use your git configuration and agent)");
    ui.text_edit_singleline(&mut draft.repo_url);
    let mut source = draft.source_location.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
    ui.label("Source location (empty: developer tree, then managed source)");
    ui.text_edit_singleline(&mut source);
    draft.source_location = (!source.is_empty()).then(|| source.into());
    ui.horizontal(|ui| {
        ui.label("Keep versions");
        ui.add(egui::DragValue::new(&mut draft.keep_versions).range(2..=100));
    });
    ui.horizontal(|ui| {
        ui.label("First build disk threshold (bytes)");
        ui.add(egui::DragValue::new(&mut draft.disk_bytes));
    });
    ui.horizontal(|ui| {
        ui.label("Warm build disk threshold (bytes)");
        ui.add(egui::DragValue::new(&mut draft.warm_disk_bytes));
    });
    ui.label("Build profile");
    ui.text_edit_singleline(&mut draft.build_profile);
    ui.add_enabled(false, egui::Checkbox::new(&mut true, "Ask before restarting (always on)"));
    if ui.button("Save self-update settings").clicked() {
        cmds.push(("selfupdate.settings", serde_json::to_value(&draft).unwrap_or(Value::Null)));
    }
    for (label, id) in [
        ("Check requirements", "selfupdate.preflight"),
        ("Prepare managed source", "selfupdate.prepareSource"),
        ("Update CodeCraft from upstream", "selfupdate.updateSource"),
    ] {
        if ui.add_enabled(!s.selfupdate.busy, egui::Button::new(label)).clicked() {
            cmds.push((id, Value::Null));
        }
    }
    if let Some(report) = &s.selfupdate.report {
        for check in &report.checks {
            ui.label(format!("{:?}: {} — {}", check.level, check.name, check.message));
            if check.level != codecraft_selfupdate::preflight::Level::Ok {
                ui.label(&check.fix);
            }
        }
    }
    if let Some(error) = &s.selfupdate.error {
        ui.label(error);
    }
    if ui.button("Open rustup.rs").clicked() {
        cmds.push(("selfupdate.openRustup", Value::Null));
    }
    st.selfupdate_settings = Some(draft);
}
