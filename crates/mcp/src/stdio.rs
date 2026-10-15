//! MCP over stdio: one JSON-RPC message per line (the transport `claude mcp add`, Codex and
//! Gemini use for local servers). The same protocol core as the WebSocket server
//! ([`crate::rpc`]); tool calls reach the host as [`ServerEvent::ToolCall`] with a [`Reply`].
//!
//! A reader thread decodes lines; a writer thread owns the output and writes each reply as soon
//! as the host sends it, so a slow tool never blocks other answers. End of input ends the session
//! ([`ServerEvent::Disconnected`]).

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde_json::Value;

use crate::rpc::{self, Dispatch, ServerInfo};
use crate::server::{Outgoing, lock};
use crate::{ClientId, McpError, Reply, Result, ServerConfig, ServerEvent, ToolCall};

/// The only client of a stdio server.
pub const STDIO_CLIENT: ClientId = ClientId(1);
/// Longest accepted line (a message); longer ones are answered with a parse error.
const MAX_LINE: usize = 64 * 1024 * 1024;

struct Shared {
    out: Sender<Outgoing>,
    closed: AtomicBool,
    initialized: AtomicBool,
    info: Mutex<Value>,
}

/// A running stdio server. Its threads end when the input ends and every reply is written.
pub struct StdioServer {
    shared: Arc<Shared>,
}

impl StdioServer {
    /// Serve `config.tools` over `input`/`output` (normally stdin/stdout). `config.lock_dir` and
    /// the auth token are ignored: the client launched this process.
    pub fn start<R, W>(config: ServerConfig, input: R, output: W) -> Result<(Self, Receiver<ServerEvent>)>
    where
        R: BufRead + Send + 'static,
        W: Write + Send + 'static,
    {
        let (out_tx, out_rx) = unbounded::<Outgoing>();
        let (ev_tx, ev_rx) = unbounded::<ServerEvent>();
        let shared = Arc::new(Shared {
            out: out_tx,
            closed: AtomicBool::new(false),
            initialized: AtomicBool::new(false),
            info: Mutex::new(Value::Null),
        });
        thread::Builder::new()
            .name("mcp-stdio-writer".into())
            .spawn(move || write_loop(output, &out_rx))
            .map_err(|e| McpError::Io(format!("cannot start the writer thread: {e}")))?;
        let s = shared.clone();
        thread::Builder::new()
            .name("mcp-stdio-reader".into())
            .spawn(move || read_loop(&s, &config, input, &ev_tx))
            .map_err(|e| McpError::Io(format!("cannot start the reader thread: {e}")))?;
        Ok((Self { shared }, ev_rx))
    }

    /// The input ended (the client went away).
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    /// The client sent `notifications/initialized`.
    pub fn is_initialized(&self) -> bool {
        self.shared.initialized.load(Ordering::Relaxed)
    }

    /// `clientInfo` from `initialize` (`{name, version}`), or null.
    pub fn client_info(&self) -> Value {
        lock(&self.shared.info).clone()
    }

    /// Send a JSON-RPC notification to the client.
    pub fn notify(&self, method: &str, params: Value) -> bool {
        self.shared.out.send(Outgoing::Text(rpc::notification(method, params).to_string())).is_ok()
    }
}

fn write_loop<W: Write>(mut output: W, rx: &Receiver<Outgoing>) {
    for Outgoing::Text(text) in rx.iter() {
        let ok = output.write_all(text.as_bytes()).and_then(|()| output.write_all(b"\n")).and_then(|()| output.flush());
        if let Err(e) = ok {
            log::info!("MCP stdio: output closed: {e}");
            return;
        }
    }
}

/// Read one line into `buf` (without the newline). `Ok(false)` at end of input; an over-long
/// line is skipped up to its newline and reported as `Err`.
fn read_line<R: BufRead>(input: &mut R, buf: &mut Vec<u8>) -> std::io::Result<Result<bool>> {
    buf.clear();
    let mut too_long = false;
    loop {
        let (done, used) = {
            let chunk = input.fill_buf()?;
            if chunk.is_empty() {
                return Ok(if too_long { Err(McpError::Io("line too long".into())) } else { Ok(!buf.is_empty()) });
            }
            match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    if !too_long {
                        buf.extend_from_slice(chunk.get(..i).unwrap_or_default());
                    }
                    (true, i.saturating_add(1))
                }
                None => {
                    if !too_long {
                        buf.extend_from_slice(chunk);
                    }
                    (false, chunk.len())
                }
            }
        };
        input.consume(used);
        if buf.len() > MAX_LINE {
            too_long = true;
            buf.clear();
        }
        if done {
            return Ok(if too_long { Err(McpError::Io("line too long".into())) } else { Ok(true) });
        }
    }
}

fn read_loop<R: BufRead>(shared: &Arc<Shared>, config: &ServerConfig, mut input: R, events: &Sender<ServerEvent>) {
    let info = ServerInfo { name: &config.ide_name, version: &config.server_version, tools: &config.tools };
    let _ = events.send(ServerEvent::Connected { client: STDIO_CLIENT });
    let mut buf = Vec::new();
    loop {
        let line = match read_line(&mut input, &mut buf) {
            Ok(Ok(true)) => String::from_utf8_lossy(&buf).into_owned(),
            Ok(Ok(false)) => break,
            Ok(Err(e)) => {
                let msg = rpc::error_response(Value::Null, rpc::PARSE_ERROR, e.to_string());
                let _ = shared.out.send(Outgoing::Text(msg.to_string()));
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::info!("MCP stdio: input error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        for d in rpc::dispatch(line.trim(), &info) {
            let reply = match d {
                Dispatch::Respond(v) => Some(v),
                Dispatch::Initialize { response, client_info } => {
                    *lock(&shared.info) = client_info;
                    Some(response)
                }
                Dispatch::Initialized => {
                    shared.initialized.store(true, Ordering::Relaxed);
                    let client_info = lock(&shared.info).clone();
                    let _ = events.send(ServerEvent::Initialized { client: STDIO_CLIENT, client_info });
                    None
                }
                Dispatch::ToolCall { id, name, arguments } => {
                    let reply = Reply::new(id.clone(), shared.out.clone());
                    let call = ToolCall { client: STDIO_CLIENT, request: id.clone(), name, arguments, reply };
                    if events.send(ServerEvent::ToolCall(call)).is_ok() {
                        None
                    } else {
                        Some(rpc::error_response(id, rpc::INTERNAL_ERROR, "the server is shutting down"))
                    }
                }
                Dispatch::Cancelled { request } => {
                    let _ = events.send(ServerEvent::Cancelled { client: STDIO_CLIENT, request });
                    None
                }
                Dispatch::Ignore => None,
            };
            if let Some(v) = reply {
                let _ = shared.out.send(Outgoing::Text(v.to_string()));
            }
        }
    }
    shared.closed.store(true, Ordering::Relaxed);
    let _ = events.send(ServerEvent::Disconnected { client: STDIO_CLIENT });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolResult;
    use serde_json::json;
    use std::io::Cursor;
    use std::time::Duration;

    /// An output that collects lines for the test.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            lock(&self.0).extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn lines(&self) -> Vec<Value> {
            let text = String::from_utf8(lock(&self.0).clone()).unwrap();
            text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
        }
    }

    fn wait_lines(sink: &Sink, n: usize) -> Vec<Value> {
        let start = std::time::Instant::now();
        while sink.lines().len() < n && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        sink.lines()
    }

    #[test]
    fn stdio_round_trip() {
        let input = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"t"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"project_info","arguments":{}}}),
        ]
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
            + "\n\nnot json\n";
        let sink = Sink::default();
        let mut config = ServerConfig::claude_code("CodeCraft", Vec::new(), None);
        config.tools = crate::engine_tools::engine_tools();
        let (server, events) = StdioServer::start(config, Cursor::new(input.into_bytes()), sink.clone()).unwrap();
        let mut call = None;
        let mut disconnected = false;
        while let Ok(ev) = events.recv_timeout(Duration::from_secs(5)) {
            match ev {
                ServerEvent::ToolCall(c) => call = Some(c),
                ServerEvent::Disconnected { .. } => {
                    disconnected = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(disconnected && server.is_closed() && server.is_initialized());
        assert_eq!(server.client_info()["name"], "t");
        let call = call.unwrap();
        assert_eq!(call.name, "project_info");
        assert!(call.reply.send(ToolResult::text("hi")));
        let lines = wait_lines(&sink, 4);
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[1]["result"]["tools"].as_array().unwrap().len(), crate::engine_tools::engine_tools().len());
        assert_eq!(lines[2]["error"]["code"], rpc::PARSE_ERROR);
        assert_eq!(lines[3]["id"], 3);
        assert_eq!(lines[3]["result"]["content"][0]["text"], "hi");
        assert!(server.notify("x", json!({})));
    }

    #[test]
    fn long_lines_are_refused_and_reading_continues() {
        let mut data = vec![b'x'; 10];
        data.push(b'\n');
        data.extend_from_slice(b"{\"a\":1}");
        let mut r = Cursor::new(data);
        let mut buf = Vec::new();
        assert!(matches!(read_line(&mut r, &mut buf), Ok(Ok(true))));
        assert_eq!(buf, b"xxxxxxxxxx");
        assert!(matches!(read_line(&mut r, &mut buf), Ok(Ok(true))));
        assert_eq!(buf, b"{\"a\":1}");
        assert!(matches!(read_line(&mut r, &mut buf), Ok(Ok(false))));
    }
}
