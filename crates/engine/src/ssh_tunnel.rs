//! SSH tunnels for data sources (M11 track S): a data source with an SSH tunnel connects
//! through `ssh -N -L 127.0.0.1:<free port>:<db host>:<db port> <server>`. The tunnel is either
//! defined on the data source (`ssh`: host, port, user and agent / key / password
//! authentication, like DataGrip's SSH/SSL tab) or names a server (`ssh_tunnel`: a host of
//! `~/.ssh/config` or Tools › Deployment, or `[user@]host[:port]`).
//!
//! Passwords and key passphrases come from the secret store and reach ssh through
//! `SSH_ASKPASS` (a fixed script that prints an environment variable of the ssh process), never
//! through the command line or a file.
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

use codecraft_db::{DataSourceConfig, SecretStore, SshAuth, SshTunnelConfig};
use codecraft_remote::command as rcmd;
use codecraft_remote::server::{ServerSource, SshServer};
use codecraft_remote::{SshOptions, TunnelAuth};

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
    auth: TunnelAuth,
    /// The password or passphrase when it is known already (typed this session).
    secret: Option<String>,
    /// Where to read it otherwise (store, account).
    secret_store: Option<(Arc<dyn SecretStore>, String)>,
}

/// The variable of the ssh process that the askpass script prints.
const SECRET_ENV: &str = "CODECRAFT_SSH_SECRET";

/// A server for a tunnel defined on the data source.
fn adhoc_server(t: &SshTunnelConfig) -> SshServer {
    let user = t.user.trim();
    let key = t.identity_file.trim();
    SshServer {
        name: t.display(),
        host: t.host.trim().to_string(),
        port: t.port,
        user: (!user.is_empty()).then(|| user.to_string()),
        identity_file: (t.auth != SshAuth::Agent && !key.is_empty()).then(|| key.to_string()),
        source: ServerSource::CodeCraft,
    }
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

    /// The tunnel a data source needs (`None` without one), for agent/key authentication
    /// without a secret.
    pub fn plan(&self, cfg: &DataSourceConfig) -> Result<Option<TunnelPlan>, String> {
        self.plan_with(cfg, None, None)
    }

    /// The tunnel a data source needs. `secret` is a password or passphrase typed this session;
    /// `store` is read (on the connection thread) when it is saved there instead.
    pub fn plan_with(
        &self,
        cfg: &DataSourceConfig,
        secret: Option<String>,
        store: Option<Arc<dyn SecretStore>>,
    ) -> Result<Option<TunnelPlan>, String> {
        let own = cfg.ssh.as_ref().filter(|t| !t.host.trim().is_empty());
        let named = cfg.ssh_tunnel.as_deref().map(str::trim).filter(|n| !n.is_empty());
        if own.is_none() && named.is_none() {
            return Ok(None);
        }
        if cfg.kind.is_file_based() {
            return Err(format!("{} data sources cannot use an SSH tunnel", cfg.kind.label()));
        }
        let env = self.lock().env.clone().unwrap_or_default();
        let (servers, _) = crate::remote::load_servers_at(env.ssh_config.as_deref(), env.servers_file.as_deref());
        let known = |name: &str| servers.iter().find(|s| s.name == name).cloned();
        let (server, auth) = match (own, named) {
            (Some(t), _) => {
                let plain = t.user.trim().is_empty() && t.port.is_none() && t.auth == SshAuth::Agent;
                // A bare server name (`bastion`) uses that server, with all its ssh options.
                match known(t.host.trim()).filter(|_| plain) {
                    Some(server) => (server, TunnelAuth::Batch),
                    None => {
                        let auth = match t.auth {
                            SshAuth::Agent => TunnelAuth::Batch,
                            SshAuth::Key if t.identity_file.trim().is_empty() => {
                                return Err("the SSH tunnel uses a key pair but no private key file is set".into());
                            }
                            // A key without a passphrase needs no prompt.
                            SshAuth::Key if t.secret_saved || secret.is_some() => TunnelAuth::Passphrase,
                            SshAuth::Key => TunnelAuth::Batch,
                            SshAuth::Password => TunnelAuth::Password,
                        };
                        (adhoc_server(t), auth)
                    }
                }
            }
            (None, Some(name)) => match known(name) {
                Some(server) => (server, TunnelAuth::Batch),
                None => {
                    let t = SshTunnelConfig::parse_destination(name).ok_or_else(|| {
                        format!(
                            "unknown SSH server `{name}` for the tunnel: use a host of ~/.ssh/config or Tools › \
                             Deployment, or user@host[:port]"
                        )
                    })?;
                    (adhoc_server(&t), TunnelAuth::Batch)
                }
            },
            (None, None) => return Ok(None),
        };
        // The destination must be a valid ssh word.
        server.destination().map_err(|e| format!("SSH tunnel: {e}"))?;
        let host = if cfg.host.trim().is_empty() { "localhost".to_string() } else { cfg.host.trim().to_string() };
        let port = cfg.port_or_default();
        if port == 0 {
            return Err("the data source has no port".into());
        }
        let saved = own.is_some_and(|t| t.secret_saved);
        let secret_store = if auth != TunnelAuth::Batch && saved {
            store.map(|st| (st, cfg.ssh_secret_account()))
        } else {
            None
        };
        Ok(Some(TunnelPlan {
            source: cfg.id.clone(),
            server,
            ssh: env.ssh,
            host,
            port,
            tunnels: self.clone(),
            auth,
            secret: secret.filter(|_| auth != TunnelAuth::Batch),
            secret_store,
        }))
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
        rcmd::ssh_tunnel_auth(&self.server, &opts, local_port, &self.host, self.port, self.auth)
            .map_err(|e| e.to_string())
    }

    /// Open the tunnel (blocking: call on a connection thread) and return the config that
    /// connects through it.
    pub fn open(self, cfg: DataSourceConfig) -> Result<DataSourceConfig, String> {
        let port = free_port()?;
        self.open_on(cfg, port)
    }

    /// The password or passphrase (blocking: may read the keychain).
    fn resolve_secret(&self) -> Result<String, String> {
        if let Some(s) = &self.secret {
            return Ok(s.clone());
        }
        let what = if self.auth == TunnelAuth::Password { "password" } else { "key passphrase" };
        let (store, account) = self.secret_store.as_ref().ok_or_else(|| {
            format!("the SSH {what} for {} is not saved: enter it in the data source's SSH tunnel settings", self.server.name)
        })?;
        store
            .get(account)
            .map_err(|e| format!("cannot read the SSH {what}: {e}"))?
            .ok_or_else(|| format!("the SSH {what} for {} is missing from the keychain: enter it again", self.server.name))
    }

    fn open_on(self, mut cfg: DataSourceConfig, port: u16) -> Result<DataSourceConfig, String> {
        let args = self.args(port)?;
        let mut command = Command::new(crate::remote::resolve_program(&self.ssh));
        command.args(&args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        if self.auth != TunnelAuth::Batch {
            let secret = self.resolve_secret()?;
            let script = askpass_script()?;
            command
                .env("SSH_ASKPASS", script)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env(SECRET_ENV, secret);
            if std::env::var_os("DISPLAY").is_none() {
                // Older OpenSSH only uses SSH_ASKPASS with a DISPLAY.
                command.env("DISPLAY", ":0");
            }
        }
        let mut child = command.spawn().map_err(|e| format!("cannot run ssh for the tunnel: {e}"))?;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let start = Instant::now();
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.by_ref().take(16 * 1024).read_to_string(&mut err);
                }
                return Err(explain_failure(&self.server, &err, &status.to_string()));
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

/// An ssh failure as an error the user can act on.
fn explain_failure(server: &SshServer, stderr: &str, status: &str) -> String {
    let who = match &server.user {
        Some(u) if !server.name.contains('@') => format!("{u}@{}", server.name),
        _ => server.name.clone(),
    };
    let last = stderr.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("").trim();
    if stderr.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") || stderr.contains("Host key verification failed") {
        return format!(
            "the SSH host key of {} does not match ~/.ssh/known_hosts: if the server was reinstalled, verify its \
             key and remove the old entry with `ssh-keygen -R {}`",
            server.host, server.host
        );
    }
    if stderr.contains("Permission denied") || stderr.contains("Too many authentication failures") {
        return format!(
            "SSH authentication failed for {who}: check the user name and the key, agent or password ({last})"
        );
    }
    if stderr.contains("Could not resolve hostname") {
        return format!("cannot resolve the SSH host {}: {last}", server.host);
    }
    if stderr.contains("Connection refused") || stderr.contains("timed out") || stderr.contains("No route to host") {
        return format!("cannot reach the SSH server {who}: {last}");
    }
    if stderr.contains("forwarding failed") || stderr.contains("open failed") || stderr.contains("administratively prohibited") {
        return format!("the SSH server {who} refused to forward to the database: {last}");
    }
    format!("the SSH tunnel to {who} closed ({status}) {last}").trim().to_string()
}

/// The askpass helper: prints `$CODECRAFT_SSH_SECRET` (set only on that ssh process). The
/// script holds no secret; it lives in a directory only this user can read.
#[cfg(unix)]
fn askpass_script() -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let user = std::env::var("USER").unwrap_or_default();
    let user: String = user.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).take(32).collect();
    let dir = std::env::temp_dir().join(format!("codecraft-askpass-{user}"));
    if !dir.is_dir() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symbolic link; refusing to use it", dir.display()));
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot secure {}: {e}", dir.display()))?;
    let script = dir.join("askpass.sh");
    let text = format!("#!/bin/sh\nprintf '%s\\n' \"${SECRET_ENV}\"\n");
    if std::fs::read_to_string(&script).ok().as_deref() != Some(text.as_str()) {
        let tmp = dir.join(format!("askpass.{}.tmp", std::process::id()));
        std::fs::write(&tmp, &text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &script).map_err(|e| format!("cannot write {}: {e}", script.display()))?;
    }
    Ok(script)
}

#[cfg(not(unix))]
fn askpass_script() -> Result<PathBuf, String> {
    Err("SSH password and passphrase authentication for tunnels needs macOS or Linux; use the agent".into())
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
    fn plans_a_known_server_or_a_host() {
        let dir = tempfile::tempdir().unwrap();
        let t = Tunnels::default();
        t.configure(env_with_server(dir.path(), "ssh"));
        assert!(t.plan(&pg(None)).unwrap().is_none());
        // A name that is no server is a host name now (DataGrip-like); garbage is an error.
        assert_eq!(t.plan(&pg(Some("nope"))).unwrap().unwrap().args(1).unwrap().last().map(String::as_str), Some("nope"));
        assert!(t.plan(&pg(Some("a b"))).is_err());
        let plan = t.plan(&pg(Some("bastion"))).unwrap().unwrap();
        let args = plan.args(40000).unwrap();
        assert!(args.contains(&"127.0.0.1:40000:db.internal:5433".to_string()));
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("b.example"));
        let sqlite = DataSourceConfig { kind: DbKind::Sqlite, ..pg(Some("bastion")) };
        assert!(t.plan(&sqlite).is_err());
    }

    fn pg_ssh(t: SshTunnelConfig) -> DataSourceConfig {
        DataSourceConfig { ssh: Some(t), ..pg(None) }
    }

    #[test]
    fn plans_an_ip_or_user_at_host_without_a_configured_server() {
        let dir = tempfile::tempdir().unwrap();
        let t = Tunnels::default();
        t.configure(env_with_server(dir.path(), "ssh"));
        // What DataGrip accepts: a bare IP in the tunnel field.
        let plan = t.plan(&pg(Some("203.0.113.7"))).unwrap().unwrap();
        let args = plan.args(40000).unwrap();
        assert_eq!(args.last().map(String::as_str), Some("203.0.113.7"));
        assert!(args.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        let plan = t.plan(&pg(Some("ops@203.0.113.7:2222"))).unwrap().unwrap();
        let args = plan.args(40000).unwrap();
        assert!(args.windows(2).any(|w| w == ["-p", "2222"]) && args.windows(2).any(|w| w == ["-l", "ops"]), "{args:?}");
        let err = t.plan(&pg(Some("-oProxyCommand=evil"))).err().unwrap();
        assert!(err.contains("unknown SSH server"), "{err}");

        // Defined on the data source: host, port, user, key.
        let cfg = pg_ssh(SshTunnelConfig {
            host: "203.0.113.7".into(),
            port: Some(22),
            user: "ops".into(),
            auth: SshAuth::Key,
            identity_file: "~/.ssh/prod".into(),
            ..Default::default()
        });
        let args = t.plan(&cfg).unwrap().unwrap().args(1).unwrap();
        assert!(args.windows(2).any(|w| w == ["-i", "~/.ssh/prod"]) && args.contains(&"BatchMode=yes".to_string()));
        // A key without a file is an error; a bare server name uses the server.
        let mut no_key = cfg.clone();
        no_key.ssh.as_mut().unwrap().identity_file.clear();
        assert!(t.plan(&no_key).is_err());
        let named = pg_ssh(SshTunnelConfig { host: "bastion".into(), ..Default::default() });
        assert_eq!(t.plan(&named).unwrap().unwrap().args(1).unwrap().last().map(String::as_str), Some("b.example"));
        // Password authentication: no BatchMode, and the password must be known.
        let pw = pg_ssh(SshTunnelConfig { host: "203.0.113.7".into(), auth: SshAuth::Password, ..Default::default() });
        let plan = t.plan(&pw).unwrap().unwrap();
        assert!(!plan.args(1).unwrap().contains(&"BatchMode=yes".to_string()));
        let err = plan.resolve_secret().unwrap_err();
        assert!(err.contains("not saved"), "{err}");
    }

    #[test]
    fn saved_secrets_come_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let t = Tunnels::default();
        t.configure(env_with_server(dir.path(), "ssh"));
        let store: Arc<dyn SecretStore> = Arc::new(codecraft_db::MemoryStore::default());
        store.set("pg:ssh", "s3cret").unwrap();
        let cfg = pg_ssh(SshTunnelConfig {
            host: "h".into(),
            auth: SshAuth::Password,
            secret_saved: true,
            ..Default::default()
        });
        let plan = t.plan_with(&cfg, None, Some(store.clone())).unwrap().unwrap();
        assert_eq!(plan.resolve_secret().unwrap(), "s3cret");
        // Typed this session wins.
        let plan = t.plan_with(&cfg, Some("typed".into()), Some(store)).unwrap().unwrap();
        assert_eq!(plan.resolve_secret().unwrap(), "typed");
    }

    #[test]
    fn failures_are_explained() {
        let server = adhoc_server(&SshTunnelConfig { host: "203.0.113.7".into(), user: "ops".into(), ..Default::default() });
        let e = explain_failure(&server, "ops@203.0.113.7: Permission denied (publickey,password).\n", "exit 255");
        assert!(e.starts_with("SSH authentication failed for ops@203.0.113.7"), "{e}");
        let e = explain_failure(&server, "@@@\nWARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!\nHost key verification failed.\n", "x");
        assert!(e.contains("ssh-keygen -R 203.0.113.7"), "{e}");
        let e = explain_failure(&server, "ssh: connect to host 203.0.113.7 port 22: Operation timed out\n", "x");
        assert!(e.starts_with("cannot reach the SSH server"), "{e}");
        let e = explain_failure(&server, "channel 2: open failed: administratively prohibited\n", "x");
        assert!(e.contains("refused to forward"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn password_reaches_ssh_through_askpass_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let got = dir.path().join("got");
        let argv = dir.path().join("argv");
        // A fake ssh that asks its SSH_ASKPASS like the real one, then holds the tunnel.
        let ssh = dir.path().join("fake-ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\necho \"$@\" > '{}'\n[ \"$SSH_ASKPASS_REQUIRE\" = force ] || exit 9\n\"$SSH_ASKPASS\" 'Password:' > '{}'\nexec sleep 30\n",
                argv.display(),
                got.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let t = Tunnels::default();
        t.configure(env_with_server(dir.path(), ssh.to_str().unwrap()));
        let cfg = pg_ssh(SshTunnelConfig { host: "203.0.113.7".into(), auth: SshAuth::Password, ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let plan = t.plan_with(&cfg, Some("pa ss'w\"rd".into()), None).unwrap().unwrap();
        plan.open_on(cfg, port).unwrap();
        let start = Instant::now();
        while std::fs::read_to_string(&got).map_or(true, |t| !t.ends_with('\n')) && start.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&got).unwrap(), "pa ss'w\"rd\n");
        assert!(!std::fs::read_to_string(&argv).unwrap().contains("pa ss"), "never on the command line");
        t.close(Some("pg"));
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
