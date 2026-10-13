//! Incremental document sync: the sync kind a server announces and ranged `didChange`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use codecraft_lsp::lsp_types::{
    Position, Range, ServerCapabilities, TextDocumentContentChangeEvent, TextDocumentSyncCapability,
    TextDocumentSyncKind, TextDocumentSyncOptions,
};
use codecraft_lsp::{ServerEvent, sync_kind};
use common::*;
use serde_json::json;

#[test]
fn sync_kind_reads_both_capability_shapes() {
    let mut caps = ServerCapabilities::default();
    assert_eq!(sync_kind(&caps), TextDocumentSyncKind::NONE);
    caps.text_document_sync = Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::INCREMENTAL));
    assert_eq!(sync_kind(&caps), TextDocumentSyncKind::INCREMENTAL);
    caps.text_document_sync = Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
        change: Some(TextDocumentSyncKind::FULL),
        ..Default::default()
    }));
    assert_eq!(sync_kind(&caps), TextDocumentSyncKind::FULL);
    caps.text_document_sync = Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions::default()));
    assert_eq!(sync_kind(&caps), TextDocumentSyncKind::NONE);
}

fn start_with_sync(sync: serde_json::Value) -> (codecraft_lsp::LspClient, FakeServer) {
    let (client, events, mut server) = start();
    let init = server.expect("initialize");
    server.reply(&init, json!({ "capabilities": { "textDocumentSync": sync } }));
    server.expect("initialized");
    match next_event(&events) {
        ServerEvent::Initialized(_) => {}
        other => panic!("expected Initialized, got {other:?}"),
    }
    (client, server)
}

#[test]
fn incremental_servers_get_ranged_changes() {
    let (client, server) = start_with_sync(json!({ "openClose": true, "change": 2 }));
    assert!(client.incremental_sync());
    let range = Range::new(Position::new(0, 3), Position::new(0, 7));
    client
        .did_change(
            &file(),
            3,
            vec![TextDocumentContentChangeEvent { range: Some(range), range_length: None, text: "start".into() }],
        )
        .unwrap();
    let change = server.expect("textDocument/didChange");
    assert_eq!(
        change["params"],
        json!({
            "textDocument": { "uri": uri_of(&file()), "version": 3 },
            "contentChanges": [{
                "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 7 } },
                "text": "start",
            }],
        })
    );
}

#[test]
fn full_sync_servers_are_not_incremental() {
    let (client, _server) = start_with_sync(json!(1));
    assert!(!client.incremental_sync());
    let (client, _server) = start_with_sync(json!({ "openClose": true }));
    assert!(!client.incremental_sync());
}
