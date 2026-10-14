//! Track DB UI: the transaction controls of SQL consoles (mode, isolation, Commit/Rollback, the
//! open-transaction indicator), the status-bar transaction widget, the pending-transaction
//! question (Commit / Rollback / Cancel before disconnecting, closing a console or the project,
//! or quitting), and the Data Tools window (Compare Data, Dump, Restore, Export to File,
//! Full-text Search, with their jobs). OWNED BY the database track. Every action is a `db.*`
//! command; form fields edit the engine's state in place.

use std::time::Duration;

use egui::{Align, Align2, Color32, Layout, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::data_tools::{DataJob, JobResult, JobState, Tool};
use codecraft_engine::database::tx::GuardState;
use codecraft_engine::db::tx::Isolation;
use codecraft_engine::db::{Cell, DbKind};

use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// View state of the Data Tools window.
#[derive(Default)]
pub struct DataUi {
    /// Compare result tab: 0 only left, 1 only right, 2 different.
    pub compare_tab: usize,
}

fn mmss(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

// ---------------------------------------------------------------- console toolbar

/// The console's transaction controls: Tx mode, isolation, Commit, Rollback and the indicator.
pub fn console_tx(
    ui: &mut egui::Ui,
    s: &Session,
    st: &UiState,
    worker: &str,
    source: &str,
) -> Option<(&'static str, Value)> {
    let ds = s.db.source(source)?;
    let mut cmd = None;
    let manual = s.db.manual_commit(worker, source);
    let mut chosen = manual;
    egui::ComboBox::from_id_salt("db-tx-mode")
        .selected_text(if manual { "Tx: Manual" } else { "Tx: Auto" })
        .width(92.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut chosen, false, "Auto-commit");
            ui.selectable_value(&mut chosen, true, "Manual commit");
        })
        .response
        .on_hover_text("Transaction mode of this console");
    if chosen != manual {
        cmd = Some(("db.tx.setMode", json!({ "mode": if chosen { "manual" } else { "auto" }, "connection": worker })));
    }
    let level = s.db.isolation(worker, source);
    let mut picked = level;
    let kind: DbKind = ds.config.kind;
    egui::ComboBox::from_id_salt("db-tx-isolation")
        .selected_text(level.map(Isolation::label).unwrap_or("Default"))
        .width(118.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut picked, None, "Default");
            for l in Isolation::supported(kind) {
                ui.selectable_value(&mut picked, Some(*l), l.label());
            }
        })
        .response
        .on_hover_text("Transaction isolation level (from the next transaction)");
    if picked != level {
        let id = picked.map(Isolation::id).unwrap_or("default");
        cmd = Some(("db.tx.setIsolation", json!({ "level": id, "connection": worker })));
    }
    let tx = s.db.txs.conn(worker);
    let open = s.db.transaction_open(worker);
    let theme = &st.theme;
    if ui
        .add_enabled(open, egui::Button::new(RichText::new("✔").color(theme.success)).frame(false))
        .on_hover_text("Commit")
        .clicked()
    {
        cmd = Some(("db.tx.commit", json!({ "connection": worker })));
    }
    if ui
        .add_enabled(open, egui::Button::new(RichText::new("↶").color(theme.error)).frame(false))
        .on_hover_text("Rollback")
        .clicked()
    {
        cmd = Some(("db.tx.rollback", json!({ "connection": worker })));
    }
    if let Some(c) = tx {
        if c.open && c.failed {
            ui.label(RichText::new("✖ Tx failed: roll back").size(12.0).color(theme.error)).on_hover_text(
                "A statement failed in the transaction; only Rollback works (Commit would not save anything)",
            );
        } else if c.open {
            let dur = c.duration().map(mmss).unwrap_or_default();
            let n = c.statements;
            ui.label(
                RichText::new(format!("● pending transaction · {n} stmt{} · {dur}", if n == 1 { "" } else { "s" }))
                    .size(12.0)
                    .color(theme.warning),
            )
            .on_hover_text("Uncommitted transaction: Commit or Rollback");
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        if let Some(lost) = &c.lost {
            ui.label(RichText::new("⚠ connection lost").size(12.0).color(theme.error)).on_hover_text(lost);
        }
    }
    cmd
}

// ---------------------------------------------------------------- status bar

/// The status bar's transaction widget: open transactions (click for Commit/Rollback per
/// connection) and lost connections.
pub fn status_widget(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let pending = s.db.pending_transactions(None);
    let lost = s.db.txs.conns.values().filter(|c| c.lost.is_some()).count();
    let jobs = s.db.data.jobs.iter().filter(|j| j.is_running()).count();
    let mut cmd: Option<(&'static str, Value)> = None;
    if jobs > 0 {
        let resp = ui.add(
            egui::Label::new(
                RichText::new(format!("⛁ {jobs} data job{}", if jobs == 1 { "" } else { "s" }))
                    .size(12.0)
                    .color(st.theme.text_muted),
            )
            .sense(egui::Sense::click()),
        );
        if resp.on_hover_text("Data Tools: dumps, exports, comparisons, searches").clicked() {
            s.db.data.open = true;
        }
    }
    if lost > 0 {
        ui.label(RichText::new(format!("⚠ {lost} lost")).size(12.0).color(st.theme.error))
            .on_hover_text("Database connections were lost; they reconnect for the next query (db.tx.pending)");
    }
    if pending.is_empty() {
        if let Some((id, p)) = cmd {
            run_command(s, st, id, &p);
        }
        return;
    }
    let failed = pending.iter().any(|t| t.failed);
    let color = if failed { st.theme.error } else { st.theme.warning };
    ui.menu_button(RichText::new(format!("● Tx {}", pending.len())).size(12.0).color(color), |ui| {
        ui.label(RichText::new("Open transactions").strong());
        for t in &pending {
            ui.separator();
            let state = if t.failed { " (failed)" } else { "" };
            ui.label(format!(
                "{} · {} · {} statement{} · {}{state}",
                t.data_source_name,
                short_conn(&t.connection),
                t.statements,
                if t.statements == 1 { "" } else { "s" },
                mmss(Duration::from_millis(t.duration_ms))
            ));
            ui.horizontal(|ui| {
                if ui.add_enabled(!t.failed, egui::Button::new("Commit")).clicked() {
                    cmd = Some(("db.tx.commit", json!({ "connection": t.connection })));
                    ui.close();
                }
                if ui.button("Rollback").clicked() {
                    cmd = Some(("db.tx.rollback", json!({ "connection": t.connection })));
                    ui.close();
                }
            });
        }
    })
    .response
    .on_hover_text("Uncommitted database transactions");
    ui.ctx().request_repaint_after(Duration::from_secs(1));
    if let Some((id, p)) = cmd {
        run_command(s, st, id, &p);
    }
}

/// `console:/…/consoles/pg/console.sql` → `console.sql`.
fn short_conn(c: &str) -> String {
    match c.split_once(':') {
        Some(("console", path)) => {
            format!(
                "console {}",
                std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
            )
        }
        Some((kind, _)) => kind.to_string(),
        None => c.to_string(),
    }
}

// ---------------------------------------------------------------- windows

/// The pending-transaction question and the Data Tools window.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    let mut cmds: Cmds = Vec::new();
    guard(&ctx, s, st, &mut cmds);
    data_tools(&ctx, s, st, &mut cmds);
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn guard(ctx: &egui::Context, s: &Session, st: &UiState, cmds: &mut Cmds) {
    let Some(g) = &s.db.txs.guard else { return };
    let theme = &st.theme;
    let mut open = true;
    egui::Window::new("Uncommitted Transactions")
        .id(egui::Id::new("db-tx-guard"))
        .collapsible(false)
        .resizable(false)
        .default_width(480.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .open(&mut open)
        .show(ctx, |ui| {
            let n = g.transactions.len();
            ui.label(format!(
                "{} transaction{} open. Commit or roll back before: {}.",
                if n == 1 { "One".to_string() } else { n.to_string() },
                if n == 1 { " is" } else { "s are" },
                g.action.to_lowercase()
            ));
            ui.add_space(4.0);
            egui::Grid::new("db-tx-guard-grid").num_columns(4).spacing([12.0, 4.0]).show(ui, |ui| {
                for t in &g.transactions {
                    ui.label(RichText::new(&t.data_source_name).color(theme.text));
                    ui.label(RichText::new(short_conn(&t.connection)).color(theme.text_muted));
                    ui.label(format!("{} stmt{}", t.statements, if t.statements == 1 { "" } else { "s" }));
                    if t.failed {
                        ui.label(RichText::new("failed: only Rollback works").color(theme.error));
                    } else {
                        ui.label(mmss(Duration::from_millis(t.duration_ms)));
                    }
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            let busy = matches!(g.state, GuardState::Ending(_));
            match &g.state {
                GuardState::Ending(commit) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(if *commit { "Committing…" } else { "Rolling back…" });
                    });
                }
                GuardState::Failed(e) => {
                    ui.label(
                        RichText::new(format!("{e}\nThe {} did not happen.", g.action.to_lowercase()))
                            .color(theme.error),
                    );
                }
                GuardState::Asking => {}
            }
            let any_failed = g.transactions.iter().any(|t| t.failed);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
                    cmds.push(("db.tx.resolve", json!({ "choice": "cancel" })));
                }
                if ui.add_enabled(!busy, egui::Button::new("Rollback")).clicked() {
                    cmds.push(("db.tx.resolve", json!({ "choice": "rollback" })));
                }
                let commit = egui::Button::new("Commit").fill(theme.accent);
                if ui.add_enabled(!busy && !any_failed && !matches!(g.state, GuardState::Failed(_)), commit).clicked() {
                    cmds.push(("db.tx.resolve", json!({ "choice": "commit" })));
                }
            });
        });
    if !open && !matches!(g.state, GuardState::Ending(_)) {
        cmds.push(("db.tx.resolve", json!({ "choice": "cancel" })));
    }
}

/// Context-menu entries of the Database tree (`kind`: source, schema or table).
pub fn tree_items(
    ui: &mut egui::Ui,
    kind: &str,
    source: &str,
    schema: Option<&str>,
    table: Option<&str>,
) -> Option<(&'static str, Value)> {
    let mut out = None;
    let mut item = |ui: &mut egui::Ui, label: &str, tool: &str| {
        if ui.button(label).clicked() {
            out = Some((
                "db.dataTools.open",
                json!({ "tool": tool, "dataSource": source, "schema": schema, "table": table }),
            ));
            ui.close();
        }
    };
    ui.separator();
    match kind {
        "table" => {
            item(ui, "Export Data to File…", "export");
            item(ui, "Compare Data…", "compare");
            item(ui, "Dump Table…", "dump");
        }
        "schema" => {
            item(ui, "Full-text Search…", "search");
            item(ui, "Dump…", "dump");
        }
        _ => {
            item(ui, "Full-text Search…", "search");
            item(ui, "Compare Data…", "compare");
            item(ui, "Dump…", "dump");
            item(ui, "Restore…", "restore");
        }
    }
    out
}

fn source_combo(ui: &mut egui::Ui, s: &Session, id: &str, value: &mut String) {
    let label = s.db.source(value).map(|d| d.config.name.clone()).unwrap_or_else(|| "<data source>".into());
    egui::ComboBox::from_id_salt(id).selected_text(label).width(180.0).show_ui(ui, |ui| {
        for ds in &s.db.sources {
            ui.selectable_value(value, ds.config.id.clone(), &ds.config.name);
        }
    });
}

fn data_tools(ctx: &egui::Context, s: &mut Session, st: &mut UiState, cmds: &mut Cmds) {
    if !s.db.data.open {
        return;
    }
    let mut open = true;
    let theme = st.theme.clone();
    egui::Window::new("Data Tools")
        .id(egui::Id::new("db-data-tools"))
        .collapsible(true)
        .resizable(true)
        .default_size([760.0, 520.0])
        .open(&mut open)
        .show(ctx, |ui| {
            // Tool picker and form.
            ui.horizontal(|ui| {
                for (tool, label) in [
                    (Tool::Compare, "Compare Data"),
                    (Tool::Dump, "Dump"),
                    (Tool::Restore, "Restore"),
                    (Tool::Export, "Export to File"),
                    (Tool::Search, "Full-text Search"),
                ] {
                    let sel = s.db.data.dialog.as_ref().is_some_and(|d| d.tool == tool);
                    if ui.selectable_label(sel, label).clicked() {
                        let mut d = s.db.data.dialog.clone().unwrap_or_default();
                        d.tool = tool;
                        if d.left.data_source.is_empty()
                            && let Some(first) = s.db.sources.first()
                        {
                            d.left.data_source = first.config.id.clone();
                            d.right.data_source = first.config.id.clone();
                        }
                        s.db.data.dialog = Some(d);
                    }
                }
            });
            ui.separator();
            form(ui, s, &theme, cmds);
            ui.separator();
            jobs(ui, s, st, &theme, cmds);
        });
    if !open {
        cmds.push(("db.dataTools.close", Value::Null));
    }
}

fn form(ui: &mut egui::Ui, s: &mut Session, theme: &crate::theme::Theme, cmds: &mut Cmds) {
    let snapshot = s.db.data.dialog.clone();
    let Some(mut d) = snapshot else {
        ui.label(RichText::new("Choose a tool above.").color(theme.text_muted));
        return;
    };
    let busy = s.db.data.busy();
    egui::Grid::new("db-data-form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        let label = |ui: &mut egui::Ui, t: &str| {
            ui.label(RichText::new(t).color(theme.text));
        };
        match d.tool {
            Tool::Compare => {
                for (name, side, salt) in [("Left", &mut d.left, "db-cmp-l"), ("Right", &mut d.right, "db-cmp-r")] {
                    label(ui, name);
                    ui.horizontal(|ui| {
                        source_combo(ui, s, salt, &mut side.data_source);
                        ui.add(TextEdit::singleline(&mut side.schema).hint_text("schema").desired_width(90.0));
                        ui.add(TextEdit::singleline(&mut side.table).hint_text("table").desired_width(130.0));
                        ui.add(TextEdit::singleline(&mut side.sql).hint_text("or a query").desired_width(200.0));
                    });
                    ui.end_row();
                }
                label(ui, "Key columns");
                ui.add(
                    TextEdit::singleline(&mut d.keys)
                        .hint_text("id, … (default: the primary key)")
                        .desired_width(300.0),
                );
                ui.end_row();
            }
            Tool::Dump | Tool::Restore | Tool::Export | Tool::Search => {
                label(ui, "Data source");
                source_combo(ui, s, "db-tool-src", &mut d.left.data_source);
                ui.end_row();
            }
        }
        match d.tool {
            Tool::Dump => {
                label(ui, "Tables");
                let mut tables = d.dump.tables.join(", ");
                if ui
                    .add(TextEdit::singleline(&mut tables).hint_text("all (or schema.table, …)").desired_width(300.0))
                    .changed()
                {
                    d.dump.tables =
                        tables.split(',').map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).collect();
                }
                ui.end_row();
                label(ui, "Content");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut d.dump.data_only, "Data only");
                    ui.checkbox(&mut d.dump.schema_only, "Schema only");
                    let pg = s.db.source(&d.left.data_source).is_some_and(|x| x.config.kind == DbKind::Postgres);
                    if pg {
                        ui.checkbox(&mut d.dump.custom_format, "Custom format (pg_restore)");
                    }
                });
                ui.end_row();
            }
            Tool::Export => {
                label(ui, "Table or query");
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut d.left.schema).hint_text("schema").desired_width(90.0));
                    ui.add(TextEdit::singleline(&mut d.left.table).hint_text("table").desired_width(130.0));
                    ui.add(TextEdit::singleline(&mut d.left.sql).hint_text("or a query").desired_width(220.0));
                });
                ui.end_row();
                label(ui, "Format");
                egui::ComboBox::from_id_salt("db-export-format").selected_text(d.format.to_uppercase()).show_ui(
                    ui,
                    |ui| {
                        for f in ["csv", "tsv", "json", "sql", "markdown"] {
                            ui.selectable_value(&mut d.format, f.to_string(), f.to_uppercase());
                        }
                    },
                );
                ui.end_row();
            }
            Tool::Search => {
                label(ui, "Schema");
                ui.add(TextEdit::singleline(&mut d.left.schema).hint_text("default schema").desired_width(160.0));
                ui.end_row();
                label(ui, "Text");
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut d.text).hint_text("text to find").desired_width(220.0));
                    ui.checkbox(&mut d.case_sensitive, "Match case");
                    ui.add(TextEdit::singleline(&mut d.limit).desired_width(40.0)).on_hover_text("Rows per table");
                });
                ui.end_row();
            }
            Tool::Compare | Tool::Restore => {}
        }
        if matches!(d.tool, Tool::Dump | Tool::Restore | Tool::Export) {
            label(ui, if d.tool == Tool::Restore { "Dump file" } else { "Output file" });
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut d.path).desired_width(360.0));
                if ui.button("…").clicked() {
                    let dialog = rfd::FileDialog::new();
                    let picked = if d.tool == Tool::Restore { dialog.pick_file() } else { dialog.save_file() };
                    if let Some(p) = picked {
                        d.path = p.display().to_string();
                    }
                }
            });
            ui.end_row();
        }
    });
    if d.tool == Tool::Restore {
        let prod = s.db.source(&d.left.data_source).is_some_and(|x| x.config.confirm_writes);
        let text = if prod {
            "Restoring modifies this production data source (one transaction; stops at the first error)."
        } else {
            "Runs the dump in one transaction and stops at the first error."
        };
        ui.label(RichText::new(text).color(if prod { theme.warning } else { theme.text_muted }));
    }
    let run_label = match d.tool {
        Tool::Compare => "Compare",
        Tool::Dump => "Dump",
        Tool::Restore => "Restore",
        Tool::Export => "Export",
        Tool::Search => "Search",
    };
    ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
        let fill = if d.tool == Tool::Restore { theme.error } else { theme.accent };
        if ui.add_enabled(!busy, egui::Button::new(run_label).fill(fill)).clicked() {
            cmds.push(("db.dataTools.run", Value::Null));
        }
    });
    if s.db.data.dialog.as_ref() != Some(&d) {
        s.db.data.dialog = Some(d);
    }
}

fn jobs(ui: &mut egui::Ui, s: &Session, st: &mut UiState, theme: &crate::theme::Theme, cmds: &mut Cmds) {
    if s.db.data.jobs.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for j in s.db.data.jobs.iter().rev() {
            let sel = s.db.data.active == Some(j.id);
            let (icon, color) = match &j.state {
                JobState::Running => ("⏳", theme.warning),
                JobState::Done => ("✔", theme.success),
                JobState::Failed(_) => ("✖", theme.error),
                JobState::Cancelled => ("■", theme.text_muted),
            };
            let text = RichText::new(format!("{icon} {}", j.kind.label())).color(if sel { theme.text } else { color });
            if ui.selectable_label(sel, text).on_hover_text(&j.title).clicked() {
                st.db.data.compare_tab = 0;
                cmds.push(("db.job.select", json!({ "job": j.id })));
            }
        }
    });
    let Some(j) = s.db.data.active.and_then(|id| s.db.data.job(id)) else { return };
    job_view(ui, s, st, theme, j, cmds);
}

fn job_view(
    ui: &mut egui::Ui,
    _s: &Session,
    st: &mut UiState,
    theme: &crate::theme::Theme,
    j: &DataJob,
    cmds: &mut Cmds,
) {
    ui.label(RichText::new(&j.title).strong().color(theme.text));
    ui.horizontal(|ui| {
        match &j.state {
            JobState::Running => {
                match j.fraction() {
                    Some(f) => {
                        ui.add(egui::ProgressBar::new(f).desired_width(260.0).text(j.progress_text()));
                    }
                    None => {
                        ui.spinner();
                        ui.label(j.progress_text());
                    }
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("db.job.cancel", json!({ "job": j.id })));
                }
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            }
            JobState::Done => {
                let t = j.elapsed_ms.map(|m| format!(" in {:.1} s", m as f64 / 1000.0)).unwrap_or_default();
                ui.label(RichText::new(format!("Done{t}: {}", j.progress_text())).color(theme.success));
            }
            JobState::Failed(e) => {
                ui.label(RichText::new(e).color(theme.error));
            }
            JobState::Cancelled => {
                ui.label(RichText::new("Cancelled").color(theme.text_muted));
            }
        }
        if !j.is_running() && ui.small_button("Close").clicked() {
            cmds.push(("db.job.close", json!({ "job": j.id })));
        }
    });
    match &j.result {
        Some(JobResult::Compare(c)) => {
            let n = c.result.counts;
            ui.label(format!(
                "{} left rows, {} right rows · {} only left · {} only right · {} different · {} equal{}",
                n.left,
                n.right,
                n.only_left,
                n.only_right,
                n.different,
                n.same,
                if c.result.truncated { " (differences truncated)" } else { "" }
            ));
            ui.horizontal(|ui| {
                for (i, label) in ["Only left", "Only right", "Different"].iter().enumerate() {
                    if ui.selectable_label(st.db.data.compare_tab == i, *label).clicked() {
                        st.db.data.compare_tab = i;
                    }
                }
                ui.separator();
                let can_sync = c.right.is_table() && !c.result.truncated && !n.identical();
                if ui.add_enabled(can_sync, egui::Button::new("Copy Sync Script")).clicked() {
                    cmds.push(("db.compareData.syncScript", json!({ "job": j.id, "copy": true })));
                }
                if ui
                    .add_enabled(can_sync, egui::Button::new("Run Sync Script…"))
                    .on_hover_text("Make the right table equal to the left (production data sources ask first)")
                    .clicked()
                {
                    cmds.push(("db.compareData.syncScript", json!({ "job": j.id, "run": true })));
                }
            });
            ScrollArea::both().id_salt("db-compare-rows").max_height(260.0).show(ui, |ui| {
                egui::Grid::new("db-compare-grid").striped(true).spacing([10.0, 2.0]).show(ui, |ui| {
                    for name in &c.result.columns {
                        ui.label(RichText::new(name).strong());
                    }
                    ui.end_row();
                    let cell = |ui: &mut egui::Ui, v: &Cell, color: Color32| {
                        ui.label(RichText::new(v.display()).monospace().color(color));
                    };
                    match st.db.data.compare_tab {
                        0 => {
                            for row in c.result.only_left.iter().take(1000) {
                                for v in row {
                                    cell(ui, v, theme.diff_added);
                                }
                                ui.end_row();
                            }
                        }
                        1 => {
                            for row in c.result.only_right.iter().take(1000) {
                                for v in row {
                                    cell(ui, v, theme.diff_removed);
                                }
                                ui.end_row();
                            }
                        }
                        _ => {
                            for d in c.result.different.iter().take(1000) {
                                for (i, v) in d.left.iter().enumerate() {
                                    if d.changed.contains(&i) {
                                        let right = d.right.get(i).map(Cell::display).unwrap_or_default();
                                        ui.label(
                                            RichText::new(format!("{} → {right}", v.display()))
                                                .monospace()
                                                .background_color(theme.diff_modified_bg)
                                                .color(theme.text_editor),
                                        );
                                    } else {
                                        cell(ui, v, theme.text_editor);
                                    }
                                }
                                ui.end_row();
                            }
                        }
                    }
                });
            });
        }
        Some(JobResult::Search { text, tables }) => {
            ui.label(format!("“{text}”: {} tables with matches", tables.iter().filter(|t| !t.rows.is_empty()).count()));
            ScrollArea::vertical().id_salt("db-search-hits").max_height(300.0).show(ui, |ui| {
                for (ti, t) in tables.iter().enumerate() {
                    let title = match &t.error {
                        Some(e) => format!("{}.{} — {e}", t.schema, t.table),
                        None => {
                            format!("{}.{} ({}{})", t.schema, t.table, t.rows.len(), if t.truncated { "+" } else { "" })
                        }
                    };
                    egui::CollapsingHeader::new(title)
                        .id_salt(("db-hit", j.id, ti))
                        .default_open(tables.len() <= 5)
                        .show(ui, |ui| {
                            for (ri, row) in t.rows.iter().enumerate() {
                                let line: Vec<String> = t
                                    .columns
                                    .iter()
                                    .zip(row)
                                    .filter(|(c, v)| {
                                        t.searched.contains(&c.name)
                                            && v.display().to_lowercase().contains(&text.to_lowercase())
                                            || t.key_columns.contains(&c.name)
                                    })
                                    .map(|(c, v)| {
                                        format!("{}={}", c.name, v.display().chars().take(80).collect::<String>())
                                    })
                                    .collect();
                                if ui.link(line.join("  ")).on_hover_text("Open the row").clicked() {
                                    cmds.push((
                                        "db.fullTextSearch.open",
                                        json!({ "job": j.id, "table": ti, "row": ri }),
                                    ));
                                }
                            }
                        });
                }
            });
        }
        Some(JobResult::File { path, rows, bytes }) => {
            let what = match (rows, bytes) {
                (Some(r), _) => format!("{r} rows"),
                (_, Some(b)) => format!("{:.1} MB", *b as f64 / 1048576.0),
                _ => String::new(),
            };
            ui.label(format!("{} {what}", path.display()));
        }
        Some(JobResult::Restored { path, statements }) => {
            ui.label(format!(
                "Restored {}{}",
                path.display(),
                statements.map(|n| format!(" ({n} statements)")).unwrap_or_default()
            ));
        }
        None => {}
    }
    if !j.log.is_empty() {
        egui::CollapsingHeader::new(format!("Log ({})", j.log.len())).id_salt(("db-job-log", j.id)).show(ui, |ui| {
            ScrollArea::vertical().max_height(140.0).stick_to_bottom(true).show(ui, |ui| {
                for l in &j.log {
                    ui.label(RichText::new(l).monospace().size(11.5).color(theme.text_muted));
                }
            });
        });
    }
}
