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
    fn params_are_a_text_document_position() {
        let v = serde_json::to_value(position(Path::new("/w/src/lib.rs"), 3, 7).unwrap()).unwrap();
        assert_eq!(
            v,
            json!({ "textDocument": { "uri": "file:///w/src/lib.rs" }, "position": { "line": 3, "character": 7 } })
        );
    }
}
