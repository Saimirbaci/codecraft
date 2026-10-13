//! Dev Containers: `.devcontainer/devcontainer.json` (JSON with comments), started with the
//! `devcontainer` CLI when it is installed, else with plain `docker build`/`docker run` from the
//! configuration's `image` or `build.dockerfile`.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::runs::ProcessCommand;

/// Label CodeCraft (like the `devcontainer` CLI) puts on Dev Containers: the project folder.
pub const LOCAL_FOLDER_LABEL: &str = "devcontainer.local_folder";
/// Largest devcontainer.json read.
const MAX_CONFIG_BYTES: usize = 1024 * 1024;

/// The parts of devcontainer.json CodeCraft uses.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DevContainerConfig {
    pub name: Option<String>,
    pub image: Option<String>,
    /// `build.dockerfile` (or the legacy top-level `dockerFile`), relative to the config file.
    pub dockerfile: Option<String>,
    /// `build.context` (default: the Dockerfile's directory), relative to the config file.
    pub context: Option<String>,
    pub build_args: Vec<(String, String)>,
    /// Where the project is mounted (default `/workspaces/<folder>`).
    pub workspace_folder: Option<String>,
    pub forward_ports: Vec<String>,
    pub container_env: Vec<(String, String)>,
    pub run_args: Vec<String>,
    pub post_create_command: Option<String>,
    pub remote_user: Option<String>,
    /// `dockerComposeFile` configurations need the `devcontainer` CLI.
    pub compose: bool,
}

/// JSON with comments (`//`, `/* */`) and trailing commas, as devcontainer.json allows.
pub fn parse_jsonc(text: &str) -> Result<Value, String> {
    let text = text.trim_start_matches('\u{feff}');
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    serde_json::from_str(&strip_trailing_commas(&out)).map_err(|e| format!("devcontainer.json: {e}"))
}

/// Remove commas directly before `}` or `]` (outside strings).
fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(&n) = chars.get(i + 1) {
                    out.push(n);
                }
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == ',' {
            let next = chars.get(i + 1..).unwrap_or(&[]).iter().find(|n| !n.is_whitespace());
            if matches!(next, Some('}' | ']')) {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

fn string_map(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(Value::as_object)
        .map(|m| {
            m.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect()
        })
        .unwrap_or_default()
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// The fields of a devcontainer.json text.
pub fn parse_config(text_in: &str) -> Result<DevContainerConfig, String> {
    let v = parse_jsonc(text_in)?;
    if !v.is_object() {
        return Err("devcontainer.json: expected an object".into());
    }
    let build = v.get("build");
    let command = |v: Option<&Value>| -> Option<String> {
        match v? {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            Value::Array(a) => {
                let words: Vec<String> = a.iter().filter_map(Value::as_str).map(codecraft_cargo::shell_quote).collect();
                (!words.is_empty()).then(|| words.join(" "))
            }
            Value::Object(m) => {
                let parts: Vec<String> = m.values().filter_map(Value::as_str).map(String::from).collect();
                (!parts.is_empty()).then(|| parts.join(" && "))
            }
            _ => None,
        }
    };
    Ok(DevContainerConfig {
        name: text(v.get("name")),
        image: text(v.get("image")),
        dockerfile: text(build.and_then(|b| b.get("dockerfile"))).or_else(|| text(v.get("dockerFile"))),
        context: text(build.and_then(|b| b.get("context"))).or_else(|| text(v.get("context"))),
        build_args: string_map(build.and_then(|b| b.get("args"))),
        workspace_folder: text(v.get("workspaceFolder")),
        forward_ports: v
            .get("forwardPorts")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|p| match p {
                        Value::Number(n) => Some(n.to_string()),
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        container_env: string_map(v.get("containerEnv")),
        run_args: v
            .get("runArgs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default(),
        post_create_command: command(v.get("postCreateCommand")),
        remote_user: text(v.get("remoteUser")).or_else(|| text(v.get("containerUser"))),
        compose: v.get("dockerComposeFile").is_some(),
    })
}

/// The project's devcontainer.json: `.devcontainer/devcontainer.json`, `.devcontainer.json`, or
/// the first `.devcontainer/<name>/devcontainer.json`.
pub fn find_config(root: &Path) -> Option<PathBuf> {
    let candidates = [root.join(".devcontainer").join("devcontainer.json"), root.join(".devcontainer.json")];
    if let Some(p) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(p);
    }
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(root.join(".devcontainer"))
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("devcontainer.json").is_file())
        .collect();
    subdirs.sort();
    subdirs.first().map(|d| d.join("devcontainer.json"))
}

/// Read and parse a devcontainer.json file.
pub fn read_config(path: &Path) -> Result<DevContainerConfig, String> {
    let text = codecraft_fs::read_text_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if text.len() > MAX_CONFIG_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    parse_config(&text)
}

/// A name usable in container names and image tags (lowercase letters, digits, `-`).
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars().take(60) {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "project".into() } else { s }
}

fn folder_name(root: &Path) -> String {
    root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into())
}

/// The container CodeCraft creates for a project (without the `devcontainer` CLI).
pub fn container_name(root: &Path) -> String {
    format!("codecraft-devcontainer-{}", slug(&folder_name(root)))
}

/// Where the project is mounted in the container.
pub fn workspace_folder(root: &Path, cfg: &DevContainerConfig) -> String {
    cfg.workspace_folder.clone().unwrap_or_else(|| format!("/workspaces/{}", folder_name(root)))
}

fn q(s: &str) -> String {
    codecraft_cargo::shell_quote(s)
}

fn q_path(p: &Path) -> String {
    q(&p.display().to_string())
}

/// The command that builds and starts the Dev Container. With the `devcontainer` CLI:
/// `devcontainer up --workspace-folder <root> --config <file>`; else a `sh -c` script running
/// `docker build` (for `build.dockerfile`), removing an old container, `docker run -d` with the
/// project mounted and a keep-alive entrypoint, then `postCreateCommand`.
pub fn up_command(
    docker: &str,
    cli: Option<&Path>,
    root: &Path,
    config_path: &Path,
    cfg: &DevContainerConfig,
) -> Result<ProcessCommand, String> {
    if let Some(cli) = cli {
        let mut cmd = ProcessCommand::new(cli.display().to_string(), &["up", "--workspace-folder"], root);
        cmd.args.push(root.display().to_string());
        cmd.args.push("--config".into());
        cmd.args.push(config_path.display().to_string());
        return Ok(cmd);
    }
    if cfg.compose {
        return Err(
            "Docker Compose Dev Containers need the `devcontainer` CLI (npm install -g @devcontainers/cli)".into()
        );
    }
    let config_dir = config_path.parent().unwrap_or(root);
    let name = container_name(root);
    let mut steps: Vec<String> = Vec::new();
    let image = match (&cfg.dockerfile, &cfg.image) {
        (Some(dockerfile), _) => {
            let tag = format!("codecraft-devcontainer-{}:latest", slug(&folder_name(root)));
            let file = config_dir.join(dockerfile);
            let context = match &cfg.context {
                Some(c) => config_dir.join(c),
                None => file.parent().map(Path::to_path_buf).unwrap_or_else(|| config_dir.to_path_buf()),
            };
            let mut build = vec![q(docker), "build".into(), "-f".into(), q_path(&file), "-t".into(), q(&tag)];
            for (k, v) in &cfg.build_args {
                build.push("--build-arg".into());
                build.push(q(&format!("{k}={v}")));
            }
            build.push(q_path(&context));
            steps.push(build.join(" "));
            tag
        }
        (None, Some(image)) => image.clone(),
        (None, None) => return Err("devcontainer.json has neither `image` nor `build.dockerfile`".into()),
    };
    steps.push(format!("({} rm -f {} >/dev/null 2>&1; true)", q(docker), q(&name)));
    let wf = workspace_folder(root, cfg);
    let mut run = vec![
        q(docker),
        "run".into(),
        "-d".into(),
        "--name".into(),
        q(&name),
        "--label".into(),
        q(&format!("{LOCAL_FOLDER_LABEL}={}", root.display())),
        "--label".into(),
        q(&format!("devcontainer.config_file={}", config_path.display())),
        "-v".into(),
        q(&format!("{}:{wf}", root.display())),
        "-w".into(),
        q(&wf),
    ];
    for p in &cfg.forward_ports {
        let p = p.trim();
        if !p.is_empty() {
            run.push("-p".into());
            run.push(q(&if p.contains(':') { p.to_string() } else { format!("{p}:{p}") }));
        }
    }
    for (k, v) in &cfg.container_env {
        run.push("-e".into());
        run.push(q(&format!("{k}={v}")));
    }
    if let Some(u) = &cfg.remote_user {
        run.push("-u".into());
        run.push(q(u));
    }
    run.extend(cfg.run_args.iter().map(|a| q(a)));
    run.extend(["--entrypoint".into(), "/bin/sh".into(), q(&image), "-c".into()]);
    run.push(q("trap 'exit 0' TERM INT; while sleep 1000 & wait $!; do :; done"));
    steps.push(run.join(" "));
    if let Some(post) = &cfg.post_create_command {
        steps.push(format!("{} exec -w {} {} sh -c {}", q(docker), q(&wf), q(&name), q(post)));
    }
    Ok(ProcessCommand::shell(steps.join(" && "), root))
}

/// Shell started in the Dev Container's Terminal tab (bash when the image has it).
pub const SHELL_SCRIPT: &str = "if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh; fi";

/// `program args…` for a Terminal in the Dev Container.
pub fn terminal_command(
    docker: &str,
    cli: Option<&Path>,
    root: &Path,
    config_path: &Path,
    cfg: &DevContainerConfig,
) -> (String, Vec<String>) {
    if let Some(cli) = cli {
        let args = vec![
            "exec".into(),
            "--workspace-folder".into(),
            root.display().to_string(),
            "--config".into(),
            config_path.display().to_string(),
            "sh".into(),
            "-c".into(),
            SHELL_SCRIPT.into(),
        ];
        return (cli.display().to_string(), args);
    }
    let args = vec![
        "exec".into(),
        "-it".into(),
        "-w".into(),
        workspace_folder(root, cfg),
        container_name(root),
        "sh".into(),
        "-c".into(),
        SHELL_SCRIPT.into(),
    ];
    (docker.to_string(), args)
}

/// devcontainer.json for File › New › Dev Container Config… (a Rust image).
pub fn template(name: &str) -> String {
    let name = serde_json::to_string(name).unwrap_or_else(|_| "\"project\"".into());
    format!(
        "// Dev Container configuration: https://containers.dev/implementors/json_reference/\n{{\n  \"name\": {name},\n  \"image\": \"rust:1\",\n  // \"build\": {{ \"dockerfile\": \"Dockerfile\" }},\n  \"forwardPorts\": [],\n  \"containerEnv\": {{ \"CARGO_TERM_COLOR\": \"always\" }},\n  \"postCreateCommand\": \"rustup component add rustfmt clippy\"\n}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"// A comment
{
  "name": "Shop", /* block */
  "build": { "dockerfile": "Dockerfile", "context": "..", "args": { "V": "1" }, },
  "forwardPorts": [8080, "9000:9001"],
  "containerEnv": { "URL": "http://x//y" },
  "runArgs": ["--cap-add=SYS_PTRACE"],
  "postCreateCommand": ["cargo", "fetch"],
  "remoteUser": "vscode",
}"#;

    #[test]
    fn parses_jsonc_with_comments_and_trailing_commas() {
        let c = parse_config(CONFIG).unwrap();
        assert_eq!(c.name.as_deref(), Some("Shop"));
        assert_eq!(c.dockerfile.as_deref(), Some("Dockerfile"));
        assert_eq!(c.context.as_deref(), Some(".."));
        assert_eq!(c.build_args, vec![("V".to_string(), "1".to_string())]);
        assert_eq!(c.forward_ports, vec!["8080".to_string(), "9000:9001".to_string()]);
        assert_eq!(c.container_env, vec![("URL".to_string(), "http://x//y".to_string())]);
        assert_eq!(c.post_create_command.as_deref(), Some("cargo fetch"));
        assert_eq!(c.remote_user.as_deref(), Some("vscode"));
        assert!(!c.compose);
        assert!(parse_config("[1]").is_err());
        assert!(parse_config("{").is_err());
        assert_eq!(parse_jsonc("{\"a\": \"/* not a comment */\", \"b\": [1,],}").unwrap()["a"], "/* not a comment */");
        for junk in ["", "/*", "//", "\"", "{\"a\":\"\\", ",,,]"] {
            let _ = parse_jsonc(junk);
        }
    }

    #[test]
    fn up_without_cli_builds_and_runs() {
        let root = Path::new("/p/My Shop");
        let cfg = parse_config(CONFIG).unwrap();
        let cmd = up_command("docker", None, root, &root.join(".devcontainer/devcontainer.json"), &cfg).unwrap();
        assert_eq!(cmd.program, "sh");
        let script = &cmd.args[1];
        assert!(script.contains("docker build -f '/p/My Shop/.devcontainer/Dockerfile' -t codecraft-devcontainer-my-shop:latest --build-arg V=1 '/p/My Shop/.devcontainer/..'"), "{script}");
        assert!(script.contains("rm -f codecraft-devcontainer-my-shop"), "{script}");
        assert!(script.contains("-v '/p/My Shop:/workspaces/My Shop'"), "{script}");
        assert!(script.contains("-p 8080:8080 -p 9000:9001"), "{script}");
        assert!(script.contains("--label 'devcontainer.local_folder=/p/My Shop'"), "{script}");
        assert!(
            script.contains(
                "-u vscode --cap-add=SYS_PTRACE --entrypoint /bin/sh codecraft-devcontainer-my-shop:latest -c"
            ),
            "{script}"
        );
        assert!(script.ends_with("sh -c 'cargo fetch'"), "{script}");
        let image = DevContainerConfig { image: Some("rust:1".into()), ..Default::default() };
        let cmd = up_command("docker", None, root, &root.join(".devcontainer.json"), &image).unwrap();
        assert!(!cmd.args[1].contains(" build "));
        assert!(up_command("docker", None, root, root, &DevContainerConfig::default()).is_err());
        let compose = DevContainerConfig { compose: true, ..Default::default() };
        assert!(up_command("docker", None, root, root, &compose).is_err());
    }

    #[test]
    fn cli_commands_and_terminal() {
        let root = Path::new("/p/app");
        let cfg = DevContainerConfig::default();
        let cli = Path::new("/bin/devcontainer");
        let file = root.join(".devcontainer/devcontainer.json");
        let up = up_command("docker", Some(cli), root, &file, &cfg).unwrap();
        assert_eq!(up.program, "/bin/devcontainer");
        assert_eq!(up.args[..3], ["up", "--workspace-folder", "/p/app"]);
        let (p, a) = terminal_command("docker", Some(cli), root, &file, &cfg);
        assert_eq!((p.as_str(), a[0].as_str()), ("/bin/devcontainer", "exec"));
        let (p, a) = terminal_command("docker", None, root, &file, &cfg);
        assert_eq!(p, "docker");
        assert_eq!(a[..5], ["exec", "-it", "-w", "/workspaces/app", "codecraft-devcontainer-app"]);
        assert_eq!(slug("--"), "project");
        assert!(parse_jsonc(&template("a\"b")).is_ok());
    }

    #[test]
    fn finds_configs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_config(dir.path()).is_none());
        std::fs::create_dir_all(dir.path().join(".devcontainer/rust")).unwrap();
        std::fs::write(dir.path().join(".devcontainer/rust/devcontainer.json"), "{}").unwrap();
        assert!(find_config(dir.path()).unwrap().ends_with("rust/devcontainer.json"));
        std::fs::write(dir.path().join(".devcontainer/devcontainer.json"), "{}").unwrap();
        assert!(find_config(dir.path()).unwrap().ends_with(".devcontainer/devcontainer.json"));
    }
}
