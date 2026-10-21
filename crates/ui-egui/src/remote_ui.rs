//! SSH and remote (M11 track S): the File Transfer and Remote Host tool windows, the server
//! chooser, Deployment › Configuration… and Options…, the sync preview, delete confirmation, File ›
//! Remote Development… and the SSH run-target fields. Every action runs an engine command.

use egui::{Align2, Id, Key, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::remote::{TestStatus, TransferStatus};
use codecraft_engine::ssh_target::SshTarget;

use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

fn run_all(s: &mut Session, st: &mut UiState, cmds: Cmds) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn escape_pressed(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_pressed(Key::Escape))
}

/// Whether one of the modal dialogs is open (they read the keyboard themselves).
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    let r = &s.remote;
    r.picker.is_some()
        || r.dialog.is_some()
        || r.options_open
        || r.remote_dev.is_some()
        || r.confirm.is_some()
        || r.sync_preview.is_some()
}

// ---------------------------------------------------------------- File Transfer

/// File Transfer: the log of uploads, downloads and syncs, newest first.
pub fn show_file_transfer(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    ui.horizontal(|ui| {
        if ui.button("Clear").on_hover_text("Remove finished entries").clicked() {
            cmds.push(("remote.fileTransfer.clear", json!({})));
        }
        if ui.button("Browse Remote Host").clicked() {
            cmds.push(("tools.deployment.browseRemoteHost", json!({})));
        }
        if ui.button("Deployment Configuration…").clicked() {
            cmds.push(("tools.deployment.configuration", json!({})));
        }
    });
    if s.remote.project.is_some() {
        ui.horizontal(|ui| {
            if ui.button(if s.remote.auto_refresh { "Pause Auto-refresh" } else { "Resume Auto-refresh" }).clicked() {
                cmds.push(("remote.project.autoRefresh", json!({})));
            }
            if ui.button(format!("Conflicts ({})", s.remote.conflicts.len())).clicked() {
                cmds.push(("remote.project.conflicts", json!({})));
            }
        });
        if s.remote.conflicts_open {
            for path in s.remote.conflicts.clone() {
                ui.horizontal(|ui| {
                    ui.colored_label(theme.warning, path.display().to_string());
                    for (label, action) in [("Compare", "compare"), ("Keep Mine", "keepMine"), ("Take Remote", "takeRemote"), ("Merge", "merge")] {
                        if ui.button(label).clicked() { cmds.push(("remote.project.resolve", json!({"path": path, "action": action}))); }
                    }
                });
            }
        }
    }
    ui.separator();
    if s.remote.log.is_empty() {
        ui.label(
            RichText::new("No transfers yet. Use Tools › Deployment to upload, download or sync files.")
                .color(theme.text_muted),
        );
    }
    let now = codecraft_engine::remote::now_ms();
    ScrollArea::vertical().id_salt("file-transfer-log").auto_shrink([false, false]).show(ui, |ui| {
        for e in s.remote.log.iter().rev() {
            let (icon, color, detail) = match &e.status {
                TransferStatus::Running => ("⟳", theme.accent, String::new()),
                TransferStatus::Done => ("✔", theme.success, String::new()),
                TransferStatus::Failed(err) => ("✖", theme.error, err.clone()),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).color(color));
                ui.label(
                    RichText::new(codecraft_engine::local_history::when(e.time_ms, now))
                        .color(theme.text_muted)
                        .small(),
                );
                ui.label(RichText::new(&e.server).strong());
                ui.label(&e.summary);
            });
            if !detail.is_empty() {
                ui.label(RichText::new(format!("    {detail}")).color(theme.error).small());
            }
            if !e.output.trim().is_empty() {
                egui::CollapsingHeader::new(RichText::new("    output").color(theme.text_muted).small())
                    .id_salt(("file-transfer-output", e.id))
                    .show(ui, |ui| {
                        ui.label(RichText::new(e.output.trim()).monospace().small());
                    });
            }
        }
    });
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- Remote Host

fn temp_text(ctx: &egui::Context, key: &str) -> String {
    ctx.data(|d| d.get_temp::<String>(Id::new(("remote-host", key)))).unwrap_or_default()
}

fn set_temp_text(ctx: &egui::Context, key: &str, value: Option<String>) {
    ctx.data_mut(|d| match value {
        Some(v) => {
            d.insert_temp(Id::new(("remote-host", key)), v);
        }
        None => d.remove::<String>(Id::new(("remote-host", key))),
    });
}

/// Remote Host: browse a server's folders; open, rename, delete, new folder.
pub fn show_remote_host(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let ctx = ui.ctx().clone();
    let mut cmds: Cmds = Vec::new();
    let b = s.remote.browser.clone();
    let names: Vec<String> = s.remote.loaded_servers().iter().map(|x| x.name.clone()).collect();
    ui.horizontal(|ui| {
        let current = b.server.clone().unwrap_or_else(|| "Choose server…".into());
        egui::ComboBox::from_id_salt("remote-host-server").selected_text(current).show_ui(ui, |ui| {
            for n in &names {
                if ui.selectable_label(b.server.as_deref() == Some(n.as_str()), n).clicked() {
                    cmds.push(("remote.host.browse", json!({ "server": n })));
                }
            }
            if ui.selectable_label(false, "Other…").clicked() {
                cmds.push(("tools.deployment.browseRemoteHost", json!({})));
            }
        });
        let connected = b.server.is_some();
        if ui.add_enabled(connected, egui::Button::new("⬆")).on_hover_text("Parent folder").clicked() {
            cmds.push(("remote.host.up", json!({})));
        }
        if ui.add_enabled(connected, egui::Button::new("⟳")).on_hover_text("Refresh").clicked() {
            cmds.push(("remote.host.refresh", json!({})));
        }
        if ui.add_enabled(connected, egui::Button::new("New Folder…")).clicked() {
            set_temp_text(&ctx, "mkdir", Some(String::new()));
        }
        if b.loading {
            ui.spinner();
        }
    });
    if b.server.is_none() {
        ui.label(
            RichText::new("Choose a server to browse (Tools › Deployment › Browse Remote Host).")
                .color(theme.text_muted),
        );
        run_all(s, st, cmds);
        return;
    }
    // The path: type a folder and press Enter.
    let mut path = ctx
        .data(|d| d.get_temp::<(String, String)>(Id::new("remote-host-path")))
        .filter(|(orig, _)| *orig == b.path)
        .map(|(_, p)| p)
        .unwrap_or_else(|| b.path.clone());
    let resp = ui.add(TextEdit::singleline(&mut path).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
    if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !path.trim().is_empty() {
        cmds.push(("remote.host.browse", json!({ "server": b.server, "path": path.trim() })));
    }
    ctx.data_mut(|d| d.insert_temp(Id::new("remote-host-path"), (b.path.clone(), path)));
    if let Some(mut name) = ctx.data(|d| d.get_temp::<String>(Id::new(("remote-host", "mkdir")))) {
        ui.horizontal(|ui| {
            ui.label("New folder:");
            let r = ui.add(TextEdit::singleline(&mut name).desired_width(200.0));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if (ui.button("Create").clicked() || enter) && !name.trim().is_empty() {
                cmds.push(("remote.host.mkdir", json!({ "name": name.trim() })));
                set_temp_text(&ctx, "mkdir", None);
            } else if ui.button("Cancel").clicked() {
                set_temp_text(&ctx, "mkdir", None);
            } else {
                set_temp_text(&ctx, "mkdir", Some(name));
            }
        });
    }
    if let Some(err) = &b.error {
        ui.label(RichText::new(err).color(theme.error));
    }
    let renaming = temp_text(&ctx, "rename-from");
    ui.separator();
    ScrollArea::vertical().id_salt("remote-host-list").auto_shrink([false, false]).show(ui, |ui| {
        for e in &b.entries {
            let selected = b.selected.as_deref() == Some(e.name.as_str());
            let icon = match e.kind {
                codecraft_engine::remote::EntryKind::Dir => "📁",
                codecraft_engine::remote::EntryKind::Symlink => "↪",
                _ => "📄",
            };
            if !renaming.is_empty() && renaming == e.name {
                let mut new_name = temp_text(&ctx, "rename-to");
                ui.horizontal(|ui| {
                    ui.label(icon);
                    let r = ui.add(TextEdit::singleline(&mut new_name).desired_width(240.0));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    if (enter || ui.button("Rename").clicked()) && !new_name.trim().is_empty() {
                        cmds.push(("remote.host.rename", json!({ "name": e.name, "newName": new_name.trim() })));
                        set_temp_text(&ctx, "rename-from", None);
                    } else if ui.button("Cancel").clicked() {
                        set_temp_text(&ctx, "rename-from", None);
                    } else {
                        set_temp_text(&ctx, "rename-to", Some(new_name));
                    }
                });
                continue;
            }
            let text = format!("{icon} {}", e.name);
            let detail = if e.kind == codecraft_engine::remote::EntryKind::File {
                format!("{} bytes · {}", e.size, e.modified)
            } else {
                e.modified.clone()
            };
            let r = ui
                .horizontal(|ui| {
                    let r = ui.add(egui::Button::selectable(selected, text).frame_when_inactive(false));
                    ui.label(RichText::new(detail).color(theme.text_muted).small());
                    r
                })
                .inner;
            if r.clicked() {
                cmds.push(("remote.host.select", json!({ "name": e.name })));
            }
            if r.double_clicked() {
                cmds.push(("remote.host.open", json!({ "name": e.name })));
            }
            r.context_menu(|ui| {
                if ui.button("Open").clicked() {
                    cmds.push(("remote.host.open", json!({ "name": e.name })));
                    ui.close();
                }
                if ui.button("Rename…").clicked() {
                    set_temp_text(&ctx, "rename-from", Some(e.name.clone()));
                    set_temp_text(&ctx, "rename-to", Some(e.name.clone()));
                    ui.close();
                }
                if ui.button("Delete…").clicked() {
                    cmds.push(("remote.host.delete", json!({ "name": e.name })));
                    ui.close();
                }
            });
        }
        if b.entries.is_empty() && !b.loading && b.error.is_none() {
            ui.label(RichText::new("Empty folder").color(theme.text_disabled));
        }
    });
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- run target fields

/// The SSH fields of the Edit Configurations "Run on" row.
pub(crate) fn ssh_target_fields(ui: &mut egui::Ui, t: &mut SshTarget) {
    egui::Grid::new("ssh-target-fields").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
        ui.label("SSH server:");
        ui.add(
            TextEdit::singleline(&mut t.server)
                .hint_text("a ~/.ssh/config host or a server from Tools › Deployment")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
        ui.label("Remote folder:");
        ui.add(
            TextEdit::singleline(&mut t.remote_dir)
                .hint_text("(the project's deployment mapping)")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
        ui.label("");
        ui.checkbox(&mut t.sync, "Upload the project before running (rsync)");
        ui.end_row();
        ui.label("Python interpreter:");
        ui.add(
            TextEdit::singleline(&mut t.python)
                .hint_text("(python3; Settings › Python › Remote interpreters)")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
    });
}

// ---------------------------------------------------------------- dialogs

/// Every dialog of this module.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    picker(&ctx, s, st);
    configuration(&ctx, s, st);
    options(&ctx, s, st);
    sync_preview(&ctx, s, st);
    confirm_delete(&ctx, s, st);
    remote_development(&ctx, s, st);
}

fn picker(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(p) = s.remote.picker.clone() else { return };
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    egui::Window::new(&p.title)
        .id(Id::new("remote-picker"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(ctx, |ui| {
            ui.label(RichText::new("Choose a server").color(st.theme.text_muted));
            for name in &p.servers {
                if ui.add(egui::Button::new(name).frame_when_inactive(false)).clicked() {
                    let mut params = p.params.clone();
                    if let Some(o) = params.as_object_mut() {
                        o.insert("server".into(), json!(name));
                    }
                    cmds.push(("remote.dialog.close", json!({ "which": "picker" })));
                    if let Some(id) = static_id(&p.command) {
                        cmds.push((id, params));
                    }
                }
            }
        });
    if !open || escape_pressed(ctx) {
        cmds.push(("remote.dialog.close", json!({ "which": "picker" })));
    }
    run_all(s, st, cmds);
}

/// The chooser re-runs one of these commands.
fn static_id(id: &str) -> Option<&'static str> {
    const IDS: &[&str] = &[
        "tools.deployment.uploadTo",
        "tools.deployment.uploadAllOpenFilesTo",
        "tools.deployment.downloadFrom",
        "tools.deployment.compareWithDeployedTo",
        "tools.deployment.syncWithDeployedTo",
        "tools.deployment.browseRemoteHost",
        "tools.startSshSession",
        "remote.host.browse",
        "file.remoteDevelopment",
    ];
    IDS.iter().copied().find(|x| *x == id)
}

fn configuration(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(mut dialog) = s.remote.dialog.clone() else { return };
    let theme = st.theme.clone();
    let servers = s.remote.loaded_servers().to_vec();
    let root = s.ws.project.as_ref().map(|p| p.root.clone());
    let cfg = root.as_deref().and_then(|r| codecraft_engine::deployment::load(r).ok()).unwrap_or_default();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    egui::Window::new("Deployment")
        .id(Id::new("remote-deployment"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(720.0, 420.0))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 70.0))
        .show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(220.0);
                    ui.horizontal(|ui| {
                        if ui
                            .button("+ Server")
                            .on_hover_text("Add an SSH server (key or agent authentication)")
                            .clicked()
                        {
                            dialog.new_server = Some(Default::default());
                        }
                        if ui.button("Reload").on_hover_text("Read ~/.ssh/config again").clicked() {
                            cmds.push(("remote.servers.reload", json!({})));
                        }
                    });
                    ScrollArea::vertical().id_salt("remote-servers").max_height(320.0).show(ui, |ui| {
                        for sv in &servers {
                            let mapped = cfg.server(&sv.name).is_some();
                            let default = cfg.default_deployment().is_some_and(|d| d.server == sv.name);
                            let mark = if default {
                                "★ "
                            } else if mapped {
                                "● "
                            } else {
                                ""
                            };
                            let sel = dialog.selected.as_deref() == Some(sv.name.as_str());
                            let r = ui.add(
                                egui::Button::selectable(sel, format!("{mark}{}", sv.name)).frame_when_inactive(false),
                            );
                            if r.clicked() {
                                cmds.push(("remote.dialog.select", json!({ "server": sv.name })));
                            }
                            r.on_hover_text(sv.label());
                        }
                        if servers.is_empty() {
                            ui.label(RichText::new("No servers yet").color(theme.text_disabled));
                        }
                    });
                });
                ui.separator();
                ui.vertical(|ui| {
                    let mut cancel_new = false;
                    if let Some(form) = &mut dialog.new_server {
                        ui.label(RichText::new("New SSH server").strong());
                        egui::Grid::new("remote-new-server").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                            for (label, field, hint) in [
                                ("Name:", &mut form.name, "my-server"),
                                ("Host:", &mut form.host, "example.com"),
                                ("Port:", &mut form.port, "22"),
                                ("User:", &mut form.user, "(ssh default)"),
                                ("Private key:", &mut form.identity_file, "~/.ssh/id_ed25519 (else the agent)"),
                            ] {
                                ui.label(label);
                                ui.add(TextEdit::singleline(field).hint_text(hint).desired_width(300.0));
                                ui.end_row();
                            }
                        });
                        ui.label(
                            RichText::new("Passwords are never stored: use a key or ssh-agent.")
                                .color(theme.text_muted)
                                .small(),
                        );
                        let params = json!({
                            "name": form.name, "host": form.host, "port": form.port,
                            "user": form.user, "identityFile": form.identity_file,
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Test Connection").clicked() {
                                cmds.push(("remote.servers.test", params.clone()));
                            }
                            if ui.button("Save").clicked() {
                                cmds.push(("remote.servers.add", params.clone()));
                            }
                            if ui.button("Cancel").clicked() {
                                cancel_new = true;
                            }
                        });
                        let key = if form.name.trim().is_empty() {
                            form.host.trim().to_string()
                        } else {
                            form.name.trim().to_string()
                        };
                        test_label(ui, &theme, s.remote.tests.get(&key));
                    }
                    if cancel_new {
                        dialog.new_server = None;
                    }
                    if dialog.new_server.is_some() {
                        return;
                    }
                    let Some(name) = dialog.selected.clone() else {
                        ui.label(
                            RichText::new("Add a server, or select one from ~/.ssh/config.").color(theme.text_muted),
                        );
                        return;
                    };
                    let info = servers.iter().find(|x| x.name == name);
                    ui.label(RichText::new(&name).strong().size(15.0));
                    if let Some(i) = info {
                        let source = match i.source {
                            codecraft_engine::remote::ServerSource::SshConfig => "from ~/.ssh/config (read-only)",
                            codecraft_engine::remote::ServerSource::CodeCraft => "CodeCraft server",
                        };
                        ui.label(RichText::new(format!("{} · {source}", i.label())).color(theme.text_muted));
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Test Connection").clicked() {
                            cmds.push(("remote.servers.test", json!({ "server": name })));
                        }
                        if info.is_some_and(|i| i.source == codecraft_engine::remote::ServerSource::CodeCraft)
                            && ui.button("Remove Server").clicked()
                        {
                            cmds.push(("remote.servers.remove", json!({ "name": name })));
                            dialog.selected = None;
                        }
                    });
                    test_label(ui, &theme, s.remote.tests.get(&name));
                    ui.add_space(8.0);
                    ui.label(RichText::new("Mapping").strong());
                    egui::Grid::new("remote-mapping").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                        ui.label("Local path:");
                        ui.add(
                            TextEdit::singleline(&mut dialog.mapping.local)
                                .hint_text("(project root)")
                                .desired_width(320.0),
                        );
                        ui.end_row();
                        ui.label("Deployment path:");
                        ui.add(
                            TextEdit::singleline(&mut dialog.mapping.remote)
                                .hint_text("~/projects/app or /srv/app")
                                .desired_width(320.0),
                        );
                        ui.end_row();
                        ui.label("Excluded paths:");
                        ui.add(
                            TextEdit::singleline(&mut dialog.mapping.excluded)
                                .hint_text("target; .git; *.log")
                                .desired_width(320.0),
                        );
                        ui.end_row();
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Save Mapping").clicked() {
                            cmds.push((
                                "remote.deployment.setMapping",
                                json!({
                                    "server": name,
                                    "local": dialog.mapping.local,
                                    "remote": dialog.mapping.remote,
                                    "excluded": dialog.mapping.excluded,
                                }),
                            ));
                        }
                        let mapped = cfg.server(&name).is_some();
                        let is_default = cfg.default_deployment().is_some_and(|d| d.server == name);
                        if ui.add_enabled(mapped && !is_default, egui::Button::new("Use as Default")).clicked() {
                            cmds.push(("remote.deployment.setDefault", json!({ "server": name })));
                        }
                        if ui.add_enabled(mapped, egui::Button::new("Remove Mapping")).clicked() {
                            cmds.push(("remote.deployment.removeServer", json!({ "server": name })));
                        }
                    });
                });
            });
            if let Some(err) = &dialog.error {
                ui.label(RichText::new(err).color(theme.error));
            }
            for w in s.remote.server_warnings() {
                ui.label(RichText::new(w).color(theme.warning).small());
            }
        });
    // Text fields are edited in place; command results may replace the dialog.
    if s.remote.dialog.is_some() {
        s.remote.dialog = Some(dialog);
    }
    if !open || escape_pressed(ctx) {
        cmds.push(("remote.dialog.close", json!({ "which": "configuration" })));
    }
    run_all(s, st, cmds);
}

fn test_label(ui: &mut egui::Ui, theme: &crate::theme::Theme, status: Option<&TestStatus>) {
    match status {
        Some(TestStatus::Running) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Connecting…");
            });
        }
        Some(TestStatus::Ok(m)) => {
            ui.label(RichText::new(format!("✔ {m}")).color(theme.success));
        }
        Some(TestStatus::Failed(e)) => {
            ui.label(RichText::new(format!("✖ {e}")).color(theme.error));
        }
        None => {}
    }
}

fn options(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if !s.remote.options_open {
        return;
    }
    let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) else {
        s.remote.options_open = false;
        return;
    };
    let cfg = codecraft_engine::deployment::load(&root).unwrap_or_default();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let (mut auto, mut delete) = (cfg.auto_upload, cfg.delete_target_items);
    egui::Window::new("Deployment Options")
        .id(Id::new("remote-options"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(ctx, |ui| {
            if ui.checkbox(&mut auto, "Upload changed files automatically to the default server (on save)").changed() {
                cmds.push(("remote.deployment.setOptions", json!({ "autoUpload": auto })));
            }
            if ui.checkbox(&mut delete, "Delete target items when source ones do not exist (Sync)").changed() {
                cmds.push(("remote.deployment.setOptions", json!({ "deleteTargetItems": delete })));
            }
            ui.label(
                RichText::new(match cfg.default_deployment() {
                    Some(d) => format!("Default server: {}", d.server),
                    None => "No default server: set one in Configuration…".into(),
                })
                .color(st.theme.text_muted),
            );
        });
    if !open || escape_pressed(ctx) {
        cmds.push(("remote.dialog.close", json!({ "which": "options" })));
    }
    run_all(s, st, cmds);
}

fn sync_preview(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(p) = s.remote.sync_preview.clone() else { return };
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    egui::Window::new(format!("Sync with {}:{}", p.server, p.remote))
        .id(Id::new("remote-sync"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(620.0, 380.0))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 70.0))
        .show(ctx, |ui| {
            if p.items.is_empty() {
                ui.label("The local folder and the deployed one are in sync.");
            }
            ScrollArea::vertical().id_salt("remote-sync-items").max_height(300.0).show(ui, |ui| {
                for (i, item) in p.items.iter().enumerate() {
                    let (arrow, what, color) = match (item.direction, item.change) {
                        (_, codecraft_engine::remote::Change::Delete) => ("✖", "delete on server", theme.vcs_deleted),
                        (codecraft_engine::remote::Direction::Upload, _) => ("→", "upload", theme.vcs_modified),
                        (codecraft_engine::remote::Direction::Download, _) => ("←", "download", theme.vcs_added),
                    };
                    let mut on = item.selected;
                    ui.horizontal(|ui| {
                        if ui.checkbox(&mut on, "").changed() {
                            cmds.push(("remote.sync.toggle", json!({ "index": i, "selected": on })));
                        }
                        ui.label(RichText::new(arrow).color(color));
                        ui.label(&item.path);
                        ui.label(RichText::new(what).color(theme.text_muted).small());
                    });
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                let any = p.items.iter().any(|i| i.selected);
                if ui.add_enabled(any, egui::Button::new("Synchronize Selected")).clicked() {
                    cmds.push(("remote.sync.apply", json!({})));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("remote.dialog.close", json!({ "which": "sync" })));
                }
            });
        });
    if !open || escape_pressed(ctx) {
        cmds.push(("remote.dialog.close", json!({ "which": "sync" })));
    }
    run_all(s, st, cmds);
}

fn confirm_delete(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(c) = s.remote.confirm.clone() else { return };
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    egui::Window::new("Delete Remote Files")
        .id(Id::new("remote-confirm-delete"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 120.0))
        .show(ctx, |ui| {
            ui.label(format!("Delete from {}? This cannot be undone.", c.server));
            for p in c.paths.iter().take(10) {
                ui.label(RichText::new(p).monospace());
            }
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Delete").color(st.theme.error)).clicked() {
                    let names: Vec<String> = c
                        .paths
                        .iter()
                        .filter_map(|p| {
                            std::path::Path::new(p).file_name().and_then(|n| n.to_str()).map(str::to_string)
                        })
                        .collect();
                    cmds.push(("remote.host.delete", json!({ "names": names, "confirm": true })));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("remote.dialog.close", json!({ "which": "confirm" })));
                }
            });
        });
    if !open || escape_pressed(ctx) {
        cmds.push(("remote.dialog.close", json!({ "which": "confirm" })));
    }
    run_all(s, st, cmds);
}

fn remote_development(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(mut d) = s.remote.remote_dev.clone() else { return };
    let names: Vec<String> = s.remote.loaded_servers().iter().map(|x| x.name.clone()).collect();
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    egui::Window::new("Remote Development")
        .id(Id::new("remote-dev"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(ctx, |ui| {
            ui.label(
                RichText::new(
                    "Open an SSH project in a new window. Saved files upload automatically while connected. \
                     Remote refreshes preserve local changes and report conflicts.",
                )
                .color(theme.text_muted),
            );
            egui::Grid::new("remote-dev-grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Server:");
                egui::ComboBox::from_id_salt("remote-dev-server").selected_text(d.server.clone()).show_ui(ui, |ui| {
                    for n in &names {
                        ui.selectable_value(&mut d.server, n.clone(), n);
                    }
                });
                ui.end_row();
                ui.label("Project folder:");
                ui.add(TextEdit::singleline(&mut d.path).hint_text("~/projects/app").desired_width(320.0));
                ui.end_row();
            });
            if ui.add_enabled(!d.server.is_empty() && !s.remote.busy(), egui::Button::new("Browse Folders…")).clicked()
            {
                cmds.push(("tools.deployment.browseRemoteHost", json!({"server": d.server, "path": d.path})));
            }
            let browser = &s.remote.browser;
            if browser.server.as_deref() == Some(&d.server) {
                ui.label(format!("Remote folder: {}", browser.path));
                if browser.loading {
                    ui.spinner();
                }
                if let Some(error) = &browser.error {
                    ui.label(RichText::new(error).color(theme.error));
                }
                if !browser.loading && browser.error.is_none() {
                    if ui.button("Parent Folder").clicked() {
                        cmds.push(("remote.host.up", json!({})));
                    }
                    ScrollArea::vertical().id_salt("remote-dev-folders").max_height(180.0).show(ui, |ui| {
                        for entry in
                            browser.entries.iter().filter(|e| e.kind == codecraft_engine::remote::EntryKind::Dir)
                        {
                            if ui.button(format!("📁 {}", entry.name)).clicked() {
                                cmds.push(("remote.host.open", json!({"name": entry.name})));
                            }
                        }
                    });
                    if ui.button("Use This Folder").clicked() {
                        cmds.push(("remote.project.chooseFolder", json!({})));
                    }
                }
            }
            if s.remote.busy() {
                ui.label("Transferring… See File Transfer for progress and errors.");
            }
            if let Some(e) = &d.error {
                ui.label(RichText::new(e).color(theme.error));
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        !s.remote.busy() && !d.server.is_empty() && !d.path.trim().is_empty(),
                        egui::Button::new("Open Project in New Window"),
                    )
                    .clicked()
                {
                    cmds.push(("file.remoteDevelopment", json!({ "server": d.server, "path": d.path.trim() })));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("remote.dialog.close", json!({ "which": "remoteDevelopment" })));
                }
            });
        });
    if s.remote.remote_dev.is_some() {
        s.remote.remote_dev = Some(d);
    }
    if !open || escape_pressed(ctx) {
        cmds.push(("remote.dialog.close", json!({ "which": "remoteDevelopment" })));
    }
    run_all(s, st, cmds);
}

/// The remote marker survives status-message changes and recent-project reopen.
pub(crate) fn status_widget(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let Some(project) = s.remote.project.clone() else { return };
    let state = match project.connection {
        codecraft_engine::remote::ConnectionState::Cached => "cached",
        codecraft_engine::remote::ConnectionState::Connecting => "connecting",
        codecraft_engine::remote::ConnectionState::Connected => "connected",
        codecraft_engine::remote::ConnectionState::Disconnected => "disconnected",
        codecraft_engine::remote::ConnectionState::Failed => "connection failed",
    };
    ui.menu_button(
        RichText::new(format!("SSH {} · {state}", project.destination)).color(st.theme.text_muted).size(12.0),
        |ui| {
            ui.label(format!("{}:{}", project.server, project.remote));
            if let Some(error) = &project.error {
                ui.label(RichText::new(error).color(st.theme.error));
            }
            if !project.pending_uploads.is_empty() {
                ui.label(format!("{} saved file(s) await review and upload", project.pending_uploads.len()));
            }
            if ui.button(if s.remote.auto_refresh { "Pause Auto-refresh" } else { "Resume Auto-refresh" }).clicked() {
                run_command(s, st, "remote.project.autoRefresh", &Value::Null);
            }
            if !s.remote.conflicts.is_empty() && ui.button(format!("Resolve {} Conflict(s)…", s.remote.conflicts.len())).clicked() {
                run_command(s, st, "remote.project.conflicts", &Value::Null);
                s.ws.layout.show(codecraft_workspace::ToolWindowId::FileTransfer);
            }
            let connected = project.connection == codecraft_engine::remote::ConnectionState::Connected;
            if ui
                .add_enabled(!s.remote.busy(), egui::Button::new(if connected { "Disconnect" } else { "Reconnect" }))
                .clicked()
            {
                ui.close();
                run_command(
                    s,
                    st,
                    if connected { "remote.project.disconnect" } else { "remote.project.reconnect" },
                    &Value::Null,
                );
            }
            if ui.add_enabled(connected && !s.remote.busy(), egui::Button::new("Refresh from Remote")).clicked() {
                ui.close();
                run_command(s, st, "remote.syncFromRemote", &Value::Null);
            }
        },
    );
}
