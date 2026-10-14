//! The SQL editor and explorer UI of track DC: Navigate › Database Object… (a popup over every
//! table, view, routine and schema of the connected data sources), the schema selector and
//! Run 'file.sql' on an attached file's toolbar, the script execution log and its dialogs
//! (continue after an error? confirm data modification), and the Schemas to Introspect chooser.
//! Everything the user does runs a `db.*` command; text fields edit the engine's state in place.

use std::path::Path;

use egui::{Align, Align2, Color32, FontId, Frame, Key, Layout, Margin, RichText, ScrollArea, Sense, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::sql_nav::{self, DbObjectKind};
use codecraft_engine::database::{ScriptMode, ScriptPhase, StmtStatus};

use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// Whether a dialog of this module reads the keyboard itself (the IDE's shortcuts pause).
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    s.db.nav.goto.is_some() || s.db.nav.schemas_dialog.is_some()
}

/// Draw the popups and dialogs that are open.
pub(crate) fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    let mut cmds: Cmds = Vec::new();
    goto_object(&ctx, s, st, &mut cmds);
    schemas_dialog(&ctx, s, st, &mut cmds);
    script_ask(&ctx, s, st, &mut cmds);
    script_confirm(&ctx, s, st, &mut cmds);
    if s.db.nav.goto.is_some() || codecraft_engine::database::sql_nav::busy(&s.db) || s.db.scripts.busy() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn popup_frame(st: &UiState) -> Frame {
    Frame::new()
        .fill(st.theme.popup_bg)
        .stroke(egui::Stroke::new(1.0, st.theme.separator))
        .corner_radius(8)
        .inner_margin(Margin::same(8))
        .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(120) })
}

fn kind_icon(kind: DbObjectKind, st: &UiState) -> (&'static str, Color32) {
    match kind {
        DbObjectKind::Table => ("☰", st.theme.function),
        DbObjectKind::View => ("👁", st.theme.constant),
        DbObjectKind::Routine => ("ƒ", st.theme.keyword),
        DbObjectKind::Schema => ("⊞", st.theme.type_),
    }
}

// ---------------------------------------------------------------- Go to Database Object

fn goto_object(ctx: &egui::Context, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let Some(popup) = s.db.nav.goto.clone() else { return };
    let objects = sql_nav::objects(&s.db);
    let ranked = sql_nav::rank_objects(&objects, &popup.query, sql_nav::GOTO_LIMIT);
    let (up, down, enter, shift, escape) = ctx.input(|i| {
        (
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
            i.key_pressed(Key::Enter),
            i.modifiers.shift,
            i.key_pressed(Key::Escape),
        )
    });
    let mut selected = popup.selected.min(ranked.len().saturating_sub(1));
    if down && selected + 1 < ranked.len() {
        selected += 1;
    }
    if up {
        selected = selected.saturating_sub(1);
    }
    if escape {
        cmds.push(("db.gotoObjectClose", Value::Null));
        return;
    }
    if enter && !ranked.is_empty() {
        let open = if shift { "data" } else { "tree" };
        cmds.push(("db.gotoObjectAccept", json!({ "index": selected, "open": open })));
        return;
    }
    let screen = ctx.content_rect();
    let width = (screen.width() * 0.5).clamp(360.0, 720.0);
    let mut clicked: Option<(usize, &'static str)> = None;
    let not_connected =
        s.db.sources
            .iter()
            .filter(|d| !matches!(d.status, codecraft_engine::database::ConnStatus::Connected(_)))
            .count();
    egui::Area::new(egui::Id::new("db-goto-object"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, [0.0, 80.0])
        .show(ctx, |ui| {
            popup_frame(st).show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Database Object").strong().color(st.theme.text));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new("⇧↩ open data").size(11.0).color(st.theme.text_muted));
                    });
                });
                let Some(g) = s.db.nav.goto.as_mut() else { return };
                let resp = ui.add(
                    TextEdit::singleline(&mut g.query)
                        .id(egui::Id::new("db-goto-object-query"))
                        .hint_text("table, view, routine or schema (schema.name)")
                        .desired_width(f32::INFINITY),
                );
                resp.request_focus();
                if resp.changed() {
                    g.selected = 0;
                    selected = 0;
                } else {
                    g.selected = selected;
                }
                ui.add_space(4.0);
                if ranked.is_empty() {
                    let msg = if objects.is_empty() && not_connected > 0 {
                        "Connect a data source to search its objects"
                    } else if objects.is_empty() {
                        "Loading…"
                    } else {
                        "Nothing found"
                    };
                    ui.label(RichText::new(msg).color(st.theme.text_muted));
                }
                ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for (row, (i, hits)) in ranked.iter().enumerate() {
                        let Some(o) = objects.get(*i) else { continue };
                        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                        if row == selected {
                            ui.painter().rect_filled(rect, 4.0, st.theme.list_selection);
                            if down || up {
                                ui.scroll_to_rect(rect, None);
                            }
                        } else if resp.hovered() {
                            ui.painter().rect_filled(rect, 4.0, st.theme.hover);
                        }
                        let (icon, color) = kind_icon(o.kind, st);
                        ui.painter().text(
                            rect.left_center() + vec2(10.0, 0.0),
                            Align2::CENTER_CENTER,
                            icon,
                            FontId::proportional(12.0),
                            color,
                        );
                        let mut job = egui::text::LayoutJob::default();
                        for (k, ch) in o.name.chars().enumerate() {
                            let hit = hits.contains(&k);
                            job.append(
                                &ch.to_string(),
                                0.0,
                                egui::TextFormat {
                                    font_id: FontId::proportional(13.0),
                                    color: if hit { st.theme.accent } else { st.theme.text },
                                    ..Default::default()
                                },
                            );
                        }
                        let where_ = if o.schema.is_empty() {
                            format!("  {} · {}", o.kind.label(), o.source_name)
                        } else {
                            format!("  {} · {} · {}", o.kind.label(), o.schema, o.source_name)
                        };
                        job.append(
                            &where_,
                            0.0,
                            egui::TextFormat {
                                font_id: FontId::proportional(12.0),
                                color: st.theme.text_muted,
                                ..Default::default()
                            },
                        );
                        let galley = ui.fonts_mut(|f| f.layout_job(job));
                        ui.painter().galley(
                            rect.left_center() + vec2(22.0, -galley.size().y / 2.0),
                            galley,
                            st.theme.text,
                        );
                        if resp.clicked() || resp.double_clicked() {
                            clicked = Some((row, "tree"));
                        }
                    }
                });
            });
        });
    if let Some((row, open)) = clicked {
        cmds.push(("db.gotoObjectAccept", json!({ "index": row, "open": open })));
    }
}

// ---------------------------------------------------------------- Schemas to Introspect

fn schemas_dialog(ctx: &egui::Context, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let Some(d) = s.db.nav.schemas_dialog.as_mut() else { return };
    let mut open = true;
    let mut cancel = false;
    let source = d.source.clone();
    egui::Window::new(format!("Schemas to Introspect — {source}"))
        .id(egui::Id::new("db-schemas-dialog"))
        .collapsible(false)
        .resizable(true)
        .default_width(360.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(RichText::new("Only the chosen schemas are listed and loaded.").color(st.theme.text_muted));
            ui.horizontal(|ui| {
                if ui.small_button("All").clicked() {
                    d.chosen = d.all.iter().cloned().collect();
                }
                if ui.small_button("None").clicked() {
                    d.chosen.clear();
                }
            });
            ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for name in d.all.clone() {
                    let mut on = d.chosen.contains(&name);
                    if ui.checkbox(&mut on, &name).changed() {
                        if on {
                            d.chosen.insert(name);
                        } else {
                            d.chosen.remove(&name);
                        }
                    }
                }
            });
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                if ui.add(egui::Button::new("OK").fill(st.theme.accent)).clicked() {
                    let all = d.chosen.len() == d.all.len() || d.chosen.is_empty();
                    let schemas: Value = if all { Value::Null } else { json!(d.chosen.iter().collect::<Vec<_>>()) };
                    cmds.push(("db.introspectSchemas", json!({ "dataSource": source, "schemas": schemas })));
                }
            });
        });
    if !open || cancel || ctx.input(|i| i.key_pressed(Key::Escape)) {
        s.db.nav.schemas_dialog = None;
    }
}

// ---------------------------------------------------------------- scripts

fn first_line(sql: &str, max: usize) -> String {
    let line = sql.lines().find(|l| !l.trim().is_empty() && !l.trim().starts_with("--")).unwrap_or("").trim();
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max || sql.lines().filter(|l| !l.trim().is_empty()).count() > 1 {
        out.push('…');
    }
    out
}

fn script_ask(ctx: &egui::Context, s: &Session, st: &UiState, cmds: &mut Cmds) {
    let Some(run) = s.db.scripts.runs.iter().rev().find(|r| r.state == ScriptPhase::Asking) else { return };
    let Some(at) = run.asking_at() else { return };
    let Some(stmt) = run.statements.get(at) else { return };
    let error = match &stmt.status {
        StmtStatus::Failed { error } => error.clone(),
        _ => String::new(),
    };
    egui::Window::new(format!("Run '{}'", run.title))
        .id(egui::Id::new("db-script-ask"))
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .show(ctx, |ui| {
            ui.label(
                RichText::new(format!("Statement {} (line {}) failed:", at + 1, stmt.line + 1)).color(st.theme.text),
            );
            ui.label(RichText::new(first_line(&stmt.text, 120)).monospace().color(st.theme.text_editor));
            ui.label(RichText::new(&error).color(st.theme.error));
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Stop").clicked() {
                    cmds.push(("db.scriptDecision", json!({ "script": run.id, "action": "stop" })));
                }
                if ui.button("Continue All").on_hover_text("Don't ask again for this run").clicked() {
                    cmds.push(("db.scriptDecision", json!({ "script": run.id, "action": "continueAll" })));
                }
                if ui.add(egui::Button::new("Continue").fill(st.theme.accent)).clicked() {
                    cmds.push(("db.scriptDecision", json!({ "script": run.id, "action": "continue" })));
                }
            });
        });
}

fn script_confirm(ctx: &egui::Context, s: &Session, st: &UiState, cmds: &mut Cmds) {
    let Some(c) = &s.db.scripts.confirm else { return };
    let mut open = true;
    egui::Window::new("Confirm Data Modification")
        .id(egui::Id::new("db-script-confirm"))
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(
                RichText::new(format!(
                    "{} is a production data source. Run the script? It modifies data:",
                    c.source_name
                ))
                .color(st.theme.warning),
            );
            ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for w in c.writes.iter().take(50) {
                    ui.label(RichText::new(first_line(w, 120)).monospace().color(st.theme.text_editor));
                }
            });
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cmds.push(("db.scriptConfirm", json!({ "confirm": false })));
                }
                if ui.add(egui::Button::new("Execute").fill(st.theme.error)).clicked() {
                    cmds.push(("db.scriptConfirm", json!({ "confirm": true })));
                }
            });
        });
    if !open {
        cmds.push(("db.scriptConfirm", json!({ "confirm": false })));
    }
}

/// The execution log of the script shown in Services (instead of the Output log).
pub(crate) fn script_log(ui: &mut egui::Ui, s: &Session, st: &UiState, cmds: &mut Cmds) -> bool {
    let Some(run) = s.db.scripts.shown.and_then(|id| s.db.scripts.get(id)) else { return false };
    ui.horizontal(|ui| {
        let state = match run.state {
            ScriptPhase::Running => "running…",
            ScriptPhase::Asking => "waiting for an answer",
            ScriptPhase::Done => "done",
            ScriptPhase::Stopped => "stopped",
        };
        ui.label(
            RichText::new(format!("Run '{}' · {} · {}", run.title, run.mode.label(), state))
                .strong()
                .color(st.theme.text),
        );
        ui.label(RichText::new(run.summary()).color(if run.failed() > 0 {
            st.theme.error
        } else {
            st.theme.text_muted
        }));
        if run.state == ScriptPhase::Running {
            ui.spinner();
        }
    });
    ui.add(egui::Separator::default().spacing(2.0));
    let current = run.statements.iter().position(|x| x.status == StmtStatus::Pending);
    let path = run.path.clone();
    ScrollArea::both().id_salt("db-script-log").auto_shrink([false, false]).show_rows(
        ui,
        20.0,
        run.statements.len(),
        |ui, range| {
            for (k, stmt) in run.statements.iter().enumerate().skip(range.start).take(range.len()) {
                let (icon, color, outcome) = match &stmt.status {
                    StmtStatus::Pending if run.state == ScriptPhase::Running && current == Some(k) => {
                        ("⏳", st.theme.warning, "running".to_string())
                    }
                    StmtStatus::Pending => ("·", st.theme.text_muted, String::new()),
                    StmtStatus::Done { ms, rows, affected } => {
                        let what = match (rows, affected) {
                            (Some(r), _) => format!("{r} row(s) retrieved"),
                            (None, Some(a)) => format!("{a} row(s) affected"),
                            _ => "completed".into(),
                        };
                        ("✔", st.theme.success, format!("{what} in {ms} ms"))
                    }
                    StmtStatus::Failed { error } => ("✖", st.theme.error, error.clone()),
                    StmtStatus::Skipped => ("⏭", st.theme.text_muted, "skipped".into()),
                };
                let resp = ui
                    .horizontal(|ui| {
                        ui.label(RichText::new(icon).color(color));
                        ui.label(
                            RichText::new(format!("{:>4}", stmt.line + 1)).monospace().color(st.theme.line_number),
                        );
                        ui.label(RichText::new(first_line(&stmt.text, 80)).monospace().color(st.theme.text_editor));
                        ui.label(RichText::new(outcome).color(if color == st.theme.error {
                            st.theme.error
                        } else {
                            st.theme.text_muted
                        }));
                    })
                    .response
                    .interact(Sense::click());
                if resp.double_clicked() {
                    cmds.push(("navigate.openLocation", json!({ "path": path, "line": stmt.line, "column": 0 })));
                }
                resp.on_hover_text(stmt.text.chars().take(2000).collect::<String>());
            }
        },
    );
    true
}

// ---------------------------------------------------------------- the attached file's toolbar

/// The schema selector (consoles and files) and Run 'file.sql' with its error mode (files).
pub(crate) fn file_toolbar(
    ui: &mut egui::Ui,
    s: &Session,
    st: &UiState,
    path: Option<&Path>,
    source: Option<&str>,
    is_console: bool,
) -> Option<(&'static str, Value)> {
    let path = path?;
    let ds = source.and_then(|id| s.db.source(id))?;
    let mut cmd = None;
    let schemas: Vec<String> = ds.tree.schemas.keys().cloned().collect();
    let current = s.db.nav.schema_for_path(path);
    if schemas.len() > 1 || current.is_some() {
        let label = current.clone().unwrap_or_else(|| "<default schema>".into());
        let mut chosen = current.clone();
        egui::ComboBox::from_id_salt("db-file-schema").selected_text(label).width(140.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut chosen, None, "<default schema>");
            for sc in &schemas {
                ui.selectable_value(&mut chosen, Some(sc.clone()), sc);
            }
        });
        if chosen != current {
            cmd = Some(("db.attachFile", json!({ "path": path, "dataSource": ds.config.id, "schema": chosen })));
        }
    }
    if !is_console {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let running = s.db.scripts.runs.iter().any(|r| r.path == path && r.state == ScriptPhase::Running);
        let mode = s.db.nav.settings.script_mode;
        if ui
            .add_enabled(
                !running,
                egui::Button::new(RichText::new(format!("▶▶ Run '{name}'")).size(12.5).color(st.theme.success))
                    .frame(false),
            )
            .on_hover_text(format!("Run every statement of the file ({})", mode.label()))
            .clicked()
        {
            cmd = Some(("db.runFile", json!({ "path": path })));
        }
        ui.menu_button(RichText::new("⚙").size(12.0).color(st.theme.text_muted), |ui| {
            for m in ScriptMode::ALL {
                if ui.radio(mode == m, m.label()).clicked() {
                    cmd = Some(("db.setScriptMode", json!({ "mode": m })));
                    ui.close();
                }
            }
            if let Some(r) = s.db.scripts.latest_for(path)
                && ui.button("Show Last Execution Log").clicked()
            {
                cmd = Some(("db.showScriptLog", json!({ "script": r.id })));
                ui.close();
            }
        })
        .response
        .on_hover_text("Script error mode");
    }
    cmd
}
