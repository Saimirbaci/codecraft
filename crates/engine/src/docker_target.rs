//! Docker as a run target: run a configuration inside an image or a running container with the
//! project mounted (M11 track D).
//!
//! An image target becomes `docker run --rm -i -v <project>:<mount> -w <mapped cwd> [-e K=V]
//! [options] <image> <program> <args>`; a container target becomes `docker exec -i -w <mapped
//! cwd> [-e K=V] <container> <program> <args>`; a Compose service target (M16 track RT) becomes
//! `docker compose [-f file] run --rm -T -v <project>:<mount> -w <mapped cwd> [-e K=V] <service>
//! <program> <args>`. Paths under the project in the program and its arguments are mapped to the
//! mount. A Python interpreter set on the target replaces the configuration's (local) one.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::run_config_store::split_words;
use crate::runs::ProcessCommand;
use crate::{CommandError, Session};

/// Where the project is mounted when [`DockerTarget::mount`] is empty.
pub const DEFAULT_MOUNT: &str = "/workspace";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DockerTarget {
    /// Image to run in (`docker run --rm`), e.g. `rust:1`.
    pub image: String,
    /// A running container to run in instead (`docker exec`); empty = use `image`.
    pub container: String,
    /// Where the project is mounted in the container (empty = `/workspace`).
    pub mount: String,
    /// Extra `docker run` options (`-p 8080:80 --network host`), shell-style quoting.
    pub run_options: String,
    /// The Docker connection (a `docker context`); empty = the selected one in Services.
    pub context: String,
    /// A Docker Compose service to run in instead (`docker compose run --rm`), M16 track RT.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub service: String,
    /// The Compose file for `service` (relative = from the project; empty = Compose's default).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub compose_file: String,
    /// The Python interpreter in the container (`/venv/bin/python`); empty = the configuration's
    /// interpreter by name (`python3`). Settings › Python › Remote interpreters sets it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub python: String,
}

impl DockerTarget {
    pub fn label(&self) -> String {
        if !self.service.trim().is_empty() {
            return format!("service {}", self.service.trim());
        }
        if self.container.trim().is_empty() { self.image.clone() } else { format!("container {}", self.container) }
    }

    pub fn mount_path(&self) -> String {
        let m = self.mount.trim().trim_end_matches('/');
        if m.is_empty() { DEFAULT_MOUNT.into() } else { m.to_string() }
    }

    /// `run --rm` targets (an image or a Compose service) publish ports; `exec` targets don't.
    pub fn publishes_ports(&self) -> bool {
        !self.service.trim().is_empty() || self.container.trim().is_empty()
    }
}

/// `path` under `root` as a path under `mount`; other paths unchanged.
pub(crate) fn map_path(path: &Path, root: &Path, mount: &str) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut out = mount.to_string();
    for c in rel.components() {
        out.push('/');
        out.push_str(&c.as_os_str().to_string_lossy());
    }
    Some(out)
}

/// An argument that is (or starts with, as in `--manifest-path=/p/Cargo.toml`) an absolute path
/// under the project, mapped to the mount.
pub(crate) fn map_arg(arg: &str, root: &Path, mount: &str) -> String {
    let root_s = root.display().to_string();
    if root_s.len() <= 1 {
        return arg.to_string();
    }
    let (prefix, value) = match arg.split_once('=') {
        Some((k, v)) if k.starts_with('-') => (format!("{k}="), v),
        _ => (String::new(), arg),
    };
    let under = value == root_s || value.strip_prefix(&root_s).is_some_and(|rest| rest.starts_with('/'));
    if !under {
        return arg.to_string();
    }
    match map_path(Path::new(value), root, mount) {
        Some(m) => format!("{prefix}{m}"),
        None => arg.to_string(),
    }
}

/// The Python interpreter to run on a target for `program`: the target's interpreter when set
/// and `program` is a Python interpreter (`python3`, `/v/bin/python3.12`).
pub fn target_python(python: &str, program: &str) -> Option<String> {
    let python = python.trim();
    let base = Path::new(program).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    (!python.is_empty() && base.starts_with("python")).then(|| python.to_string())
}

/// The `docker …` command running `cmd` on target `t` for a project at `root`.
pub fn wrap_command(
    docker: &str,
    base: &[String],
    t: &DockerTarget,
    root: &Path,
    cmd: &ProcessCommand,
) -> Result<ProcessCommand, String> {
    wrap_command_with(docker, base, t, root, cmd, &[])
}

/// The leading `docker` arguments of target `t` up to (and including) the image, container or
/// service: `[--context c] run --rm -i -v root:mount [-p …] -w <workdir> [-e …] [options] image`.
pub(crate) fn docker_args(
    base: &[String],
    t: &DockerTarget,
    root: &Path,
    workdir: String,
    env: &[(String, String)],
    ports: &[u16],
) -> Result<Vec<String>, String> {
    let image = t.image.trim();
    let container = t.container.trim();
    let service = t.service.trim();
    if image.is_empty() && container.is_empty() && service.is_empty() {
        return Err("the Docker target needs an image, a container or a Compose service".into());
    }
    let mount = t.mount_path();
    let mut args: Vec<String> = if t.context.trim().is_empty() {
        base.to_vec()
    } else {
        vec!["--context".into(), t.context.trim().to_string()]
    };
    let mount_arg = format!("{}:{mount}", root.display());
    if !service.is_empty() {
        args.push("compose".into());
        let file = t.compose_file.trim();
        if !file.is_empty() {
            let f = Path::new(file);
            let f = if f.is_absolute() { f.to_path_buf() } else { root.join(f) };
            args.extend(["-f".into(), f.display().to_string()]);
        }
        args.extend(["run".into(), "--rm".into(), "-T".into(), "-v".into(), mount_arg]);
    } else if container.is_empty() {
        args.extend(["run".into(), "--rm".into(), "-i".into(), "-v".into(), mount_arg]);
    } else {
        args.extend(["exec".into(), "-i".into()]);
    }
    if t.publishes_ports() {
        for p in ports {
            args.extend(["-p".into(), format!("127.0.0.1:{p}:{p}")]);
        }
    }
    args.extend(["-w".into(), workdir]);
    for (k, v) in env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    if !service.is_empty() {
        args.push(service.to_string());
    } else if container.is_empty() {
        args.extend(split_words(&t.run_options).map_err(|e| format!("Docker run options: {e}"))?);
        args.push(image.to_string());
    } else {
        args.push(container.to_string());
    }
    Ok(args)
}

/// [`wrap_command`] publishing `ports` on the host's loopback (`-p 127.0.0.1:P:P`; `run` and
/// Compose targets only: `exec` targets reach a port through a relay instead).
pub fn wrap_command_with(
    docker: &str,
    base: &[String],
    t: &DockerTarget,
    root: &Path,
    cmd: &ProcessCommand,
    ports: &[u16],
) -> Result<ProcessCommand, String> {
    let mount = t.mount_path();
    let workdir = map_path(&cmd.cwd, root, &mount).unwrap_or_else(|| mount.clone());
    let mut env = cmd.env.clone();
    // Linux build artifacts go to their own directory, not over the host's `target/`.
    if cmd.program == "cargo" && !env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR") {
        env.push(("CARGO_TARGET_DIR".into(), format!("{mount}/target/docker")));
    }
    let mut args = docker_args(base, t, root, workdir, &env, ports)?;
    // A host path to a tool (`/usr/local/bin/cargo`) runs by name in the container.
    let program = Path::new(&cmd.program);
    let program = if let Some(py) = target_python(&t.python, &cmd.program) {
        py
    } else if program.is_absolute() {
        map_path(program, root, &mount)
            .or_else(|| program.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| cmd.program.clone())
    } else {
        cmd.program.clone()
    };
    args.push(program);
    args.extend(cmd.args.iter().map(|a| map_arg(a, root, &mount)));
    let mut out = ProcessCommand::new(docker, &[], root);
    out.args = args;
    out.build = cmd.build;
    Ok(out)
}

pub(crate) fn wrap(s: &mut Session, t: &DockerTarget, cmd: ProcessCommand) -> Result<ProcessCommand, CommandError> {
    let root: PathBuf = s.project_root().filter(|r| cmd.cwd.starts_with(r)).unwrap_or_else(|| cmd.cwd.clone());
    let docker = s.docker.program_path().display().to_string();
    let base = s.docker.base_args();
    wrap_command(&docker, &base, t, &root, &cmd).map_err(CommandError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(image: &str) -> DockerTarget {
        DockerTarget { image: image.into(), ..Default::default() }
    }

    #[test]
    fn image_target_mounts_the_project() {
        let mut cmd = ProcessCommand::new(
            "cargo",
            &["run", "--manifest-path=/p/app/Cargo.toml", "/p/data.txt", "/other"],
            "/p/app",
        );
        cmd.env = vec![("RUST_LOG".into(), "debug".into())];
        let t = DockerTarget { run_options: "-p 8080:80 --network 'my net'".into(), ..target("rust:1") };
        let out = wrap_command("docker", &[], &t, Path::new("/p"), &cmd).unwrap();
        assert_eq!(out.program, "docker");
        assert_eq!(out.cwd, Path::new("/p"));
        assert_eq!(
            out.args,
            [
                "run",
                "--rm",
                "-i",
                "-v",
                "/p:/workspace",
                "-w",
                "/workspace/app",
                "-e",
                "RUST_LOG=debug",
                "-e",
                "CARGO_TARGET_DIR=/workspace/target/docker",
                "-p",
                "8080:80",
                "--network",
                "my net",
                "rust:1",
                "cargo",
                "run",
                "--manifest-path=/workspace/app/Cargo.toml",
                "/workspace/data.txt",
                "/other",
            ]
        );
    }

    #[test]
    fn container_target_execs_and_contexts() {
        let cmd = ProcessCommand::new("/usr/bin/python3", &["/p/main.py"], "/p");
        let t = DockerTarget {
            container: "dev".into(),
            mount: "/src/".into(),
            context: "remote".into(),
            ..Default::default()
        };
        let out = wrap_command("/bin/docker", &["--context".into(), "x".into()], &t, Path::new("/p"), &cmd).unwrap();
        assert_eq!(out.args, ["--context", "remote", "exec", "-i", "-w", "/src", "dev", "python3", "/src/main.py"]);
        assert_eq!(t.label(), "container dev");
        assert!(wrap_command("docker", &[], &DockerTarget::default(), Path::new("/p"), &cmd).is_err());
        let bad = DockerTarget { run_options: "'open".into(), ..target("x") };
        assert!(wrap_command("docker", &[], &bad, Path::new("/p"), &cmd).is_err());
        // `/pp` is not under `/p`.
        assert_eq!(map_arg("/pp/x", Path::new("/p"), "/w"), "/pp/x");
        assert_eq!(map_arg("/", Path::new("/"), "/w"), "/");
    }

    #[test]
    fn compose_services_publish_ports_and_use_the_target_python() {
        let cmd = ProcessCommand::new("/p/.venv/bin/python3", &["/p/app/main.py"], "/p/app");
        let t = DockerTarget {
            service: "web".into(),
            compose_file: "deploy/compose.yaml".into(),
            python: "/venv/bin/python".into(),
            ..Default::default()
        };
        assert_eq!(t.label(), "service web");
        assert!(t.publishes_ports());
        let out = wrap_command_with("docker", &[], &t, Path::new("/p"), &cmd, &[5678]).unwrap();
        assert_eq!(
            out.args,
            [
                "compose",
                "-f",
                "/p/deploy/compose.yaml",
                "run",
                "--rm",
                "-T",
                "-v",
                "/p:/workspace",
                "-p",
                "127.0.0.1:5678:5678",
                "-w",
                "/workspace/app",
                "web",
                "/venv/bin/python",
                "/workspace/app/main.py",
            ]
        );
        // `exec` targets publish nothing; an image does.
        let exec = DockerTarget { container: "dev".into(), ..Default::default() };
        assert!(!exec.publishes_ports());
        let out = wrap_command_with("docker", &[], &exec, Path::new("/p"), &cmd, &[1]).unwrap();
        assert!(!out.args.iter().any(|a| a == "-p"), "{:?}", out.args);
        // Without a target interpreter, the project's own venv path is mapped into the mount.
        assert!(out.args.contains(&"/workspace/.venv/bin/python3".to_string()), "{:?}", out.args);
        let img = wrap_command_with("docker", &[], &target("py:3"), Path::new("/p"), &cmd, &[9]).unwrap();
        assert!(img.args.windows(2).any(|w| w == ["-p", "127.0.0.1:9:9"]));
        assert_eq!(target_python("/v/py", "cargo"), None);
        assert_eq!(target_python("", "python3"), None);
        let back: DockerTarget = serde_json::from_str(r#"{"image":"x"}"#).unwrap();
        assert!(back.service.is_empty() && back.python.is_empty());
        assert!(!serde_json::to_string(&back).unwrap().contains("service"));
    }

    #[test]
    fn session_wrap_uses_the_configured_docker_and_round_trips() {
        let mut s = Session::new();
        s.docker.program = "/fake/docker".into();
        s.docker.connection = Some("ctx".into());
        let cmd = ProcessCommand::new("echo", &["hi"], "/tmp");
        let out = wrap(&mut s, &target("alpine"), cmd).unwrap();
        assert_eq!(out.program, "/fake/docker");
        assert_eq!(out.args[..3], ["--context", "ctx", "run"]);
        let rt = crate::run_targets::RunTarget::Docker(target("alpine"));
        let json = serde_json::to_value(&rt).unwrap();
        assert_eq!(json["kind"], "docker");
        assert_eq!(serde_json::from_value::<crate::run_targets::RunTarget>(json).unwrap(), rt);
    }
}
