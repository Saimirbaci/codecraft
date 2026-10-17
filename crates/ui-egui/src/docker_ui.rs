//! Docker in the Services tool window (M11 track D): connection → Containers / Compose /
//! Images / Volumes / Networks / Dev Containers, with streamed logs, inspect JSON and actions.
//!
//! Every action is a `docker.*` / `tools.services.*` command; this module paints
//! [`codecraft_engine::docker::DockerState`] and keeps only the tree selection (egui memory).

use std::time::Duration;

use egui::{Color32, FontId, Frame, Margin, RichText, ScrollArea, TextEdit};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::docker::{self, Container, PruneKind};
use codecraft_engine::docker_target::{DEFAULT_MOUNT, DockerTarget};

use crate::theme::Theme;
use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// The selected tree node: `container:<id>`, `image:<id>`, `volume:<name>`, `network:<id>`,
/// `compose:<name>`.
fn selection_id() -> egui::Id {
    egui::Id::new("docker-services-selection")
}

/// Docker in Services.
pub fn show_services(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    docker::auto_refresh(s);
    let busy = s.docker.busy();
    ui.ctx().request_repaint_after(if busy { Duration::from_millis(100) } else { Duration::from_secs(2) });
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    toolbar(ui, s, &theme, &mut cmds);
    if let Some(c) = &s.docker.confirm {
        Frame::new().inner_margin(Margin::symmetric(6, 4)).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&c.message).color(theme.warning));
                if ui.button("Prune").clicked() {
                    cmds.push(("docker.prune", json!({ "what": c.prune.key(), "confirm": true })));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("docker.prune.cancel", Value::Null));
                }
            });
        });
    }
    if let Some(e) = &s.docker.error {
        ui.label(RichText::new(format!("Cannot connect to Docker: {e}")).color(theme.error));
    }
    ui.add(egui::Separator::default().spacing(2.0));
    let selected: Option<String> = ui.data(|d| d.get_temp(selection_id()));
    let width = ui.available_width();
    egui::Panel::left("docker-tree")
        .resizable(true)
        .default_size((width * 0.38).clamp(180.0, 480.0))
        .size_range(140.0..=(width * 0.8).max(160.0))
        .frame(Frame::new().inner_margin(Margin { right: 6, ..Margin::ZERO }))
        .show(ui, |ui| tree(ui, s, &theme, selected.as_deref(), &mut cmds));
    details(ui, s, &theme, selected.as_deref(), &mut cmds);
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn toolbar(ui: &mut egui::Ui, s: &Session, theme: &Theme, cmds: &mut Cmds) {
    let d = &s.docker;
    ui.horizontal(|ui| {
        let current = d.connection.clone();
        let selected = match &current {
            Some(c) => c.clone(),
            None => format!("{} (current)", d.connection_label()),
        };
        egui::ComboBox::from_id_salt("docker-connection").selected_text(selected).width(180.0).show_ui(ui, |ui| {
            if ui.selectable_label(current.is_none(), "Current context").clicked() {
                cmds.push(("tools.services.docker.connection", json!({ "context": null })));
            }
            for c in &d.contexts {
                let label = if c.current { format!("{} ●", c.name) } else { c.name.clone() };
                let resp = ui.selectable_label(current.as_deref() == Some(c.name.as_str()), label);
                let resp = resp.on_hover_text(format!("{}\n{}", c.description, c.endpoint));
                if resp.clicked() {
                    cmds.push(("tools.services.docker.connection", json!({ "context": c.name })));
                }
            }
            ui.separator();
            if ui.button("Reload Connections From Contexts").clicked() {
                cmds.push(("tools.services.docker.connectionsFromContexts", Value::Null));
            }
        });
        if ui.button("⟳").on_hover_text("Refresh").clicked() {
            cmds.push(("docker.refresh", Value::Null));
        }
        ui.menu_button("Prune", |ui| {
            for k in PruneKind::ALL {
                if ui.button(k.label()).clicked() {
                    cmds.push(("docker.prune", json!({ "what": k.key() })));
                    ui.close();
                }
            }
        });
        ui.menu_button("Dev Containers", |ui| {
            let items: [(&str, &'static str); 4] = [
                ("Show Dev Containers", "tools.services.devContainers.showDevContainers"),
                ("Build and Start", "docker.devContainer.start"),
                ("Open Terminal", "docker.devContainer.terminal"),
                ("New Dev Container Configuration…", "tools.services.devContainers.newDevContainerConfiguration"),
            ];
            for (label, id) in items {
                if ui.add_enabled(s.enabled(id).is_ok(), egui::Button::new(label)).clicked() {
                    cmds.push((id, Value::Null));
                    ui.close();
                }
            }
        });
        ui.menu_button("Registry", |ui| {
            let status = d.registry.as_ref();
            match status {
                None => {
                    ui.label(RichText::new("Login status not read yet").color(theme.text_muted));
                }
                Some(r) if r.registries.is_empty() => {
                    ui.label(RichText::new("Not logged in to any registry").color(theme.text_muted));
                }
                Some(r) => {
                    for name in &r.registries {
                        ui.label(format!("✓ {name}"));
                    }
                }
            }
            if let Some(store) = status.and_then(|r| r.creds_store.as_ref()) {
                ui.label(RichText::new(format!("Credentials store: {store}")).color(theme.text_muted));
            }
            ui.separator();
            if ui.button("Check Login Status").clicked() {
                cmds.push(("tools.services.dockerRegistry", Value::Null));
            }
            if ui.button("Log In… (docker login)").clicked() {
                cmds.push(("tools.services.dockerRegistry", json!({ "login": true })));
                ui.close();
            }
        });
        if d.loading || !d.operations.is_empty() {
            ui.spinner();
            let text = d.operations.last().cloned().unwrap_or_else(|| "Loading…".into());
            ui.label(RichText::new(text).color(theme.text_muted));
        }
    });
}

fn state_color(theme: &Theme, c: &Container) -> Color32 {
    if c.is_paused() {
        theme.warning
    } else if c.is_running() {
        theme.success
    } else if c.state == "dead" {
        theme.error
    } else {
        theme.text_disabled
    }
}

fn select(ui: &egui::Ui, key: String) {
    ui.data_mut(|d| d.insert_temp(selection_id(), key));
}

/// One selectable tree row; returns its response.
fn row(
    ui: &mut egui::Ui,
    selected: Option<&str>,
    key: &str,
    dot: Option<Color32>,
    text: &str,
    detail: &str,
    theme: &Theme,
) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    if let Some(color) = dot {
        job.append("● ", 0.0, egui::TextFormat { color, font_id: FontId::proportional(11.0), ..Default::default() });
    }
    job.append(
        text,
        0.0,
        egui::TextFormat { color: theme.text, font_id: FontId::proportional(13.0), ..Default::default() },
    );
    if !detail.is_empty() {
        job.append(
            &format!("  {detail}"),
            0.0,
            egui::TextFormat { color: theme.text_muted, font_id: FontId::proportional(12.0), ..Default::default() },
        );
    }
    let resp = ui.selectable_label(selected == Some(key), job);
    if resp.clicked() {
        select(ui, key.to_string());
    }
    resp
}

fn container_menu(ui: &mut egui::Ui, c: &Container, cmds: &mut Cmds) {
    let id = json!({ "id": c.id });
    let mut item = |ui: &mut egui::Ui, label: &str, cmd: &'static str, params: Value, enabled: bool| {
        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
            cmds.push((cmd, params));
            ui.close();
        }
    };
    let (running, paused) = (c.is_running(), c.is_paused());
    item(ui, "Start", "docker.container.start", id.clone(), !running && !paused);
    item(ui, "Stop", "docker.container.stop", id.clone(), running || paused);
    item(ui, "Restart", "docker.container.restart", id.clone(), true);
    item(ui, "Pause", "docker.container.pause", id.clone(), running && !paused);
    item(ui, "Unpause", "docker.container.unpause", id.clone(), paused);
    ui.separator();
    item(ui, "Show Log", "docker.container.logs", id.clone(), true);
    item(ui, "Inspect", "docker.inspect", id.clone(), true);
    item(ui, "Exec (Terminal)", "docker.container.exec", id.clone(), running);
    ui.separator();
    item(ui, "Delete", "docker.container.remove", json!({ "id": c.id, "force": running }), true);
}

fn tree(ui: &mut egui::Ui, s: &Session, theme: &Theme, selected: Option<&str>, cmds: &mut Cmds) {
    let d = &s.docker;
    ScrollArea::vertical().id_salt("docker-tree-scroll").auto_shrink([false, false]).show(ui, |ui| {
        let header = format!("🐳 {}", d.connection_label());
        egui::CollapsingHeader::new(RichText::new(header).strong()).id_salt("docker-conn").default_open(true).show(
            ui,
            |ui| {
                if !d.loaded {
                    ui.label(RichText::new("Loading…").color(theme.text_muted));
                    return;
                }
                let projects = d.data.compose_projects();
                let loose: Vec<&Container> =
                    d.data.containers.iter().filter(|c| c.compose_project().is_none()).collect();
                let containers_title = format!("Containers ({})", d.data.containers.len());
                egui::CollapsingHeader::new(containers_title).id_salt("docker-containers").default_open(true).show(
                    ui,
                    |ui| {
                        for c in &loose {
                            container_row(ui, c, theme, selected, cmds);
                        }
                        if loose.is_empty() {
                            ui.label(
                                RichText::new("No containers outside Compose projects").color(theme.text_disabled),
                            );
                        }
                    },
                );
                if !projects.is_empty() {
                    egui::CollapsingHeader::new(format!("Docker Compose ({})", projects.len()))
                        .id_salt("docker-compose")
                        .default_open(true)
                        .show(ui, |ui| {
                            for p in &projects {
                                let key = format!("compose:{}", p.name);
                                let color = if p.running > 0 { theme.success } else { theme.text_disabled };
                                let title = format!("{} ({}/{})", p.name, p.running, p.containers.len());
                                let resp = egui::CollapsingHeader::new(RichText::new(title).color(color))
                                    .id_salt(("docker-compose-project", &p.name))
                                    .default_open(false)
                                    .show(ui, |ui| {
                                        for id in &p.containers {
                                            if let Some(c) = d.data.container(id) {
                                                container_row(ui, c, theme, selected, cmds);
                                            }
                                        }
                                    });
                                if resp.header_response.clicked() {
                                    select(ui, key.clone());
                                }
                                resp.header_response.context_menu(|ui| {
                                    for (label, cmd) in [
                                        ("Up", "docker.compose.up"),
                                        ("Start", "docker.compose.start"),
                                        ("Stop", "docker.compose.stop"),
                                        ("Restart", "docker.compose.restart"),
                                        ("Down", "docker.compose.down"),
                                    ] {
                                        if ui.button(label).clicked() {
                                            cmds.push((cmd, json!({ "project": p.name })));
                                            ui.close();
                                        }
                                    }
                                });
                            }
                        });
                }
                egui::CollapsingHeader::new(format!("Images ({})", d.data.images.len())).id_salt("docker-images").show(
                    ui,
                    |ui| {
                        for i in &d.data.images {
                            let key = format!("image:{}", i.id);
                            let resp = row(ui, selected, &key, None, &i.reference(), &i.size, theme);
                            resp.context_menu(|ui| {
                                let r = i.reference();
                                let items: [(&str, &'static str, Value); 4] = [
                                    ("Create Container", "docker.image.run", json!({ "image": r })),
                                    ("Pull", "docker.image.pull", json!({ "image": r })),
                                    ("Inspect", "docker.inspect", json!({ "id": i.id, "kind": "image" })),
                                    ("Delete", "docker.image.remove", json!({ "id": i.id })),
                                ];
                                for (label, cmd, params) in items {
                                    if ui.button(label).clicked() {
                                        cmds.push((cmd, params));
                                        ui.close();
                                    }
                                }
                            });
                        }
                    },
                );
                egui::CollapsingHeader::new(format!("Volumes ({})", d.data.volumes.len()))
                    .id_salt("docker-volumes")
                    .show(ui, |ui| {
                        for v in &d.data.volumes {
                            let key = format!("volume:{}", v.name);
                            let short: String = v.name.chars().take(40).collect();
                            let resp = row(ui, selected, &key, None, &short, &v.driver, theme).on_hover_text(&v.name);
                            resp.context_menu(|ui| {
                                if ui.button("Inspect").clicked() {
                                    cmds.push(("docker.inspect", json!({ "id": v.name, "kind": "volume" })));
                                    ui.close();
                                }
                                if ui.button("Delete").clicked() {
                                    cmds.push(("docker.volume.remove", json!({ "name": v.name })));
                                    ui.close();
                                }
                            });
                        }
                    });
                egui::CollapsingHeader::new(format!("Networks ({})", d.data.networks.len()))
                    .id_salt("docker-networks")
                    .show(ui, |ui| {
                        for n in &d.data.networks {
                            let key = format!("network:{}", n.id);
                            let resp = row(ui, selected, &key, None, &n.name, &n.driver, theme);
                            resp.context_menu(|ui| {
                                if ui.button("Inspect").clicked() {
                                    cmds.push(("docker.inspect", json!({ "id": n.id, "kind": "network" })));
                                    ui.close();
                                }
                                if ui.button("Delete").clicked() {
                                    cmds.push(("docker.network.remove", json!({ "id": n.id })));
                                    ui.close();
                                }
                            });
                        }
                    });
                if !d.dev_containers.is_empty() {
                    egui::CollapsingHeader::new(format!("Dev Containers ({})", d.dev_containers.len()))
                        .id_salt("docker-dev")
                        .default_open(true)
                        .show(ui, |ui| {
                            for c in &d.dev_containers {
                                container_row(ui, c, theme, selected, cmds);
                            }
                        });
                }
            },
        );
    });
}

fn container_row(ui: &mut egui::Ui, c: &Container, theme: &Theme, selected: Option<&str>, cmds: &mut Cmds) {
    let key = format!("container:{}", c.id);
    let resp = row(ui, selected, &key, Some(state_color(theme, c)), &c.name, &c.image, theme)
        .on_hover_text(format!("{}\n{}\n{}", c.status, c.command, c.ports));
    if resp.double_clicked() {
        cmds.push(("docker.container.logs", json!({ "id": c.id })));
    }
    resp.context_menu(|ui| container_menu(ui, c, cmds));
}

fn details(ui: &mut egui::Ui, s: &Session, theme: &Theme, selected: Option<&str>, cmds: &mut Cmds) {
    let d = &s.docker;
    // Tabs: open logs and the inspect view.
    if !d.logs.is_empty() || d.inspect.is_some() {
        ScrollArea::horizontal().id_salt("docker-view-tabs").max_height(28.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                for l in &d.logs {
                    let title = if l.running { format!("▶ {}", l.title) } else { l.title.clone() };
                    if ui.selectable_label(d.active_log == Some(l.id) && d.inspect.is_none(), title).clicked() {
                        cmds.push(("docker.logs.select", json!({ "log": l.id })));
                    }
                    if ui
                        .add(egui::Button::new(RichText::new("×").color(theme.text_muted)).frame(false))
                        .on_hover_text("Close")
                        .clicked()
                    {
                        cmds.push(("docker.logs.close", json!({ "log": l.id })));
                    }
                }
                if let Some(i) = &d.inspect {
                    let _ = ui.selectable_label(true, &i.title);
                    if ui.add(egui::Button::new(RichText::new("×").color(theme.text_muted)).frame(false)).clicked() {
                        cmds.push(("docker.inspect.close", Value::Null));
                    }
                }
            });
        });
        ui.add(egui::Separator::default().spacing(2.0));
    }
    if let Some(i) = &d.inspect {
        ScrollArea::both().id_salt("docker-inspect").auto_shrink([false, false]).show(ui, |ui| {
            let mut text = i.json.as_str();
            ui.add(TextEdit::multiline(&mut text).font(FontId::monospace(12.0)).desired_width(f32::INFINITY));
        });
        return;
    }
    if let Some(l) = d.active_log.and_then(|id| d.log(id)) {
        ScrollArea::both().id_salt(("docker-log", l.id)).stick_to_bottom(true).auto_shrink([false, false]).show(
            ui,
            |ui| {
                for line in &l.lines {
                    ui.label(RichText::new(line).monospace().size(12.0).color(theme.text));
                }
            },
        );
        return;
    }
    let (kind, id) = selected.and_then(|k| k.split_once(':')).unwrap_or(("", ""));
    match kind {
        "container" => {
            let Some(c) = d.data.container(id).or_else(|| d.dev_containers.iter().find(|c| c.id == id)) else {
                return output(ui, s, theme);
            };
            ui.label(RichText::new(&c.name).strong().size(15.0));
            ui.horizontal_wrapped(|ui| container_buttons(ui, c, cmds));
            ui.add_space(4.0);
            egui::Grid::new("docker-container-info").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                for (k, v) in [
                    ("Status", c.status.as_str()),
                    ("Image", c.image.as_str()),
                    ("Command", c.command.as_str()),
                    ("Ports", c.ports.as_str()),
                    ("Created", c.created.as_str()),
                    ("ID", c.id.as_str()),
                ] {
                    ui.label(RichText::new(k).color(theme.text_muted));
                    ui.label(v);
                    ui.end_row();
                }
                if let (Some(p), Some(svc)) = (c.compose_project(), c.compose_service()) {
                    ui.label(RichText::new("Compose").color(theme.text_muted));
                    ui.label(format!("{p} › {svc}"));
                    ui.end_row();
                }
            });
        }
        "compose" => {
            ui.label(RichText::new(format!("Compose project {id}")).strong().size(15.0));
            ui.horizontal_wrapped(|ui| {
                for (label, cmd) in [
                    ("Up", "docker.compose.up"),
                    ("Start", "docker.compose.start"),
                    ("Stop", "docker.compose.stop"),
                    ("Restart", "docker.compose.restart"),
                    ("Down", "docker.compose.down"),
                ] {
                    if ui.button(label).clicked() {
                        cmds.push((cmd, json!({ "project": id })));
                    }
                }
            });
            if let Some(p) = d.data.compose_projects().into_iter().find(|p| p.name == id) {
                ui.label(RichText::new(&p.working_dir).color(theme.text_muted));
                for f in &p.config_files {
                    ui.label(RichText::new(f).monospace().color(theme.text_muted));
                }
            }
        }
        "image" => {
            let Some(i) = d.data.images.iter().find(|i| i.id == id) else { return output(ui, s, theme) };
            ui.label(RichText::new(i.reference()).strong().size(15.0));
            ui.label(RichText::new(format!("{} · {} · {}", i.id, i.size, i.created)).color(theme.text_muted));
            ui.horizontal(|ui| {
                if ui.button("Create Container").clicked() {
                    cmds.push(("docker.image.run", json!({ "image": i.reference() })));
                }
                if ui.button("Pull").clicked() {
                    cmds.push(("docker.image.pull", json!({ "image": i.reference() })));
                }
                if ui.button("Inspect").clicked() {
                    cmds.push(("docker.inspect", json!({ "id": i.id, "kind": "image" })));
                }
                if ui.button(RichText::new("Delete").color(theme.error)).clicked() {
                    cmds.push(("docker.image.remove", json!({ "id": i.id })));
                }
            });
        }
        "volume" | "network" => {
            ui.label(RichText::new(id).strong().size(15.0));
            let (inspect_kind, remove, params) = if kind == "volume" {
                ("volume", "docker.volume.remove", json!({ "name": id }))
            } else {
                ("network", "docker.network.remove", json!({ "id": id }))
            };
            ui.horizontal(|ui| {
                if ui.button("Inspect").clicked() {
                    cmds.push(("docker.inspect", json!({ "id": id, "kind": inspect_kind })));
                }
                if ui.button(RichText::new("Delete").color(theme.error)).clicked() {
                    cmds.push((remove, params.clone()));
                }
            });
        }
        _ => output(ui, s, theme),
    }
}

fn container_buttons(ui: &mut egui::Ui, c: &Container, cmds: &mut Cmds) {
    let id = json!({ "id": c.id });
    let (running, paused) = (c.is_running(), c.is_paused());
    let buttons: [(&str, &'static str, bool); 7] = [
        ("Start", "docker.container.start", !running && !paused),
        ("Stop", "docker.container.stop", running || paused),
        ("Restart", "docker.container.restart", true),
        (
            if paused { "Unpause" } else { "Pause" },
            if paused { "docker.container.unpause" } else { "docker.container.pause" },
            running || paused,
        ),
        ("Log", "docker.container.logs", true),
        ("Inspect", "docker.inspect", true),
        ("Terminal", "docker.container.exec", running),
    ];
    for (label, cmd, enabled) in buttons {
        if ui.add_enabled(enabled, egui::Button::new(label)).clicked() {
            cmds.push((cmd, id.clone()));
        }
    }
    if ui.button("Delete").clicked() {
        cmds.push(("docker.container.remove", json!({ "id": c.id, "force": running })));
    }
}

/// The operations log (when nothing is selected).
fn output(ui: &mut egui::Ui, s: &Session, theme: &Theme) {
    let d = &s.docker;
    if d.output.is_empty() {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(
                    "Select a container, image, volume or network. Double-click a container to follow its log.",
                )
                .color(theme.text_muted),
            );
        });
        return;
    }
    ScrollArea::vertical().id_salt("docker-output").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        for line in &d.output {
            ui.label(RichText::new(line).monospace().size(12.0).color(theme.console_system));
        }
    });
}

/// The Docker target's fields in Edit Configurations (Run on: Docker).
pub(crate) fn target_fields(ui: &mut egui::Ui, t: &mut DockerTarget) {
    egui::Grid::new("docker-target-fields").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
        ui.label("Image:");
        ui.add(TextEdit::singleline(&mut t.image).hint_text("rust:1").desired_width(f32::INFINITY));
        ui.end_row();
        ui.label("Or running container:");
        ui.add(
            TextEdit::singleline(&mut t.container)
                .hint_text("(docker exec into it instead)")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
        ui.label("Project mounted at:");
        ui.add(TextEdit::singleline(&mut t.mount).hint_text(DEFAULT_MOUNT).desired_width(f32::INFINITY));
        ui.end_row();
        ui.label("Run options:");
        ui.add(
            TextEdit::singleline(&mut t.run_options)
                .hint_text("-p 8080:80 --network host")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
        ui.label("Docker context:");
        ui.add(
            TextEdit::singleline(&mut t.context)
                .hint_text("(the one selected in Services)")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
        ui.label("Or Compose service:");
        ui.add(
            TextEdit::singleline(&mut t.service)
                .hint_text("(docker compose run --rm <service>)")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
        ui.label("Compose file:");
        ui.add(TextEdit::singleline(&mut t.compose_file).hint_text("compose.yaml").desired_width(f32::INFINITY));
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
