//! A stdio ⇄ WebSocket bridge to a running IDE server: an agent that only speaks MCP over stdio
//! (Codex, Gemini, `claude -p --mcp-config`, ACP agents) launches the bridge as its MCP server
//! and reaches the IDE's tools through it. Lines from the agent go to the IDE unchanged, except
//! that `initialize` gets the bridge's tag in `clientInfo` (how the IDE knows which agent job is
//! calling); everything the IDE sends comes back as lines.

use std::io::{BufRead, ErrorKind, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{TryRecvError, unbounded};
use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

use crate::{AUTH_HEADER, McpError, Result};

const POLL: Duration = Duration::from_millis(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Where and how to connect.
#[derive(Clone, Debug)]
pub struct BridgeConfig {
    pub port: u16,
    pub token: String,
    /// Extra fields merged into the client's `initialize` `clientInfo` (e.g. `{"codecraftJob": 3}`).
    pub client_tag: serde_json::Map<String, Value>,
}

fn connect(cfg: &BridgeConfig) -> Result<WebSocket<TcpStream>> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, cfg.port));
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| McpError::Io(format!("cannot reach the IDE on port {}: {e}", cfg.port)))?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT)).map_err(|e| McpError::Io(e.to_string()))?;
    let mut req = format!("ws://127.0.0.1:{}", cfg.port)
        .into_client_request()
        .map_err(|e| McpError::Io(format!("bad request: {e}")))?;
    let token =
        HeaderValue::from_str(&cfg.token).map_err(|_| McpError::Io("the token is not a valid header".into()))?;
    req.headers_mut().insert(AUTH_HEADER, token);
    let (ws, _) =
        tungstenite::client(req, stream).map_err(|e| McpError::Io(format!("the IDE refused the connection: {e}")))?;
    ws.get_ref().set_read_timeout(Some(POLL)).map_err(|e| McpError::Io(e.to_string()))?;
    Ok(ws)
}

/// Add `tag` to the `clientInfo` of an `initialize` request (other messages are unchanged).
pub fn tag_initialize(line: &str, tag: &serde_json::Map<String, Value>) -> String {
    if tag.is_empty() {
        return line.to_string();
    }
    let Ok(mut msg) = serde_json::from_str::<Value>(line) else { return line.to_string() };
    let is_init = msg.get("method").and_then(Value::as_str) == Some("initialize");
    if !is_init {
        return line.to_string();
    }
    let Some(obj) = msg.as_object_mut() else { return line.to_string() };
    let params = obj.entry("params").or_insert_with(|| Value::Object(Default::default()));
    let Some(params) = params.as_object_mut() else { return line.to_string() };
    let info = params.entry("clientInfo").or_insert_with(|| Value::Object(Default::default()));
    if let Some(info) = info.as_object_mut() {
        for (k, v) in tag {
            info.insert(k.clone(), v.clone());
        }
    }
    msg.to_string()
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted))
}

/// Run the bridge until the input ends or the IDE closes the connection. Blocks the caller.
pub fn run<R, W>(cfg: &BridgeConfig, input: R, mut output: W) -> Result<()>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    let mut ws = connect(cfg)?;
    let (tx, rx) = unbounded::<String>();
    thread::Builder::new()
        .name("mcp-bridge-input".into())
        .spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else { break };
                if !line.trim().is_empty() && tx.send(line).is_err() {
                    break;
                }
            }
        })
        .map_err(|e| McpError::Io(format!("cannot start the input thread: {e}")))?;
    loop {
        loop {
            match rx.try_recv() {
                Ok(line) => {
                    let line = tag_initialize(&line, &cfg.client_tag);
                    if let Err(e) = ws.write(Message::text(line))
                        && !is_timeout(&e)
                    {
                        return Err(McpError::Io(format!("the IDE connection failed: {e}")));
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let _ = ws.close(None);
                    let _ = ws.flush();
                    return Ok(());
                }
            }
        }
        if let Err(e) = ws.flush()
            && !is_timeout(&e)
        {
            return Err(McpError::Io(format!("the IDE connection failed: {e}")));
        }
        let text = match ws.read() {
            Ok(Message::Text(t)) => t.as_str().to_string(),
            Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => continue,
            Err(e) if is_timeout(&e) => continue,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Ok(()),
            Err(e) => return Err(McpError::Io(format!("the IDE connection failed: {e}"))),
        };
        // One message per line: compact any pretty-printed JSON.
        let line = serde_json::from_str::<Value>(&text).map(|v| v.to_string()).unwrap_or(text);
        output
            .write_all(line.as_bytes())
            .and_then(|()| output.write_all(b"\n"))
            .and_then(|()| output.flush())
            .map_err(|e| McpError::Io(format!("output closed: {e}")))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_is_tagged_and_other_lines_pass_through() {
        let mut tag = serde_json::Map::new();
        tag.insert("codecraftJob".into(), json!(3));
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"codex"}}}"#;
        let out: Value = serde_json::from_str(&tag_initialize(init, &tag)).unwrap();
        assert_eq!(out["params"]["clientInfo"], json!({ "name": "codex", "codecraftJob": 3 }));
        let bare: Value =
            serde_json::from_str(&tag_initialize(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#, &tag)).unwrap();
        assert_eq!(bare["params"]["clientInfo"]["codecraftJob"], 3);
        let other = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#;
        assert_eq!(tag_initialize(other, &tag), other);
        assert_eq!(tag_initialize("garbage", &tag), "garbage");
        assert_eq!(tag_initialize(init, &serde_json::Map::new()), init);
        assert_eq!(
            tag_initialize(r#"{"method":"initialize","params":5}"#, &tag),
            r#"{"method":"initialize","params":5}"#
        );
    }

    #[test]
    fn connecting_to_nothing_fails_cleanly() {
        // Port 1 on loopback is never an IDE server.
        let cfg = BridgeConfig { port: 1, token: "t".into(), client_tag: Default::default() };
        assert!(run(&cfg, std::io::Cursor::new(Vec::new()), Vec::new()).is_err());
    }
}
