//! HTTP Client responses in the Services tool window, and the HTTP Client dialogs (Convert
//! cURL, the Postman collection picker, Save Response) (M11 track H). Thin: every action is an
//! engine command.

use std::sync::Arc;

use egui::{Align2, Color32, FontId, Id, Key, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::http_client::{self, Exchange, ExchangeState};

use crate::theme::Theme;
use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

fn run_all(s: &mut Session, st: &mut UiState, cmds: Cmds) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn toolbar_button(ui: &mut egui::Ui, text: &str, tip: &str, enabled: bool) -> bool {
    let resp = ui.add_enabled(enabled, egui::Button::new(text).min_size(vec2(24.0, 20.0)));
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tip));
    resp.on_hover_text(tip).clicked()
}

fn status_color(theme: &Theme, status: u16) -> Color32 {
    match status {
        200..=299 => theme.success,
        300..=399 => theme.link,
        400..=499 => theme.warning,
        _ => theme.error,
    }
}

fn state_color(theme: &Theme, e: &Exchange) -> Color32 {
    match &e.state {
        ExchangeState::Running => theme.text_muted,
        ExchangeState::Done(c) => status_color(theme, c.response.status),
        ExchangeState::Failed(_) => theme.error,
    }
}

/// The detail tab shown for a response (view state).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Tab {
    #[default]
    Body,
    Headers,
    Request,
}

/// The body's lines, cached per response so big bodies are split once.
#[derive(Clone, Default)]
struct BodyLines {
    id: u64,
    lines: Arc<Vec<String>>,
}

fn body_lines(ui: &egui::Ui, id: u64, text: &str) -> Arc<Vec<String>> {
    let key = Id::new("http-body-lines");
    if let Some(c) = ui.data(|d| d.get_temp::<BodyLines>(key)).filter(|c| c.id == id) {
        return c.lines;
    }
    let lines: Arc<Vec<String>> = Arc::new(text.split('\n').map(str::to_owned).collect());
    ui.data_mut(|d| d.insert_temp(key, BodyLines { id, lines: lines.clone() }));
    lines
}

/// Monospace lines in a virtualized scroll area.
fn mono_lines(ui: &mut egui::Ui, salt: &str, lines: &[String], color: Color32, font: &FontId) {
    let row_h = ui.fonts_mut(|f| f.row_height(font)).max(1.0);
    ScrollArea::both().id_salt(salt).auto_shrink([false, false]).show_rows(ui, row_h, lines.len(), |ui, range| {
        for line in lines.get(range).unwrap_or_default() {
            ui.add(egui::Label::new(RichText::new(line).font(font.clone()).color(color)).extend().selectable(true));
        }
    });
}

/// Services › HTTP Requests: environment selector and actions, this session's responses (or the
/// project history) on the left, the selected response on the right.
pub fn show_services(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    http_client::refresh_environments(s);
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let http_editor = http_client::is_http_editor(s);
    let show_history = s.http.show_history;

    // ------------------------------------------------------------ toolbar
    ui.horizontal(|ui| {
        let selected = s.http.environment.clone();
        let label = selected.clone().unwrap_or_else(|| "No environment".into());
        egui::ComboBox::from_id_salt("http-env")
            .selected_text(label)
            .show_ui(ui, |ui| {
                if ui.selectable_label(selected.is_none(), "No environment").clicked() {
                    cmds.push(("http.selectEnvironment", json!({ "env": null })));
                }
                for name in &s.http.env_names {
                    if ui.selectable_label(selected.as_deref() == Some(name), name).clicked() {
                        cmds.push(("http.selectEnvironment", json!({ "env": name })));
                    }
                }
                if let Some(sel) = selected.as_ref().filter(|e| !s.http.env_names.contains(e)) {
                    ui.label(RichText::new(format!("{sel} (not defined for this file)")).color(theme.warning));
                }
            })
            .response
            .on_hover_text("Environment: variables from http-client.env.json and http-client.private.env.json");
        if toolbar_button(ui, "▶", "Run the Request at the Caret", http_editor) {
            cmds.push(("http.run", Value::Null));
        }
        if toolbar_button(ui, "⏩", "Run All Requests in File", http_editor) {
            cmds.push(("http.runAll", Value::Null));
        }
        ui.separator();
        if ui.selectable_label(!show_history, "Responses").clicked() && show_history {
            s.http.show_history = false;
        }
        if ui.selectable_label(show_history, "History").clicked() && !show_history {
            cmds.push(("tools.httpClient.showHttpRequestsHistory", Value::Null));
        }
        ui.separator();
        if toolbar_button(ui, "＋", "Create Request in HTTP Client", true) {
            cmds.push(("tools.httpClient.createRequestInHttpClient", Value::Null));
        }
        if toolbar_button(ui, "⇄", "Convert cURL to HTTP Request…", true) {
            cmds.push(("tools.httpClient.convertCURLToHttpRequest", Value::Null));
        }
        if toolbar_button(ui, "🗑", if show_history { "Clear History" } else { "Clear Responses" }, true) {
            cmds.push((if show_history { "http.clearHistory" } else { "http.clearResponses" }, Value::Null));
        }
    });
    ui.separator();

    let avail = ui.available_size();
    let list_w = (avail.x * 0.35).clamp(180.0, 420.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui(vec2(list_w, avail.y), |ui| {
            ui.set_min_size(vec2(list_w, avail.y));
            ScrollArea::vertical().id_salt("http-list").auto_shrink([false, false]).show(ui, |ui| {
                if show_history {
                    history_list(ui, s, &theme, &mut cmds);
                } else {
                    response_list(ui, s, &theme, &mut cmds);
                }
            });
        });
        ui.separator();
        ui.vertical(|ui| {
            if show_history {
                ui.label(
                    RichText::new("Click an entry to open its response; the log opens as a .http file.")
                        .color(theme.text_muted),
                );
            } else {
                details(ui, s, &theme, &mut cmds);
            }
        });
    });
    run_all(s, st, cmds);
}

fn response_list(ui: &mut egui::Ui, s: &Session, theme: &Theme, cmds: &mut Cmds) {
    if s.http.exchanges.is_empty() {
        ui.label(
            RichText::new("No requests yet. Run one with ▶ in the gutter of a .http file.").color(theme.text_muted),
        );
        return;
    }
    let current = s.http.current().map(|e| e.id);
    for e in s.http.exchanges.iter().rev() {
        let status = match &e.state {
            ExchangeState::Running => "…".to_string(),
            ExchangeState::Done(c) => c.response.status.to_string(),
            ExchangeState::Failed(_) => "✖".to_string(),
        };
        let mut job = egui::text::LayoutJob::default();
        job.append(
            &format!("{status:>3}  "),
            0.0,
            egui::TextFormat::simple(FontId::monospace(12.0), state_color(theme, e)),
        );
        job.append(&format!("{} ", e.method), 0.0, egui::TextFormat::simple(FontId::proportional(13.0), theme.keyword));
        job.append(&e.title, 0.0, egui::TextFormat::simple(FontId::proportional(13.0), theme.text));
        let resp = ui.add(egui::Button::selectable(current == Some(e.id), job).truncate()).on_hover_text(format!(
            "{} {}\n{}",
            e.method,
            e.url,
            e.summary()
        ));
        if resp.clicked() {
            cmds.push(("http.selectResponse", json!({ "id": e.id })));
        }
    }
}

fn history_list(ui: &mut egui::Ui, s: &Session, theme: &Theme, cmds: &mut Cmds) {
    if s.http.history.is_empty() {
        ui.label(RichText::new("The project's request history is empty.").color(theme.text_muted));
        return;
    }
    for (i, h) in s.http.history.iter().enumerate() {
        let (status, color) = match (h.status, &h.error) {
            (Some(code), _) => (code.to_string(), status_color(theme, code)),
            (None, _) => ("✖".to_string(), theme.error),
        };
        let mut job = egui::text::LayoutJob::default();
        job.append(&format!("{status:>3}  "), 0.0, egui::TextFormat::simple(FontId::monospace(12.0), color));
        job.append(&format!("{} ", h.method), 0.0, egui::TextFormat::simple(FontId::proportional(13.0), theme.keyword));
        job.append(&h.url, 0.0, egui::TextFormat::simple(FontId::proportional(13.0), theme.text));
        let tip = match &h.error {
            Some(e) => e.clone(),
            None => format!("{} {} · {} ms · {} bytes", h.status.unwrap_or(0), h.status_text, h.elapsed_ms, h.size),
        };
        if ui.add(egui::Button::selectable(false, job).truncate()).on_hover_text(tip).clicked() {
            cmds.push(("http.openHistoryEntry", json!({ "index": i })));
        }
    }
}

fn details(ui: &mut egui::Ui, s: &Session, theme: &Theme, cmds: &mut Cmds) {
    let Some(e) = s.http.current() else {
        ui.label(RichText::new("Select a response.").color(theme.text_muted));
        return;
    };
    let id = e.id;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} {}", e.method, e.url)).strong());
        if let Some(env) = &e.env {
            ui.label(RichText::new(format!("({env})")).color(theme.text_muted));
        }
    });
    let c = match &e.state {
        ExchangeState::Running => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Running…");
            });
            return;
        }
        ExchangeState::Failed(err) => {
            ui.label(RichText::new(err).color(theme.error));
            if e.source.is_some() && ui.button("Rerun").clicked() {
                cmds.push(("http.rerun", json!({ "id": id })));
            }
            return;
        }
        ExchangeState::Done(c) => c,
    };
    let r = &c.response;
    ui.horizontal(|ui| {
        ui.label(RichText::new(r.status_line()).color(status_color(theme, r.status)).strong());
        ui.label(
            RichText::new(format!("{} ms · {}", r.elapsed_ms, codecraft_engine::http_client::human_size(r.body_len)))
                .color(theme.text_muted),
        );
        ui.separator();
        if e.source.is_some() && toolbar_button(ui, "↻", "Rerun", true) {
            cmds.push(("http.rerun", json!({ "id": id })));
        }
        if toolbar_button(ui, "⤓", "Save Response to File…", true) {
            cmds.push(("http.saveResponse", json!({ "id": id })));
        }
        if toolbar_button(ui, "↗", "Open in Editor", true) {
            cmds.push(("http.openResponseInEditor", json!({ "id": id })));
        }
        if toolbar_button(ui, "⧉", "Copy Response Body", true) {
            ui.ctx().copy_text(c.display.clone());
        }
        if let Some((path, line)) = &e.source
            && toolbar_button(ui, "cURL", "Copy as cURL", true)
        {
            cmds.push(("http.copyAsCurl", json!({ "path": path, "line": line })));
        }
    });
    if !c.request.unresolved.is_empty() {
        ui.label(
            RichText::new(format!("Unresolved variables: {}", c.request.unresolved.join(", "))).color(theme.warning),
        );
    }
    if let Some(out) = &c.output_file {
        ui.label(RichText::new(format!("Saved to {}", out.display())).color(theme.text_muted));
    }
    let tab_key = Id::new("http-detail-tab");
    let mut tab: Tab = ui.data(|d| d.get_temp(tab_key)).unwrap_or_default();
    ui.horizontal(|ui| {
        let tabs = [
            (Tab::Body, "Body".to_string()),
            (Tab::Headers, format!("Headers ({})", r.headers.len())),
            (Tab::Request, "Request".to_string()),
        ];
        for (t, label) in tabs {
            if ui.selectable_label(tab == t, label).clicked() {
                tab = t;
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(tab_key, tab));
    let font = FontId::monospace(12.5);
    match tab {
        Tab::Body => {
            let lines = body_lines(ui, id, &c.display);
            mono_lines(ui, "http-body", &lines, theme.text_editor, &font);
        }
        Tab::Headers => {
            ScrollArea::both().id_salt("http-headers").auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("http-headers-grid").num_columns(2).striped(true).show(ui, |ui| {
                    for (k, v) in &r.headers {
                        ui.label(RichText::new(k).font(font.clone()).color(theme.field));
                        ui.add(
                            egui::Label::new(RichText::new(v).font(font.clone()).color(theme.string)).selectable(true),
                        );
                        ui.end_row();
                    }
                });
            });
        }
        Tab::Request => {
            let lines: Vec<String> = c.request.to_http_text().split('\n').map(str::to_owned).collect();
            mono_lines(ui, "http-request", &lines, theme.text_editor, &font);
        }
    }
}

// ---------------------------------------------------------------- dialogs

/// Convert cURL to HTTP Request, the Postman collection picker and Save Response. Called once
/// per frame by the shell.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let mut cmds: Cmds = Vec::new();
    if s.http.pick_postman {
        s.http.pick_postman = false;
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Import from Postman Collection File")
            .add_filter("Postman collection", &["json"])
            .pick_file()
        {
            cmds.push(("tools.httpClient.importFromPostmanCollectionFile", json!({ "path": path })));
        }
    }
    if let Some(id) = s.http.save_pick.take() {
        let ext = s
            .http
            .exchange(id)
            .and_then(|e| match &e.state {
                ExchangeState::Done(c) => {
                    c.response.body_file.as_ref().and_then(|p| p.extension()).map(|x| x.to_string_lossy().into_owned())
                }
                _ => None,
            })
            .unwrap_or_else(|| "txt".into());
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Save Response to File")
            .set_file_name(format!("response.{ext}"))
            .save_file()
        {
            cmds.push(("http.saveResponse", json!({ "id": id, "path": path })));
        }
    }
    if let Some(mut text) = s.http.curl_dialog.clone() {
        let theme = st.theme.clone();
        let ctx = ui.ctx().clone();
        let mut open = true;
        let mut convert = false;
        let mut cancel = false;
        egui::Window::new("Convert cURL to HTTP Request")
            .id(Id::new("http-curl"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .anchor(Align2::CENTER_TOP, vec2(0.0, 100.0))
            .show(&ctx, |ui| {
                ui.label(
                    RichText::new("Paste a cURL command; the request is inserted into the active .http file.")
                        .color(theme.text_muted),
                );
                let edit = ui.add(
                    TextEdit::multiline(&mut text)
                        .font(FontId::monospace(12.5))
                        .desired_rows(8)
                        .desired_width(f32::INFINITY)
                        .hint_text("curl -X POST https://example.org/api -H 'Content-Type: application/json' -d '{}'"),
                );
                if !edit.has_focus() && text.is_empty() {
                    edit.request_focus();
                }
                ui.horizontal(|ui| {
                    convert = ui.add_enabled(!text.trim().is_empty(), egui::Button::new("Convert")).clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if s.http.curl_dialog.is_some() {
            s.http.curl_dialog = Some(text.clone());
        }
        if convert {
            cmds.push(("tools.httpClient.convertCURLToHttpRequest", json!({ "curl": text })));
        } else if cancel || !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
            cmds.push(("http.closeCurlDialog", Value::Null));
        }
    }
    run_all(s, st, cmds);
}
