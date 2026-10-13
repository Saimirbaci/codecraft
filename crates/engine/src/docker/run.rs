//! Dockerfile and Docker Compose run configurations, and their gutter ▶ in `Dockerfile` and
//! `compose.yaml`/`docker-compose.yml`.

use std::path::Path;

use crate::runs::{ProcessCommand, RunConfig};

/// The `docker` program for run configurations (resolved on `PATH` and the usual tool dirs).
pub fn docker_program() -> String {
    super::resolve_program("docker").display().to_string()
}

/// `Dockerfile`, `Dockerfile.dev`, `app.dockerfile`, `Containerfile`.
pub fn is_dockerfile(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.starts_with("Dockerfile")
        || name.starts_with("Containerfile")
        || name.to_ascii_lowercase().ends_with(".dockerfile")
}

/// `compose.yaml`, `compose.yml`, `docker-compose.yml`, `docker-compose.yaml` and their
/// `.override`/`.<env>` variants (`docker-compose.dev.yml`).
pub fn is_compose_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    (name.ends_with(".yml") || name.ends_with(".yaml"))
        && (name.starts_with("compose.") || name.starts_with("docker-compose."))
}

/// An image tag for a configuration name (`codecraft-<slug>`).
pub fn image_tag(name: &str) -> String {
    format!("codecraft-{}", super::devcontainer::slug(name))
}

fn q(s: &str) -> String {
    codecraft_cargo::shell_quote(s)
}

/// A Dockerfile configuration: `words` are the Dockerfile (relative to `project`), then
/// `docker run` options (`-p 8080:80 -v data:/data`), then optionally `--` and the container's
/// command. Builds the image (context: the Dockerfile's directory) and runs it with `--rm`; the
/// configuration's environment variables become `-e NAME=value`.
pub fn dockerfile_command(
    name: &str,
    words: &[String],
    project: &Path,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<ProcessCommand, String> {
    let (file, rest) = words.split_first().ok_or("the Dockerfile is missing")?;
    let (options, command) = match rest.iter().position(|w| w == "--") {
        Some(i) => (rest.get(..i).unwrap_or_default(), rest.get(i + 1..).unwrap_or_default()),
        None => (rest, &[][..]),
    };
    let path = project.join(file);
    let context = path.parent().unwrap_or(project);
    let docker = docker_program();
    let tag = image_tag(name);
    let build = [q(&docker), "build".into(), "-f".into(), q(&path.display().to_string()), "-t".into(), q(&tag)]
        .into_iter()
        .chain(std::iter::once(q(&context.display().to_string())))
        .collect::<Vec<_>>()
        .join(" ");
    let mut run = vec![q(&docker), "run".into(), "--rm".into()];
    for (k, v) in env {
        run.push("-e".into());
        run.push(q(&format!("{k}={v}")));
    }
    run.extend(options.iter().map(|o| q(o)));
    run.push(q(&tag));
    run.extend(command.iter().map(|c| q(c)));
    Ok(ProcessCommand::shell(format!("{build} && {}", run.join(" ")), cwd))
}

/// A Docker Compose configuration: `words` are an optional compose file (`*.yml`/`*.yaml`,
/// relative to `project`), then `docker compose` arguments (default `up`): `up`, `up web`,
/// `down`, `up --build -d`. The environment applies to `docker compose` (interpolation).
pub fn compose_command(words: &[String], project: &Path, cwd: &Path, env: &[(String, String)]) -> ProcessCommand {
    let mut args: Vec<String> = vec!["compose".into()];
    let mut rest = words;
    if let Some((first, tail)) = words.split_first() {
        let lower = first.to_ascii_lowercase();
        if lower.ends_with(".yml") || lower.ends_with(".yaml") {
            args.push("-f".into());
            args.push(project.join(first).display().to_string());
            rest = tail;
        }
    }
    if rest.is_empty() {
        args.push("up".into());
    } else {
        args.extend(rest.iter().cloned());
    }
    let mut cmd = ProcessCommand::new(docker_program(), &[], cwd);
    cmd.args = args;
    cmd.env = env.to_vec();
    cmd
}

/// The `services:` line and each service of a compose file: `(line, Some(service))`, with
/// `None` for `services:` itself.
pub fn compose_services(text: &str) -> Vec<(usize, Option<String>)> {
    let mut out = Vec::new();
    let mut in_services = false;
    let mut indent: Option<usize> = None;
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_end();
        let content = t.trim_start();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let lead = t.len() - content.len();
        if lead == 0 {
            in_services = content
                .strip_prefix("services:")
                .is_some_and(|r| r.trim().is_empty() || r.trim_start().starts_with('#'));
            indent = None;
            if in_services {
                out.push((i, None));
            }
            continue;
        }
        if !in_services {
            continue;
        }
        let level = *indent.get_or_insert(lead);
        if lead != level {
            continue;
        }
        let Some((key, _)) = content.split_once(':') else { continue };
        let key = key.trim().trim_matches(|c| c == '"' || c == '\'');
        if !key.is_empty() && !key.contains(' ') && !content.starts_with('-') {
            out.push((i, Some(key.to_string())));
        }
    }
    out
}

/// The first `FROM` line of a Dockerfile.
fn first_from(text: &str) -> Option<usize> {
    text.lines().position(|l| {
        let t = l.trim_start();
        t.get(..4).is_some_and(|w| w.eq_ignore_ascii_case("FROM")) && t.chars().nth(4).is_some_and(char::is_whitespace)
    })
}

/// Gutter ▶ lines of a Dockerfile (its first `FROM`) or compose file (`services:` and each
/// service); `None` for other files.
pub fn gutter_run_lines(path: &Path, text: &str) -> Option<Vec<usize>> {
    if is_dockerfile(path) {
        return Some(first_from(text).into_iter().collect());
    }
    if is_compose_file(path) {
        return Some(compose_services(text).into_iter().map(|(l, _)| l).collect());
    }
    None
}

/// What the gutter ▶ (or Run Context Configuration) runs at `line` of a Dockerfile or compose
/// file; `None` for other files.
pub fn context_config(root: &Path, path: &Path, text: &str, line: usize) -> Option<RunConfig> {
    let file = path.strip_prefix(root).unwrap_or(path).display().to_string();
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if is_dockerfile(path) {
        let config_name = format!("{name}: build and run");
        let cmd = dockerfile_command(&config_name, std::slice::from_ref(&file), root, root, &[]).ok()?;
        return Some(RunConfig::process(config_name, cmd));
    }
    if is_compose_file(path) {
        let service = compose_services(text).into_iter().rfind(|(l, _)| *l <= line).and_then(|(_, s)| s);
        let mut words = vec![file, "up".to_string()];
        let title = match &service {
            Some(s) => {
                words.push(s.clone());
                format!("Compose: {s}")
            }
            None => format!("Compose: {name}"),
        };
        return Some(RunConfig::process(title, compose_command(&words, root, root, &[])));
    }
    None
}

/// Dockerfile for File › New › Dockerfile: a two-stage Rust build when the project has a
/// `Cargo.toml` (`bin`: the binary name), else a minimal image.
pub fn dockerfile_template(rust: bool, bin: &str) -> String {
    if rust {
        format!(
            "# syntax=docker/dockerfile:1\n\nFROM rust:1 AS build\nWORKDIR /app\nCOPY . .\nRUN cargo build --release --locked\n\nFROM debian:bookworm-slim\nCOPY --from=build /app/target/release/{bin} /usr/local/bin/{bin}\nCMD [\"{bin}\"]\n"
        )
    } else {
        "# syntax=docker/dockerfile:1\n\nFROM alpine:3\nWORKDIR /app\nCOPY . .\nCMD [\"sh\"]\n".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn dockerfile_builds_then_runs() {
        let env = vec![("RUST_LOG".to_string(), "debug info".to_string())];
        let cmd = dockerfile_command(
            "My App",
            &words("docker/Dockerfile -p 8080:80 -- serve --port 80"),
            Path::new("/p"),
            Path::new("/p"),
            &env,
        )
        .unwrap();
        assert_eq!(cmd.program, "sh");
        let script = &cmd.args[1];
        assert!(script.contains(" build -f /p/docker/Dockerfile -t codecraft-my-app /p/docker && "), "{script}");
        assert!(
            script.ends_with(" run --rm -e 'RUST_LOG=debug info' -p 8080:80 codecraft-my-app serve --port 80"),
            "{script}"
        );
        assert!(dockerfile_command("x", &[], Path::new("/p"), Path::new("/p"), &[]).is_err());
    }

    #[test]
    fn compose_defaults_to_up() {
        let c = compose_command(&words("compose.yaml"), Path::new("/p"), Path::new("/p"), &[]);
        assert_eq!(c.args, ["compose", "-f", "/p/compose.yaml", "up"]);
        let c = compose_command(&words("down -v"), Path::new("/p"), Path::new("/p"), &[]);
        assert_eq!(c.args, ["compose", "down", "-v"]);
        let c = compose_command(&[], Path::new("/p"), Path::new("/p"), &[]);
        assert_eq!(c.args, ["compose", "up"]);
    }

    #[test]
    fn compose_services_and_gutter() {
        let text = "name: shop\nservices:\n  # the app\n  web:\n    image: web\n    ports:\n      - \"80:80\"\n  \"db\":\n    image: postgres\nvolumes:\n  data:\n";
        assert_eq!(compose_services(text), vec![(1, None), (3, Some("web".to_string())), (7, Some("db".to_string()))]);
        assert_eq!(gutter_run_lines(Path::new("/p/compose.yaml"), text), Some(vec![1, 3, 7]));
        assert_eq!(gutter_run_lines(Path::new("/p/ci.yaml"), text), None);
        let df = "# syntax=docker/dockerfile:1\n\nfrom rust:1 AS build\nFROM debian\n";
        assert_eq!(gutter_run_lines(Path::new("/p/Dockerfile"), df), Some(vec![2]));
        assert_eq!(gutter_run_lines(Path::new("/p/Dockerfile.dev"), "FROMx\n"), Some(vec![]));
        let root = Path::new("/p");
        let c = context_config(root, Path::new("/p/compose.yaml"), text, 5).unwrap();
        assert_eq!(c.name, "Compose: web");
        assert_eq!(c.command.unwrap().args, ["compose", "-f", "/p/compose.yaml", "up", "web"]);
        let c = context_config(root, Path::new("/p/compose.yaml"), text, 1).unwrap();
        assert_eq!(c.command.unwrap().args, ["compose", "-f", "/p/compose.yaml", "up"]);
        let c = context_config(root, Path::new("/p/Dockerfile"), df, 2).unwrap();
        assert!(c.command.unwrap().args[1].contains("build -f /p/Dockerfile -t codecraft-dockerfile-build-and-run /p"));
        assert!(context_config(root, Path::new("/p/main.rs"), "", 0).is_none());
    }

    #[test]
    fn file_kinds() {
        for p in ["Dockerfile", "Dockerfile.dev", "api.Dockerfile", "Containerfile"] {
            assert!(is_dockerfile(Path::new(p)), "{p}");
        }
        for p in ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.prod.yaml"] {
            assert!(is_compose_file(Path::new(p)), "{p}");
        }
        assert!(!is_compose_file(Path::new("compose.json")) && !is_dockerfile(Path::new("docker.md")));
        assert!(dockerfile_template(true, "app").contains("target/release/app"));
        assert!(dockerfile_template(false, "x").starts_with("# syntax"));
    }
}
