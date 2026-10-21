//! Workspace automation: `cargo xtask <layers|catalog|parity|bundle|dmg|notarize|package-linux|package-deb|
//! package-windows|package-windows-installer|version>`.
//!
//! - `layers`  checks crate layering (a crate may only depend on lower layers; no UI deps below L3).
//! - `catalog` regenerates `crates/engine/src/catalog.rs` from the observed RustRover menus.
//! - `parity`  regenerates `docs/parity-checklist.md` from the catalog and the engine's commands.
//! - `bundle`, `dmg`, `package-linux`, `package-deb`, `package-windows`, `package-windows-installer`
//!   build and package the app (see
//!   `package.rs` and docs/development.md); options `--release`, `--layout-only`, `--dry-run`,
//!   and for the macOS tasks `--sign <identity>` (default ad hoc) and `--keychain <path>`.
//! - `notarize` submits the signed dmg (or `--app`) with `xcrun notarytool` and a keychain profile,
//!   staples and assesses it (`sign.rs`, docs/release.md); `--profile <name>`, `--dmg <path>`,
//!   `--app <path>`, `--keychain <path>`, `--dry-run`.
//! - `version [patch|minor|major|<x.y.z>] [--dry-run]` prints or bumps the workspace version
//!   (`version.rs`, docs/release.md).

mod icon;
mod package;
mod sign;
mod version;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;

const MENUS_JSON: &str = "docs/observation/rustrover-2026.1-menus.json";
const CATALOG_RS: &str = "crates/engine/src/catalog.rs";
const PARITY_MD: &str = "docs/parity-checklist.md";

fn main() -> ExitCode {
    let root = workspace_root();
    let task = std::env::args().nth(1).unwrap_or_default();
    let rest: Vec<String> = std::env::args().skip(2).collect();
    let opts = || package::Options::parse(&rest);
    let result = match task.as_str() {
        "layers" => layers(&root),
        "catalog" => catalog(&root),
        "parity" => parity(&root),
        "bundle" => opts().and_then(|o| package::bundle(&root, &o)).map(drop),
        "dmg" => opts().and_then(|o| package::dmg(&root, &o)).map(drop),
        "notarize" => sign::NotarizeOptions::parse(&rest).and_then(|o| package::notarize(&root, &o)),
        "package-linux" => opts().and_then(|o| package::package_linux(&root, &o)).map(drop),
        "package-windows" => opts().and_then(|o| package::package_windows(&root, &o)).map(drop),
        "package-windows-installer" => opts().and_then(|o| package::package_windows_installer(&root, &o)).map(drop),
        "package-deb" => opts().and_then(|o| package::package_deb(&root, &o)).map(drop),
        "version" => version::run(&root, &rest),
        _ => Err("usage: cargo xtask <layers|catalog|parity|bundle|dmg|notarize|package-linux|package-deb|package-windows|\
                  package-windows-installer|version> \
                  [--release] [--dry-run] [--sign <identity>] [--profile <notarytool profile>]"
            .to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask {task}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn workspace_root() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.parent().map(Path::to_path_buf).unwrap_or(dir)
}

// ---------------------------------------------------------------- layers

fn layer_of(name: &str) -> Option<u8> {
    Some(match name {
        "codecraft-text" | "codecraft-syntax" | "codecraft-fs" | "codecraft-lsp" | "codecraft-cargo"
        | "codecraft-dap" | "codecraft-db" | "codecraft-agent" | "codecraft-mcp" | "codecraft-term"
        | "codecraft-vcs" | "codecraft-http" | "codecraft-remote" | "codecraft-profile" => 0,
        "codecraft-workspace" => 1,
        "codecraft-engine" => 2,
        "codecraft-ui-egui" => 3,
        _ => return None,
    })
}

fn layers(root: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    let crates = std::fs::read_dir(root.join("crates")).map_err(|e| e.to_string())?;
    for entry in crates.flatten() {
        let manifest = entry.path().join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else { continue };
        let name = package_name(&text).ok_or_else(|| format!("{}: no package name", manifest.display()))?;
        let Some(layer) = layer_of(&name) else {
            errors.push(format!("{name}: not registered in xtask layer_of()"));
            continue;
        };
        for dep in normal_dependencies(&text) {
            if let Some(dep_layer) = layer_of(&dep)
                && dep_layer >= layer
            {
                errors.push(format!("{name} (L{layer}) depends on {dep} (L{dep_layer})"));
            }
            if layer < 3 && matches!(dep.as_str(), "egui" | "eframe" | "rfd" | "winit") {
                errors.push(format!("{name} (L{layer}) depends on UI crate {dep}"));
            }
        }
    }
    if errors.is_empty() {
        println!("layers ok");
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

fn package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_package = t == "[package]";
        } else if in_package && let Some(rest) = t.strip_prefix("name") {
            return Some(rest.trim_start_matches([' ', '=']).trim().trim_matches('"').to_string());
        }
    }
    None
}

fn normal_dependencies(manifest: &str) -> Vec<String> {
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in manifest.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
        } else if in_deps && !t.is_empty() && !t.starts_with('#') {
            let name: String = t.chars().take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_').collect();
            if !name.is_empty() {
                deps.push(name);
            }
        }
    }
    deps
}

// ---------------------------------------------------------------- catalog

#[derive(Deserialize)]
struct MenuEntry {
    path: Vec<String>,
    #[serde(default)]
    separator: bool,
    #[serde(default)]
    title: String,
    #[serde(default)]
    shortcut: String,
    #[serde(default)]
    submenu: bool,
}

/// Commands whose observed label embeds the selected run configuration's name.
const ID_OVERRIDES: &[(&str, &str)] = &[
    ("Run 'Run'", "run"),
    ("Debug 'Run'", "debug"),
    ("Run 'Run' with Coverage", "runWithCoverage"),
    ("Profile 'Run'", "profile"),
    ("Run 'Run' with Valgrind Memcheck", "runWithValgrindMemcheck"),
    ("Run…", "chooseAndRun"),
    ("Debug…", "chooseAndDebug"),
    ("Cyclic Expand Word (Backward)", "cyclicExpandWordBackward"),
];

/// Items contributed by plugins outside RustRover's Rust focus (third-party or other stacks),
/// or by the operating system. Matched against "path › title".
const OUT_OF_SCOPE: &[&str] = &[
    "Copilot",
    "Junie",
    "AI Assistant",
    "Educational",
    "JetBrains Academy",
    "Learn and Teach",
    "Emmet",
    "XPath",
    "XML",
    "JSONPath",
    "HTTP",
    "Deployment",
    "Deployed",
    "Qodana",
    "SSH",
    "Remote Development",
    "Dev Container",
    "Dockerfile",
    "Coding Agent",
    "Backup and Sync",
    "Semantic Tokens Inspector",
    "Language Servers",
    "LSP ",
    "File Transfer",
    "My Productivity",
    "Register…",
    "Contact Support",
    "Submit a Bug Report",
    "Submit Feedback",
    "Collect Junie",
    "Check for Updates",
    "Keyboard Shortcuts PDF",
    "Edit Custom VM Options",
    "Change Memory Settings",
    "Delete Leftover IDE Directories",
    "Start CPU Usage Profiling",
    "Capture Memory Snapshot",
    "Diagnostic Tools",
    "Collect Logs",
    "Show Log in Finder",
    "Edit Custom Properties",
    "Edit as Table",
    "Merge Requests",
    "Pull Requests",
    "Gist",
    "Fork",
    "Share Project on",
    "Encode XML",
    "Bidi Text",
    "UML",
    "Create Command Line Launcher",
    "Import Tests from File",
    "Repair IDE",
    "Manage IDE Settings",
    "Tip of the Day",
    "Getting Started",
    "What's New",
    "Move to Built-in Display",
    "Services › Docker",
    "Import Word Document",
    "Configure Pandoc",
    // macOS native window tabs (NSWindow merge): CodeCraft's project windows are egui viewports,
    // which cannot become tabs of one window.
    "Merge All Project Windows",
];

/// Submenus whose items are bound to two-stroke chords, which the menu dump cannot express.
const CHORD_SUBMENUS: &[&str] = &["Expand to Level", "Expand All to Level"];

/// Provided by the macOS application menu / window server rather than the app.
const OS_ITEMS: &[&str] = &[
    "RustRover › Services",
    "RustRover › Hide RustRover",
    "RustRover › Hide Others",
    "RustRover › Show All",
    "Window › Minimize",
    "Window › Zoom",
    "Edit › Emoji & Symbols",
];

/// In scope for RustRover parity but planned after the editor/build/LSP core (see docs/roadmap.md).
const LATER: &[&str] = &[
    "Rust REPL",
    "Presentation Assistant",
    "Python",
    "Zig",
    "TypeScript",
    "setup.py",
    "Debug",
    "Breakpoint",
    "Coverage",
    "Profile",
    "Profiler",
    "Valgrind",
    "Attach",
    "Git ›",
    "Local History",
    "Macros",
    "Live Template",
    "Structurally",
    "Inspect Code",
    "Code Cleanup",
    "Analyze",
    "Print",
    "Export",
    "Hierarchy",
    "Tasks & Contexts",
    "Test History",
    "Manage Targets",
    "Edit Configurations",
    "Run Configuration Templates",
    "Settings for New Projects",
    "Save File as Template",
    "Update Copyright",
    "Rearrange Code",
    "Fill Paragraph",
    "Generate",
    "Override Methods",
    "Implement Methods",
    "Surround With",
    "Unwrap/Remove",
    "Split",
    "Stretch Editor",
    "Splitter",
    "Unsplit",
    "Layouts",
    "Bookmarks",
    "Scratch File",
    "Project from Version Control",
    "Associate with File Type",
    "File Encoding",
    "BOM",
    "Inline Completion",
    "Inline Proposal",
    "Command Completion",
    "Type-Matching",
    "Cyclic Expand Word",
    "Context Info",
    "Quick Type Definition",
    "Compare With",
    "Compare with Clipboard",
    "Quick Switch Scheme",
    "Custom Folding",
    "Live Template Parameter",
    "Related Symbol",
    "Super Method",
    "Method Hierarchy",
    "Type Hierarchy",
    "Call Hierarchy",
    "Safe Delete",
    "Move File",
    "Copy File",
    "Refactor This",
    "Analyze Stack Trace",
    "Save as Live Template",
    "Modify Selected Code",
    "Stop Background Processes",
    "Show Running List",
    "Processes",
    "Notifications ›",
    "Active Tool Window",
    "Configure Editor Tabs",
    "Show Hidden Tabs",
    "Keep Tab Open",
    "Close All Read-Only",
    "Close All but Pinned",
    "Recently Changed Files",
    "Recent Changes",
    "Make file read-only",
    "Power Save Mode",
    "Find Action",
    "Search Structurally",
];

fn camel(words: &str) -> String {
    let cleaned: String = words
        .replace("…", "")
        .replace("...", "")
        .replace("(s)", "s")
        .replace('&', " ")
        .replace('\'', "")
        .replace(['/', ':', '-', '.', ','], " ");
    // Drop parenthesized details, e.g. "CRLF - Windows (\r\n)".
    let mut out = String::new();
    let mut depth = 0usize;
    for c in cleaned.chars() {
        match c {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let mut id = String::new();
    for (i, w) in out.split_whitespace().enumerate() {
        let w: String = w.chars().filter(|c| c.is_alphanumeric()).collect();
        if w.is_empty() {
            continue;
        }
        let all_caps = w.chars().all(|c| !c.is_lowercase());
        let lower = w.to_lowercase();
        let mut chars = if all_caps { lower.chars() } else { w.chars() };
        if i == 0 || id.is_empty() {
            id.extend(chars.next().map(|c| c.to_ascii_lowercase()));
            if all_caps {
                id.extend(chars);
            } else {
                id.push_str(chars.as_str());
            }
        } else {
            id.extend(chars.next().map(|c| c.to_ascii_uppercase()));
            id.push_str(chars.as_str());
        }
    }
    id
}

fn command_id(path: &[String], title: &str) -> String {
    let mut parts: Vec<String> =
        path.iter().map(|p| if p == "RustRover" { "app".to_string() } else { camel(p) }).collect();
    let last =
        ID_OVERRIDES.iter().find(|(t, _)| *t == title).map(|(_, id)| id.to_string()).unwrap_or_else(|| camel(title));
    parts.push(last);
    parts.join(".")
}

fn scope_of(path: &[String], title: &str) -> &'static str {
    let full = format!("{} › {}", path.join(" › "), title);
    if OS_ITEMS.iter().any(|p| full.starts_with(p)) {
        return "Os";
    }
    if OUT_OF_SCOPE.iter().any(|k| full.contains(k)) {
        return "Out";
    }
    if full.starts_with("Git ›") || LATER.iter().any(|k| full.contains(k)) {
        return "Later";
    }
    "Core"
}

fn catalog(root: &Path) -> Result<(), String> {
    let json = std::fs::read_to_string(root.join(MENUS_JSON)).map_err(|e| format!("{MENUS_JSON}: {e}"))?;
    let entries: Vec<MenuEntry> = serde_json::from_str(&json).map_err(|e| e.to_string())?;
    let mut out = String::new();
    out.push_str(
        "//! RustRover's menu structure, observed from the running app (see docs/observation/), with the\n\
         //! CodeCraft command id for every item. Items whose id is registered in the engine are live.\n\
         //!\n\
         //! GENERATED by `cargo xtask catalog` from docs/observation/rustrover-2026.1-menus.json. Do not edit.\n\n\
         use crate::commands::{CatalogItem, Scope};\n\n\
         pub static CATALOG: &[CatalogItem] = &[\n",
    );
    let mut seen = BTreeSet::new();
    let mut count = 0usize;
    for e in &entries {
        let path = e.path.iter().map(|p| format!("{p:?}")).collect::<Vec<_>>().join(", ");
        if e.separator {
            let _ = writeln!(
                out,
                "    CatalogItem {{ path: &[{path}], label: \"---\", shortcut: None, id: \"---\", submenu: false, scope: Scope::Core }},"
            );
            continue;
        }
        let mut id = command_id(&e.path, &e.title);
        let mut n = 2;
        while !seen.insert(id.clone()) {
            id = format!("{}{n}", command_id(&e.path, &e.title));
            n += 1;
        }
        // Two-stroke chords (Cmd+* then a digit) show only their first stroke in the menu.
        let chord = e.path.iter().any(|p| CHORD_SUBMENUS.contains(&p.as_str()));
        let shortcut =
            if e.shortcut.is_empty() || chord { "None".to_string() } else { format!("Some({:?})", e.shortcut) };
        let scope = scope_of(&e.path, &e.title);
        let _ = writeln!(
            out,
            "    CatalogItem {{ path: &[{path}], label: {:?}, shortcut: {shortcut}, id: {id:?}, submenu: {}, scope: Scope::{scope} }},",
            e.title, e.submenu
        );
        count += 1;
    }
    out.push_str("];\n");
    std::fs::write(root.join(CATALOG_RS), out).map_err(|e| e.to_string())?;
    println!("wrote {CATALOG_RS} ({count} items)");
    Ok(())
}

// ---------------------------------------------------------------- parity

#[derive(Deserialize)]
struct CliCatalogItem {
    path: Vec<String>,
    label: String,
    shortcut: Option<String>,
    id: String,
    submenu: bool,
    scope: String,
    live: bool,
}

#[derive(Deserialize)]
struct CliCommand {
    id: String,
    label: String,
    params: String,
}

/// Control actions supplement observed menus without changing the menu parity counts.
fn remote_project_commands(md: &mut String, commands: &[CliCommand]) {
    md.push_str("\n## Remote project control commands\n\nRegistered engine/CLI actions supplementing File › Remote Development (not additional observed menu items).\n\n");
    for command in commands.iter().filter(|c| c.id.starts_with("remote.project.") || c.id == "remote.syncFromRemote") {
        let params = command.params.split_whitespace().collect::<Vec<_>>().join(" ");
        let params = if params.is_empty() { "{}".to_string() } else { params };
        let _ = writeln!(md, "- [x] {} — `{}`: {}", command.label, command.id, params);
    }
}

fn parity(root: &Path) -> Result<(), String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(root)
        .args(["run", "--quiet", "-p", "codecraft-cli", "--", "catalog", "--json"])
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    let items: Vec<CliCatalogItem> = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    let leaves: Vec<&CliCatalogItem> = items.iter().filter(|i| i.id != "---" && !i.submenu).collect();
    let mut per_scope: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for i in &leaves {
        let e = per_scope.entry(i.scope.as_str()).or_default();
        e.1 += 1;
        if i.live {
            e.0 += 1;
        }
    }
    let (core_live, core_total) = per_scope.get("Core").copied().unwrap_or_default();
    let (later_live, later_total) = per_scope.get("Later").copied().unwrap_or_default();
    let pct = |a: usize, b: usize| if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 };
    let mut md = String::new();
    md.push_str("# Menu parity checklist\n\nGenerated by `cargo xtask parity` from `crates/engine/src/catalog.rs` (RustRover 2026.1 menus, observed) and the engine's command registry. Do not edit.\n\n");
    let _ = writeln!(md, "| Scope | Live | Total | % |\n|---|---|---|---|");
    let _ = writeln!(md, "| Core | {core_live} | {core_total} | {:.0}% |", pct(core_live, core_total));
    let _ = writeln!(md, "| Later | {later_live} | {later_total} | {:.0}% |", pct(later_live, later_total));
    let in_scope_live = core_live + later_live;
    let in_scope_total = core_total + later_total;
    let _ = writeln!(
        md,
        "| **In scope** | **{in_scope_live}** | **{in_scope_total}** | **{:.0}%** |",
        pct(in_scope_live, in_scope_total)
    );
    for (scope, label) in [("Out", "Out of scope"), ("Os", "Provided by macOS")] {
        let (l, t) = per_scope.get(scope).copied().unwrap_or_default();
        let _ = writeln!(md, "| {label} | {l} | {t} | |");
    }
    md.push_str("\nCore = the editor/navigation/build/run/LSP surface of the current milestones. Later = RustRover features planned for later milestones (debugger, VCS, coverage, profiler, refactorings, …). Out = items from third-party or non-Rust plugins in the observed install.\n");
    let mut current_top = String::new();
    for i in &leaves {
        if i.scope == "Out" || i.scope == "Os" {
            continue;
        }
        let top = i.path.first().cloned().unwrap_or_default();
        if top != current_top {
            let _ = writeln!(md, "\n## {top}\n");
            current_top = top;
        }
        let mark = if i.live { "x" } else { " " };
        let sub = if i.path.len() > 1 { format!("{} › ", i.path[1..].join(" › ")) } else { String::new() };
        let sc = i.shortcut.as_deref().map(|s| format!(" `{s}`")).unwrap_or_default();
        let later = if i.scope == "Later" { " _(later)_" } else { "" };
        let _ = writeln!(md, "- [{mark}] {sub}{}{sc} — `{}`{later}", i.label, i.id);
    }
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .current_dir(root)
        .args(["run", "--quiet", "-p", "codecraft-cli", "--", "commands", "--json"])
        .output().map_err(|e| e.to_string())?;
    if !output.status.success() { return Err(String::from_utf8_lossy(&output.stderr).into_owned()); }
    let commands: Vec<CliCommand> = serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    remote_project_commands(&mut md, &commands);
    std::fs::write(root.join(PARITY_MD), md).map_err(|e| e.to_string())?;
    println!(
        "wrote {PARITY_MD}: core {core_live}/{core_total} ({:.0}%), in scope {in_scope_live}/{in_scope_total} ({:.0}%)",
        pct(core_live, core_total),
        pct(in_scope_live, in_scope_total)
    );
    Ok(())
}

#[cfg(test)]
mod remote_project_command_tests {
    use super::*;

    #[test]
    fn includes_registered_remote_controls_and_parameter_docs_only() {
        let mut md = String::new();
        remote_project_commands(&mut md, &[
            CliCommand { id: "remote.project.reconnect".into(), label: "Reconnect".into(), params: "{} — retry\n the host".into() },
            CliCommand { id: "remote.syncFromRemote".into(), label: "Refresh".into(), params: "{server?: string}".into() },
            CliCommand { id: "remote.project.status".into(), label: "Status".into(), params: String::new() },
            CliCommand { id: "editor.type".into(), label: "Type".into(), params: "{}".into() },
        ]);
        assert!(md.contains("`remote.project.reconnect`: {} — retry the host"));
        assert!(md.contains("`remote.syncFromRemote`: {server?: string}"));
        assert!(md.contains("`remote.project.status`: {}\n"));
        assert!(!md.contains("editor.type"));
    }
}
