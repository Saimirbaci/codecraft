//! The WebSocket listener and per-connection threads.
//!
//! One listener thread accepts on `127.0.0.1:<random port>`; each connection gets its own
//! thread that owns the socket. The socket has a short read timeout so the thread can
//! interleave reading client messages with writing queued replies and notifications (sync
//! tungstenite resumes a partially read frame after a timeout). Threads are never joined: they
//! notice the stop flag within one poll interval, so stopping never blocks the caller.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde_json::Value;
use tungstenite::handshake::server::{Callback, ErrorResponse, Request, Response};
use tungstenite::http::{HeaderValue, StatusCode};
use tungstenite::{Message, WebSocket};

use crate::rpc::{self, Dispatch, ServerInfo};
use crate::{AUTH_HEADER, ClientId, ClientSummary, McpError, Result, ServerConfig, ServerEvent, ToolCall, ToolDef};

/// How often idle threads look at their queues and the stop flag.
const POLL: Duration = Duration::from_millis(15);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Keepalive ping interval; a client that stops answering is dropped by the TCP write timeout.
const PING_EVERY: Duration = Duration::from_secs(30);
const MAX_CLIENTS: usize = 32;
/// The MCP-over-WebSocket subprotocol, echoed back when the client offers it.
const SUBPROTOCOL: &str = "mcp";

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A message queued for one connection.
#[derive(Debug)]
pub(crate) enum Outgoing {
    Text(String),
}

struct ClientEntry {
    out: Sender<Outgoing>,
    initialized: bool,
    info: Value,
}

struct Shared {
    stop: AtomicBool,
    clients: Mutex<BTreeMap<ClientId, ClientEntry>>,
    next_client: AtomicU64,
    events: Sender<ServerEvent>,
    token: String,
    name: String,
    version: String,
    tools: Vec<ToolDef>,
}

impl Shared {
    fn emit(&self, event: ServerEvent) -> bool {
        self.events.send(event).is_ok()
    }
}

/// A running IDE server. Dropping it stops the server and removes its lock file.
pub struct IdeServer {
    shared: Arc<Shared>,
    port: u16,
    lock_path: Option<PathBuf>,
}

impl IdeServer {
    /// Bind, write the lock file (after removing stale ones of the same IDE) and start
    /// accepting. Events (tool calls, connections) arrive on the returned channel.
    pub fn start(config: ServerConfig) -> Result<(Self, Receiver<ServerEvent>)> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .map_err(|e| McpError::Io(format!("cannot listen on 127.0.0.1: {e}")))?;
        let port = listener.local_addr().map_err(|e| McpError::Io(e.to_string()))?.port();
        listener.set_nonblocking(true).map_err(|e| McpError::Io(e.to_string()))?;
        let token = match config.auth_token {
            Some(t) if !t.is_empty() => t,
            _ => crate::new_token()?,
        };
        let (tx, rx) = unbounded();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            clients: Mutex::new(BTreeMap::new()),
            next_client: AtomicU64::new(0),
            events: tx,
            token: token.clone(),
            name: config.ide_name.clone(),
            version: config.server_version.clone(),
            tools: config.tools,
        });
        let lock_path = match config.lock_dir {
            Some(dir) => {
                crate::lockfile::cleanup_stale(&dir, &config.ide_name);
                let info = crate::lockfile::LockInfo {
                    pid: std::process::id(),
                    workspace_folders: config.workspace_folders.iter().map(|p| p.display().to_string()).collect(),
                    ide_name: config.ide_name.clone(),
                    transport: "ws".into(),
                    running_in_windows: cfg!(windows),
                    auth_token: token,
                };
                Some(crate::lockfile::write(&dir, port, &info)?)
            }
            None => None,
        };
        let s = shared.clone();
        let spawned =
            thread::Builder::new().name(format!("ide-server-{port}")).spawn(move || accept_loop(&s, &listener));
        if let Err(e) = spawned {
            if let Some(p) = &lock_path {
                crate::lockfile::remove(p);
            }
            return Err(McpError::Io(format!("cannot start the listener thread: {e}")));
        }
        // Best effort: also accept on [::1] for clients that resolve `localhost` to IPv6 first.
        if let Ok(v6) = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port)))
            && v6.set_nonblocking(true).is_ok()
        {
            let s = shared.clone();
            let _ = thread::Builder::new().name(format!("ide-server6-{port}")).spawn(move || accept_loop(&s, &v6));
        }
        log::info!("IDE server listening on 127.0.0.1:{port}");
        Ok((Self { shared, port, lock_path }, rx))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The token clients must send in the `x-claude-code-ide-authorization` header.
    pub fn auth_token(&self) -> &str {
        &self.shared.token
    }

    /// The lock file this server wrote (None when started without a lock directory).
    pub fn lock_path(&self) -> Option<&Path> {
        self.lock_path.as_deref()
    }

    pub fn is_running(&self) -> bool {
        !self.shared.stop.load(Ordering::Relaxed)
    }

    /// Connected clients (ordered by id).
    pub fn clients(&self) -> Vec<ClientSummary> {
        lock(&self.shared.clients)
            .iter()
            .map(|(id, c)| ClientSummary { id: *id, initialized: c.initialized, info: c.info.clone() })
            .collect()
    }

    pub fn client_count(&self) -> usize {
        lock(&self.shared.clients).len()
    }

    /// Send a JSON-RPC notification to every connected client; returns how many it was queued for.
    pub fn notify(&self, method: &str, params: Value) -> usize {
        let text = rpc::notification(method, params).to_string();
        let clients = lock(&self.shared.clients);
        clients.values().filter(|c| c.out.send(Outgoing::Text(text.clone())).is_ok()).count()
    }

    /// Send a notification to one client; false if it is gone.
    pub fn notify_client(&self, client: ClientId, method: &str, params: Value) -> bool {
        let text = rpc::notification(method, params).to_string();
        lock(&self.shared.clients).get(&client).is_some_and(|c| c.out.send(Outgoing::Text(text)).is_ok())
    }

    /// Stop accepting, close every connection and remove the lock file. Idempotent; never blocks.
    pub fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(p) = self.lock_path.take() {
            crate::lockfile::remove(&p);
        }
    }
}

impl Drop for IdeServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(shared: &Arc<Shared>, listener: &TcpListener) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    continue;
                }
                if lock(&shared.clients).len() >= MAX_CLIENTS {
                    log::warn!("IDE server: too many clients; refusing {peer}");
                    continue;
                }
                let id = ClientId(shared.next_client.fetch_add(1, Ordering::Relaxed) + 1);
                let s = shared.clone();
                let spawned = thread::Builder::new().name(format!("ide-client-{}", id.0)).spawn(move || {
                    serve(&s, stream, id);
                });
                if let Err(e) = spawned {
                    log::warn!("IDE server: cannot spawn a connection thread: {e}");
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted => thread::sleep(POLL),
            Err(e) => {
                log::warn!("IDE server: accept failed: {e}");
                thread::sleep(POLL);
            }
        }
    }
}

fn is_timeout(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted))
}

/// Constant-time comparison (the token must not leak through timing).
fn token_matches(given: &[u8], expected: &[u8]) -> bool {
    if given.len() != expected.len() {
        return false;
    }
    given.iter().zip(expected).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn reject(status: StatusCode, why: &str) -> ErrorResponse {
    let mut r = ErrorResponse::new(Some(why.to_string()));
    *r.status_mut() = status;
    r
}

/// The upgrade check: require the token, echo the `mcp` subprotocol when offered.
struct UpgradeCheck<'a> {
    token: &'a [u8],
}

impl Callback for UpgradeCheck<'_> {
    fn on_request(self, req: &Request, mut resp: Response) -> std::result::Result<Response, ErrorResponse> {
        let given = req.headers().get(AUTH_HEADER).map(HeaderValue::as_bytes).unwrap_or_default();
        if !token_matches(given, self.token) {
            return Err(reject(StatusCode::UNAUTHORIZED, "invalid or missing authentication token"));
        }
        let offers_mcp = req
            .headers()
            .get_all("sec-websocket-protocol")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|p| p.trim() == SUBPROTOCOL);
        if offers_mcp {
            resp.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static(SUBPROTOCOL));
        }
        Ok(resp)
    }
}

fn handshake(shared: &Shared, stream: TcpStream) -> Option<WebSocket<TcpStream>> {
    let setup = stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)))
        .and_then(|()| stream.set_write_timeout(Some(WRITE_TIMEOUT)))
        .and_then(|()| stream.set_nodelay(true));
    if let Err(e) = setup {
        log::warn!("IDE server: socket setup failed: {e}");
        return None;
    }
    match tungstenite::accept_hdr(stream, UpgradeCheck { token: shared.token.as_bytes() }) {
        Ok(ws) => Some(ws),
        Err(e) => {
            log::info!("IDE server: handshake refused: {e}");
            None
        }
    }
}

fn serve(shared: &Arc<Shared>, stream: TcpStream, id: ClientId) {
    let Some(mut ws) = handshake(shared, stream) else { return };
    if let Err(e) = ws.get_mut().set_read_timeout(Some(POLL)) {
        log::warn!("IDE server: socket setup failed: {e}");
        return;
    }
    let (out_tx, out_rx) = unbounded();
    lock(&shared.clients).insert(id, ClientEntry { out: out_tx.clone(), initialized: false, info: Value::Null });
    shared.emit(ServerEvent::Connected { client: id });
    log::info!("IDE server: client {} connected", id.0);
    run_connection(shared, &mut ws, id, &out_tx, &out_rx);
    lock(&shared.clients).remove(&id);
    shared.emit(ServerEvent::Disconnected { client: id });
    log::info!("IDE server: client {} disconnected", id.0);
}

fn run_connection(
    shared: &Arc<Shared>,
    ws: &mut WebSocket<TcpStream>,
    id: ClientId,
    out_tx: &Sender<Outgoing>,
    out_rx: &Receiver<Outgoing>,
) {
    let info = ServerInfo { name: &shared.name, version: &shared.version, tools: &shared.tools };
    let inflight = crate::InflightRef::default();
    let mut last_ping = Instant::now();
    loop {
        if shared.stop.load(Ordering::Relaxed) {
            let _ = ws.close(None);
            let _ = ws.flush();
            return;
        }
        while let Ok(Outgoing::Text(text)) = out_rx.try_recv() {
            if let Err(e) = ws.write(Message::text(text))
                && !is_timeout(&e)
            {
                return;
            }
        }
        if last_ping.elapsed() >= PING_EVERY {
            last_ping = Instant::now();
            if let Err(e) = ws.write(Message::Ping(Default::default()))
                && !is_timeout(&e)
            {
                return;
            }
        }
        if let Err(e) = ws.flush()
            && !is_timeout(&e)
        {
            return;
        }
        let text = match ws.read() {
            Ok(Message::Text(t)) => t.as_str().to_string(),
            Ok(Message::Binary(b)) => match std::str::from_utf8(&b) {
                Ok(t) => t.to_string(),
                Err(_) => continue,
            },
            Ok(_) => continue,
            Err(e) if is_timeout(&e) => continue,
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return,
            Err(e) => {
                log::info!("IDE server: client {} read error: {e}", id.0);
                return;
            }
        };
        for d in rpc::dispatch(&text, &info) {
            let reply = match d {
                Dispatch::Respond(v) => Some(v),
                Dispatch::Initialize { response, client_info } => {
                    if let Some(c) = lock(&shared.clients).get_mut(&id) {
                        c.info = client_info;
                    }
                    Some(response)
                }
                Dispatch::Initialized => {
                    let client_info = lock(&shared.clients).get_mut(&id).map(|c| {
                        c.initialized = true;
                        c.info.clone()
                    });
                    shared.emit(ServerEvent::Initialized { client: id, client_info: client_info.unwrap_or_default() });
                    None
                }
                Dispatch::ToolCall { id: request, name, arguments } => {
                    let reply = crate::Reply::tracked(request.clone(), out_tx.clone(), &inflight);
                    let call = ToolCall { client: id, request: request.clone(), name, arguments, reply };
                    if shared.emit(ServerEvent::ToolCall(call)) {
                        None
                    } else {
                        Some(rpc::error_response(request, rpc::INTERNAL_ERROR, "the IDE is shutting down"))
                    }
                }
                Dispatch::Cancelled { request } => {
                    crate::Inflight::cancel(&inflight, &request);
                    shared.emit(ServerEvent::Cancelled { client: id, request });
                    None
                }
                Dispatch::Ignore => None,
            };
            if let Some(v) = reply
                && let Err(e) = ws.write(Message::text(v.to_string()))
                && !is_timeout(&e)
            {
                return;
            }
        }
    }
}
