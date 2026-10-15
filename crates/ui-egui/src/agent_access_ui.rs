//! Settings › Tools › Agent Access: what agents connected over MCP (Claude Code, built-in agent
//! jobs, `codecraft-cli mcp --connect`) may do with CodeCraft's tools. Every change is a command
//! (`agentAccess.*`).

use egui::RichText;
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent_access::{Capability, Level};

use crate::UiState;
use crate::settings_ui::Cmds;

/// Width of the Off / Ask / Allow buttons.
const CONTROLS_W: f32 = 170.0;

pub(crate) fn page(ui: &mut egui::Ui, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let theme = &st.theme;
    ui.label(
        RichText::new(
            "Agents use CodeCraft's tools to debug, analyze, profile, test and query your project. Ask shows an \
             approval prompt (Allow once / Always for this session / Deny); agent jobs in Auto mode are not asked.",
        )
        .color(theme.text_muted),
    );
    ui.add_space(8.0);
    let policy = s.agent_access.to_json();
    // One row per capability: the text takes the width the Off/Ask/Allow buttons leave (a grid
    // sized its first column from the previous frame and squeezed the labels to a word per line
    // in a 900 px window; QA M14).
    for cap in Capability::ALL {
        ui.horizontal(|ui| {
            let text_w = (ui.available_width() - CONTROLS_W - 12.0).max(160.0);
            ui.vertical(|ui| {
                ui.set_width(text_w);
                ui.label(cap.label());
                ui.label(RichText::new(cap.description()).small().color(theme.text_muted));
            });
            let current = s.agent_access.level(cap);
            ui.horizontal(|ui| {
                for level in Level::ALL {
                    let button = egui::Button::selectable(current == level, level.label());
                    let hint = match level {
                        Level::Off => "Agents may never do this",
                        Level::Ask => "Ask me each time (or allow for the rest of the session)",
                        Level::Allow => "Agents may do this without asking",
                    };
                    if ui.add(button).on_hover_text(hint).clicked() && current != level {
                        cmds.push(("agentAccess.set", json!({ "capability": cap.id(), "level": level.id() })));
                    }
                }
            });
        });
        ui.add_space(8.0);
    }
    ui.add_space(10.0);
    let approvals = policy["sessionApprovals"].as_array().cloned().unwrap_or_default();
    if approvals.is_empty() {
        ui.label(RichText::new("No \"Always for this session\" approvals.").color(theme.text_muted));
    } else {
        ui.label(RichText::new("Allowed for this session").strong());
        for a in &approvals {
            ui.label(format!(
                "  {} — {}",
                a["caller"].as_str().unwrap_or("?"),
                a["capability"].as_str().unwrap_or("?")
            ));
        }
        if ui.button("Forget session approvals").clicked() {
            cmds.push(("agentAccess.clearSession", Value::Null));
        }
    }
    ui.add_space(6.0);
    let waiting = s.agent_tools.pending();
    if waiting > 0 {
        ui.label(RichText::new(format!("{waiting} agent tool call(s) waiting")).color(theme.warning));
    }
    if let Some(e) = policy["error"].as_str() {
        ui.label(RichText::new(e).color(theme.error));
    }
    ui.horizontal(|ui| {
        if ui.button("Reset to Defaults").on_hover_text("Read: Allow; everything else: Ask").clicked() {
            cmds.push(("agentAccess.reset", Value::Null));
        }
        if ui
            .button("Connect an External Agent…")
            .on_hover_text("Configuration for Claude Code, Codex, Gemini CLI, Cursor, Grok or any MCP client")
            .clicked()
        {
            cmds.push(("agent.connectExternal", Value::Null));
        }
    });
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Headless: `codecraft-cli mcp <project> --allow run,debug,profile` (nothing is asked there; what is not \
             allowed is denied). See docs/agents.md.",
        )
        .small()
        .color(theme.text_muted),
    );
}
