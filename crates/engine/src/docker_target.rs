//! Docker as a run target: run a configuration inside an image or a running container with the
//! project mounted (M11 track D).
//!
//! An image target becomes `docker run --rm -i -v <project>:<mount> -w <mapped cwd> [-e K=V]
//! [options] <image> <program> <args>`; a container target becomes `docker exec -i -w <mapped
//! cwd> [-e K=V] <container> <program> <args>`. Paths under the project in the program and its
//! arguments are mapped to the mount.

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
}

impl DockerTarget {
    pub fn label(&self) -> String {
        if self.container.trim().is_empty() { self.image.clone() } else { format!("container {}", self.container) }
    }

    pub fn mount_path(&self) -> String {
        let m = self.mount.trim().trim_end_matches('/');
        if m.is_empty() { DEFAULT_MOUNT.into() } else { m.to_string() }
    }
}

/// `path` under `root` as a path under `mount`; other paths unchanged.
fn map_path(path: &Path, root: &Path, mount: &str) -> Option<String> {
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
fn map_arg(arg: &str, root: &Path, mount: &str) -> String {
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

/// The `docker …` command running `cmd` on target `t` for a project at `root`.
pub fn wrap_command(
    docker: &str,
    base: &[String],
    t: &DockerTarget,
    root: &Path,
    cmd: &ProcessCommand,
) -> Result<ProcessCommand, String> {
    let image = t.image.trim();
    let container = t.container.trim();
    if image.is_empty() && container.is_empty() {
        return Err("the Docker target needs an image or a container".into());
    }
    let mount = t.mount_path();
    let workdir = map_path(&cmd.cwd, root, &mount).unwrap_or_else(|| mount.clone());
    let mut args: Vec<String> = if t.context.trim().is_empty() {
        base.to_vec()
    } else {
        vec!["--context".into(), t.context.trim().to_string()]
    };
    if container.is_empty() {
        args.extend(["run".into(), "--rm".into(), "-i".into(), "-v".into(), format!("{}:{mount}", root.display())]);
    } else {
        args.extend(["exec".into(), "-i".into()]);
    }
    args.extend(["-w".into(), workdir]);
    let mut env = cmd.env.clone();
    // Linux build artifacts go to their own directory, not over the host's `target/`.
    if cmd.program == "cargo" && !env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR") {
        env.push(("CARGO_TARGET_DIR".into(), format!("{mount}/target/docker")));
    }
    for (k, v) in &env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    if container.is_empty() {
        args.extend(split_words(&t.run_options).map_err(|e| format!("Docker run options: {e}"))?);
        args.push(image.to_string());
    } else {
        args.push(container.to_string());
    }
    // A host path to a tool (`/usr/local/bin/cargo`) runs by name in the container.
    let program = Path::new(&cmd.program);
    let program = if program.is_absolute() {
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
