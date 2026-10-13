//! The Profiler tool window: flame graph, call tree and method list (M11 track P), plus the
//! Attach Profiler to Process list and the Open Profiler Snapshot file dialog.
//!
//! Thin: the layout (call tree, flame graph rectangles, method order) is computed by
//! `codecraft-profile` in the engine; this draws only the visible rows and rectangles and turns
//! clicks into `profiler.*` commands.

use egui::{
    Align, Align2, Color32, FontId, Key, Layout, Pos2, Rect, RichText, ScrollArea, Sense, TextEdit, pos2, vec2,
};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::profiler::profile::{CallNode, Frame};
use codecraft_engine::profiler::{MethodColumn, ProfilerTab, Snapshot};

use crate::theme::Theme;
use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

const ROW_H: f32 = 20.0;
const FLAME_ROW_H: f32 = 18.0;

fn run_all(s: &mut Session, st: &mut UiState, cmds: Cmds) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

/// The Profiler tool window.
pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    // Loads the recent list once (a small file).
    let _ = s.profiler.recent();
    recordings_bar(ui, s, &theme, &mut cmds);
    if let Some(tool) = s.profiler.missing {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{} is not installed.", tool.program())).color(theme.warning));
            ui.label(RichText::new(tool.install_hint()).color(theme.text_muted));
            if tool.install_command().is_some() && ui.small_button("Install in Terminal").clicked() {
                cmds.push(("profiler.install", json!({ "tool": tool.program() })));
            }
        });
    }
    if let Some(e) = &s.profiler.error {
        ui.label(RichText::new(e).color(theme.error));
    }
    if let Some(p) = &s.profiler.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(format!("Loading {}…", p.display())).color(theme.text_muted));
        });
    }
    match &s.profiler.snapshot {
        None => empty_state(ui, s, &theme, &mut cmds),
        Some(snap) => {
            toolbar(ui, snap, s.profiler.tab, &theme, &mut cmds);
            ui.separator();
            match s.profiler.tab {
                ProfilerTab::FlameGraph => flame_graph(ui, snap, &theme, &mut cmds),
                ProfilerTab::CallTree => call_tree(ui, snap, &theme, &mut cmds),
                ProfilerTab::MethodList => method_list(ui, snap, &theme, &mut cmds),
            }
        }
    }
    run_all(s, st, cmds);
}

/// Floating parts: the process list and the snapshot file dialog (shown even when the tool
/// window is hidden).
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    if s.profiler.pick_snapshot {
        s.profiler.pick_snapshot = false;
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Open Profiler Snapshot")
            .add_filter("Profiles", &["json", "gz", "txt", "folded", "collapsed", "perf", "out", "speedscope"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            run_command(s, st, "run.openProfilerSnapshot", &json!({ "path": path }));
        }
    }
    attach_window(ui, s, st);
}

fn recordings_bar(ui: &mut egui::Ui, s: &Session, theme: &Theme, cmds: &mut Cmds) {
    for r in &s.profiler.recordings {
        ui.horizontal(|ui| {
            ui.label(RichText::new("●").color(theme.error));
            let secs = r.started.elapsed().as_secs();
            let state = if r.stop_requested.is_some() { "saving the snapshot…" } else { "recording" };
            ui.label(format!("{}: {} with {} ({}:{:02})", r.title, state, r.tool.program(), secs / 60, secs % 60));
            let label = if r.stop_requested.is_some() { "Kill" } else { "■ Stop" };
            if ui.small_button(label).on_hover_text("Stop recording and open the snapshot").clicked() {
                cmds.push(("profiler.stop", json!({ "tab": r.tab.0 })));
            }
        });
    }
}

fn empty_state(ui: &mut egui::Ui, s: &Session, theme: &Theme, cmds: &mut Cmds) {
    ui.add_space(8.0);
    ui.label(RichText::new("No profiler snapshot is open.").color(theme.text_muted));
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        let can_profile = s.enabled("run.profile").is_ok();
        if ui.add_enabled(can_profile, egui::Button::new("Profile 'Run'")).clicked() {
            cmds.push(("run.profile", Value::Null));
        }
        if ui.button("Attach Profiler to Process…").clicked() {
            cmds.push(("run.attachProfilerToProcess", Value::Null));
        }
        if ui.button("Open Snapshot…").clicked() {
            cmds.push(("run.openProfilerSnapshot", Value::Null));
        }
    });
    let recent = s.profiler.recent_cached();
    if !recent.is_empty() {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Recent snapshots").strong());
            if ui.small_button("Clear").clicked() {
                cmds.push(("profiler.clearRecent", Value::Null));
            }
        });
        for p in recent {
            let name = p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
            if ui.link(name).on_hover_text(p.display().to_string()).clicked() {
                cmds.push(("run.openProfilerSnapshot", json!({ "path": p })));
            }
        }
    }
}

fn toolbar(ui: &mut egui::Ui, snap: &Snapshot, tab: ProfilerTab, theme: &Theme, cmds: &mut Cmds) {
    let p = &snap.profile;
    ui.horizontal(|ui| {
        for (t, label, id) in [
            (ProfilerTab::FlameGraph, "Flame Graph", "flameGraph"),
            (ProfilerTab::CallTree, "Call Tree", "callTree"),
            (ProfilerTab::MethodList, "Method List", "methodList"),
        ] {
            if ui.selectable_label(tab == t, label).clicked() && tab != t {
                cmds.push(("profiler.selectTab", json!({ "tab": id })));
            }
        }
        ui.separator();
        let current = match snap.analysis.thread.and_then(|t| p.threads.get(t)) {
            Some(t) => t.name.clone(),
            None => "All threads".to_string(),
        };
        egui::ComboBox::from_id_salt("profiler-thread").selected_text(current).width(180.0).show_ui(ui, |ui| {
            if ui.selectable_label(snap.analysis.thread.is_none(), "All threads").clicked() {
                cmds.push(("profiler.selectThread", Value::Null));
            }
            for (i, t) in p.threads.iter().enumerate() {
                let text = format!("{}  ·  {}", t.name, p.unit.format(t.total()));
                if ui.selectable_label(snap.analysis.thread == Some(i), text).clicked() {
                    cmds.push(("profiler.selectThread", json!({ "thread": i })));
                }
            }
        });
        let mut query = snap.search.clone();
        let field = ui.add(TextEdit::singleline(&mut query).hint_text("Search").desired_width(160.0));
        if field.changed() {
            cmds.push(("profiler.search", json!({ "query": query })));
        }
        if !snap.search.trim().is_empty() {
            ui.label(
                RichText::new(format!("{:.1}% matched", snap.analysis.tree.percent(snap.matched)))
                    .color(theme.text_muted)
                    .small(),
            );
        }
        if tab == ProfilerTab::FlameGraph && snap.zoom.is_some() && ui.small_button("Reset Zoom").clicked() {
            cmds.push(("profiler.resetZoom", Value::Null));
        }
        if tab == ProfilerTab::CallTree {
            if ui.small_button("Expand All").clicked() {
                cmds.push(("profiler.expandAll", Value::Null));
            }
            if ui.small_button("Collapse All").clicked() {
                cmds.push(("profiler.collapseAll", Value::Null));
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("✕").on_hover_text("Close the snapshot").clicked() {
                cmds.push(("profiler.close", Value::Null));
            }
            if ui.small_button("Open…").on_hover_text("Open Profiler Snapshot").clicked() {
                cmds.push(("run.openProfilerSnapshot", Value::Null));
            }
            ui.label(
                RichText::new(format!(
                    "{} · {} · {}",
                    snap.title(),
                    p.format.label(),
                    p.unit.format(snap.analysis.tree.total)
                ))
                .color(theme.text_muted)
                .small(),
            )
            .on_hover_text(snap.path.display().to_string());
        });
    });
}

// ---------------------------------------------------------------- shared helpers

fn frame_of<'a>(snap: &'a Snapshot, node: &CallNode) -> Option<&'a Frame> {
    snap.profile.frame(node.frame)
}

/// The hover text for a call node: name, module, location, total and self.
fn node_tooltip(ui: &mut egui::Ui, snap: &Snapshot, node: &CallNode, theme: &Theme) {
    let unit = snap.profile.unit;
    let tree = &snap.analysis.tree;
    if let Some(f) = frame_of(snap, node) {
        ui.label(RichText::new(&f.name).strong());
        if let Some(m) = &f.module {
            ui.label(RichText::new(m).color(theme.text_muted));
        }
        if let Some(loc) = f.location() {
            ui.label(RichText::new(loc).color(theme.link));
        }
    }
    ui.label(format!("Total: {} ({:.1}%)", unit.format(node.total), tree.percent(node.total)));
    ui.label(format!("Self: {} ({:.1}%)", unit.format(node.self_weight), tree.percent(node.self_weight)));
}

/// A warm flame colour for a name, mixed from theme tokens (stable per name).
fn flame_color(theme: &Theme, name: &str) -> Color32 {
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(16777619);
    }
    let t = (h % 1000) as f32 / 1000.0;
    let base = theme.warning.lerp_to_gamma(theme.error, t * 0.8);
    base.lerp_to_gamma(theme.panel_bg, ((h >> 10) % 100) as f32 / 400.0)
}

fn text_on_flame(theme: &Theme) -> Color32 {
    if theme.dark { theme.editor_bg } else { theme.text }
}

// ---------------------------------------------------------------- flame graph

fn flame_graph(ui: &mut egui::Ui, snap: &Snapshot, theme: &Theme, cmds: &mut Cmds) {
    let tree = &snap.analysis.tree;
    let flame = &snap.analysis.flame;
    if flame.total == 0 {
        ui.label(RichText::new("No samples in this selection.").color(theme.text_muted));
        return;
    }
    let (z_start, z_width, z_depth) = match snap.zoom.and_then(|z| Some((flame.span(tree, z)?, tree.node(z)?.depth))) {
        Some(((start, width), depth)) if width > 0 => (start, width, depth as usize),
        _ => (0, flame.total, 0),
    };
    let zoom_path: Vec<u32> = snap.zoom.map(|z| tree.path(z)).unwrap_or_default();
    let searching = !snap.hits.is_empty();
    let font = FontId::proportional(11.5);
    let depth = flame.depth();
    ScrollArea::vertical().id_salt("profiler-flame").auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
        let width = ui.available_width().max(50.0);
        let height = (depth as f32 * FLAME_ROW_H).max(ui.available_height());
        let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click());
        let painter = ui.painter_at(rect);
        let scale = width as f64 / z_width as f64;
        let x_of = |w: u64| rect.left() + ((w.saturating_sub(z_start)) as f64 * scale) as f32;
        let first_row = (viewport.min.y / FLAME_ROW_H).floor().max(0.0) as usize;
        let last_row = ((viewport.max.y / FLAME_ROW_H).ceil().max(0.0) as usize).min(depth);
        let z_end = z_start.saturating_add(z_width);
        for d in first_row..last_row {
            let y = rect.top() + d as f32 * FLAME_ROW_H;
            let rects = flame.visible(d, z_start, z_end);
            let mut i = 0;
            while let Some(r) = rects.get(i) {
                i += 1;
                let Some(node) = tree.node(r.node) else { continue };
                let (x0, x1) = if d < z_depth {
                    (rect.left(), rect.right())
                } else {
                    (x_of(r.start.max(z_start)), x_of(r.start.saturating_add(r.width).min(z_end)))
                };
                if x1 - x0 < 0.6 {
                    // Skip the rest of this pixel's sub-pixel rectangles in one search, so a row
                    // costs at most about one step per pixel however many frames it holds.
                    let next = z_start.saturating_add((f64::from(x0 + 1.0 - rect.left()) / scale).max(0.0) as u64);
                    i += rects.get(i..).map_or(0, |rest| rest.partition_point(|q| q.start < next));
                    continue;
                }
                let cell = Rect::from_min_max(pos2(x0, y), pos2(x1 - 0.5, y + FLAME_ROW_H - 1.0));
                let name = frame_of(snap, node).map_or("", |f| f.name.as_str());
                let mut color = flame_color(theme, name);
                if d < z_depth && zoom_path.contains(&r.node) {
                    color = theme.text_disabled;
                } else if searching {
                    color = if snap.is_hit(node.frame) { theme.accent } else { color.gamma_multiply(0.35) };
                }
                painter.rect_filled(cell, 1.0, color);
                if snap.selected == Some(r.node) {
                    painter.rect_stroke(cell, 1.0, egui::Stroke::new(1.5, theme.text), egui::StrokeKind::Inside);
                }
                if cell.width() > 28.0 {
                    let label = fit_text(name, cell.width() - 6.0, 6.2);
                    painter.with_clip_rect(cell).text(
                        pos2(cell.left() + 3.0, cell.center().y),
                        Align2::LEFT_CENTER,
                        label,
                        font.clone(),
                        text_on_flame(theme),
                    );
                }
            }
        }
        // Hover and clicks: find the rectangle under the pointer.
        let hovered = resp.hover_pos().and_then(|p| node_at(snap, rect, p, z_start, z_width, z_depth));
        if let Some(id) = hovered
            && let Some(node) = tree.node(id)
        {
            let node = node.clone();
            let resp = resp.clone().on_hover_ui_at_pointer(|ui| node_tooltip(ui, snap, &node, theme));
            if resp.double_clicked() {
                cmds.push(("profiler.select", json!({ "node": id })));
                cmds.push(("profiler.jumpToSource", json!({ "node": id })));
            } else if resp.clicked() {
                cmds.push(("profiler.zoom", json!({ "node": id })));
            }
        }
        if let Some(id) = hovered {
            resp.context_menu(|ui| {
                if ui.button("Zoom In").clicked() {
                    cmds.push(("profiler.zoom", json!({ "node": id })));
                    ui.close();
                }
                if ui.button("Jump to Source").clicked() {
                    cmds.push(("profiler.jumpToSource", json!({ "node": id })));
                    ui.close();
                }
                if ui.button("Show in Call Tree").clicked() {
                    cmds.push(("profiler.select", json!({ "node": id })));
                    cmds.push(("profiler.selectTab", json!({ "tab": "callTree" })));
                    ui.close();
                }
                if snap.zoom.is_some() && ui.button("Reset Zoom").clicked() {
                    cmds.push(("profiler.resetZoom", Value::Null));
                    ui.close();
                }
            });
        }
    });
    if ui.input(|i| i.key_pressed(Key::Escape)) && snap.zoom.is_some() {
        cmds.push(("profiler.resetZoom", Value::Null));
    }
}

/// The call node drawn at a point of the flame graph.
fn node_at(snap: &Snapshot, rect: Rect, p: Pos2, z_start: u64, z_width: u64, z_depth: usize) -> Option<u32> {
    let flame = &snap.analysis.flame;
    let d = ((p.y - rect.top()) / FLAME_ROW_H).floor();
    if d < 0.0 {
        return None;
    }
    let d = d as usize;
    let row = flame.rows.get(d)?;
    if d < z_depth {
        // Ancestors of the zoomed node span the whole width.
        return flame
            .visible(d, z_start, z_start.saturating_add(1))
            .first()
            .map(|r| r.node)
            .or(row.first().map(|r| r.node));
    }
    let frac = f64::from((p.x - rect.left()) / rect.width().max(1.0));
    let w = z_start.saturating_add((frac.clamp(0.0, 1.0) * z_width as f64) as u64);
    flame.visible(d, w, w.saturating_add(1)).first().map(|r| r.node)
}

/// `text` cut with an ellipsis to fit `width` pixels (an estimate: `char_w` per character).
fn fit_text(text: &str, width: f32, char_w: f32) -> String {
    let max = (width / char_w).floor().max(0.0) as usize;
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 1 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------- call tree

fn call_tree(ui: &mut egui::Ui, snap: &Snapshot, theme: &Theme, cmds: &mut Cmds) {
    let tree = &snap.analysis.tree;
    let unit = snap.profile.unit;
    // Visible rows: the expanded part of the tree, depth first.
    let mut rows: Vec<u32> = Vec::new();
    let mut work: Vec<u32> = tree.roots.iter().rev().copied().collect();
    while let Some(id) = work.pop() {
        rows.push(id);
        if snap.expanded.contains(&id)
            && let Some(n) = tree.node(id)
        {
            work.extend(n.children.iter().rev().copied());
        }
    }
    header(ui, theme, &["Name", "Total %", "Total", "Self"]);
    let col_w = 90.0;
    ScrollArea::both().id_salt("profiler-tree").auto_shrink([false, false]).show_rows(
        ui,
        ROW_H,
        rows.len(),
        |ui, range| {
            for id in rows.get(range).unwrap_or(&[]) {
                let Some(node) = tree.node(*id) else { continue };
                let width = ui.available_width().max(500.0);
                let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
                let painter = ui.painter_at(rect);
                if snap.selected == Some(*id) {
                    painter.rect_filled(rect, 0.0, theme.list_selection);
                } else if resp.hovered() {
                    painter.rect_filled(rect, 0.0, theme.hover);
                }
                let indent = rect.left() + 4.0 + node.depth as f32 * 14.0;
                let has_kids = !node.children.is_empty();
                let toggle = Rect::from_min_size(pos2(indent, rect.top()), vec2(14.0, ROW_H));
                if has_kids {
                    let glyph = if snap.expanded.contains(id) { "▾" } else { "▸" };
                    painter.text(
                        toggle.center(),
                        Align2::CENTER_CENTER,
                        glyph,
                        FontId::proportional(12.0),
                        theme.text_muted,
                    );
                }
                let f = frame_of(snap, node);
                let name = f.map_or(String::new(), |f| f.name.clone());
                let hit = snap.is_hit(node.frame);
                let name_color = if hit { theme.accent } else { theme.text };
                let name_right = rect.right() - 3.0 * col_w - 8.0;
                let name_rect = Rect::from_min_max(
                    pos2(indent + 16.0, rect.top()),
                    pos2(name_right.max(indent + 16.0), rect.bottom()),
                );
                let painter_name = painter.with_clip_rect(name_rect);
                let galley_end = painter_name
                    .text(
                        pos2(name_rect.left(), rect.center().y),
                        Align2::LEFT_CENTER,
                        &name,
                        FontId::proportional(13.0),
                        name_color,
                    )
                    .right();
                if let Some(m) = f.and_then(|f| f.module.as_ref()) {
                    painter_name.text(
                        pos2(galley_end + 6.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        m,
                        FontId::proportional(11.5),
                        theme.text_muted,
                    );
                }
                let pct = tree.percent(node.total);
                let cols = [format!("{pct:.1}%"), unit.format(node.total), unit.format(node.self_weight)];
                for (i, text) in cols.iter().enumerate() {
                    let right = rect.right() - (2 - i) as f32 * col_w - 6.0;
                    if i == 0 {
                        // A bar behind the percentage.
                        let bar_w = (col_w - 10.0) * (pct as f32 / 100.0).clamp(0.0, 1.0);
                        let bar =
                            Rect::from_min_size(pos2(right - col_w + 10.0, rect.top() + 4.0), vec2(bar_w, ROW_H - 8.0));
                        painter.rect_filled(bar, 2.0, theme.selection);
                    }
                    painter.text(
                        pos2(right, rect.center().y),
                        Align2::RIGHT_CENTER,
                        text,
                        FontId::monospace(12.0),
                        theme.text_muted,
                    );
                }
                let toggled = has_kids
                    && resp.clicked()
                    && resp.interact_pointer_pos().is_some_and(|p| toggle.expand2(vec2(2.0, 0.0)).contains(p));
                if toggled {
                    cmds.push(("profiler.toggleNode", json!({ "node": id })));
                } else if resp.double_clicked() {
                    cmds.push(("profiler.jumpToSource", json!({ "node": id })));
                } else if resp.clicked() {
                    cmds.push(("profiler.select", json!({ "node": id })));
                }
                let node = node.clone();
                let resp = resp.on_hover_ui(|ui| node_tooltip(ui, snap, &node, theme));
                resp.context_menu(|ui| {
                    if ui.button("Jump to Source").clicked() {
                        cmds.push(("profiler.jumpToSource", json!({ "node": id })));
                        ui.close();
                    }
                    if ui.button("Focus in Flame Graph").clicked() {
                        cmds.push(("profiler.zoom", json!({ "node": id })));
                        cmds.push(("profiler.selectTab", json!({ "tab": "flameGraph" })));
                        ui.close();
                    }
                    if has_kids && ui.button(if snap.expanded.contains(id) { "Collapse" } else { "Expand" }).clicked() {
                        cmds.push(("profiler.toggleNode", json!({ "node": id })));
                        ui.close();
                    }
                });
            }
        },
    );
    tree_keys(ui, snap, &rows, cmds);
}

/// ↑/↓ move the selection, →/← expand/collapse, Enter jumps to source (when no text field has focus).
fn tree_keys(ui: &egui::Ui, snap: &Snapshot, rows: &[u32], cmds: &mut Cmds) {
    if ui.ctx().memory(|m| m.focused().is_some()) || !ui.ui_contains_pointer() {
        return;
    }
    let (up, down, left, right, enter) = ui.input(|i| {
        (
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
            i.key_pressed(Key::ArrowLeft),
            i.key_pressed(Key::ArrowRight),
            i.key_pressed(Key::Enter),
        )
    });
    let Some(sel) = snap.selected else {
        if (up || down) && !rows.is_empty() {
            cmds.push(("profiler.select", json!({ "node": rows.first().copied().unwrap_or(0) })));
        }
        return;
    };
    let pos = rows.iter().position(|r| *r == sel);
    if up || down {
        let next = match pos {
            Some(i) if up => i.checked_sub(1).and_then(|j| rows.get(j)),
            Some(i) => rows.get(i + 1),
            None => rows.first(),
        };
        if let Some(n) = next {
            cmds.push(("profiler.select", json!({ "node": n })));
        }
    }
    if right && !snap.expanded.contains(&sel) {
        cmds.push(("profiler.toggleNode", json!({ "node": sel, "expanded": true })));
    }
    if left {
        if snap.expanded.contains(&sel) {
            cmds.push(("profiler.toggleNode", json!({ "node": sel, "expanded": false })));
        } else if let Some(parent) = snap.analysis.tree.node(sel).and_then(|n| n.parent) {
            cmds.push(("profiler.select", json!({ "node": parent })));
        }
    }
    if enter {
        cmds.push(("profiler.jumpToSource", json!({ "node": sel })));
    }
}

fn header(ui: &mut egui::Ui, theme: &Theme, cols: &[&str]) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(cols.first().copied().unwrap_or("")).strong().small());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for c in cols.iter().skip(1).rev() {
                ui.add_sized([84.0, 16.0], egui::Label::new(RichText::new(*c).strong().small().color(theme.text)));
            }
        });
    });
}

// ---------------------------------------------------------------- method list

fn method_list(ui: &mut egui::Ui, snap: &Snapshot, theme: &Theme, cmds: &mut Cmds) {
    let unit = snap.profile.unit;
    let tree = &snap.analysis.tree;
    if snap.method.is_some() {
        egui::Panel::bottom("profiler-method-edges").resizable(true).default_size(160.0).show(ui, |ui| {
            method_edges(ui, snap, theme, cmds);
        });
    }
    // Sortable header.
    let col_w = 96.0;
    ui.horizontal(|ui| {
        let arrow = |c: MethodColumn| {
            if snap.sort == c { if snap.descending { " ▼" } else { " ▲" } } else { "" }
        };
        if ui
            .add(
                egui::Button::new(RichText::new(format!("Method{}", arrow(MethodColumn::Name))).strong().small())
                    .frame(false),
            )
            .clicked()
        {
            cmds.push(("profiler.sortMethods", json!({ "column": "name" })));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            for (c, label, id) in [(MethodColumn::Total, "Total", "total"), (MethodColumn::SelfWeight, "Self", "self")]
            {
                let text = RichText::new(format!("{label}{}", arrow(c))).strong().small();
                if ui.add_sized([col_w * 1.6, 16.0], egui::Button::new(text).frame(false)).clicked() {
                    cmds.push(("profiler.sortMethods", json!({ "column": id })));
                }
            }
        });
    });
    let order = &snap.method_order;
    ScrollArea::vertical().id_salt("profiler-methods").auto_shrink([false, false]).show_rows(
        ui,
        ROW_H,
        order.len(),
        |ui, range| {
            for i in order.get(range).unwrap_or(&[]) {
                let Some(m) = snap.analysis.methods.get(*i) else { continue };
                let width = ui.available_width().max(400.0);
                let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_H), Sense::click());
                let painter = ui.painter_at(rect);
                if snap.method == Some(m.frame) {
                    painter.rect_filled(rect, 0.0, theme.list_selection);
                } else if resp.hovered() {
                    painter.rect_filled(rect, 0.0, theme.hover);
                }
                let f = snap.profile.frame(m.frame);
                let label = f.map_or(String::new(), Frame::label);
                let color = if snap.is_hit(m.frame) { theme.accent } else { theme.text };
                let name_rect = Rect::from_min_max(
                    rect.min + vec2(6.0, 0.0),
                    pos2(rect.right() - 2.0 * col_w * 1.6, rect.bottom()),
                );
                painter.with_clip_rect(name_rect).text(
                    pos2(name_rect.left(), rect.center().y),
                    Align2::LEFT_CENTER,
                    label,
                    FontId::proportional(13.0),
                    color,
                );
                let cols = [
                    format!("{} ({:.1}%)", unit.format(m.self_weight), tree.percent(m.self_weight)),
                    format!("{} ({:.1}%)", unit.format(m.total), tree.percent(m.total)),
                ];
                for (k, text) in cols.iter().enumerate() {
                    let right = rect.right() - (1 - k) as f32 * col_w * 1.6 - 6.0;
                    painter.text(
                        pos2(right, rect.center().y),
                        Align2::RIGHT_CENTER,
                        text,
                        FontId::monospace(12.0),
                        theme.text_muted,
                    );
                }
                if resp.double_clicked() {
                    cmds.push(("profiler.jumpToSource", json!({ "frame": m.frame })));
                } else if resp.clicked() {
                    cmds.push(("profiler.selectMethod", json!({ "frame": m.frame })));
                }
                let resp = match f.and_then(Frame::location) {
                    Some(loc) => resp.on_hover_text(loc),
                    None => resp,
                };
                resp.context_menu(|ui| {
                    if ui.button("Jump to Source").clicked() {
                        cmds.push(("profiler.jumpToSource", json!({ "frame": m.frame })));
                        ui.close();
                    }
                    if ui.button("Search in Flame Graph").clicked() {
                        let name = f.map_or(String::new(), |f| f.name.clone());
                        cmds.push(("profiler.search", json!({ "query": name })));
                        cmds.push(("profiler.selectTab", json!({ "tab": "flameGraph" })));
                        ui.close();
                    }
                });
            }
        },
    );
}

/// Callers and callees of the selected method.
fn method_edges(ui: &mut egui::Ui, snap: &Snapshot, theme: &Theme, cmds: &mut Cmds) {
    let unit = snap.profile.unit;
    let tree = &snap.analysis.tree;
    let (callers, callees) = &snap.method_edges;
    ui.columns(2, |cols| {
        let [left, right] = cols else { return };
        for (ui, title, edges) in [(left, "Callers", callers), (right, "Callees", callees)] {
            ui.label(RichText::new(title).strong().small());
            ScrollArea::vertical().id_salt(title).auto_shrink([false, false]).show(ui, |ui| {
                if edges.is_empty() {
                    ui.label(RichText::new("None").color(theme.text_muted).small());
                }
                for e in edges.iter().take(200) {
                    let name = match e.frame.and_then(|f| snap.profile.frame(f)) {
                        Some(f) => f.label(),
                        None => "[root]".to_string(),
                    };
                    let text = format!("{name}  ·  {} ({:.1}%)", unit.format(e.weight), tree.percent(e.weight));
                    let resp = ui.selectable_label(false, RichText::new(text).small());
                    if let Some(f) = e.frame {
                        if resp.double_clicked() {
                            cmds.push(("profiler.jumpToSource", json!({ "frame": f })));
                        } else if resp.clicked() {
                            cmds.push(("profiler.selectMethod", json!({ "frame": f })));
                        }
                    }
                }
            });
        }
    });
}

// ---------------------------------------------------------------- attach

/// Run › Attach Profiler to Process…: the process list (filter, ↑/↓, Enter to record, Esc to close).
fn attach_window(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let Some(picker) = s.profiler.attach.clone() else { return };
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let visible = picker.visible();
    egui::Window::new("Attach Profiler to Process")
        .id(egui::Id::new("attach-profiler-to-process"))
        .default_size([640.0, 420.0])
        .open(&mut open)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ui.ctx(), |ui| {
            let mut query = picker.query.clone();
            ui.horizontal(|ui| {
                let field = ui.add(
                    TextEdit::singleline(&mut query).hint_text("Filter by name, command or pid").desired_width(360.0),
                );
                if !field.has_focus() && !ui.ctx().memory(|m| m.focused().is_some()) {
                    field.request_focus();
                }
                let mut all = picker.all_users;
                if ui.checkbox(&mut all, "All users").changed() {
                    cmds.push(("profiler.attach.refresh", json!({ "allUsers": all })));
                }
                if ui.button("Refresh").clicked() {
                    cmds.push(("profiler.attach.refresh", json!({ "allUsers": picker.all_users })));
                }
            });
            if query != picker.query {
                cmds.push(("profiler.attach.filter", json!({ "query": query })));
            }
            if picker.loading {
                ui.label(RichText::new("Listing processes…").color(theme.text_muted));
            }
            if let Some(e) = &picker.error {
                ui.label(RichText::new(e).color(theme.error));
            }
            if !picker.loading && visible.is_empty() {
                ui.label(RichText::new("No matching processes.").color(theme.text_muted));
            }
            ScrollArea::vertical().auto_shrink([false, false]).max_height(320.0).show(ui, |ui| {
                for (row, index) in visible.iter().enumerate().take(500) {
                    let Some(p) = picker.processes.get(*index) else { continue };
                    let kind = match p.kind {
                        codecraft_engine::debug::ProcessKind::Python => "Python (py-spy)",
                        _ => "Native (samply)",
                    };
                    let text = format!("{:>7}  {}  ·  {kind}", p.pid, p.name);
                    let resp = ui.selectable_label(row == picker.selected, text).on_hover_text(&p.command);
                    if resp.double_clicked() {
                        cmds.push(("profiler.attach.confirm", json!({ "index": row })));
                    } else if resp.clicked() {
                        let delta = i64::try_from(row).unwrap_or(0) - i64::try_from(picker.selected).unwrap_or(0);
                        cmds.push(("profiler.attach.move", json!({ "delta": delta })));
                    }
                }
            });
            ui.separator();
            ui.label(
                RichText::new("On macOS, attaching needs `samply setup` once; py-spy may need root.")
                    .color(theme.text_muted)
                    .small(),
            );
            ui.horizontal(|ui| {
                let can = picker.selected_process().is_some();
                if ui.add_enabled(can, egui::Button::new("Start Profiling")).clicked() {
                    cmds.push(("profiler.attach.confirm", Value::Null));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("profiler.attach.cancel", Value::Null));
                }
            });
        });
    let (up, down, enter, escape) = ui.ctx().input(|i| {
        (
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
            i.key_pressed(Key::Enter),
            i.key_pressed(Key::Escape),
        )
    });
    if up || down {
        cmds.push(("profiler.attach.move", json!({ "delta": if up { -1 } else { 1 } })));
    }
    if enter {
        cmds.push(("profiler.attach.confirm", Value::Null));
    }
    if escape || !open {
        cmds.push(("profiler.attach.cancel", Value::Null));
    }
    run_all(s, st, cmds);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_fitting() {
        assert_eq!(fit_text("short", 100.0, 6.0), "short");
        assert_eq!(fit_text("a_long_function_name", 60.0, 6.0), "a_long_fu…");
        assert_eq!(fit_text("abc", 3.0, 6.0), "");
        assert_eq!(fit_text("", 0.0, 6.0), "");
    }

    #[test]
    fn flame_colours_are_stable_and_from_the_theme() {
        let theme = Theme::dark();
        assert_eq!(flame_color(&theme, "main"), flame_color(&theme, "main"));
        let _ = flame_color(&theme, "");
    }

    /// The tool window draws every view of a snapshot (and the empty state) without panicking.
    #[test]
    fn profiler_window_renders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.txt");
        let mut text = String::new();
        for i in 0..500 {
            text.push_str(&format!("main;f{};g{} {}\n", i % 17, i % 5, i + 1));
        }
        std::fs::write(&path, text).unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.profiler.dir_override = Some(dir.path().join("profiler"));
        let mut st = UiState::new();
        let ctx = egui::Context::default();
        let frame = |s: &mut Session, st: &mut UiState| {
            for _ in 0..2 {
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                    show(ui, s, st);
                    windows(ui, s, st);
                });
                out.textures_delta.clear();
            }
        };
        frame(&mut s, &mut st);
        s.execute("run.openProfilerSnapshot", &json!({ "path": path })).unwrap();
        let start = std::time::Instant::now();
        while s.profiler.snapshot.is_none() {
            assert!(start.elapsed().as_secs() < 20);
            s.pump();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for tab in ["flameGraph", "callTree", "methodList"] {
            s.execute("profiler.selectTab", &json!({ "tab": tab })).unwrap();
            frame(&mut s, &mut st);
        }
        s.execute("profiler.zoom", &json!({ "node": 1 })).unwrap();
        s.execute("profiler.search", &json!({ "query": "g1" })).unwrap();
        s.execute("profiler.selectMethod", &json!({ "frame": 0 })).unwrap();
        s.execute("profiler.expandAll", &Value::Null).unwrap();
        for tab in ["flameGraph", "callTree", "methodList"] {
            s.execute("profiler.selectTab", &json!({ "tab": tab })).unwrap();
            frame(&mut s, &mut st);
        }
        s.execute("run.attachProfilerToProcess", &Value::Null).unwrap();
        frame(&mut s, &mut st);
        s.execute("profiler.close", &Value::Null).unwrap();
        frame(&mut s, &mut st);
        s.shutdown();
    }
}
