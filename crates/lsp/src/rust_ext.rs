//! rust-analyzer protocol extensions used by the Rust tools (M12 track RS): `rust-analyzer/expandMacro`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{LspClient, RequestId, Result, position};

/// The answer to `rust-analyzer/expandMacro`: the macro's name and its recursive expansion.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpandedMacro {
    pub name: String,
    pub expansion: String,
}

impl LspClient {
    /// `rust-analyzer/expandMacro` at a position: the macro call under it (`name!(…)`, a derive or
    /// an attribute macro) expanded recursively. The server answers `null` off a macro call.
    pub fn expand_macro(&self, path: &Path, line: u32, character: u32) -> Result<RequestId> {
        self.request_raw("rust-analyzer/expandMacro", serde_json::to_value(position(path, line, character)?)?)
    }
}

/// Decode an `expandMacro` result (`None` for `null` or an unexpected shape).
pub fn decode_expanded_macro(value: serde_json::Value) -> Option<ExpandedMacro> {
    serde_json::from_value::<Option<ExpandedMacro>>(value).ok().flatten()
}

/// A test from `experimental/runnables` (M13 track RC): its path as libtest names it
/// (`tests::adds`) and where it is declared (0-based line, UTF-16 column of its name).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRunnable {
    /// The libtest name (`module::test`); for a test module, the module path.
    pub test_path: String,
    /// A `#[test]` function (false: a test module or doc test).
    pub is_test: bool,
    /// `--package` from the runnable's cargo arguments.
    pub package: Option<String>,
    pub path: std::path::PathBuf,
    pub line: u32,
    pub utf16_column: u32,
}

impl LspClient {
    /// `experimental/runnables` for a file: rust-analyzer's runnables (binaries, tests, test
    /// modules, doc tests, benches) with their locations.
    pub fn runnables(&self, path: &Path) -> Result<RequestId> {
        let uri = crate::path_to_uri(path)?;
        self.request_raw("experimental/runnables", serde_json::json!({ "textDocument": { "uri": uri.as_str() } }))
    }
}

/// The tests and test modules of a `runnables` answer (other runnables and odd shapes skipped).
pub fn decode_test_runnables(value: serde_json::Value) -> Vec<TestRunnable> {
    use serde_json::Value;
    let Value::Array(items) = value else { return Vec::new() };
    let mut out = Vec::new();
    for r in items.iter().take(10_000) {
        let label = r.get("label").and_then(Value::as_str).unwrap_or("");
        let (is_test, kind) = if label.starts_with("test-mod ") {
            (false, "test-mod")
        } else if label.starts_with("test ") {
            (true, "test")
        } else {
            continue;
        };
        let args = r.get("args");
        let exec = args.and_then(|a| a.get("executableArgs")).and_then(Value::as_array);
        let test_path = exec
            .and_then(|e| e.first())
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| label.strip_prefix(kind).map(|l| l.trim().to_string()))
            .unwrap_or_default();
        if test_path.is_empty() {
            continue;
        }
        let cargo = args.and_then(|a| a.get("cargoArgs")).and_then(Value::as_array);
        let package = cargo.and_then(|c| {
            let words: Vec<&str> = c.iter().filter_map(Value::as_str).collect();
            words
                .iter()
                .position(|w| *w == "--package" || *w == "-p")
                .and_then(|i| words.get(i + 1))
                .map(|w| w.to_string())
        });
        let Some(location) = r.get("location") else { continue };
        let Some(uri) = location.get("targetUri").and_then(Value::as_str) else { continue };
        let Ok(uri) = uri.parse::<lsp_types::Uri>() else { continue };
        let Some(path) = crate::uri_to_path(&uri) else { continue };
        let range = location.get("targetSelectionRange").or_else(|| location.get("targetRange"));
        let start = range.and_then(|r| r.get("start"));
        let line = start.and_then(|s| s.get("line")).and_then(Value::as_u64).and_then(|l| u32::try_from(l).ok());
        let col = start.and_then(|s| s.get("character")).and_then(Value::as_u64).and_then(|c| u32::try_from(c).ok());
        let (Some(line), Some(utf16_column)) = (line, col) else { continue };
        out.push(TestRunnable { test_path, is_test, package, path, line, utf16_column });
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_expansions_and_null() {
        let e = decode_expanded_macro(json!({ "name": "vec", "expansion": "<[_]>::into_vec(…)" })).unwrap();
        assert_eq!(e.name, "vec");
        assert!(e.expansion.contains("into_vec"));
        assert_eq!(decode_expanded_macro(json!(null)), None);
        assert_eq!(decode_expanded_macro(json!({ "name": 3 })), None);
        assert_eq!(decode_expanded_macro(json!("x")), None);
    }

    #[cfg(unix)]
    #[test]
    fn decodes_test_runnables() {
        let loc = |line: u32, ch: u32| {
            json!({ "targetUri": "file:///w/src/lib.rs",
                    "targetRange": { "start": { "line": line, "character": 0 }, "end": { "line": line + 3, "character": 1 } },
                    "targetSelectionRange": { "start": { "line": line, "character": ch }, "end": { "line": line, "character": ch + 4 } } })
        };
        let answer = json!([
            { "label": "test-mod tests", "kind": "cargo", "location": loc(4, 4),
              "args": { "cargoArgs": ["test", "--package", "demo", "--lib"], "executableArgs": ["tests", "--show-output"] } },
            { "label": "test tests::adds", "kind": "cargo", "location": loc(8, 7),
              "args": { "cargoArgs": ["test", "--package", "demo", "--lib"], "executableArgs": ["tests::adds", "--exact", "--show-output"] } },
            { "label": "run demo", "kind": "cargo", "location": loc(0, 3), "args": { "cargoArgs": ["run"] } },
            { "label": "test broken", "kind": "cargo" },
            "junk"
        ]);
        let tests = decode_test_runnables(answer);
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0].test_path, "tests");
        assert!(!tests[0].is_test);
        assert_eq!((tests[1].test_path.as_str(), tests[1].line, tests[1].utf16_column), ("tests::adds", 8, 7));
        assert_eq!(tests[1].package.as_deref(), Some("demo"));
        assert_eq!(tests[1].path, Path::new("/w/src/lib.rs"));
        assert!(decode_test_runnables(json!(null)).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn params_are_a_text_document_position() {
        let v = serde_json::to_value(position(Path::new("/w/src/lib.rs"), 3, 7).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({ "textDocument": { "uri": "file:///w/src/lib.rs" }, "position": { "line": 3, "character": 7 } })
        );
    }
}
