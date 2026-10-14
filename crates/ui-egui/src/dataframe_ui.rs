//! View as DataFrame / Array (M12 track PY): a window with one tab per viewed value, the shape
//! and dtypes, a filter, paging (rows and column windows) and the paged grid
//! ([`crate::data_grid`]); clicking a header sorts in the Python process. Thin: every action is
//! a `python.dataframe.*` command; pages come from the process in chunks.

use egui::{Id, Key, RichText, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::dataframe::{COLUMN_WINDOW, FrameSource};

use crate::data_grid::{self, GridAction, GridData, GridState};
use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

const PAGE_SIZES: [usize; 5] = [50, 100, 200, 500, 1000];

/// The viewer window (shown while a viewer is open).
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    if s.jupyter.frames.views.is_empty() {
        return;
    }
    let ctx = ui.ctx().clone();
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    egui::Window::new("Data View")
        .id(Id::new("python-dataframe-window"))
        .open(&mut open)
        .default_size(vec2(860.0, 520.0))
        .resizable(true)
        .show(&ctx, |ui| {
            let frames = &s.jupyter.frames;
            ui.horizontal_wrapped(|ui| {
                for (i, v) in frames.views.iter().enumerate() {
                    let src = match v.source {
                        FrameSource::Console => "console",
                        FrameSource::Notebook { .. } => "notebook",
                    };
                    let resp = ui.add(egui::Button::selectable(i == frames.active, RichText::new(&v.expr).monospace()));
                    if resp.on_hover_text(format!("{} ({src})", v.type_name)).clicked() {
                        cmds.push(("python.dataframe.select", json!({ "index": i })));
                    }
                    if ui.small_button("×").on_hover_text("Close").clicked() {
                        cmds.push(("python.dataframe.close", json!({ "index": i })));
                    }
                }
            });
            ui.separator();
            let Some(view) = frames.active() else { return };
            ui.horizontal(|ui| {
                let dtype_tip = view
                    .columns
                    .iter()
                    .zip(&view.dtypes)
                    .map(|(c, d)| format!("{c}: {d}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.label(RichText::new(format!("{} · {}", view.type_name, view.summary())).color(theme.text_muted))
                    .on_hover_text(dtype_tip);
                if view.loading.is_some() {
                    ui.spinner();
                }
            });
            ui.horizontal(|ui| {
                let filter_id = Id::new(("python-dataframe-filter", view.id));
                let mut filter = ui.data(|d| d.get_temp::<String>(filter_id)).unwrap_or_else(|| view.filter.clone());
                let resp = ui.add(TextEdit::singleline(&mut filter).hint_text("Filter rows (Enter)").desired_width(220.0));
                if resp.changed() {
                    ui.data_mut(|d| d.insert_temp(filter_id, filter.clone()));
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && filter != view.filter {
                    cmds.push(("python.dataframe.filter", json!({ "text": filter })));
                }
                if !view.filter.is_empty() && ui.small_button("Clear filter").clicked() {
                    ui.data_mut(|d| d.insert_temp(filter_id, String::new()));
                    cmds.push(("python.dataframe.filter", json!({ "text": "" })));
                }
                ui.separator();
                let page = view.page();
                let pages = view.page_count();
                if ui.add_enabled(page > 0, egui::Button::new("◀")).on_hover_text("Previous page").clicked() {
                    cmds.push(("python.dataframe.previousPage", Value::Null));
                }
                ui.label(format!("page {} / {pages}", page + 1));
                if ui.add_enabled(page + 1 < pages, egui::Button::new("▶")).on_hover_text("Next page").clicked() {
                    cmds.push(("python.dataframe.nextPage", Value::Null));
                }
                let mut size = view.page_size;
                egui::ComboBox::from_id_salt(("python-dataframe-size", view.id))
                    .selected_text(format!("{size} rows"))
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        for p in PAGE_SIZES {
                            ui.selectable_value(&mut size, p, format!("{p} rows"));
                        }
                    });
                if size != view.page_size {
                    cmds.push(("python.dataframe.page", json!({ "pageSize": size, "page": 0 })));
                }
                if view.cols_total > COLUMN_WINDOW {
                    ui.separator();
                    if ui
                        .add_enabled(view.col_start > 0, egui::Button::new("◀ cols"))
                        .clicked()
                    {
                        cmds.push(("python.dataframe.page", json!({ "colStart": view.col_start.saturating_sub(COLUMN_WINDOW) })));
                    }
                    if ui
                        .add_enabled(view.col_start + COLUMN_WINDOW < view.cols_total, egui::Button::new("cols ▶"))
                        .clicked()
                    {
                        cmds.push(("python.dataframe.page", json!({ "colStart": view.col_start + COLUMN_WINDOW })));
                    }
                }
                if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                    cmds.push(("python.dataframe.refresh", Value::Null));
                }
            });
            if let Some(e) = &view.error {
                ui.label(RichText::new(e).color(theme.error));
            }
            let (columns, rows) = view.grid();
            let sort = view
                .sort
                .filter(|(c, _)| *c >= view.col_start && *c < view.col_start + view.columns.len())
                .map(|(c, asc)| (c - view.col_start + 1, !asc));
            let status = view.summary();
            let data = GridData {
                columns: &columns,
                rows: &rows,
                sort,
                status: &status,
                status_is_error: view.error.is_some(),
                edits: None,
                editable: false,
            };
            let grid_id = Id::new(("python-dataframe-grid", view.id));
            let mut state = ui.data_mut(|d| d.get_temp::<GridState>(grid_id)).unwrap_or_default();
            let actions = data_grid::show(ui, grid_id, &mut state, &data, &theme);
            ui.data_mut(|d| d.insert_temp(grid_id, state));
            for a in actions {
                match a {
                    GridAction::Sort(col) if col > 0 => {
                        let column = view.col_start + col - 1;
                        let descending = view.sort.is_some_and(|(c, asc)| c == column && asc);
                        cmds.push(("python.dataframe.sort", json!({ "column": column, "descending": descending })));
                    }
                    GridAction::Copy(text) => ui.ctx().copy_text(text),
                    _ => {}
                }
            }
        });
    if !open {
        for _ in 0..s.jupyter.frames.views.len() {
            cmds.push(("python.dataframe.close", json!({ "index": 0 })));
        }
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}
