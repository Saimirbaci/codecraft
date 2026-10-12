//! MCP over JSON-RPC 2.0: decoding client messages and answering the methods the server core
//! handles itself (`initialize`, `ping`, `tools/list`, empty prompt/resource lists). Tool calls
//! are handed to the host.

use serde_json::{Value, json};

use crate::ToolDef;

/// JSON-RPC error codes.
pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const INVALID_REQUEST: i64 = -32600;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const INTERNAL_ERROR: i64 = -32603;

/// MCP revisions this server speaks; the first is the one it offers when the client asks for
/// one it does not know. Our use of MCP (tools with text content) is the same in all of them.
pub(crate) const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-11-25", "2025-03-26", "2024-11-05"];

/// What the connection should do with one incoming message.
#[derive(Debug, PartialEq)]
pub(crate) enum Dispatch {
    /// Send this JSON-RPC response.
    Respond(Value),
    /// `initialize` was answered (the response is the first element); remember the client info.
    Initialize { response: Value, client_info: Value },
    /// The client finished the handshake (`notifications/initialized`).
    Initialized,
    /// A `tools/call` for a registered tool: the host answers it.
    ToolCall { id: Value, name: String, arguments: Value },
    /// The client gave up on an earlier request (`notifications/cancelled`).
    Cancelled { request: Value },
    /// A notification or response we don't act on.
    Ignore,
}

pub(crate) struct ServerInfo<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub tools: &'a [ToolDef],
}

pub(crate) fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub(crate) fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// Decode one WebSocket text message (a JSON-RPC message or a batch of them).
pub(crate) fn dispatch(text: &str, server: &ServerInfo<'_>) -> Vec<Dispatch> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(items)) if !items.is_empty() => items.iter().map(|m| dispatch_one(m, server)).collect(),
        Ok(Value::Array(_)) => vec![Dispatch::Respond(error_response(Value::Null, INVALID_REQUEST, "empty batch"))],
        Ok(msg) => vec![dispatch_one(&msg, server)],
        Err(e) => vec![Dispatch::Respond(error_response(Value::Null, PARSE_ERROR, format!("parse error: {e}")))],
    }
}

fn dispatch_one(msg: &Value, server: &ServerInfo<'_>) -> Dispatch {
    let Some(obj) = msg.as_object() else {
        return Dispatch::Respond(error_response(Value::Null, INVALID_REQUEST, "expected a JSON-RPC object"));
    };
    let id = obj.get("id").cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        // A response to something we sent (we send no requests) or garbage.
        return match id {
            Some(id) if !obj.contains_key("result") && !obj.contains_key("error") => {
                Dispatch::Respond(error_response(id, INVALID_REQUEST, "missing method"))
            }
            _ => Dispatch::Ignore,
        };
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return match method {
            "notifications/initialized" | "initialized" => Dispatch::Initialized,
            "notifications/cancelled" => {
                Dispatch::Cancelled { request: params.get("requestId").cloned().unwrap_or(Value::Null) }
            }
            _ => Dispatch::Ignore,
        };
    };
    match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str);
            let version = asked.filter(|v| PROTOCOL_VERSIONS.contains(v)).or(PROTOCOL_VERSIONS.first().copied());
            let result = json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": true }, "logging": {} },
                "serverInfo": { "name": server.name, "version": server.version },
            });
            Dispatch::Initialize {
                response: response(id, result),
                client_info: params.get("clientInfo").cloned().unwrap_or(Value::Null),
            }
        }
        "ping" => Dispatch::Respond(response(id, json!({}))),
        "tools/list" => {
            let tools: Vec<Value> = server
                .tools
                .iter()
                .map(|t| json!({ "name": t.name, "description": t.description, "inputSchema": t.input_schema }))
                .collect();
            Dispatch::Respond(response(id, json!({ "tools": tools })))
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Dispatch::Respond(error_response(id, INVALID_PARAMS, "tools/call needs a tool name"));
            };
            if !server.tools.iter().any(|t| t.name == name) {
                return Dispatch::Respond(error_response(id, INVALID_PARAMS, format!("unknown tool: {name}")));
            }
            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(a) => a.clone(),
            };
            Dispatch::ToolCall { id, name: name.to_string(), arguments }
        }
        "prompts/list" => Dispatch::Respond(response(id, json!({ "prompts": [] }))),
        "resources/list" => Dispatch::Respond(response(id, json!({ "resources": [] }))),
        "resources/templates/list" => Dispatch::Respond(response(id, json!({ "resourceTemplates": [] }))),
        "logging/setLevel" => Dispatch::Respond(response(id, json!({}))),
        other => Dispatch::Respond(error_response(id, METHOD_NOT_FOUND, format!("method not found: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(tools: &[ToolDef]) -> ServerInfo<'_> {
        ServerInfo { name: "CodeCraft", version: "0.1.0", tools }
    }

    fn tools() -> Vec<ToolDef> {
        vec![ToolDef { name: "getOpenEditors".into(), description: "tabs".into(), input_schema: json!({}) }]
    }

    #[test]
    fn initialize_negotiates_version() {
        let t = tools();
        let d = dispatch(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","clientInfo":{"name":"claude-code"}}}"#,
            &server(&t),
        );
        let [Dispatch::Initialize { response, client_info }] = &d[..] else { panic!("{d:?}") };
        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(response["result"]["serverInfo"]["name"], "CodeCraft");
        assert_eq!(client_info["name"], "claude-code");
        let d = dispatch(
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#,
            &server(&t),
        );
        let [Dispatch::Initialize { response, .. }] = &d[..] else { panic!("{d:?}") };
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[test]
    fn tools_list_and_call() {
        let t = tools();
        let d = dispatch(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#, &server(&t));
        let [Dispatch::Respond(r)] = &d[..] else { panic!("{d:?}") };
        assert_eq!(r["id"], "a");
        assert_eq!(r["result"]["tools"][0]["name"], "getOpenEditors");
        assert!(r["result"]["tools"][0].get("inputSchema").is_some());

        let d = dispatch(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"getOpenEditors"}}"#,
            &server(&t),
        );
        assert_eq!(d, vec![Dispatch::ToolCall { id: json!(3), name: "getOpenEditors".into(), arguments: json!({}) }]);

        let d = dispatch(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"rm"}}"#, &server(&t));
        let [Dispatch::Respond(r)] = &d[..] else { panic!("{d:?}") };
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn notifications_errors_and_garbage() {
        let t = tools();
        let s = server(&t);
        assert_eq!(
            dispatch(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &s),
            vec![Dispatch::Initialized]
        );
        assert_eq!(
            dispatch(r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":9}}"#, &s),
            vec![Dispatch::Cancelled { request: json!(9) }]
        );
        assert_eq!(
            dispatch(r#"{"jsonrpc":"2.0","method":"ide_connected","params":{"pid":1}}"#, &s),
            vec![Dispatch::Ignore]
        );
        assert_eq!(dispatch(r#"{"jsonrpc":"2.0","id":5,"result":{}}"#, &s), vec![Dispatch::Ignore]);
        for (text, code) in [
            ("{nope", PARSE_ERROR),
            ("[]", INVALID_REQUEST),
            ("42", INVALID_REQUEST),
            (r#"{"jsonrpc":"2.0","id":6,"method":"bogus"}"#, METHOD_NOT_FOUND),
            (r#"{"jsonrpc":"2.0","id":7}"#, INVALID_REQUEST),
            (r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":5}}"#, INVALID_PARAMS),
        ] {
            let d = dispatch(text, &s);
            let [Dispatch::Respond(r)] = &d[..] else { panic!("{text}: {d:?}") };
            assert_eq!(r["error"]["code"], code, "{text}");
        }
        let d = dispatch(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"x"}]"#, &s);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], Dispatch::Respond(response(json!(1), json!({}))));
    }
}
