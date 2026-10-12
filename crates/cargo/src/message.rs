//! Cargo's `--message-format=json…` stdout lines, keyed by `reason`.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::json::null_default;
use crate::{CargoMessage, Diagnostic, DiagnosticSpan, Level};

/// Child diagnostics nested deeper than this are dropped (rustc nests one level in practice).
const MAX_DIAGNOSTIC_DEPTH: usize = 8;

pub(crate) fn parse_message(line: &str) -> Option<CargoMessage> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    // Two steps, so a future reason whose fields clash with ours can't break the known ones.
    // serde_json's recursion limit keeps hostile nesting from overflowing the stack.
    let value: Value = serde_json::from_str(line).ok()?;
    match value.get("reason")?.as_str()? {
        "compiler-message" => {
            let raw = RawCompilerMessage::deserialize(&value).ok()?;
            Some(CargoMessage::CompilerMessage {
                package_id: raw.package_id,
                manifest_path: raw.manifest_path,
                diagnostic: raw.message.into_diagnostic(0),
            })
        }
        "compiler-artifact" => {
            let raw = RawArtifact::deserialize(&value).ok()?;
            Some(CargoMessage::CompilerArtifact {
                package_id: raw.package_id,
                target_name: raw.target.name,
                executable: raw.executable,
                fresh: raw.fresh,
            })
        }
        "build-script-executed" => {
            let raw = RawBuildScript::deserialize(&value).ok()?;
            Some(CargoMessage::BuildScriptExecuted { package_id: raw.package_id })
        }
        "build-finished" => {
            let raw = RawBuildFinished::deserialize(&value).ok()?;
            Some(CargoMessage::BuildFinished { success: raw.success })
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct RawCompilerMessage {
    #[serde(default, deserialize_with = "null_default")]
    package_id: String,
    #[serde(default)]
    manifest_path: Option<PathBuf>,
    message: RawDiagnostic,
}

#[derive(Deserialize)]
struct RawArtifact {
    #[serde(default, deserialize_with = "null_default")]
    package_id: String,
    #[serde(default, deserialize_with = "null_default")]
    target: RawTargetName,
    #[serde(default)]
    executable: Option<PathBuf>,
    #[serde(default, deserialize_with = "null_default")]
    fresh: bool,
}

#[derive(Default, Deserialize)]
struct RawTargetName {
    #[serde(default, deserialize_with = "null_default")]
    name: String,
}

#[derive(Deserialize)]
struct RawBuildScript {
    #[serde(default, deserialize_with = "null_default")]
    package_id: String,
}

#[derive(Deserialize)]
struct RawBuildFinished {
    success: bool,
}

/// rustc's JSON diagnostic (`rustc --error-format=json`), the fields we keep.
#[derive(Deserialize)]
struct RawDiagnostic {
    #[serde(default, deserialize_with = "null_default")]
    message: String,
    #[serde(default)]
    code: Option<Value>,
    #[serde(default, deserialize_with = "null_default")]
    level: String,
    #[serde(default, deserialize_with = "null_default")]
    spans: Vec<RawSpan>,
    #[serde(default, deserialize_with = "null_default")]
    children: Vec<RawDiagnostic>,
    #[serde(default)]
    rendered: Option<String>,
}

#[derive(Deserialize)]
struct RawSpan {
    #[serde(default, deserialize_with = "null_default")]
    file_name: String,
    #[serde(default, deserialize_with = "null_default")]
    line_start: usize,
    #[serde(default, deserialize_with = "null_default")]
    line_end: usize,
    #[serde(default, deserialize_with = "null_default")]
    column_start: usize,
    #[serde(default, deserialize_with = "null_default")]
    column_end: usize,
    #[serde(default, deserialize_with = "null_default")]
    is_primary: bool,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    suggested_replacement: Option<String>,
}

impl RawDiagnostic {
    fn into_diagnostic(self, depth: usize) -> Diagnostic {
        let children = if depth >= MAX_DIAGNOSTIC_DEPTH {
            Vec::new()
        } else {
            self.children.into_iter().map(|c| c.into_diagnostic(depth.saturating_add(1))).collect()
        };
        Diagnostic {
            level: parse_level(&self.level),
            message: self.message,
            code: self.code.and_then(code_string),
            spans: self.spans.into_iter().map(DiagnosticSpan::from).collect(),
            children,
            rendered: self.rendered,
        }
    }
}

impl From<RawSpan> for DiagnosticSpan {
    fn from(raw: RawSpan) -> Self {
        Self {
            file_name: raw.file_name,
            line_start: raw.line_start,
            line_end: raw.line_end,
            column_start: raw.column_start,
            column_end: raw.column_end,
            is_primary: raw.is_primary,
            label: raw.label,
            suggested_replacement: raw.suggested_replacement,
        }
    }
}

/// rustc writes `"code": {"code": "E0308", "explanation": "…"}`; accept a bare string too.
fn code_string(code: Value) -> Option<String> {
    match code {
        Value::String(code) => Some(code),
        Value::Object(mut fields) => match fields.remove("code") {
            Some(Value::String(code)) => Some(code),
            _ => None,
        },
        _ => None,
    }
    .filter(|c| !c.is_empty())
}

fn parse_level(level: &str) -> Level {
    match level {
        "error" => Level::Error,
        "warning" => Level::Warning,
        "note" => Level::Note,
        "help" => Level::Help,
        "failure-note" => Level::FailureNote,
        "error: internal compiler error" => Level::Ice,
        other if other.contains("internal compiler error") => Level::Ice,
        other if other.starts_with("error") => Level::Error,
        other if other.starts_with("warning") => Level::Warning,
        _ => Level::Note,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// `cargo check --message-format=json-diagnostic-rendered-ansi` for an unused variable: a primary
    /// span with a label, a `note` child and a `help` child carrying a machine-applicable suggestion.
    const UNUSED_VARIABLE: &str = r#"{"reason":"compiler-message","package_id":"path+file:///work/app#0.1.0","manifest_path":"/work/app/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"app","src_path":"/work/app/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true},"message":{"rendered":"\u001b[0m\u001b[1m\u001b[33mwarning\u001b[0m\u001b[0m\u001b[1m: unused variable: `x`\u001b[0m\n\u001b[0m \u001b[0m\u001b[0m\u001b[1m\u001b[38;5;12m--> \u001b[0m\u001b[0msrc/main.rs:2:9\u001b[0m\n\n","$message_type":"diagnostic","children":[{"children":[],"code":null,"level":"note","message":"`#[warn(unused_variables)]` on by default","rendered":null,"spans":[]},{"children":[],"code":null,"level":"help","message":"if this is intentional, prefix it with an underscore","rendered":null,"spans":[{"byte_end":21,"byte_start":20,"column_end":10,"column_start":9,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":null,"line_end":2,"line_start":2,"suggested_replacement":"_x","suggestion_applicability":"MaybeIncorrect","text":[{"highlight_end":10,"highlight_start":9,"text":"    let x = 1;"}]}]}],"code":{"code":"unused_variables","explanation":null},"level":"warning","message":"unused variable: `x`","spans":[{"byte_end":21,"byte_start":20,"column_end":10,"column_start":9,"expansion":null,"file_name":"src/main.rs","is_primary":true,"label":null,"line_end":2,"line_start":2,"suggested_replacement":null,"suggestion_applicability":null,"text":[{"highlight_end":10,"highlight_start":9,"text":"    let x = 1;"}]}]}}"#;

    /// A type mismatch (E0308) with two spans (secondary span first) and a macro expansion.
    const MISMATCHED_TYPES: &str = r#"{"reason":"compiler-message","package_id":"path+file:///work/app#0.1.0","manifest_path":"/work/app/Cargo.toml","target":{"name":"app"},"message":{"rendered":"error[E0308]: mismatched types\n","children":[],"code":{"code":"E0308","explanation":"Expected type did not match the received type.\n"},"level":"error","message":"mismatched types","spans":[{"byte_end":17,"byte_start":14,"column_end":17,"column_start":14,"expansion":null,"file_name":"src/main.rs","is_primary":false,"label":"expected due to this","line_end":2,"line_start":2,"suggested_replacement":null,"suggestion_applicability":null,"text":[]},{"byte_end":23,"byte_start":20,"column_end":23,"column_start":20,"expansion":{"def_site_span":{"byte_end":0,"byte_start":0,"column_end":1,"column_start":1,"expansion":null,"file_name":"<std macros>","is_primary":false,"label":null,"line_end":1,"line_start":1,"suggested_replacement":null,"suggestion_applicability":null,"text":[]},"macro_decl_name":"format!","span":{"byte_end":1,"byte_start":0,"column_end":2,"column_start":1,"expansion":null,"file_name":"src/main.rs","is_primary":false,"label":null,"line_end":1,"line_start":1,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}},"file_name":"src/main.rs","is_primary":true,"label":"expected `u32`, found `&str`","line_end":2,"line_start":2,"suggested_replacement":null,"suggestion_applicability":null,"text":[]}]}}"#;

    const ARTIFACT_BIN: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///work/app#0.1.0","manifest_path":"/work/app/Cargo.toml","target":{"kind":["bin"],"crate_types":["bin"],"name":"app","src_path":"/work/app/src/main.rs","edition":"2021","doc":true,"doctest":false,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["/work/app/target/debug/app"],"executable":"/work/app/target/debug/app","fresh":false}"#;

    const ARTIFACT_LIB: &str = r#"{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#libc@0.2.150","manifest_path":"/home/u/.cargo/registry/src/libc-0.2.150/Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"libc","src_path":"/home/u/.cargo/registry/src/libc-0.2.150/src/lib.rs","edition":"2015","doc":true,"doctest":true,"test":true},"profile":{"opt_level":"0","debuginfo":0,"debug_assertions":true,"overflow_checks":true,"test":false},"features":["default","std"],"filenames":["/work/app/target/debug/deps/liblibc-1.rlib","/work/app/target/debug/deps/liblibc-1.rmeta"],"executable":null,"fresh":true}"#;

    const BUILD_SCRIPT: &str = r#"{"reason":"build-script-executed","package_id":"registry+https://github.com/rust-lang/crates.io-index#libc@0.2.150","linked_libs":[],"linked_paths":[],"cfgs":["freebsd11","libc_priv_mod_use"],"env":[],"out_dir":"/work/app/target/debug/build/libc-2/out"}"#;

    fn compiler_message(line: &str) -> (String, Option<PathBuf>, Diagnostic) {
        match parse_message(line) {
            Some(CargoMessage::CompilerMessage { package_id, manifest_path, diagnostic }) => {
                (package_id, manifest_path, diagnostic)
            }
            other => panic!("expected a compiler message, got {other:?}"),
        }
    }

    #[test]
    fn warning_with_children_and_suggestion() {
        let (package_id, manifest_path, diagnostic) = compiler_message(UNUSED_VARIABLE);
        assert_eq!(package_id, "path+file:///work/app#0.1.0");
        assert_eq!(manifest_path.as_deref(), Some(Path::new("/work/app/Cargo.toml")));
        assert_eq!(diagnostic.level, Level::Warning);
        assert_eq!(diagnostic.message, "unused variable: `x`");
        assert_eq!(diagnostic.code.as_deref(), Some("unused_variables"));
        assert_eq!(
            diagnostic.primary_span(),
            Some(&DiagnosticSpan {
                file_name: "src/main.rs".into(),
                line_start: 2,
                line_end: 2,
                column_start: 9,
                column_end: 10,
                is_primary: true,
                label: None,
                suggested_replacement: None,
            })
        );
        // ANSI escapes are kept: the UI strips or renders them.
        let rendered = diagnostic.rendered.as_deref().unwrap();
        assert!(rendered.starts_with("\u{1b}[0m\u{1b}[1m\u{1b}[33mwarning"));
        assert!(rendered.ends_with("\n\n"));

        assert_eq!(diagnostic.children.len(), 2);
        let note = &diagnostic.children[0];
        assert_eq!((note.level, note.message.as_str()), (Level::Note, "`#[warn(unused_variables)]` on by default"));
        assert!(note.spans.is_empty() && note.code.is_none() && note.rendered.is_none());
        let help = &diagnostic.children[1];
        assert_eq!(help.level, Level::Help);
        assert_eq!(help.spans[0].suggested_replacement.as_deref(), Some("_x"));
        assert_eq!((help.spans[0].column_start, help.spans[0].column_end), (9, 10));
    }

    #[test]
    fn error_code_labels_and_primary_span() {
        let (_, _, diagnostic) = compiler_message(MISMATCHED_TYPES);
        assert_eq!(diagnostic.level, Level::Error);
        assert_eq!(diagnostic.code.as_deref(), Some("E0308"));
        assert_eq!(diagnostic.spans.len(), 2);
        assert_eq!(diagnostic.spans[0].label.as_deref(), Some("expected due to this"));
        let primary = diagnostic.primary_span().unwrap();
        assert_eq!(primary.label.as_deref(), Some("expected `u32`, found `&str`"));
        assert_eq!((primary.line_start, primary.column_start, primary.column_end), (2, 20, 23));
    }

    #[test]
    fn levels() {
        for (raw, level) in [
            ("error", Level::Error),
            ("warning", Level::Warning),
            ("note", Level::Note),
            ("help", Level::Help),
            ("failure-note", Level::FailureNote),
            ("error: internal compiler error", Level::Ice),
            ("something new", Level::Note),
            ("", Level::Note),
        ] {
            assert_eq!(parse_level(raw), level, "{raw:?}");
        }
        let ice = r#"{"reason":"compiler-message","package_id":"p","message":{"message":"unexpected panic","level":"error: internal compiler error","spans":[],"children":[],"code":null,"rendered":"error: internal compiler error: unexpected panic\n"}}"#;
        assert_eq!(compiler_message(ice).2.level, Level::Ice);
    }

    #[test]
    fn lenient_fields() {
        // Bare-string code, nulls where lists are expected, no manifest_path, missing span fields.
        let line = r#"{"reason":"compiler-message","package_id":null,"message":{"message":"m","level":"warning","code":"W1","spans":[{"file_name":"a.rs","is_primary":true}],"children":null}}"#;
        let (package_id, manifest_path, diagnostic) = compiler_message(line);
        assert_eq!(package_id, "");
        assert_eq!(manifest_path, None);
        assert_eq!(diagnostic.code.as_deref(), Some("W1"));
        assert_eq!(diagnostic.spans[0].line_start, 0);
        assert!(diagnostic.children.is_empty());
        let empty_code = r#"{"reason":"compiler-message","message":{"message":"m","level":"note","code":{"code":""}}}"#;
        assert_eq!(compiler_message(empty_code).2.code, None);
    }

    #[test]
    fn children_depth_is_capped() {
        let mut nested = serde_json::json!({"message": "leaf", "level": "note"});
        for i in 0..20 {
            nested = serde_json::json!({"message": format!("level {i}"), "level": "note", "children": [nested]});
        }
        let line = serde_json::json!({"reason": "compiler-message", "package_id": "p", "message": nested}).to_string();
        let (_, _, diagnostic) = compiler_message(&line);
        let mut depth = 0;
        let mut current = &diagnostic;
        while let Some(child) = current.children.first() {
            depth += 1;
            current = child;
        }
        assert_eq!(depth, MAX_DIAGNOSTIC_DEPTH);
    }

    #[test]
    fn hostile_nesting_is_rejected_not_overflowed() {
        let line = format!(r#"{{"reason":"compiler-message","message":{}"#, "[".repeat(200_000));
        assert_eq!(parse_message(&line), None);
        // Well-formed but 10 000 diagnostics deep.
        let depth = 10_000;
        let nested = format!(
            r#"{}{{"message":"leaf"}}{}"#,
            r#"{"message":"m","level":"note","children":["#.repeat(depth),
            "]}".repeat(depth)
        );
        assert_eq!(parse_message(&format!(r#"{{"reason":"compiler-message","message":{nested}}}"#)), None);
    }

    #[test]
    fn artifacts() {
        assert_eq!(
            parse_message(ARTIFACT_BIN),
            Some(CargoMessage::CompilerArtifact {
                package_id: "path+file:///work/app#0.1.0".into(),
                target_name: "app".into(),
                executable: Some(PathBuf::from("/work/app/target/debug/app")),
                fresh: false,
            })
        );
        assert_eq!(
            parse_message(ARTIFACT_LIB),
            Some(CargoMessage::CompilerArtifact {
                package_id: "registry+https://github.com/rust-lang/crates.io-index#libc@0.2.150".into(),
                target_name: "libc".into(),
                executable: None,
                fresh: true,
            })
        );
    }

    #[test]
    fn build_script_and_finished() {
        assert_eq!(
            parse_message(BUILD_SCRIPT),
            Some(CargoMessage::BuildScriptExecuted {
                package_id: "registry+https://github.com/rust-lang/crates.io-index#libc@0.2.150".into()
            })
        );
        assert_eq!(
            parse_message(r#"{"reason":"build-finished","success":true}"#),
            Some(CargoMessage::BuildFinished { success: true })
        );
        assert_eq!(
            parse_message("  {\"reason\":\"build-finished\",\"success\":false}\r"),
            Some(CargoMessage::BuildFinished { success: false })
        );
    }

    #[test]
    fn not_messages() {
        for line in [
            "",
            "   Compiling app v0.1.0 (/work/app)",
            "running 2 tests",
            "{",
            "{not json}",
            "[1, 2]",
            r#"{"success":true}"#,
            r#"{"reason":42}"#,
            r#"{"reason":"timing-info","package_id":"p","duration":0.1}"#,
            r#"{"reason":"build-finished"}"#,
            r#"{"reason":"build-finished","success":"yes"}"#,
            r#"{"reason":"compiler-message","package_id":"p"}"#,
            r#"{"reason":"compiler-message","message":"just a string"}"#,
            r#"{"reason":"compiler-artifact","fresh":"no"}"#,
            r#"{"reason":"compiler-message","message":{"spans":[{"line_start":-1}]}}"#,
        ] {
            assert_eq!(parse_message(line), None, "{line:?}");
        }
    }
}
