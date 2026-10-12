//! The client against a fake server over in-memory pipes: handshake, queueing, correlation,
//! notifications, server requests, document sync, cancel, shutdown and stream loss.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use codecraft_lsp::lsp_types::{self, HoverProviderCapability, MessageType, Position, Range};
use codecraft_lsp::{LspClient, LspError, ProgressUpdate, RequestId, ResponseError, ServerEvent};
use common::*;
use crossbeam_channel::RecvTimeoutError;
use serde_json::{Value, json};

#[test]
fn client_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LspClient>();
}

#[test]
fn handshake_sends_initialize_then_initialized() {
    let (client, events, mut server) = start_with(Some(json!({ "cargo": { "features": "all" } })), None);
    let init = server.expect("initialize");
    let params = &init["params"];
    assert_eq!(params["processId"], std::process::id());
    assert_eq!(params["clientInfo"]["name"], "CodeCraft");
    assert_eq!(params["rootUri"], uri_of(&root()));
    assert_eq!(params["rootPath"], root().to_string_lossy().as_ref());
    assert_eq!(params["workspaceFolders"], json!([{ "uri": uri_of(&root()), "name": "my crate" }]));
    assert_eq!(params["initializationOptions"], json!({ "cargo": { "features": "all" } }));
    let caps = &params["capabilities"];
    assert_eq!(caps["general"]["positionEncodings"], json!(["utf-16"]));
    assert_eq!(caps["textDocument"]["completion"]["completionItem"]["snippetSupport"], true);
    assert_eq!(caps["window"]["workDoneProgress"], true);
    // The whole thing is a valid InitializeParams.
    serde_json::from_value::<lsp_types::InitializeParams>(params.clone()).unwrap();
    assert!(!client.is_initialized());
    assert!(client.capabilities().is_none());

    server.reply(
        &init,
        json!({
            "capabilities": { "hoverProvider": true, "completionProvider": { "resolveProvider": true } },
            "serverInfo": { "name": "fake" },
        }),
    );
    let initialized = server.expect("initialized");
    assert_eq!(initialized["params"], json!({}));
    assert!(initialized.get("id").is_none());
    match next_event(&events) {
        ServerEvent::Initialized(caps) => assert_eq!(caps.hover_provider, Some(HoverProviderCapability::Simple(true))),
        other => panic!("expected Initialized, got {other:?}"),
    }
    assert!(client.is_initialized());
    assert!(client.capabilities().unwrap().completion_provider.is_some());
    assert_no_event(&events);
}

#[test]
fn messages_before_initialization_are_queued_in_order() {
    let (client, events, mut server) = start();
    let init = server.expect("initialize");
    let hover = client.hover(&file(), 3, 7).unwrap();
    client.did_open(&file(), "rust", 1, "fn main() {}").unwrap();
    let definition = client.definition(&file(), 0, 3).unwrap();
    assert!(hover < definition);
    server.assert_quiet();

    server.reply(&init, json!({ "capabilities": {} }));
    server.expect("initialized");
    let message = server.expect("textDocument/hover");
    assert_eq!(message["id"], hover.0);
    assert_eq!(message["params"]["position"], json!({ "line": 3, "character": 7 }));
    assert_eq!(message["params"]["textDocument"]["uri"], uri_of(&file()));
    server.expect("textDocument/didOpen");
    assert_eq!(server.expect("textDocument/definition")["id"], definition.0);
    assert!(matches!(next_event(&events), ServerEvent::Initialized(_)));

    // Once initialized, requests go straight out.
    let references = client.references(&file(), 1, 1, true).unwrap();
    let message = server.expect("textDocument/references");
    assert_eq!(message["id"], references.0);
    assert_eq!(message["params"]["context"]["includeDeclaration"], true);
}

#[test]
fn responses_are_correlated_with_their_requests() {
    let (client, events, mut server) = start_initialized();
    let hover = client.hover(&file(), 0, 0).unwrap();
    let completion = client.completion(&file(), 0, 0).unwrap();
    let hover_request = server.expect("textDocument/hover");
    let completion_request = server.expect("textDocument/completion");

    // Answered out of order.
    server.reply(&completion_request, json!([{ "label": "foo" }]));
    server.reply_error(&hover_request, -32801, "content modified");
    match next_event(&events) {
        ServerEvent::Response { id, method, result } => {
            assert_eq!(id, completion);
            assert_eq!(method, "textDocument/completion");
            let items: Vec<lsp_types::CompletionItem> = codecraft_lsp::decode(result.unwrap()).unwrap();
            assert_eq!(items[0].label, "foo");
        }
        other => panic!("expected a completion response, got {other:?}"),
    }
    match next_event(&events) {
        ServerEvent::Response { id, method, result } => {
            assert_eq!(id, hover);
            assert_eq!(method, "textDocument/hover");
            assert_eq!(result, Err(ResponseError { code: -32801, message: "content modified".to_owned() }));
        }
        other => panic!("expected a hover response, got {other:?}"),
    }
}

#[test]
fn raw_and_typed_requests_share_the_id_sequence() {
    let (client, events, mut server) = start_initialized();
    let first = client.request_raw("rust-analyzer/analyzerStatus", json!({})).unwrap();
    let second = client.request::<lsp_types::request::WorkspaceSymbolRequest>(lsp_types::WorkspaceSymbolParams {
        query: "x".to_owned(),
        ..Default::default()
    });
    let second = second.unwrap();
    assert!(second.0 > first.0);
    let message = server.expect("rust-analyzer/analyzerStatus");
    server.reply(&message, json!("ok"));
    match next_event(&events) {
        ServerEvent::Response { id, method, result } => {
            assert_eq!((id, method.as_str(), result), (first, "rust-analyzer/analyzerStatus", Ok(json!("ok"))));
        }
        other => panic!("unexpected {other:?}"),
    }
    server.expect("workspace/symbol");
}

#[test]
fn responses_to_unknown_ids_are_ignored() {
    let (_client, events, mut server) = start_initialized();
    server.send(json!({ "jsonrpc": "2.0", "id": 999, "result": null }));
    server.send(json!({ "jsonrpc": "2.0", "id": "not-a-number", "result": null }));
    assert_no_event(&events);
}

#[test]
fn error_response_without_id_is_reported() {
    let (_client, events, mut server) = start_initialized();
    server.send(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }));
    expect_error(&events, "parse error");
}

#[test]
fn notifications_become_events() {
    let (_client, events, mut server) = start_initialized();
    server.send(json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": uri_of(&file()),
            "version": 3,
            "diagnostics": [{
                "range": { "start": { "line": 0, "character": 1 }, "end": { "line": 0, "character": 4 } },
                "severity": 1,
                "message": "boom",
            }],
        },
    }));
    server.send(json!({ "jsonrpc": "2.0", "method": "window/showMessage", "params": { "type": 1, "message": "hi" } }));
    server.send(json!({ "jsonrpc": "2.0", "method": "window/logMessage", "params": { "type": 4, "message": "log" } }));
    server.send(json!({ "jsonrpc": "2.0", "method": "experimental/serverStatus", "params": { "quiescent": true } }));
    server.send(json!({ "jsonrpc": "2.0", "method": "custom/noParams" }));
    server.send(json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": 5 }));

    match next_event(&events) {
        ServerEvent::Diagnostics(params) => {
            assert_eq!(codecraft_lsp::uri_to_path(&params.uri), Some(file()));
            assert_eq!(params.version, Some(3));
            assert_eq!(params.diagnostics[0].message, "boom");
            assert_eq!(params.diagnostics[0].severity, Some(lsp_types::DiagnosticSeverity::ERROR));
        }
        other => panic!("expected diagnostics, got {other:?}"),
    }
    match next_event(&events) {
        ServerEvent::ShowMessage { level, message } => {
            assert_eq!((level, message.as_str()), (MessageType::ERROR, "hi"))
        }
        other => panic!("expected ShowMessage, got {other:?}"),
    }
    match next_event(&events) {
        ServerEvent::Log { level, message } => assert_eq!((level, message.as_str()), (MessageType::LOG, "log")),
        other => panic!("expected Log, got {other:?}"),
    }
    match next_event(&events) {
        ServerEvent::Notification { method, params } => {
            assert_eq!(method, "experimental/serverStatus");
            assert_eq!(params, json!({ "quiescent": true }));
        }
        other => panic!("expected a notification, got {other:?}"),
    }
    match next_event(&events) {
        ServerEvent::Notification { method, params } => {
            assert_eq!(method, "custom/noParams");
            assert_eq!(params, Value::Null);
        }
        other => panic!("expected a notification, got {other:?}"),
    }
    expect_error(&events, "publishDiagnostics");
    assert_no_event(&events);
}

#[test]
fn work_done_progress_becomes_progress_events() {
    let (_client, events, mut server) = start_initialized();
    let progress = |token: Value, value: Value| json!({ "jsonrpc": "2.0", "method": "$/progress", "params": { "token": token, "value": value } });
    server.send(progress(
        json!("rustAnalyzer/Indexing"),
        json!({ "kind": "begin", "title": "Indexing", "percentage": 0 }),
    ));
    server.send(progress(
        json!("rustAnalyzer/Indexing"),
        json!({ "kind": "report", "message": "3/10", "percentage": 30 }),
    ));
    server.send(progress(json!(7), json!({ "kind": "begin", "title": "Building", "cancellable": false })));
    server.send(progress(json!("rustAnalyzer/Indexing"), json!({ "kind": "end", "message": "done" })));
    server.send(progress(json!(7), json!({ "kind": "end" })));
    // A partial-result payload is not work-done progress.
    server.send(progress(json!("partial"), json!([1, 2, 3])));

    let next_progress = || match next_event(&events) {
        ServerEvent::Progress(update) => update,
        other => panic!("expected progress, got {other:?}"),
    };
    let update =
        |token: &str, title: Option<&str>, message: Option<&str>, percentage: Option<u32>, done: bool| ProgressUpdate {
            token: token.to_owned(),
            title: title.map(str::to_owned),
            message: message.map(str::to_owned),
            percentage,
            done,
        };
    assert_eq!(next_progress(), update("rustAnalyzer/Indexing", Some("Indexing"), None, Some(0), false));
    assert_eq!(next_progress(), update("rustAnalyzer/Indexing", Some("Indexing"), Some("3/10"), Some(30), false));
    assert_eq!(next_progress(), update("7", Some("Building"), None, None, false));
    assert_eq!(next_progress(), update("rustAnalyzer/Indexing", Some("Indexing"), Some("done"), None, true));
    assert_eq!(next_progress(), update("7", Some("Building"), None, None, true));
    match next_event(&events) {
        ServerEvent::Notification { method, .. } => assert_eq!(method, "$/progress"),
        other => panic!("expected a plain notification, got {other:?}"),
    }
}

#[test]
fn common_server_requests_are_answered_automatically() {
    let settings = json!({ "checkOnSave": true });
    let (_client, events, mut server) = start_with(None, Some(settings.clone()));
    server.handshake();
    assert!(matches!(next_event(&events), ServerEvent::Initialized(_)));

    let items = json!({ "items": [{ "section": "rust-analyzer" }, { "section": "other" }] });
    server.send(json!({ "jsonrpc": "2.0", "id": 10, "method": "workspace/configuration", "params": items }));
    let reply = server.recv();
    assert_eq!(reply["id"], 10);
    assert_eq!(reply["result"], json!([settings, settings]));

    for (id, method) in [
        (json!("tok"), "window/workDoneProgress/create"),
        (json!(11), "client/registerCapability"),
        (json!(12), "client/unregisterCapability"),
    ] {
        server.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": {} }));
        let reply = server.recv();
        assert_eq!(reply["id"], id);
        assert_eq!(reply.get("result"), Some(&Value::Null), "{method} reply {reply}");
        assert!(reply.get("error").is_none());
    }

    server.send(json!({ "jsonrpc": "2.0", "id": 13, "method": "workspace/workspaceFolders" }));
    let reply = server.recv();
    assert_eq!(reply["result"], json!([{ "uri": uri_of(&root()), "name": "my crate" }]));
    assert_no_event(&events);
}

#[test]
fn configuration_is_answered_even_before_initialization() {
    let (_client, events, mut server) = start();
    let init = server.expect("initialize");
    server.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "workspace/configuration", "params": { "items": [{}] } }));
    let reply = server.recv();
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"], json!([null]));
    server.reply(&init, json!({ "capabilities": {} }));
    server.expect("initialized");
    assert!(matches!(next_event(&events), ServerEvent::Initialized(_)));
}

#[test]
fn other_server_requests_surface_and_can_be_answered() {
    let (client, events, mut server) = start_initialized();
    server.send(json!({ "jsonrpc": "2.0", "id": 21, "method": "workspace/applyEdit", "params": { "edit": {} } }));
    let id = match next_event(&events) {
        ServerEvent::ServerRequest { id, method, params } => {
            assert_eq!(method, "workspace/applyEdit");
            assert_eq!(params, json!({ "edit": {} }));
            id
        }
        other => panic!("expected a server request, got {other:?}"),
    };
    assert_eq!(id, json!(21));
    client.respond(id, Ok(json!({ "applied": true }))).unwrap();
    assert_eq!(server.recv(), json!({ "jsonrpc": "2.0", "id": 21, "result": { "applied": true } }));

    server.send(json!({ "jsonrpc": "2.0", "id": "s1", "method": "window/showMessageRequest", "params": { "type": 3, "message": "?" } }));
    let id = match next_event(&events) {
        ServerEvent::ServerRequest { id, .. } => id,
        other => panic!("expected a server request, got {other:?}"),
    };
    client.respond(id, Err(ResponseError { code: -32800, message: "dismissed".to_owned() })).unwrap();
    assert_eq!(
        server.recv(),
        json!({ "jsonrpc": "2.0", "id": "s1", "error": { "code": -32800, "message": "dismissed" } })
    );
}

#[test]
fn cancel_sends_cancel_request() {
    let (client, events, mut server) = start_initialized();
    let id = client.hover(&file(), 0, 0).unwrap();
    let request = server.expect("textDocument/hover");
    client.cancel(id).unwrap();
    let cancel = server.expect("$/cancelRequest");
    assert_eq!(cancel["params"], json!({ "id": id.0 }));
    assert!(cancel.get("id").is_none());
    server.reply_error(&request, -32800, "cancelled");
    match next_event(&events) {
        ServerEvent::Response { id: got, result: Err(error), .. } => {
            assert_eq!(got, id);
            assert_eq!(error.code, -32800);
        }
        other => panic!("expected a cancelled response, got {other:?}"),
    }
}

#[test]
fn document_sync_notifications() {
    let (client, _events, server) = start_initialized();
    let uri = uri_of(&file());
    client.did_open(&file(), "rust", 1, "fn main() {}").unwrap();
    client.did_change_full(&file(), 2, "fn main() { }").unwrap();
    client.did_save(&file()).unwrap();
    client.did_close(&file()).unwrap();

    let open = server.expect("textDocument/didOpen");
    assert_eq!(
        open["params"],
        json!({ "textDocument": { "uri": uri, "languageId": "rust", "version": 1, "text": "fn main() {}" } })
    );
    let change = server.expect("textDocument/didChange");
    assert_eq!(
        change["params"],
        json!({ "textDocument": { "uri": uri, "version": 2 }, "contentChanges": [{ "text": "fn main() { }" }] })
    );
    assert_eq!(server.expect("textDocument/didSave")["params"], json!({ "textDocument": { "uri": uri } }));
    assert_eq!(server.expect("textDocument/didClose")["params"], json!({ "textDocument": { "uri": uri } }));
}

#[test]
fn relative_document_paths_are_rejected() {
    let (client, _events, server) = start_initialized();
    let relative = PathBuf::from("src/main.rs");
    assert!(matches!(client.hover(&relative, 0, 0), Err(LspError::BadUri(_))));
    assert!(matches!(client.did_open(&relative, "rust", 1, ""), Err(LspError::BadUri(_))));
    server.assert_quiet();
}

#[test]
fn typed_helpers_send_the_right_methods_and_params() {
    let (client, events, mut server) = start_initialized();
    let f = file();
    let range = Range::new(Position::new(1, 2), Position::new(3, 4));
    let diagnostic = lsp_types::Diagnostic::new_simple(range, "unused".to_owned());
    let calls: Vec<(&str, RequestId)> = vec![
        ("textDocument/hover", client.hover(&f, 1, 2).unwrap()),
        ("textDocument/completion", client.completion(&f, 1, 2).unwrap()),
        ("textDocument/signatureHelp", client.signature_help(&f, 1, 2).unwrap()),
        ("textDocument/definition", client.definition(&f, 1, 2).unwrap()),
        ("textDocument/typeDefinition", client.type_definition(&f, 1, 2).unwrap()),
        ("textDocument/implementation", client.implementation(&f, 1, 2).unwrap()),
        ("textDocument/references", client.references(&f, 1, 2, false).unwrap()),
        ("textDocument/documentHighlight", client.document_highlight(&f, 1, 2).unwrap()),
        ("textDocument/documentSymbol", client.document_symbol(&f).unwrap()),
        ("workspace/symbol", client.workspace_symbol("Foo").unwrap()),
        ("textDocument/formatting", client.formatting(&f, 4, true).unwrap()),
        ("textDocument/prepareRename", client.prepare_rename(&f, 1, 2).unwrap()),
        ("textDocument/rename", client.rename(&f, 1, 2, "bar").unwrap()),
        ("textDocument/codeAction", client.code_action(&f, range, vec![diagnostic]).unwrap()),
        ("textDocument/inlayHint", client.inlay_hints(&f, range).unwrap()),
        ("textDocument/semanticTokens/full", client.semantic_tokens_full(&f).unwrap()),
        ("textDocument/selectionRange", client.selection_range(&f, vec![Position::new(1, 2)]).unwrap()),
    ];
    assert!(calls.windows(2).all(|pair| pair[0].1 < pair[1].1), "ids increase");

    let uri = uri_of(&f);
    let at = json!({ "line": 1, "character": 2 });
    let range_json = json!({ "start": { "line": 1, "character": 2 }, "end": { "line": 3, "character": 4 } });
    for (method, id) in &calls {
        let message = server.expect(method);
        assert_eq!(message["id"], id.0);
        let params = &message["params"];
        match *method {
            "workspace/symbol" => assert_eq!(params["query"], "Foo"),
            "textDocument/documentSymbol" | "textDocument/semanticTokens/full" => {
                assert_eq!(params, &json!({ "textDocument": { "uri": uri } }));
            }
            "textDocument/formatting" => {
                assert_eq!(params["options"], json!({ "tabSize": 4, "insertSpaces": true }));
            }
            "textDocument/codeAction" => {
                assert_eq!(params["range"], range_json);
                assert_eq!(params["context"]["diagnostics"][0]["message"], "unused");
                assert_eq!(params["context"]["triggerKind"], 1);
            }
            "textDocument/inlayHint" => assert_eq!(params["range"], range_json),
            "textDocument/selectionRange" => assert_eq!(params["positions"], json!([at])),
            _ => {
                assert_eq!(params["textDocument"]["uri"], uri, "{method}");
                assert_eq!(params["position"], at, "{method}");
            }
        }
        match *method {
            "textDocument/references" => assert_eq!(params["context"]["includeDeclaration"], false),
            "textDocument/rename" => assert_eq!(params["newName"], "bar"),
            "textDocument/completion" => assert_eq!(params["context"]["triggerKind"], 1),
            _ => {}
        }
        server.reply(&message, Value::Null);
    }
    for (method, id) in &calls {
        match next_event(&events) {
            ServerEvent::Response { id: got, method: got_method, result } => {
                assert_eq!((got, got_method.as_str(), result), (*id, *method, Ok(Value::Null)));
            }
            other => panic!("expected a response to {method}, got {other:?}"),
        }
    }
}

#[test]
fn malformed_input_is_reported_and_the_stream_recovers() {
    let (_client, events, mut server) = start_initialized();
    server.send_raw(b"Content-Length: nope\r\n\r\n");
    server.send_raw(b"Content-Length: 5\r\n\r\n{oops");
    server.send_raw(&frame(&json!([1, 2])));
    server.send_raw(&frame(&json!({ "jsonrpc": "2.0", "method": 5 })));
    server.send(json!({ "jsonrpc": "2.0", "method": "custom/after" }));
    expect_error(&events, "Content-Length");
    expect_error(&events, "invalid JSON");
    expect_error(&events, "unexpected message");
    expect_error(&events, "non-string method");
    match next_event(&events) {
        ServerEvent::Notification { method, .. } => assert_eq!(method, "custom/after"),
        other => panic!("expected the notification after the garbage, got {other:?}"),
    }
}

#[test]
fn partial_writes_are_reassembled() {
    let (_client, events, mut server) = start_initialized();
    let bytes = frame(&json!({ "jsonrpc": "2.0", "method": "custom/split", "params": { "text": "ünï ✓" } }));
    for byte in &bytes {
        server.send_raw(std::slice::from_ref(byte));
    }
    // And two messages in one write.
    let mut two = frame(&json!({ "jsonrpc": "2.0", "method": "custom/one" }));
    two.extend(frame(&json!({ "jsonrpc": "2.0", "method": "custom/two" })));
    server.send_raw(&two);
    for expected in ["custom/split", "custom/one", "custom/two"] {
        match next_event(&events) {
            ServerEvent::Notification { method, .. } => assert_eq!(method, expected),
            other => panic!("expected {expected}, got {other:?}"),
        }
    }
}

#[test]
fn endless_garbage_disconnects_with_a_single_exit() {
    let (client, events, mut server) = start_initialized();
    let garbage = "Content-Length: bad\r\n\r\n".repeat(100);
    server.send_raw(garbage.as_bytes());
    let mut errors = 0;
    loop {
        match next_event(&events) {
            ServerEvent::Error(_) => errors += 1,
            ServerEvent::Exited { code } => {
                assert_eq!(code, None);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(errors <= 40, "the reader should give up");
    }
    assert!(errors > 1);
    assert_no_event(&events);
    assert!(matches!(client.hover(&file(), 0, 0), Err(LspError::NotRunning)));
}

#[test]
fn initialize_failure_fails_queued_requests() {
    let (client, events, mut server) = start();
    let init = server.expect("initialize");
    let hover = client.hover(&file(), 0, 0).unwrap();
    client.did_open(&file(), "rust", 1, "").unwrap();
    server.reply_error(&init, -32603, "boom");
    let error = expect_error(&events, "failed to initialize");
    assert!(error.contains("boom"));
    match next_event(&events) {
        ServerEvent::Response { id, method, result: Err(error) } => {
            assert_eq!((id, method.as_str(), error.code), (hover, "textDocument/hover", -32002));
        }
        other => panic!("expected a failed response, got {other:?}"),
    }
    assert!(!client.is_initialized());
    assert!(matches!(client.hover(&file(), 0, 0), Err(LspError::Protocol(_))));
    server.assert_quiet();
}

#[test]
fn unparseable_capabilities_still_initialize() {
    let (client, events, mut server) = start();
    let init = server.expect("initialize");
    server.reply(&init, json!({ "capabilities": { "hoverProvider": true, "renameProvider": "weird" } }));
    server.expect("initialized");
    expect_error(&events, "renameProvider");
    match next_event(&events) {
        ServerEvent::Initialized(caps) => {
            assert!(caps.hover_provider.is_some());
            assert!(caps.rename_provider.is_none());
        }
        other => panic!("expected Initialized, got {other:?}"),
    }
    assert!(client.is_initialized());
}

#[test]
fn exited_is_sent_once_when_the_server_closes_the_stream() {
    let (client, events, mut server) = start_initialized();
    let pending = client.hover(&file(), 0, 0).unwrap();
    server.expect("textDocument/hover");
    server.close();
    assert_eq!(expect_exited(&events), None);
    assert_no_event(&events);
    assert!(matches!(client.hover(&file(), 0, 0), Err(LspError::NotRunning)));
    assert!(matches!(client.did_save(&file()), Err(LspError::NotRunning)));
    assert!(matches!(client.cancel(pending), Err(LspError::NotRunning)));
    assert!(matches!(client.respond(json!(1), Ok(Value::Null)), Err(LspError::NotRunning)));
    // Shutting down a dead connection is quick and does not report a second exit.
    let started = Instant::now();
    client.shutdown(WAIT).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_no_event(&events);
}

#[test]
fn shutdown_sequence() {
    let (client, events, mut server) = start_initialized();
    thread::scope(|scope| {
        let shutdown = scope.spawn(|| client.shutdown(WAIT));
        let request = server.expect("shutdown");
        assert!(request.get("params").is_none());
        server.reply(&request, Value::Null);
        let exit = server.expect("exit");
        assert!(exit.get("params").is_none());
        assert!(exit.get("id").is_none());
        // After `exit` the client closes its side.
        assert!(matches!(server.incoming.recv_timeout(WAIT), Err(RecvTimeoutError::Disconnected)));
        server.close();
        shutdown.join().unwrap().unwrap();
    });
    // The internal shutdown request does not surface as a Response.
    assert_eq!(expect_exited(&events), None);
    assert_no_event(&events);
    let started = Instant::now();
    client.shutdown(WAIT).unwrap();
    assert!(started.elapsed() < Duration::from_secs(1), "shutdown is idempotent");
    assert!(matches!(client.hover(&file(), 0, 0), Err(LspError::NotRunning)));
    assert!(matches!(client.notify_raw("x", Value::Null), Err(LspError::NotRunning)));
}

#[test]
fn shutdown_times_out_on_an_unresponsive_server() {
    let (client, events, server) = start_initialized();
    let started = Instant::now();
    client.shutdown(Duration::from_millis(300)).unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(5), "{elapsed:?}");
    server.expect("shutdown");
    server.expect("exit");
    assert_eq!(expect_exited(&events), None);
    assert_no_event(&events);
}

#[test]
fn shutdown_before_initialization_skips_the_shutdown_request() {
    let (client, events, server) = start();
    server.expect("initialize");
    let queued = client.hover(&file(), 0, 0);
    assert!(queued.is_ok());
    client.shutdown(Duration::from_millis(200)).unwrap();
    // Queued messages are dropped; only `exit` goes out.
    server.expect("exit");
    assert!(matches!(server.incoming.recv_timeout(WAIT), Err(RecvTimeoutError::Disconnected)));
    assert_eq!(expect_exited(&events), None);
}

#[test]
fn dropping_the_client_closes_its_output() {
    let (client, events, mut server) = start_initialized();
    drop(client);
    assert!(matches!(server.incoming.recv_timeout(WAIT), Err(RecvTimeoutError::Disconnected)));
    server.close();
    assert_eq!(expect_exited(&events), None);
}

#[test]
fn bad_root_is_rejected() {
    let (client_in, _server_out) = std::io::pipe().unwrap();
    let (_server_in, client_out) = std::io::pipe().unwrap();
    let result = LspClient::connect(Box::new(client_in), Box::new(client_out), PathBuf::new(), None, None);
    assert!(matches!(result, Err(LspError::BadUri(_))));
}
