//! The Redis key browser window (track DD): database, pattern and type filter, the keys grouped
//! into `:` namespaces (virtualized), and the selected key's value with its TTL, paging and
//! edits. Everything goes through `db.redis.*` commands; production data sources confirm each
//! change after showing the exact Redis commands. OWNED BY track DD.

use egui::{Align2, Color32, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::redis_browser::{RedisBrowser, TreeRow, ValueView, op_from_params, ttl_text};

use crate::{UiState, run_command};

/// View state: text being edited and the pending confirmation.
#[derive(Default)]
pub struct RedisUi {
    /// The browser the buffers below belong to.
    source: Option<String>,
    pattern: String,
    /// The key whose value buffers are loaded.
    value_key: Option<Vec<u8>>,
    text: String,
    ttl: String,
    rename: String,
    field: String,
    value: String,
    score: String,
    head: bool,
    /// A cell being edited: (row, text).
    cell: Option<(usize, String)>,
    /// The New Key form.
    new_key: Option<NewKey>,
    /// A change waiting for confirmation on a production data source: (command, params, preview).
    confirm: Option<(String, Value, String)>,
    /// Keys ticked for deletion.
    ticked: Vec<Vec<u8>>,
    /// The tree rows for (source, revision).
    tree: Option<(String, u64, Vec<TreeRow>)>,
}

#[derive(Clone, Default)]
struct NewKey {
    key: String,
    kind: String,
    field: String,
    value: String,
    score: String,
}

const TYPES: [&str; 8] = ["all", "string", "hash", "list", "set", "zset", "stream", "ReJSON-RL"];
const ROW_H: f32 = 20.0;

type Cmds = Vec<(String, Value)>;

/// `key` as text when it is UTF-8, else as hex (`keyHex`).
fn put_bytes(p: &mut Value, name: &str, bytes: &[u8]) {
    if let Some(o) = p.as_object_mut() {
        match std::str::from_utf8(bytes) {
            Ok(t) => o.insert(name.to_string(), json!(t)),
            Err(_) => {
                o.insert(format!("{name}Hex"), json!(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()))
            }
        };
    }
}

fn key_params(source: &str, key: &[u8]) -> Value {
    let mut p = json!({ "dataSource": source });
    put_bytes(&mut p, "key", key);
    p
}

fn kind_color(kind: &str, st: &UiState) -> Color32 {
    let t = &st.theme;
    match kind {
        "string" => t.string,
        "hash" => t.keyword,
        "list" => t.number,
        "set" => t.type_,
        "zset" => t.link,
        "stream" => t.warning,
        _ => t.text_muted,
    }
}

fn kind_badge(kind: &str) -> &'static str {
    match kind {
        "string" => "STR",
        "hash" => "HASH",
        "list" => "LIST",
        "set" => "SET",
        "zset" => "ZSET",
        "stream" => "STRM",
        "ReJSON-RL" => "JSON",
        _ => "?",
    }
}

/// The key browser window, when one is shown.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    let Some(source) = s.db.redis.active.clone() else { return };
    if s.db.redis.get(&source).is_none() {
        return;
    }
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let name = s.db.source(&source).map(|d| d.config.name.clone()).unwrap_or_else(|| source.clone());
    let production = s.db.source(&source).is_some_and(|d| d.config.confirm_writes);
    let read_only = s.db.source(&source).is_some_and(|d| d.config.read_only);
    let mut ru = std::mem::take(&mut st.db.redis);
    if ru.source.as_deref() != Some(source.as_str()) {
        ru = RedisUi { source: Some(source.clone()), ..RedisUi::default() };
        ru.pattern = s.db.redis.get(&source).map(|b| b.pattern.clone()).unwrap_or_default();
    }
    if let Some(b) = s.db.redis.get(&source) {
        if b.busy_ui() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
        crate::windows::resizable(
            egui::Window::new(format!("Redis — {name}")).default_size([920.0, 560.0]),
            st,
            "db-redis",
        )
        .id(egui::Id::new("db-redis-browser"))
        .pivot(Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .open(&mut open)
        .collapsible(false)
        .show(&ctx, |ui| {
            toolbar(ui, st, &mut ru, b, &source, read_only, &mut cmds);
            footer(ui, st, b);
            ui.separator();
            // The body fills the window exactly; its two columns are child UIs that clip
            // instead of growing the window (a growing window would move every widget).
            let avail = ui.available_size().max(vec2(320.0, 200.0));
            let (rect, _) = ui.allocate_exact_size(avail, egui::Sense::hover());
            let left_w = (rect.width() * 0.38).clamp(200.0, 420.0);
            let left = egui::Rect::from_min_size(rect.min, vec2(left_w, rect.height()));
            let right = egui::Rect::from_min_max(egui::pos2(left.max.x + 9.0, rect.min.y), rect.max);
            let clip = ui.clip_rect();
            let mut lui =
                ui.new_child(egui::UiBuilder::new().max_rect(left).layout(egui::Layout::top_down(egui::Align::Min)));
            lui.set_clip_rect(left.intersect(clip));
            key_list(&mut lui, st, &mut ru, b, &source, read_only, &mut cmds);
            ui.painter().vline(left.max.x + 4.0, rect.y_range(), egui::Stroke::new(1.0, st.theme.border));
            let mut rui =
                ui.new_child(egui::UiBuilder::new().max_rect(right).layout(egui::Layout::top_down(egui::Align::Min)));
            rui.set_clip_rect(right.intersect(clip));
            value_panel(&mut rui, st, &mut ru, b, &source, read_only, &mut cmds);
        });
    }
    new_key_window(&ctx, st, &mut ru, &source, &mut cmds);
    confirm_window(&ctx, st, &mut ru, &mut cmds);
    if !open {
        cmds.push(("db.redis.close".into(), json!({ "dataSource": source })));
    }
    // Production data sources: edits wait for the confirmation (with the exact commands).
    let mut run: Cmds = Vec::new();
    for (id, p) in cmds {
        let is_edit = codecraft_engine::database::redis_browser::EDIT_COMMANDS.contains(&id.as_str());
        if is_edit && production && p.get("confirm").is_none() {
            let preview = op_from_params(&id, &p).map(|op| op.preview()).unwrap_or_else(|e| format!("(invalid: {e})"));
            ru.confirm = Some((id, p, preview));
        } else {
            run.push((id, p));
        }
    }
    st.db.redis = ru;
    for (id, p) in run {
        run_command(s, st, &id, &p);
    }
}

trait BusyUi {
    fn busy_ui(&self) -> bool;
}

impl BusyUi for RedisBrowser {
    fn busy_ui(&self) -> bool {
        self.scanning || self.loading_value || self.writing
    }
}

fn toolbar(
    ui: &mut egui::Ui,
    st: &UiState,
    ru: &mut RedisUi,
    b: &RedisBrowser,
    source: &str,
    read_only: bool,
    cmds: &mut Cmds,
) {
    let theme = &st.theme;
    ui.horizontal_wrapped(|ui| {
        let label = |n: u32| match b.keyspace.get(&n) {
            Some(k) => format!("db{n} ({k})"),
            None => format!("db{n}"),
        };
        egui::ComboBox::from_id_salt("redis-db").selected_text(label(b.db)).width(110.0).show_ui(ui, |ui| {
            for n in 0..b.databases.max(1) {
                if ui.selectable_label(n == b.db, label(n)).clicked() {
                    cmds.push(("db.redis.selectDb".into(), json!({ "dataSource": source, "db": n })));
                }
            }
        });
        let resp = ui.add(TextEdit::singleline(&mut ru.pattern).hint_text("pattern, e.g. user:*").desired_width(200.0));
        let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let current = b.type_filter.clone().unwrap_or_else(|| "all".into());
        let mut kind = current.clone();
        egui::ComboBox::from_id_salt("redis-type").selected_text(&kind).width(100.0).show_ui(ui, |ui| {
            for t in TYPES {
                ui.selectable_value(&mut kind, t.to_string(), t);
            }
        });
        if enter
            || kind != current
            || ui.button("Scan").on_hover_text("SCAN with MATCH and TYPE (never KEYS)").clicked()
        {
            cmds.push(("db.redis.scan".into(), json!({ "dataSource": source, "pattern": ru.pattern, "type": kind })));
        }
        if ui.add_enabled(!read_only, egui::Button::new("+ Key")).on_hover_text("New key").clicked() {
            ru.new_key = Some(NewKey { kind: "string".into(), score: "0".into(), ..NewKey::default() });
        }
        if !ru.ticked.is_empty()
            && ui
                .add_enabled(
                    !read_only,
                    egui::Button::new(RichText::new(format!("Delete {}", ru.ticked.len())).color(theme.error)),
                )
                .clicked()
        {
            // Text keys in one UNLINK; binary keys one by one (as hex).
            let mut text: Vec<Value> = Vec::new();
            for k in &ru.ticked {
                match std::str::from_utf8(k) {
                    Ok(t) => text.push(json!(t)),
                    Err(_) => cmds.push(("db.redis.deleteKeys".into(), key_params(source, k))),
                }
            }
            if !text.is_empty() {
                cmds.push(("db.redis.deleteKeys".into(), json!({ "dataSource": source, "keys": text })));
            }
            ru.ticked.clear();
        }
        if b.scanning {
            ui.spinner();
        }
        if let Some(v) = &b.version {
            ui.label(RichText::new(v).size(11.5).color(theme.text_muted));
        }
        if read_only {
            ui.label(RichText::new("read-only").size(11.5).color(theme.text_muted));
        }
    });
}

fn key_list(
    ui: &mut egui::Ui,
    st: &UiState,
    ru: &mut RedisUi,
    b: &RedisBrowser,
    source: &str,
    read_only: bool,
    cmds: &mut Cmds,
) {
    let theme = &st.theme;
    ui.horizontal(|ui| {
        let more = if b.cursor.is_some() { " (more on the server)" } else { "" };
        ui.label(RichText::new(format!("{} keys{more}", b.keys.len())).size(12.0).color(theme.text_muted));
        if b.cursor.is_some() && ui.add_enabled(!b.scanning, egui::Button::new("Load more").small()).clicked() {
            cmds.push(("db.redis.scanMore".into(), json!({ "dataSource": source })));
        }
    });
    if let Some(e) = &b.error {
        ui.label(RichText::new(e).color(theme.error));
    }
    let stale = ru.tree.as_ref().is_none_or(|(s, r, _)| s != source || *r != b.revision);
    if stale {
        ru.tree = Some((source.to_string(), b.revision, b.tree(':')));
    }
    let rows: &[TreeRow] = ru.tree.as_ref().map(|(_, _, r)| r.as_slice()).unwrap_or_default();
    let selected = b.selected.clone();
    let mut tick: Option<Vec<u8>> = None;
    ScrollArea::vertical().id_salt("redis-keys").auto_shrink([false, false]).show_rows(
        ui,
        ROW_H,
        rows.len(),
        |ui, range| {
            for row in rows.get(range).unwrap_or_default() {
                ui.horizontal(|ui| {
                    ui.add_space(row.depth as f32 * 14.0);
                    match (row.key.and_then(|i| b.keys.get(i)), &row.prefix) {
                        (Some(k), _) => {
                            let mut on = ru.ticked.contains(&k.key);
                            if !read_only && ui.checkbox(&mut on, "").changed() {
                                tick = Some(k.key.clone());
                            }
                            ui.label(
                                RichText::new(kind_badge(&k.kind))
                                    .size(10.5)
                                    .monospace()
                                    .color(kind_color(&k.kind, st)),
                            );
                            let sel = selected.as_ref() == Some(&k.key);
                            let resp = ui.selectable_label(sel, &row.label);
                            let resp = if k.ttl_ms >= 0 {
                                resp.on_hover_text(format!("{} · TTL {}", k.name, ttl_text(k.ttl_ms)))
                            } else {
                                resp.on_hover_text(&k.name)
                            };
                            if resp.clicked() {
                                cmds.push(("db.redis.openKey".into(), key_params(source, &k.key)));
                            }
                        }
                        (None, Some(prefix)) => {
                            let arrow = if row.expanded { "▾" } else { "▸" };
                            let text = RichText::new(format!("{arrow} {}  ", row.label)).color(theme.text);
                            let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
                            ui.label(RichText::new(row.count.to_string()).size(11.0).color(theme.text_muted));
                            if resp.clicked() {
                                cmds.push((
                                    "db.redis.toggleNamespace".into(),
                                    json!({ "dataSource": source, "prefix": prefix, "expanded": !row.expanded }),
                                ));
                            }
                        }
                        (None, None) => {}
                    }
                });
            }
        },
    );
    if let Some(k) = tick {
        if let Some(i) = ru.ticked.iter().position(|t| *t == k) {
            ru.ticked.remove(i);
        } else {
            ru.ticked.push(k);
        }
    }
}

fn value_panel(
    ui: &mut egui::Ui,
    st: &UiState,
    ru: &mut RedisUi,
    b: &RedisBrowser,
    source: &str,
    read_only: bool,
    cmds: &mut Cmds,
) {
    let theme = &st.theme;
    if b.loading_value && b.value.is_none() {
        ui.spinner();
        return;
    }
    if let Some(e) = &b.value_error {
        ui.label(RichText::new(e).color(theme.error));
    }
    let Some(v) = &b.value else {
        ui.label(RichText::new("Select a key to see its value").color(theme.text_muted));
        return;
    };
    // New key shown: reset the buffers.
    if ru.value_key.as_ref() != Some(&v.key) {
        ru.value_key = Some(v.key.clone());
        ru.text = v.text.clone().unwrap_or_default();
        ru.ttl = if v.ttl_ms > 0 { format!("{}", v.ttl_ms / 1000) } else { String::new() };
        ru.rename = v.name.clone();
        ru.field.clear();
        ru.value.clear();
        ru.score = "0".into();
        ru.cell = None;
    }
    let edit = !read_only;
    let kp = || key_params(source, &v.key);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(kind_badge(&v.kind)).monospace().color(kind_color(&v.kind, st)));
        ui.add(egui::Label::new(RichText::new(&v.name).strong().color(theme.text)).selectable(true));
        let mut info = Vec::new();
        if let Some(n) = v.length {
            info.push(if v.kind == "string" { format!("{n} bytes") } else { format!("{n} items") });
        }
        if let Some(m) = v.memory {
            info.push(format!("{m} B in memory"));
        }
        info.push(format!("TTL {}", ttl_text(v.ttl_ms)));
        ui.label(RichText::new(info.join(" · ")).size(12.0).color(theme.text_muted));
        if b.loading_value || b.writing {
            ui.spinner();
        }
    });
    ui.add_enabled_ui(edit, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label("TTL (s)");
            ui.add(TextEdit::singleline(&mut ru.ttl).desired_width(80.0).hint_text("none"));
            if ui.button("Set").clicked() {
                let mut p = kp();
                p["seconds"] = json!(ru.ttl.trim());
                cmds.push(("db.redis.setTtl".into(), p));
            }
            if ui.button("Persist").on_hover_text("Remove the expiry").clicked() {
                cmds.push(("db.redis.setTtl".into(), kp()));
            }
            ui.separator();
            ui.add(TextEdit::singleline(&mut ru.rename).desired_width(160.0));
            if ui.button("Rename").clicked() && ru.rename != v.name {
                let mut p = kp();
                p["newName"] = json!(ru.rename);
                cmds.push(("db.redis.renameKey".into(), p));
            }
            if ui.button(RichText::new("Delete").color(theme.error)).clicked() {
                cmds.push(("db.redis.deleteKeys".into(), kp()));
            }
        });
    });
    ui.separator();
    if let Some(why) = &v.unsupported {
        ui.label(RichText::new(why).color(theme.text_muted));
        return;
    }
    match v.kind.as_str() {
        "string" | "ReJSON-RL" => text_value(ui, st, ru, v, edit, cmds, source),
        _ => table_value(ui, st, ru, v, edit, cmds, source),
    }
    if v.next.is_some() && ui.add_enabled(!b.loading_value, egui::Button::new("Load more values")).clicked() {
        cmds.push(("db.redis.moreValues".into(), json!({ "dataSource": source })));
    }
}

fn text_value(
    ui: &mut egui::Ui,
    st: &UiState,
    ru: &mut RedisUi,
    v: &ValueView,
    edit: bool,
    cmds: &mut Cmds,
    source: &str,
) {
    let theme = &st.theme;
    if v.binary {
        ui.label(RichText::new("Binary value (shown as hex, read-only here)").size(12.0).color(theme.text_muted));
    }
    if v.truncated {
        ui.label(RichText::new("Only the first 4 MB are shown; saving is disabled").size(12.0).color(theme.warning));
    }
    let can_save = edit && !v.binary && !v.truncated;
    ScrollArea::vertical().id_salt("redis-text").max_height(ui.available_height() - 40.0).show(ui, |ui| {
        ui.add_enabled(
            can_save,
            TextEdit::multiline(&mut ru.text).code_editor().desired_width(f32::INFINITY).desired_rows(12),
        );
    });
    let changed = v.text.as_deref() != Some(ru.text.as_str());
    ui.horizontal(|ui| {
        if ui.add_enabled(can_save && changed, egui::Button::new("Save")).clicked() {
            let mut p = key_params(source, &v.key);
            let id = if v.kind == "string" { "db.redis.setString" } else { "db.redis.jsonSet" };
            p["value"] = json!(ru.text);
            cmds.push((id.into(), p));
        }
        if ui.add_enabled(changed, egui::Button::new("Revert")).clicked() {
            ru.text = v.text.clone().unwrap_or_default();
        }
        if v.kind == "ReJSON-RL"
            && ui.button("Format").clicked()
            && let Ok(j) = serde_json::from_str::<Value>(&ru.text)
        {
            ru.text = serde_json::to_string_pretty(&j).unwrap_or_else(|_| ru.text.clone());
        }
    });
}

fn table_value(
    ui: &mut egui::Ui,
    st: &UiState,
    ru: &mut RedisUi,
    v: &ValueView,
    edit: bool,
    cmds: &mut Cmds,
    source: &str,
) {
    let theme = &st.theme;
    let kp = || key_params(source, &v.key);
    let mut commit: Option<(usize, String)> = None;
    ScrollArea::both()
        .id_salt("redis-value")
        .max_height(ui.available_height() - 64.0)
        .auto_shrink([false, false])
        .show_rows(ui, ROW_H, v.rows.len(), |ui, range| {
            egui::Grid::new(("redis-value-grid", range.start)).striped(true).num_columns(v.columns.len() + 1).show(
                ui,
                |ui| {
                    if range.start == 0 {
                        for c in &v.columns {
                            ui.label(RichText::new(c).strong().size(12.0).color(theme.text_muted));
                        }
                        ui.label("");
                        ui.end_row();
                    }
                    for i in range {
                        let Some(row) = v.rows.get(i) else { continue };
                        // The column that can be edited in place: the value (hash, list) or the score.
                        let editable_col = match v.kind.as_str() {
                            "hash" | "list" | "zset" => Some(1),
                            _ => None,
                        };
                        for (c, cell) in row.cells.iter().enumerate() {
                            match &mut ru.cell {
                                Some((r, text)) if *r == i && Some(c) == editable_col => {
                                    let resp = ui.add(TextEdit::singleline(text).desired_width(220.0));
                                    if resp.lost_focus() && ui.input(|k| k.key_pressed(egui::Key::Enter)) {
                                        commit = Some((i, text.clone()));
                                    }
                                }
                                _ => {
                                    let shown: String = cell.chars().take(200).collect();
                                    let resp = ui
                                        .add(egui::Label::new(RichText::new(shown).monospace().size(12.5)).truncate());
                                    if edit && Some(c) == editable_col && resp.double_clicked() {
                                        ru.cell = Some((i, cell.clone()));
                                    }
                                }
                            }
                        }
                        ui.horizontal(|ui| {
                            if edit && ui.small_button("🗑").on_hover_text("Remove").clicked() {
                                let mut p = kp();
                                let first = row.raw.first().cloned().unwrap_or_default();
                                let id = match v.kind.as_str() {
                                    "hash" => {
                                        put_bytes(&mut p, "field", &first);
                                        "db.redis.hashDelete"
                                    }
                                    "list" => {
                                        p["index"] = json!(String::from_utf8_lossy(&first));
                                        "db.redis.listRemove"
                                    }
                                    "set" => {
                                        put_bytes(&mut p, "member", &first);
                                        "db.redis.setRemove"
                                    }
                                    "zset" => {
                                        put_bytes(&mut p, "member", &first);
                                        "db.redis.zsetRemove"
                                    }
                                    _ => {
                                        p["id"] = json!(String::from_utf8_lossy(&first));
                                        "db.redis.streamDelete"
                                    }
                                };
                                cmds.push((id.into(), p));
                            }
                        });
                        ui.end_row();
                    }
                },
            );
        });
    if let Some((i, text)) = commit {
        ru.cell = None;
        if let Some(row) = v.rows.get(i) {
            let first = row.raw.first().cloned().unwrap_or_default();
            let mut p = kp();
            let id = match v.kind.as_str() {
                "hash" => {
                    put_bytes(&mut p, "field", &first);
                    p["value"] = json!(text);
                    "db.redis.hashSet"
                }
                "list" => {
                    p["index"] = json!(String::from_utf8_lossy(&first));
                    p["value"] = json!(text);
                    "db.redis.listSet"
                }
                _ => {
                    put_bytes(&mut p, "member", &first);
                    p["score"] = json!(text);
                    "db.redis.zsetAdd"
                }
            };
            cmds.push((id.into(), p));
        }
    }
    if !edit {
        return;
    }
    // Add an element.
    ui.horizontal_wrapped(|ui| {
        let (needs_field, needs_score, label) = match v.kind.as_str() {
            "hash" => (true, false, "Set field"),
            "list" => (false, false, "Push"),
            "set" => (false, false, "Add member"),
            "zset" => (false, true, "Add member"),
            "stream" => (true, false, "Add entry"),
            _ => return,
        };
        if needs_field {
            ui.add(TextEdit::singleline(&mut ru.field).hint_text("field").desired_width(120.0));
        }
        ui.add(
            TextEdit::singleline(&mut ru.value)
                .hint_text(if needs_field { "value" } else { "member / value" })
                .desired_width(180.0),
        );
        if needs_score {
            ui.add(TextEdit::singleline(&mut ru.score).hint_text("score").desired_width(70.0));
        }
        if v.kind == "list" {
            ui.checkbox(&mut ru.head, "at head");
        }
        if ui.button(label).clicked() {
            let mut p = kp();
            let id = match v.kind.as_str() {
                "hash" => {
                    p["field"] = json!(ru.field);
                    p["value"] = json!(ru.value);
                    "db.redis.hashSet"
                }
                "list" => {
                    p["value"] = json!(ru.value);
                    p["head"] = json!(ru.head);
                    "db.redis.listPush"
                }
                "set" => {
                    p["member"] = json!(ru.value);
                    "db.redis.setAdd"
                }
                "zset" => {
                    p["member"] = json!(ru.value);
                    p["score"] = json!(ru.score);
                    "db.redis.zsetAdd"
                }
                _ => {
                    p["fields"] = json!({ ru.field.clone(): ru.value.clone() });
                    "db.redis.streamAdd"
                }
            };
            cmds.push((id.into(), p));
            ru.value.clear();
        }
    });
}

fn footer(ui: &mut egui::Ui, st: &UiState, b: &RedisBrowser) {
    let theme = &st.theme;
    ui.horizontal(|ui| match &b.message {
        Some(m) if m.starts_with("Failed") => {
            ui.label(RichText::new(m).color(theme.error));
        }
        Some(m) => {
            ui.label(RichText::new(m).color(theme.success));
        }
        None => {
            ui.label(
                RichText::new("Double-click a value, score or item to edit it; Enter saves.")
                    .size(11.5)
                    .color(theme.text_muted),
            );
        }
    });
}

fn new_key_window(ctx: &egui::Context, st: &UiState, ru: &mut RedisUi, source: &str, cmds: &mut Cmds) {
    let Some(mut nk) = ru.new_key.take() else { return };
    let mut open = true;
    let mut done = false;
    egui::Window::new("New Redis Key")
        .id(egui::Id::new("db-redis-new-key"))
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .open(&mut open)
        .show(ctx, |ui| {
            egui::Grid::new("redis-new-key").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Key");
                ui.add(TextEdit::singleline(&mut nk.key).desired_width(260.0));
                ui.end_row();
                ui.label("Type");
                egui::ComboBox::from_id_salt("redis-new-type").selected_text(&nk.kind).show_ui(ui, |ui| {
                    for t in ["string", "hash", "list", "set", "zset", "stream", "json"] {
                        ui.selectable_value(&mut nk.kind, t.to_string(), t);
                    }
                });
                ui.end_row();
                if matches!(nk.kind.as_str(), "hash" | "stream") {
                    ui.label("Field");
                    ui.add(TextEdit::singleline(&mut nk.field).desired_width(260.0));
                    ui.end_row();
                }
                if nk.kind == "zset" {
                    ui.label("Score");
                    ui.add(TextEdit::singleline(&mut nk.score).desired_width(80.0));
                    ui.end_row();
                }
                ui.label(if matches!(nk.kind.as_str(), "set" | "zset") { "Member" } else { "Value" });
                ui.add(TextEdit::multiline(&mut nk.value).desired_rows(3).desired_width(260.0));
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.add_enabled(!nk.key.is_empty(), egui::Button::new("Create")).clicked() {
                    cmds.push((
                        "db.redis.newKey".into(),
                        json!({ "dataSource": source, "key": nk.key, "type": nk.kind, "field": nk.field, "value": nk.value, "score": nk.score }),
                    ));
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            });
            ui.label(RichText::new("Fails if the key exists.").size(11.5).color(st.theme.text_muted));
        });
    if open && !done {
        ru.new_key = Some(nk);
    }
}

fn confirm_window(ctx: &egui::Context, st: &UiState, ru: &mut RedisUi, cmds: &mut Cmds) {
    let Some((id, params, preview)) = ru.confirm.take() else { return };
    let mut open = true;
    let mut decided: Option<bool> = None;
    egui::Window::new("Confirm Change on Production")
        .id(egui::Id::new("db-redis-confirm"))
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label("This data source is marked as production. Run these Redis commands?");
            ui.add_space(4.0);
            egui::Frame::new().fill(st.theme.editor_bg).inner_margin(6.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(&preview).monospace().color(st.theme.text)).wrap());
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Execute").color(st.theme.warning)).clicked() {
                    decided = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    decided = Some(false);
                }
            });
        });
    match decided {
        Some(true) => {
            let mut p = params;
            p["confirm"] = json!(true);
            cmds.push((id, p));
        }
        Some(false) => {}
        None if open => ru.confirm = Some((id, params, preview)),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_keys_go_as_hex() {
        let p = key_params("r", b"user:1");
        assert_eq!(p["key"], "user:1");
        let p = key_params("r", &[0xff, 0x00]);
        assert_eq!(p["keyHex"], "ff00");
        assert!(p.get("key").is_none());
        let op = op_from_params("db.redis.deleteKeys", &p).unwrap();
        assert_eq!(op.preview(), "UNLINK \"\\xff\\x00\"");
        assert_eq!(kind_badge("zset"), "ZSET");
    }
}
