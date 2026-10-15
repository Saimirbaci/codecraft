//! What a profiling run runs (M14 track PF): a run configuration or an agent's command line is
//! turned into an optional build step (Cargo with `--no-run`, or the build part of a shell
//! chain such as a CMake Application's `cmake --build … && ./app`) and the program itself, so
//! the profiler records the program and not the build tool. [`wrap`] puts the program under a
//! profiler.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use codecraft_cargo::{CargoCommand, CargoInvocation, TargetSelector};

use crate::profiler::ProfilerTool;
use crate::runs::{ProcessCommand, RunConfig};

/// What is measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProfileKind {
    /// Where the time goes.
    #[default]
    Cpu,
    /// What allocated the memory that is still live (or was live at the peak).
    Memory,
}

impl ProfileKind {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "cpu" | "time" | "sampling" => Some(Self::Cpu),
            "memory" | "mem" | "alloc" | "allocations" | "heap" => Some(Self::Memory),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Memory => "Memory",
        }
    }
}

/// Built before the program runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildStep {
    /// `cargo build` / `cargo test --no-run` / `cargo bench --no-run` with JSON messages; the
    /// executable is picked from its artifacts (by target name when there are several).
    Cargo { invocation: CargoInvocation, pick: Option<String> },
    /// The build part of a shell chain.
    Shell(ProcessCommand),
}

/// The program that is profiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A native program (`None`: the executable the Cargo build step produces).
    Native { program: Option<PathBuf>, args: Vec<String> },
    /// A Python program: `python [interp…] (script | -m module) args…`.
    Python { python: String, interp: Vec<String>, module: Option<String>, script: Option<String>, args: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub name: String,
    pub build: Option<BuildStep>,
    pub target: Target,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// A Cargo debug build: timings are not representative of release code.
    pub debug_build: bool,
    /// tracemalloc: milliseconds between memory samples (`None`:
    /// [`DEFAULT_TRACEMALLOC_INTERVAL_MS`], 0: one snapshot at the end only).
    pub sample_interval_ms: Option<u64>,
}

/// Milliseconds between tracemalloc memory samples by default.
pub const DEFAULT_TRACEMALLOC_INTERVAL_MS: u64 = 100;

/// Seconds (a request's `interval`) as milliseconds; hostile values become 0 (off).
pub fn seconds_to_ms(s: f64) -> u64 {
    if s.is_finite() && s > 0.0 { (s.min(3600.0) * 1000.0).round() as u64 } else { 0 }
}

/// The tracemalloc sampling interval of a plan, in seconds (0 = off).
pub fn tracemalloc_interval(plan: &Plan) -> f64 {
    plan.sample_interval_ms.unwrap_or(DEFAULT_TRACEMALLOC_INTERVAL_MS).min(3_600_000) as f64 / 1000.0
}

impl Plan {
    pub fn is_python(&self) -> bool {
        matches!(self.target, Target::Python { .. })
    }

    /// The command line of the program (for titles and errors).
    pub fn describe(&self) -> String {
        let words: Vec<String> = match &self.target {
            Target::Native { program, args } => {
                let p = program.as_ref().map_or_else(|| "<cargo artifact>".to_string(), |p| p.display().to_string());
                std::iter::once(p).chain(args.iter().cloned()).collect()
            }
            Target::Python { python, .. } => std::iter::once(python.clone()).chain(python_argv(&self.target)).collect(),
        };
        words.iter().map(|w| codecraft_cargo::shell_quote(w)).collect::<Vec<_>>().join(" ")
    }
}

/// The arguments after the interpreter of a Python target.
pub fn python_argv(target: &Target) -> Vec<String> {
    let Target::Python { interp, module, script, args, .. } = target else { return Vec::new() };
    let mut out = interp.clone();
    match (module, script) {
        (Some(m), _) => {
            out.push("-m".into());
            out.push(m.clone());
        }
        (None, Some(s)) => out.push(s.clone()),
        (None, None) => {}
    }
    out.extend(args.iter().cloned());
    out
}

/// A program name resolved to a path (so the profiler finds the same program the IDE would).
pub fn resolve_program(program: &str) -> String {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return program.to_string();
    }
    codecraft_lsp::which(program).map_or_else(|| program.to_string(), |p| p.display().to_string())
}

/// The plan for a run configuration (`python`: the project's interpreter, for `pytest`).
pub fn plan_config(config: &RunConfig, python: Option<&Path>) -> Result<Plan, String> {
    if !config.target.is_local() {
        return Err("profiling runs only on this machine (the configuration has a remote target)".into());
    }
    match &config.command {
        Some(cmd) => plan_process(&config.name, cmd, python),
        None => plan_cargo(&config.name, &config.invocation),
    }
}

/// The plan for a command line (`["cargo", "test", "name"]`, `["pytest", "-k", "x"]`,
/// `["./build/app", "--fast"]`).
pub fn plan_command(
    words: &[String],
    cwd: &Path,
    env: Vec<(String, String)>,
    python: Option<&Path>,
) -> Result<Plan, String> {
    let (program, args) = words.split_first().ok_or("the command is empty")?;
    let base = Path::new(program).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if base == "cargo" {
        let mut inv = cargo_from_args(args, cwd).ok_or_else(|| {
            format!("cannot profile `cargo {}`: use cargo run, test or bench", args.first().map_or("", String::as_str))
        })?;
        inv.env = env;
        return plan_cargo(&format!("cargo {}", args.join(" ")), &inv);
    }
    let mut cmd = ProcessCommand::new(program.clone(), &[], cwd);
    cmd.args = args.to_vec();
    cmd.env = env;
    plan_process(&words.join(" "), &cmd, python)
}

fn plan_cargo(name: &str, inv: &CargoInvocation) -> Result<Plan, String> {
    let mut build = inv.clone();
    build.json_messages = true;
    build.test_filter = None;
    build.test_exact = false;
    build.program_args.clear();
    let pick = match &inv.target {
        Some(
            TargetSelector::Bin(n) | TargetSelector::Example(n) | TargetSelector::Test(n) | TargetSelector::Bench(n),
        ) => Some(n.clone()),
        _ => None,
    };
    let filter = inv.test_filter.clone().filter(|f| !f.is_empty());
    let args = match inv.command {
        CargoCommand::Run => {
            build.command = CargoCommand::Build;
            inv.program_args.clone()
        }
        CargoCommand::Test | CargoCommand::Bench => {
            if !build.extra_args.iter().any(|a| a == "--no-run") {
                build.extra_args.push("--no-run".into());
            }
            let mut args = Vec::new();
            if let Some(f) = filter {
                args.push(f);
                if inv.test_exact {
                    args.push("--exact".into());
                }
            }
            if inv.command == CargoCommand::Bench && !inv.program_args.iter().any(|a| a == "--bench") {
                args.push("--bench".into());
            }
            args.extend(inv.program_args.iter().cloned());
            args
        }
        other => {
            return Err(format!(
                "cargo {} configurations can't be profiled; profile a run, test or bench configuration",
                other.subcommand()
            ));
        }
    };
    Ok(Plan {
        name: name.to_string(),
        build: Some(BuildStep::Cargo { invocation: build, pick }),
        target: Target::Native { program: None, args },
        cwd: inv.cwd.clone(),
        env: inv.env.clone(),
        debug_build: !inv.release,
        sample_interval_ms: None,
    })
}

fn plan_process(name: &str, cmd: &ProcessCommand, python: Option<&Path>) -> Result<Plan, String> {
    let base = Path::new(&cmd.program).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let path_like = |p: &str| p.contains('/');
    let in_dir = |dir: &Path, p: &str| crate::normalize_path(&dir.join(p));
    let plan = |target: Target, cwd: PathBuf, build: Option<BuildStep>| Plan {
        name: name.to_string(),
        build,
        target,
        cwd,
        env: cmd.env.clone(),
        debug_build: false,
        sample_interval_ms: None,
    };
    if matches!(base.as_str(), "sh" | "bash" | "zsh") && cmd.args.first().is_some_and(|a| a == "-c") {
        let script = cmd.args.get(1).ok_or("`sh -c` without a script")?;
        let mut steps: Vec<&str> = script.split("&&").map(str::trim).filter(|s| !s.is_empty()).collect();
        let last = steps.pop().ok_or("the shell command is empty")?;
        // `sh -c '<build> && exec "$@"' <arg0> <program> <args…>` (CMake targets, C++ tests at
        // the caret): the program is in the positional arguments. QA M15: profiling a GoogleTest
        // test launched `exec` itself ("Path not found 'exec'").
        let words = if matches!(last, "exec \"$@\"" | "\"$@\"" | "exec \"${@}\"") {
            let positional = cmd.args.get(3..).unwrap_or(&[]).to_vec();
            if positional.is_empty() {
                return Err("the shell command runs no program (`exec \"$@\"` without arguments)".into());
            }
            positional
        } else {
            shell_split(last).ok_or_else(|| format!("cannot split `{last}` into words"))?
        };
        let mut cwd = cmd.cwd.clone();
        if let Some(prev) = steps.last()
            && let Some(dir) = shell_split(prev).and_then(|w| match w.as_slice() {
                [cd, dir] if cd == "cd" => Some(dir.clone()),
                _ => None,
            })
        {
            cwd = in_dir(&cmd.cwd, &dir);
            steps.pop();
        }
        let build = (!steps.is_empty()).then(|| {
            let mut b = ProcessCommand::shell(steps.join(" && "), cmd.cwd.clone());
            b.env = cmd.env.clone();
            b.build = true;
            BuildStep::Shell(b)
        });
        let inner = ProcessCommand {
            program: words.first().cloned().ok_or("the last shell step is empty")?,
            args: words.get(1..).unwrap_or(&[]).to_vec(),
            cwd: cwd.clone(),
            env: cmd.env.clone(),
            build: false,
        };
        let mut p = plan_process(name, &inner, python)?;
        if p.build.is_some() && build.is_some() {
            return Err("nested shell chains can't be profiled".into());
        }
        if build.is_some() {
            p.build = build;
        }
        return Ok(p);
    }
    if base.starts_with("python") {
        let python = if path_like(&cmd.program) {
            in_dir(&cmd.cwd, &cmd.program).display().to_string()
        } else {
            resolve_program(&cmd.program)
        };
        let target = python_target(python, &cmd.args)?;
        return Ok(plan(target, cmd.cwd.clone(), None));
    }
    if base == "pytest" || base == "py.test" {
        let python = python.map_or_else(|| resolve_program("python3"), |p| p.display().to_string());
        let target = Target::Python {
            python,
            interp: Vec::new(),
            module: Some("pytest".into()),
            script: None,
            args: cmd.args.clone(),
        };
        return Ok(plan(target, cmd.cwd.clone(), None));
    }
    let program = if path_like(&cmd.program) {
        in_dir(&cmd.cwd, &cmd.program)
    } else {
        PathBuf::from(resolve_program(&cmd.program))
    };
    Ok(plan(Target::Native { program: Some(program), args: cmd.args.clone() }, cmd.cwd.clone(), None))
}

/// `python [-u -X opt …] (script | -m module) args…`.
fn python_target(python: String, args: &[String]) -> Result<Target, String> {
    let mut interp = Vec::new();
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "-m" || !a.starts_with('-') {
            break;
        }
        if a == "-c" {
            return Err("`python -c` code can't be profiled; save it to a file".into());
        }
        interp.push(a.clone());
        if matches!(a.as_str(), "-X" | "-W" | "-Q") {
            if let Some(v) = args.get(i + 1) {
                interp.push(v.clone());
            }
            i += 1;
        }
        i += 1;
    }
    let rest = args.get(i..).unwrap_or(&[]);
    match rest.split_first() {
        Some((m, more)) if m == "-m" => {
            let (module, args) = more.split_first().ok_or("`python -m` without a module")?;
            Ok(Target::Python { python, interp, module: Some(module.clone()), script: None, args: args.to_vec() })
        }
        Some((script, args)) => {
            Ok(Target::Python { python, interp, module: None, script: Some(script.clone()), args: args.to_vec() })
        }
        None => Err("the Python command runs no script or module".into()),
    }
}

/// A Cargo invocation from `cargo` arguments: run/test/bench with `-p`, `--bin`, `--example`,
/// `--test`, `--bench`, `--lib`, `--release`, `--features`, a test filter and `-- args`.
pub fn cargo_from_args(args: &[String], cwd: &Path) -> Option<CargoInvocation> {
    let (sub, rest) = args.split_first()?;
    let command = match sub.as_str() {
        "run" | "r" => CargoCommand::Run,
        "test" | "t" => CargoCommand::Test,
        "bench" => CargoCommand::Bench,
        _ => return None,
    };
    let mut inv = CargoInvocation::new(command, cwd);
    let mut i = 0;
    while let Some(a) = rest.get(i) {
        let next = rest.get(i + 1).cloned();
        let mut take = |v: Option<String>| {
            i += 1;
            v
        };
        match a.as_str() {
            "--" => {
                let after = rest.get(i + 1..).unwrap_or(&[]);
                let mut after = after.iter().cloned();
                if command != CargoCommand::Run {
                    // libtest: a filter and `--exact` may come after `--` too.
                    for w in after.by_ref() {
                        match w.as_str() {
                            "--exact" => inv.test_exact = true,
                            _ if !w.starts_with('-') && inv.test_filter.is_none() => inv.test_filter = Some(w),
                            _ => inv.program_args.push(w),
                        }
                    }
                } else {
                    inv.program_args.extend(after);
                }
                break;
            }
            "-p" | "--package" => inv.package = take(next),
            "--bin" => inv.target = take(next).map(TargetSelector::Bin),
            "--example" => inv.target = take(next).map(TargetSelector::Example),
            "--test" => inv.target = take(next).map(TargetSelector::Test),
            "--bench" => inv.target = take(next).map(TargetSelector::Bench),
            "--lib" => inv.target = Some(TargetSelector::Lib),
            "--release" | "-r" => inv.release = true,
            "--features" | "-F" => {
                if let Some(f) = take(next) {
                    inv.features.extend(f.split([',', ' ']).filter(|x| !x.is_empty()).map(str::to_string));
                }
            }
            "--all-features" => inv.all_features = true,
            "--no-default-features" => inv.no_default_features = true,
            "--profile" | "--target" | "--manifest-path" | "-j" | "--jobs" => {
                inv.extra_args.push(a.clone());
                if let Some(v) = take(next) {
                    inv.extra_args.push(v);
                }
            }
            s if s.starts_with('-') => inv.extra_args.push(s.to_string()),
            s if command != CargoCommand::Run && inv.test_filter.is_none() => inv.test_filter = Some(s.to_string()),
            _ => return None,
        }
        i += 1;
    }
    Some(inv)
}

/// Split a shell word list (single and double quotes, backslash escapes). `None` for an
/// unterminated quote or shell syntax we don't model (pipes, redirections, `;`).
pub fn shell_split(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        x => cur.push(x),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => cur.push(chars.next()?),
                        x => cur.push(x),
                    }
                }
            }
            '\\' => {
                in_word = true;
                cur.push(chars.next()?);
            }
            '|' | ';' | '<' | '>' | '`' => return None,
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Some(words)
}

/// The profiler's command line for a resolved plan (`program`: the native executable).
pub fn wrap(
    tool: ProfilerTool,
    tool_path: Option<&Path>,
    plan: &Plan,
    program: Option<&Path>,
    output: &Path,
    helper: &Path,
    seconds: Option<u64>,
) -> Result<ProcessCommand, String> {
    let out = output.display().to_string();
    let tool_program = |name: &str| tool_path.map_or_else(|| name.to_string(), |p| p.display().to_string());
    // The profiled program's argv.
    let argv: Vec<String> = match &plan.target {
        Target::Native { args, .. } => {
            let program = program.ok_or("the program to profile is unknown (was it built?)")?;
            std::iter::once(program.display().to_string()).chain(args.iter().cloned()).collect()
        }
        Target::Python { python, .. } => std::iter::once(python.clone()).chain(python_argv(&plan.target)).collect(),
    };
    let python_only = |what: &str| -> Result<(String, Vec<String>), String> {
        match &plan.target {
            Target::Python { python, .. } => Ok((python.clone(), python_argv(&plan.target))),
            Target::Native { .. } => Err(format!("{what} profiles Python programs only")),
        }
    };
    let mut env = plan.env.clone();
    let (program, args): (String, Vec<String>) = match tool {
        ProfilerTool::Samply => {
            let mut a = vec!["record".into(), "--save-only".into(), "-o".into(), out, "--".into()];
            a.extend(argv);
            (tool_program("samply"), a)
        }
        ProfilerTool::Perf => {
            let mut a = vec!["record".into(), "-g".into(), "-o".into(), out, "--".into()];
            a.extend(argv);
            (tool_program("perf"), a)
        }
        ProfilerTool::PySpy => {
            let mut a = vec!["record".into(), "--format".into(), "speedscope".into(), "-o".into(), out, "--".into()];
            a.extend(argv);
            (tool_program("py-spy"), a)
        }
        ProfilerTool::Xctrace => {
            let mut a: Vec<String> = ["record", "--template", "Time Profiler", "--output", &out, "--no-prompt"]
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            a.extend(["--target-stdout".into(), "-".into()]);
            if let Some(s) = seconds {
                a.extend(["--time-limit".into(), format!("{}s", s.max(1))]);
            }
            a.extend(["--launch".into(), "--".into()]);
            a.extend(argv);
            (tool_program("xctrace"), a)
        }
        ProfilerTool::CProfile => {
            let (python, rest) = python_only("cProfile")?;
            let Target::Python { interp, .. } = &plan.target else { return Err("not a Python program".into()) };
            let mut a = interp.clone();
            a.extend(["-m".into(), "cProfile".into(), "-o".into(), out]);
            a.extend(rest.into_iter().skip(interp.len()));
            (python, a)
        }
        ProfilerTool::Tracemalloc => {
            let (python, rest) = python_only("tracemalloc")?;
            let Target::Python { interp, .. } = &plan.target else { return Err("not a Python program".into()) };
            let mut a = interp.clone();
            a.extend([helper.display().to_string(), "tracemalloc".into(), out]);
            let interval = tracemalloc_interval(plan);
            if interval > 0.0 {
                a.extend(["--interval".into(), format!("{interval}")]);
            }
            a.push("--".into());
            a.extend(rest.into_iter().skip(interp.len()));
            (python, a)
        }
        ProfilerTool::Memray => {
            let (python, rest) = python_only("memray")?;
            let Target::Python { interp, .. } = &plan.target else { return Err("not a Python program".into()) };
            match tool_path {
                // A memray installed as a tool runs the program with its own interpreter.
                Some(memray) => {
                    let mut a = vec!["run".into(), "-f".into(), "-o".into(), out];
                    a.extend(rest.into_iter().skip(interp.len()));
                    (memray.display().to_string(), a)
                }
                None => {
                    let mut a = interp.clone();
                    a.extend(["-m".into(), "memray".into(), "run".into(), "-f".into(), "-o".into(), out]);
                    a.extend(rest.into_iter().skip(interp.len()));
                    (python, a)
                }
            }
        }
        ProfilerTool::Leaks => {
            env.retain(|(k, _)| k != "MallocStackLogging");
            env.push(("MallocStackLogging".into(), "1".into()));
            let mut a = vec!["--atExit".into(), format!("--outputGraph={out}"), "--".into()];
            a.extend(argv);
            (tool_program("leaks"), a)
        }
        ProfilerTool::Heaptrack => {
            let mut a = vec!["-o".into(), out];
            a.extend(argv);
            (tool_program("heaptrack"), a)
        }
    };
    Ok(ProcessCommand { program, args, cwd: plan.cwd.clone(), env, build: false })
}

/// The profiler's arguments to record a running process until stopped.
pub fn attach_args(tool: ProfilerTool, pid: u32, output: &Path, seconds: Option<u64>) -> Result<Vec<String>, String> {
    let out = output.display().to_string();
    let pid = pid.to_string();
    Ok(match tool {
        ProfilerTool::Samply => vec!["record".into(), "--save-only".into(), "-o".into(), out, "-p".into(), pid],
        ProfilerTool::Perf => vec!["record".into(), "-g".into(), "-o".into(), out, "-p".into(), pid],
        ProfilerTool::PySpy => {
            vec!["record".into(), "--format".into(), "speedscope".into(), "-o".into(), out, "--pid".into(), pid]
        }
        ProfilerTool::Xctrace => {
            let mut a: Vec<String> = ["record", "--template", "Time Profiler", "--output", &out, "--no-prompt"]
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            if let Some(s) = seconds {
                a.extend(["--time-limit".into(), format!("{}s", s.max(1))]);
            }
            a.extend(["--attach".into(), pid]);
            a
        }
        other => return Err(format!("{} can't attach to a running process", other.program())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(s: &str) -> Vec<String> {
        shell_split(s).unwrap()
    }

    #[test]
    fn shell_words() {
        assert_eq!(words("a 'b c' \"d \\\" e\" f\\ g"), vec!["a", "b c", "d \" e", "f g"]);
        assert_eq!(words("  "), Vec::<String>::new());
        assert_eq!(words("''"), vec![""]);
        assert!(shell_split("a 'b").is_none());
        assert!(shell_split("a | b").is_none());
        assert!(shell_split("a \\").is_none());
    }

    #[test]
    fn cargo_plans_build_first_and_profile_the_executable() {
        let inv = cargo_from_args(&words("test -p core --lib parse::tests::x -- --exact --nocapture"), Path::new("/w"))
            .unwrap();
        assert_eq!(inv.package.as_deref(), Some("core"));
        assert_eq!(inv.test_filter.as_deref(), Some("parse::tests::x"));
        assert!(inv.test_exact);
        let p = plan_cargo("t", &inv).unwrap();
        let Some(BuildStep::Cargo { invocation, pick }) = &p.build else { panic!() };
        assert!(invocation.args().contains(&"--no-run".to_string()));
        assert!(invocation.test_filter.is_none());
        assert_eq!(pick, &None);
        assert_eq!(
            p.target,
            Target::Native {
                program: None,
                args: vec!["parse::tests::x".into(), "--exact".into(), "--nocapture".into()]
            }
        );
        assert!(p.debug_build);
        let bench = cargo_from_args(&words("bench --bench speed fib"), Path::new("/w")).unwrap();
        let p = plan_cargo("b", &bench).unwrap();
        assert_eq!(p.target, Target::Native { program: None, args: vec!["fib".into(), "--bench".into()] });
        let Some(BuildStep::Cargo { pick, .. }) = &p.build else { panic!() };
        assert_eq!(pick.as_deref(), Some("speed"));
        let run = cargo_from_args(&words("run --release --bin app -- --fast 3"), Path::new("/w")).unwrap();
        let p = plan_cargo("r", &run).unwrap();
        let Some(BuildStep::Cargo { invocation, .. }) = &p.build else { panic!() };
        assert_eq!(invocation.command, CargoCommand::Build);
        assert!(!p.debug_build);
        assert_eq!(p.target, Target::Native { program: None, args: vec!["--fast".into(), "3".into()] });
        assert!(cargo_from_args(&words("build"), Path::new("/w")).is_none());
        assert!(cargo_from_args(&words("run extra"), Path::new("/w")).is_none());
        assert!(plan_cargo("x", &CargoInvocation::new(CargoCommand::Check, "/w")).is_err());
    }

    #[test]
    fn process_plans() {
        let cmd = ProcessCommand::new("python3", &["-u", "-X", "dev", "app.py", "--n", "3"], "/p");
        let p = plan_process("py", &cmd, None).unwrap();
        let Target::Python { interp, script, args, module, .. } = &p.target else { panic!() };
        assert_eq!((interp.clone(), script.as_deref(), module.as_deref()), (words("-u -X dev"), Some("app.py"), None));
        assert_eq!(args, &words("--n 3"));
        assert_eq!(python_argv(&p.target), words("-u -X dev app.py --n 3"));
        let p = plan_process("m", &ProcessCommand::new("/v/bin/python", &["-m", "pkg.main", "x"], "/p"), None).unwrap();
        assert_eq!(python_argv(&p.target), words("-m pkg.main x"));
        assert!(plan_process("c", &ProcessCommand::new("python", &["-c", "1"], "/p"), None).is_err());
        let p =
            plan_process("t", &ProcessCommand::new("pytest", &["-k", "fast"], "/p"), Some(Path::new("/v/py"))).unwrap();
        assert_eq!(
            p.target,
            Target::Python {
                python: "/v/py".into(),
                interp: vec![],
                module: Some("pytest".into()),
                script: None,
                args: words("-k fast")
            }
        );
        // A CMake Application chain: build first, then profile the program in its directory.
        let chain = ProcessCommand::shell(
            "{ test -f b/CMakeCache.txt || cmake -S . -B b; } && cmake --build b --target app && cd run && ./b/app 'a b'",
            "/proj",
        );
        let p = plan_process("cmake", &chain, None).unwrap();
        let Some(BuildStep::Shell(b)) = &p.build else { panic!() };
        assert_eq!(b.args[1], "{ test -f b/CMakeCache.txt || cmake -S . -B b; } && cmake --build b --target app");
        assert_eq!(p.cwd, PathBuf::from("/proj/run"));
        assert_eq!(
            p.target,
            Target::Native { program: Some(PathBuf::from("/proj/run/b/app")), args: vec!["a b".into()] }
        );
        // QA M15: the build-then-run wrapper of CMake targets and C++ tests at the caret
        // (`sh -c '<build> && exec "$@"' arg0 <exe> <args…>`) profiled `exec` itself.
        let wrapper = ProcessCommand::new(
            "sh",
            &["-c", "cmake --build b --target t && exec \"$@\"", "codecraft-test", "/proj/b/t", "--gtest_filter=Perf.Phases"],
            "/proj",
        );
        let p = plan_process("t", &wrapper, None).unwrap();
        let Some(BuildStep::Shell(b)) = &p.build else { panic!("{p:?}") };
        assert_eq!(b.args[1], "cmake --build b --target t");
        assert_eq!(
            p.target,
            Target::Native { program: Some(PathBuf::from("/proj/b/t")), args: vec!["--gtest_filter=Perf.Phases".into()] }
        );
        let empty = ProcessCommand::new("sh", &["-c", "make && exec \"$@\"", "x"], "/proj");
        assert!(plan_process("t", &empty, None).is_err());
        let p = plan_process("n", &ProcessCommand::new("./app", &["1"], "/p"), None).unwrap();
        assert_eq!(p.target, Target::Native { program: Some(PathBuf::from("/p/app")), args: vec!["1".into()] });
        assert!(plan_process("e", &ProcessCommand::shell("", "/p"), None).is_err());
        let p = plan_command(&words("cargo test it"), Path::new("/w"), vec![], None).unwrap();
        assert!(matches!(p.build, Some(BuildStep::Cargo { .. })));
        assert!(plan_command(&[], Path::new("/w"), vec![], None).is_err());
        assert!(plan_command(&words("cargo fmt"), Path::new("/w"), vec![], None).is_err());
    }

    #[test]
    fn wrapping() {
        let plan = plan_process("n", &ProcessCommand::new("/bin/app", &["x"], "/p"), None).unwrap();
        let out = Path::new("/o/s.trace");
        let h = Path::new("/h.py");
        let c = wrap(ProfilerTool::Xctrace, None, &plan, Some(Path::new("/bin/app")), out, h, Some(5)).unwrap();
        assert_eq!(c.program, "xctrace");
        assert_eq!(
            c.args.join(" "),
            "record --template Time Profiler --output /o/s.trace --no-prompt --target-stdout - --time-limit 5s --launch -- /bin/app x"
        );
        assert!(wrap(ProfilerTool::CProfile, None, &plan, Some(Path::new("/bin/app")), out, h, None).is_err());
        let c = wrap(ProfilerTool::Leaks, None, &plan, Some(Path::new("/bin/app")), out, h, None).unwrap();
        assert!(c.env.contains(&("MallocStackLogging".into(), "1".into())));
        assert_eq!(c.args[1], "--outputGraph=/o/s.trace");
        let py = plan_process("py", &ProcessCommand::new("/v/python", &["-u", "a.py", "1"], "/p"), None).unwrap();
        let c = wrap(ProfilerTool::CProfile, None, &py, None, Path::new("/o/x.pstats"), h, None).unwrap();
        assert_eq!(
            (c.program.as_str(), c.args.join(" ")),
            ("/v/python", "-u -m cProfile -o /o/x.pstats a.py 1".into())
        );
        let c = wrap(ProfilerTool::Tracemalloc, None, &py, None, Path::new("/o/t.json"), h, None).unwrap();
        assert_eq!(c.args.join(" "), "-u /h.py tracemalloc /o/t.json --interval 0.1 -- a.py 1");
        let mut every = py.clone();
        every.sample_interval_ms = Some(500);
        let c = wrap(ProfilerTool::Tracemalloc, None, &every, None, Path::new("/o/t.json"), h, None).unwrap();
        assert_eq!(c.args.join(" "), "-u /h.py tracemalloc /o/t.json --interval 0.5 -- a.py 1");
        every.sample_interval_ms = Some(0);
        let c = wrap(ProfilerTool::Tracemalloc, None, &every, None, Path::new("/o/t.json"), h, None).unwrap();
        assert_eq!(c.args.join(" "), "-u /h.py tracemalloc /o/t.json -- a.py 1");
        every.sample_interval_ms = Some(seconds_to_ms(f64::NAN));
        assert_eq!(tracemalloc_interval(&every), 0.0);
        let c = wrap(ProfilerTool::Memray, None, &py, None, Path::new("/o/m.bin"), h, None).unwrap();
        assert_eq!(c.args.join(" "), "-u -m memray run -f -o /o/m.bin a.py 1");
        let c = wrap(ProfilerTool::PySpy, Some(Path::new("/t/py-spy")), &py, None, out, h, None).unwrap();
        assert_eq!(c.args.join(" "), "record --format speedscope -o /o/s.trace -- /v/python -u a.py 1");
        assert!(wrap(ProfilerTool::Samply, None, &plan, None, out, h, None).is_err());
        assert_eq!(attach_args(ProfilerTool::Xctrace, 7, out, None).unwrap().last().unwrap(), "7");
        assert!(attach_args(ProfilerTool::CProfile, 7, out, None).is_err());
    }
}
