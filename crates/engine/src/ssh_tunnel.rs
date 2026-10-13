//! SSH tunnels for data sources (M11 track S): a data source with `ssh_tunnel = <server>`
//! connects through `ssh -N -L 127.0.0.1:<free port>:<db host>:<db port> <server>`.
//!
//! The database engine plans the tunnel on the UI thread ([`Tunnels::plan`], no I/O but reading
//! the server list) and opens it on its connection thread ([`TunnelPlan::open`]: spawn ssh, wait
//! until the local port accepts). Tunnels are killed when the data source disconnects, and when
//! the session goes away.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use codecraft_db::DataSourceConfig;
use codecraft_remote::SshOptions;
use codecraft_remote::command as rcmd;
use codecraft_remote::server::SshServer;

/// How long a tunnel may take to come up.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// Where servers and `ssh` come from (kept equal to [`crate::remote::RemoteState`]'s by its pump).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TunnelEnv {
    pub ssh: String,
    pub ssh_config: Option<PathBuf>,
    pub servers_file: Option<PathBuf>,
}

impl Default for TunnelEnv {
    fn default() -> Self {
        let r = crate::remote::RemoteState::default();
        Self { ssh: r.programs.ssh.clone(), ssh_config: r.ssh_config_path(), servers_file: r.servers_file() }
    }
}

#[derive(Default)]
struct Inner {
    env: Option<TunnelEnv>,
    /// Open tunnels by data source id (one per connection).
    open: HashMap<String, Vec<Child>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        for (_, children) in self.open.drain() {
            for mut c in children {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

/// The tunnels of a session (shared with the connection threads).
#[derive(Clone, Default)]
pub struct Tunnels {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Tunnels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tunnels").field("open", &self.open_count()).finish()
    }
}

/// A tunnel to open for one connection.
pub struct TunnelPlan {
    source: String,
    server: SshServer,
    ssh: String,
    host: String,
    port: u16,
    tunnels: Tunnels,
}

impl Tunnels {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Set where servers and `ssh` come from.
    pub fn configure(&self, env: TunnelEnv) {
        let mut g = self.lock();
        if g.env.as_ref() != Some(&env) {
            g.env = Some(env);
        }
    }

    /// The tunnel a data source needs (`None` without `ssh_tunnel`).
    pub fn plan(&self, cfg: &DataSourceConfig) -> Result<Option<TunnelPlan>, String> {
        let Some(name) = cfg.ssh_tunnel.as_deref().map(str::trim).filter(|n| !n.is_empty()) else { return Ok(None) };
        if cfg.kind.is_file_based() {
            return Err(format!("{} data sources cannot use an SSH tunnel", cfg.kind.label()));
        }
        let env = self.lock().env.clone().unwrap_or_default();
        let (servers, _) = crate::remote::load_servers_at(env.ssh_config.as_deref(), env.servers_file.as_deref());
        let server = servers
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("unknown SSH server `{name}` for the tunnel"))?;
        let host = if cfg.host.trim().is_empty() { "localhost".to_string() } else { cfg.host.trim().to_string() };
        let port = cfg.port_or_default();
        if port == 0 {
            return Err("the data source has no port".into());
        }
        Ok(Some(TunnelPlan { source: cfg.id.clone(), server, ssh: env.ssh, host, port, tunnels: self.clone() }))
    }

    /// Kill the tunnels of a data source (or all).
    pub fn close(&self, source: Option<&str>) {
        let children: Vec<Child> = {
            let mut g = self.lock();
            match source {
                Some(id) => g.open.remove(id).unwrap_or_default(),
                None => g.open.drain().flat_map(|(_, v)| v).collect(),
            }
        };
        for mut c in children {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn open_count(&self) -> usize {
        self.lock().open.values().map(Vec::len).sum()
    }
}

/// A free local port (the OS picks one; a race with another process is possible but rare, and
/// ssh then fails with ExitOnForwardFailure).
fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free local port: {e}"))?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

/// The config to connect with: through the tunnel when there is one (blocking: call on a
/// connection thread).
pub fn route(plan: Option<TunnelPlan>, cfg: DataSourceConfig) -> Result<DataSourceConfig, String> {
    match plan {
        Some(p) => p.open(cfg),
        None => Ok(cfg),
    }
}

impl TunnelPlan {
    /// Register the tunnel under another key (Test Connection closes its own tunnel).
    pub fn keyed(mut self, key: String) -> TunnelPlan {
        self.source = key;
        self
    }

    /// The ssh arguments for a local port.
    pub fn args(&self, local_port: u16) -> Result<Vec<String>, String> {
        let opts = SshOptions { program: self.ssh.clone(), control_path: None, connect_timeout: Some(15) };
        rcmd::ssh_tunnel(&self.server, &opts, local_port, &self.host, self.port).map_err(|e| e.to_string())
    }

    /// Open the tunnel (blocking: call on a connection thread) and return the config that
    /// connects through it.
    pub fn open(self, cfg: DataSourceConfig) -> Result<DataSourceConfig, String> {
        let port = free_port()?;
        self.open_on(cfg, port)
    }

    fn open_on(self, mut cfg: DataSourceConfig, port: u16) -> Result<DataSourceConfig, String> {
        let args = self.args(port)?;
        let mut child = Command::new(crate::remote::resolve_program(&self.ssh))
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run ssh for the tunnel: {e}"))?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let start = Instant::now();
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.by_ref().take(16 * 1024).read_to_string(&mut err);
                }
                let err = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("").to_string();
                return Err(format!("the SSH tunnel to {} closed ({status}) {err}", self.server.name)
                    .trim()
                    .to_string());
            }
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                break;
            }
            if start.elapsed() > OPEN_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("the SSH tunnel to {} did not open in time", self.server.name));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        self.tunnels.lock().open.entry(self.source.clone()).or_default().push(child);
        cfg.host = "127.0.0.1".into();
        cfg.port = Some(port);
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_db::DbKind;

    fn env_with_server(dir: &std::path::Path, ssh: &str) -> TunnelEnv {
        let servers = dir.join("ssh.json");
        std::fs::write(&servers, r#"{"servers":[{"name":"bastion","host":"b.example","user":"me"}]}"#).unwrap();
        TunnelEnv { ssh: ssh.into(), ssh_config: Some(dir.join("no-config")), servers_file: Some(servers) }
    }

    fn pg(tunnel: Option<&str>) -> DataSourceConfig {
        DataSourceConfig {
            id: "pg".into(),
            name: "PG".into(),
            kind: DbKind::Postgres,
            host: "db.internal".into(),
            port: Some(5433),
            ssh_tunnel: tunnel.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn plans_only_with_a_known_server() {
        let dir = tempfile::tempdir().unwrap();
        let t = Tunnels::default();
        t.configure(env_with_server(dir.path(), "ssh"));
        assert!(t.plan(&pg(None)).unwrap().is_none());
        assert!(t.plan(&pg(Some("nope"))).is_err());
        let plan = t.plan(&pg(Some("bastion"))).unwrap().unwrap();
        let args = plan.args(40000).unwrap();
        assert!(args.contains(&"127.0.0.1:40000:db.internal:5433".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("b.example"));
        let sqlite = DataSourceConfig { kind: DbKind::Sqlite, ..pg(Some("bastion")) };
        assert!(t.plan(&sqlite).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn open_waits_for_the_port_and_close_kills_the_process() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv");
        let ssh = dir.path().join("fake-ssh");
        std::fs::write(&ssh, format!("#!/bin/sh\necho \"$@\" > '{}'\nexec sleep 30\n", log.display())).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = Tunnels::default();
        t.configure(env_with_server(dir.path(), ssh.to_str().unwrap()));
        // The test plays the forwarded port.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let plan = t.plan(&pg(Some("bastion"))).unwrap().unwrap();
        let cfg = plan.open_on(pg(Some("bastion")), port).unwrap();
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, Some(port));
        assert_eq!(t.open_count(), 1);
        // The port was ready at once (the test listens), so the script may still be starting.
        let start = Instant::now();
        while std::fs::read_to_string(&log).map_or(true, |t| !t.ends_with('\n'))
            && start.elapsed() < Duration::from_secs(10)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(argv.contains(&format!("-L 127.0.0.1:{port}:db.internal:5433")), "{argv}");
        t.close(Some("pg"));
        assert_eq!(t.open_count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_tunnel_that_exits_reports_its_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("fake-ssh");
        std::fs::write(&ssh, "#!/bin/sh\necho 'Permission denied (publickey).' >&2\nexit 255\n").unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = Tunnels::default();
        t.configure(env_with_server(dir.path(), ssh.to_str().unwrap()));
        let plan = t.plan(&pg(Some("bastion"))).unwrap().unwrap();
        let err = plan.open(pg(Some("bastion"))).unwrap_err();
        assert!(err.contains("Permission denied"), "{err}");
        assert_eq!(t.open_count(), 0);
    }
}
