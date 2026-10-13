//! Refactoring and hierarchy requests against the fake server: methods, params, and that hierarchy
//! items and code actions round-trip untouched (including the server's `data`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use codecraft_lsp::ServerEvent;
use codecraft_lsp::lsp_types::{Position, Range};
use common::*;
use serde_json::json;

#[test]
fn hierarchy_requests_send_items_verbatim() {
    let (client, events, mut server) = start_initialized();
    let id = client.prepare_call_hierarchy(&file(), 4, 7).unwrap();
    let msg = server.expect("textDocument/prepareCallHierarchy");
    assert_eq!(msg["params"]["position"], json!({ "line": 4, "character": 7 }));
    assert_eq!(msg["params"]["textDocument"]["uri"], uri_of(&file()));
    let item = json!({
        "name": "greet", "kind": 12, "uri": uri_of(&file()),
        "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 3, "character": 1 } },
        "selectionRange": { "start": { "line": 1, "character": 3 }, "end": { "line": 1, "character": 8 } },
        "data": { "opaque": [1, 2, 3] },
    });
    server.reply(&msg, json!([item.clone()]));
    match next_event(&events) {
        ServerEvent::Response { id: got, result, .. } => {
            assert_eq!(got, id);
            assert_eq!(result.unwrap(), json!([item.clone()]));
        }
        other => panic!("expected a response, got {other:?}"),
    }
    client.call_hierarchy_incoming(item.clone()).unwrap();
    assert_eq!(server.expect("callHierarchy/incomingCalls")["params"]["item"], item);
    client.call_hierarchy_outgoing(item.clone()).unwrap();
    assert_eq!(server.expect("callHierarchy/outgoingCalls")["params"]["item"], item);
    client.prepare_type_hierarchy(&file(), 0, 1).unwrap();
    server.expect("textDocument/prepareTypeHierarchy");
    client.type_hierarchy_supertypes(item.clone()).unwrap();
    assert_eq!(server.expect("typeHierarchy/supertypes")["params"]["item"], item);
    client.type_hierarchy_subtypes(item.clone()).unwrap();
    assert_eq!(server.expect("typeHierarchy/subtypes")["params"]["item"], item);
}

#[test]
fn navigation_and_code_action_requests() {
    let (client, _events, server) = start_initialized();
    client.declaration(&file(), 2, 5).unwrap();
    assert_eq!(server.expect("textDocument/declaration")["params"]["position"], json!({ "line": 2, "character": 5 }));
    client.parent_module(&file(), 0, 0).unwrap();
    server.expect("experimental/parentModule");
    let range = Range { start: Position::new(1, 2), end: Position::new(1, 9) };
    client.code_action_only(&file(), range, Vec::new(), &["refactor.extract", "refactor.inline"]).unwrap();
    let msg = server.expect("textDocument/codeAction");
    assert_eq!(msg["params"]["context"]["only"], json!(["refactor.extract", "refactor.inline"]));
    assert_eq!(msg["params"]["range"]["end"], json!({ "line": 1, "character": 9 }));
    let action = json!({ "title": "Extract into variable", "kind": "refactor.extract", "data": { "id": "x" } });
    client.resolve_code_action(action.clone()).unwrap();
    assert_eq!(server.expect("codeAction/resolve")["params"], action);
}

#[test]
fn file_rename_hooks() {
    let (client, _events, server) = start_initialized();
    let old = root().join("src/foo.rs");
    let new = root().join("src/bar.rs");
    client.will_rename_files(&[(old.clone(), new.clone())]).unwrap();
    let msg = server.expect("workspace/willRenameFiles");
    assert_eq!(msg["params"]["files"][0]["oldUri"], uri_of(&old));
    assert_eq!(msg["params"]["files"][0]["newUri"], uri_of(&new));
    assert!(msg.get("id").is_some());
    client.did_rename_files(&[(old, new)]).unwrap();
    let note = server.expect("workspace/didRenameFiles");
    assert!(note.get("id").is_none());
}
