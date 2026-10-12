//! CodeCraft Cargo integration (L0, no workspace deps).
//!
//! `cargo metadata` model, invocation builder for build/check/test/run/clippy/…, a process runner
//! that streams [`RunEvent`]s on a channel (stdout/stderr lines, parsed compiler messages, libtest
//! results, exit), and rustfmt over stdin/stdout.
//!
//! Public signatures are a contract the engine builds on: add items, don't rename or change them.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};

mod formatter;
mod json;
mod libtest;
mod locate;
mod message;
mod metadata;
mod runner;

#[derive(Debug, thiserror::Error)]
pub enum CargoError {
    #[error("failed to run {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{command} failed: {stderr}")]
    Failed { command: String, stderr: String },
    #[error("cannot parse cargo output: {0}")]
    Parse(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CargoError>;

// ---------------------------------------------------------------- metadata

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoMetadata {
    pub workspace_root: PathBuf,
    pub target_directory: PathBuf,
    pub packages: Vec<Package>,
    /// Package ids of the workspace members.
    pub workspace_members: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub version: String,
    pub edition: String,
    pub manifest_path: PathBuf,
    pub targets: Vec<Target>,
    pub dependencies: Vec<Dependency>,
    pub features: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    /// `bin`, `lib`, `rlib`, `proc-macro`, `test`, `example`, `bench`, `custom-build`, …
    pub kind: Vec<String>,
    pub crate_types: Vec<String>,
    pub src_path: PathBuf,
    pub required_features: Vec<String>,
}

impl Target {
    pub fn is_bin(&self) -> bool {
        self.kind.iter().any(|k| k == "bin")
    }

    pub fn is_lib(&self) -> bool {
        self.kind.iter().any(|k| matches!(k.as_str(), "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"))
    }

    pub fn is_test(&self) -> bool {
        self.kind.iter().any(|k| k == "test")
    }

    pub fn is_example(&self) -> bool {
        self.kind.iter().any(|k| k == "example")
    }

    pub fn is_bench(&self) -> bool {
        self.kind.iter().any(|k| k == "bench")
    }

    pub fn is_build_script(&self) -> bool {
        self.kind.iter().any(|k| k == "custom-build")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    pub name: String,
    pub req: String,
    /// `None` = normal, `Some("dev")`, `Some("build")`.
    pub kind: Option<String>,
    pub optional: bool,
}

impl CargoMetadata {
    pub fn workspace_packages(&self) -> impl Iterator<Item = &Package> {
        self.packages.iter().filter(|p| self.workspace_members.contains(&p.id))
    }

    /// The workspace package whose manifest directory is the longest prefix of `path`.
    pub fn package_for_file(&self, path: &Path) -> Option<&Package> {
        self.workspace_packages()
            .filter(|p| p.manifest_path.parent().is_some_and(|d| path.starts_with(d)))
            .max_by_key(|p| p.manifest_path.as_os_str().len())
    }
}

/// `$CARGO`, then `~/.cargo/bin/cargo`, then plain `cargo` (resolved through `PATH`).
pub fn find_cargo() -> PathBuf {
    locate::find_cargo()
}

/// The directory to treat as the Cargo project for `start`: walking up, the outermost
/// `Cargo.toml` that declares `[workspace]`, else the nearest `Cargo.toml`. Returns its path.
pub fn find_manifest(start: &Path) -> Option<PathBuf> {
    locate::find_manifest(start)
}

/// Run `cargo metadata --format-version 1` (with `--no-deps` when asked) in `dir`.
pub fn metadata(dir: &Path, no_deps: bool) -> Result<CargoMetadata> {
    metadata::run_metadata(dir, no_deps)
}

/// Parse `cargo metadata --format-version 1` JSON (unknown fields ignored).
pub fn parse_metadata(json: &str) -> Result<CargoMetadata> {
    metadata::parse_metadata(json)
}

// ---------------------------------------------------------------- invocations

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CargoCommand {
    Build,
    Check,
    Test,
    Run,
    Clippy,
    Doc,
    Clean,
    Bench,
    Update,
    Fmt,
}

impl CargoCommand {
    pub fn subcommand(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Check => "check",
            Self::Test => "test",
            Self::Run => "run",
            Self::Clippy => "clippy",
            Self::Doc => "doc",
            Self::Clean => "clean",
            Self::Bench => "bench",
            Self::Update => "update",
            Self::Fmt => "fmt",
        }
    }

    /// Whether `--message-format=json…` applies.
    pub fn supports_message_format(self) -> bool {
        matches!(self, Self::Build | Self::Check | Self::Test | Self::Run | Self::Clippy | Self::Doc | Self::Bench)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TargetSelector {
    Lib,
    Bin(String),
    Example(String),
    Test(String),
    Bench(String),
    AllTargets,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CargoInvocation {
    pub command: CargoCommand,
    pub cwd: PathBuf,
    pub package: Option<String>,
    pub target: Option<TargetSelector>,
    pub release: bool,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    /// Extra cargo arguments (before `--`).
    pub extra_args: Vec<String>,
    /// For `test`: the name filter (after `--`).
    pub test_filter: Option<String>,
    /// For `test`: pass `--exact` with the filter.
    pub test_exact: bool,
    /// Arguments for the program/test binary (after `--`).
    pub program_args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Add `--message-format=json-diagnostic-rendered-ansi` where supported, so the runner
    /// can emit [`RunEvent::Message`]s.
    pub json_messages: bool,
}

impl CargoInvocation {
    pub fn new(command: CargoCommand, cwd: impl Into<PathBuf>) -> Self {
        Self {
            command,
            cwd: cwd.into(),
            package: None,
            target: None,
            release: false,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            extra_args: Vec::new(),
            test_filter: None,
            test_exact: false,
            program_args: Vec::new(),
            env: Vec::new(),
            json_messages: true,
        }
    }

    /// The argument list after `cargo`, e.g. `["test", "--package", "foo", "--lib", "--", "bar", "--exact"]`.
    ///
    /// Order: subcommand, `--package`, target selector, `--release`, `--features`, `--all-features`,
    /// `--no-default-features`, `--message-format` (when [`Self::json_messages`] and the command
    /// supports it), [`Self::extra_args`], then `--` followed by the test filter, `--exact` and
    /// [`Self::program_args`] if any of those is set. Empty package names, filters and features are
    /// left out.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![self.command.subcommand().to_owned()];
        if let Some(package) = self.package.as_deref().filter(|p| !p.is_empty()) {
            args.push("--package".to_owned());
            args.push(package.to_owned());
        }
        match &self.target {
            None => {}
            Some(TargetSelector::Lib) => args.push("--lib".to_owned()),
            Some(TargetSelector::Bin(name)) => args.extend(["--bin".to_owned(), name.clone()]),
            Some(TargetSelector::Example(name)) => args.extend(["--example".to_owned(), name.clone()]),
            Some(TargetSelector::Test(name)) => args.extend(["--test".to_owned(), name.clone()]),
            Some(TargetSelector::Bench(name)) => args.extend(["--bench".to_owned(), name.clone()]),
            Some(TargetSelector::AllTargets) => args.push("--all-targets".to_owned()),
        }
        if self.release {
            args.push("--release".to_owned());
        }
        let features: Vec<&str> = self.features.iter().map(|f| f.trim()).filter(|f| !f.is_empty()).collect();
        if !features.is_empty() {
            args.push("--features".to_owned());
            args.push(features.join(","));
        }
        if self.all_features {
            args.push("--all-features".to_owned());
        }
        if self.no_default_features {
            args.push("--no-default-features".to_owned());
        }
        if self.json_messages && self.command.supports_message_format() {
            args.push(MESSAGE_FORMAT_ARG.to_owned());
        }
        args.extend(self.extra_args.iter().cloned());
        let filter = self.test_filter.as_deref().filter(|f| !f.is_empty());
        if filter.is_some() || self.test_exact || !self.program_args.is_empty() {
            args.push("--".to_owned());
            if let Some(filter) = filter {
                args.push(filter.to_owned());
            }
            if self.test_exact {
                args.push("--exact".to_owned());
            }
            args.extend(self.program_args.iter().cloned());
        }
        args
    }

    /// Human-readable command line for the Run tool window header (`cargo test --package foo …`).
    /// Arguments that a shell would split or expand are single-quoted (see [`shell_quote`]).
    pub fn command_line(&self) -> String {
        let args: Vec<String> = self.args().iter().map(|a| shell_quote(a)).collect();
        format!("cargo {}", args.join(" "))
    }
}

/// The `--message-format` [`CargoInvocation::args`] adds when JSON messages are wanted.
const MESSAGE_FORMAT_ARG: &str = "--message-format=json-diagnostic-rendered-ansi";

/// Characters a POSIX shell (or zsh) would treat specially somewhere in a word.
const SHELL_SPECIAL: &str = "'\"\\$`;&|<>()*?[]{}!#~^";

/// Quote `arg` for display in a POSIX-shell-like command line: returned as is when it is safe,
/// otherwise wrapped in single quotes (an embedded `'` becomes `'\''`). Empty strings become `''`.
pub fn shell_quote(arg: &str) -> String {
    let needs_quotes =
        arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c.is_control() || SHELL_SPECIAL.contains(c));
    if needs_quotes { format!("'{}'", arg.replace('\'', "'\\''")) } else { arg.to_owned() }
}

// ---------------------------------------------------------------- messages

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
    FailureNote,
    /// Internal compiler error.
    Ice,
}

/// A rustc span; lines and columns are **1-based**, columns count chars, as rustc reports them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSpan {
    /// As reported (relative to the workspace root for local crates).
    pub file_name: String,
    pub line_start: usize,
    pub line_end: usize,
    pub column_start: usize,
    pub column_end: usize,
    pub is_primary: bool,
    pub label: Option<String>,
    pub suggested_replacement: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
    pub code: Option<String>,
    pub spans: Vec<DiagnosticSpan>,
    pub children: Vec<Diagnostic>,
    /// rustc's rendered text (may contain ANSI colour codes).
    pub rendered: Option<String>,
}

impl Diagnostic {
    pub fn primary_span(&self) -> Option<&DiagnosticSpan> {
        self.spans.iter().find(|s| s.is_primary).or(self.spans.first())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CargoMessage {
    CompilerMessage { package_id: String, manifest_path: Option<PathBuf>, diagnostic: Diagnostic },
    CompilerArtifact { package_id: String, target_name: String, executable: Option<PathBuf>, fresh: bool },
    BuildScriptExecuted { package_id: String },
    BuildFinished { success: bool },
}

/// Parse one stdout line of `--message-format=json…`; `None` for non-JSON or unknown reasons.
pub fn parse_message(line: &str) -> Option<CargoMessage> {
    message::parse_message(line)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TestOutcome {
    Ok,
    Failed,
    Ignored,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestEvent {
    /// `running 3 tests`
    SuiteStarted { count: usize },
    /// `test path::name ... ok` (also `... FAILED`, `... ignored`, `... ignored, reason`)
    Result { name: String, outcome: TestOutcome },
    /// `test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s`
    SuiteFinished { passed: usize, failed: usize, ignored: usize, measured: usize, filtered_out: usize },
}

/// Parse one line of libtest's human output.
pub fn parse_test_line(line: &str) -> Option<TestEvent> {
    libtest::parse_test_line(line)
}

// ---------------------------------------------------------------- runner

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunEvent {
    Started { command_line: String },
    Stdout(String),
    Stderr(String),
    Message(CargoMessage),
    Test(TestEvent),
    Exited { code: Option<i32>, success: bool, duration: Duration },
    Error(String),
}

/// A running process. Dropping the handle does not kill the process; call [`RunHandle::kill`].
pub struct RunHandle {
    process: runner::Process,
}

impl RunHandle {
    /// Kill the process (and on Unix its process group, so `cargo run` children die too).
    ///
    /// On Unix this sends `SIGTERM` to the process group and returns; if the run has not finished
    /// after a short grace period, the group gets `SIGKILL`. Calling it again skips the grace
    /// period. The event stream still ends with [`RunEvent::Exited`].
    pub fn kill(&self) -> Result<()> {
        self.process.kill()
    }

    /// `false` once the process has exited and been reaped.
    pub fn is_running(&self) -> bool {
        self.process.is_running()
    }

    pub fn pid(&self) -> u32 {
        self.process.pid()
    }
}

impl std::fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunHandle").field("pid", &self.pid()).field("running", &self.is_running()).finish()
    }
}

/// Start a cargo invocation. Stdout lines that parse as cargo JSON become [`RunEvent::Message`]
/// (`compiler-message` diagnostics also produce a `Stderr` line with the rendered text so the
/// console looks like a terminal); other stdout lines are `Stdout` and also parsed with
/// [`parse_test_line`] (so `test` invocations yield [`RunEvent::Test`]s; the runner does not look at
/// the subcommand). Always ends with exactly one `Exited`.
pub fn spawn(inv: &CargoInvocation) -> Result<(RunHandle, Receiver<RunEvent>)> {
    spawn_process(&find_cargo(), &inv.args(), &inv.cwd, &inv.env, inv.json_messages)
}

/// Start any program with the same streaming (`parse_cargo_json` controls message parsing).
///
/// Stdin is null; `env` is added to the inherited environment; on Unix the process leads a new
/// process group. The first event is [`RunEvent::Started`]; libtest lines on stdout additionally
/// produce [`RunEvent::Test`] after their `Stdout` line.
pub fn spawn_process(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    parse_cargo_json: bool,
) -> Result<(RunHandle, Receiver<RunEvent>)> {
    let (process, events) = runner::spawn(program, args, cwd, env, parse_cargo_json)?;
    Ok((RunHandle { process }, events))
}

/// Format Rust source with `rustfmt --edition <edition> --emit stdout` over stdin.
///
/// rustfmt is looked up next to [`find_cargo`], then in `~/.cargo/bin`, then on `PATH`; it runs in
/// `cwd` (when that is a directory) so it picks up the project's `rustfmt.toml`. An empty
/// `edition` leaves rustfmt's default.
pub fn rustfmt(source: &str, edition: &str, cwd: &Path) -> Result<String> {
    formatter::rustfmt(source, edition, cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = "--message-format=json-diagnostic-rendered-ansi";

    fn plain(command: CargoCommand) -> CargoInvocation {
        let mut inv = CargoInvocation::new(command, "/p");
        inv.json_messages = false;
        inv
    }

    #[test]
    fn defaults() {
        let inv = CargoInvocation::new(CargoCommand::Build, "/p");
        assert!(inv.json_messages);
        assert_eq!(inv.args(), ["build", JSON]);
        assert_eq!(plain(CargoCommand::Build).args(), ["build"]);
        assert_eq!(inv.command_line(), format!("cargo build {JSON}"));
    }

    #[test]
    fn every_subcommand() {
        for (command, name, json) in [
            (CargoCommand::Build, "build", true),
            (CargoCommand::Check, "check", true),
            (CargoCommand::Test, "test", true),
            (CargoCommand::Run, "run", true),
            (CargoCommand::Clippy, "clippy", true),
            (CargoCommand::Doc, "doc", true),
            (CargoCommand::Bench, "bench", true),
            (CargoCommand::Clean, "clean", false),
            (CargoCommand::Update, "update", false),
            (CargoCommand::Fmt, "fmt", false),
        ] {
            let args = CargoInvocation::new(command, "/p").args();
            assert_eq!(args[0], name);
            assert_eq!(args.iter().any(|a| a == JSON), json, "{name}");
            assert_eq!(plain(command).args(), [name]);
        }
    }

    #[test]
    fn target_selectors() {
        for (target, expected) in [
            (TargetSelector::Lib, vec!["--lib"]),
            (TargetSelector::Bin("app".into()), vec!["--bin", "app"]),
            (TargetSelector::Example("demo".into()), vec!["--example", "demo"]),
            (TargetSelector::Test("smoke".into()), vec!["--test", "smoke"]),
            (TargetSelector::Bench("speed".into()), vec!["--bench", "speed"]),
            (TargetSelector::AllTargets, vec!["--all-targets"]),
        ] {
            let mut inv = plain(CargoCommand::Build);
            inv.target = Some(target);
            let mut want = vec!["build"];
            want.extend(expected);
            assert_eq!(inv.args(), want);
        }
    }

    #[test]
    fn individual_flags() {
        type Case = (fn(&mut CargoInvocation), &'static [&'static str]);
        let cases: [Case; 9] = [
            (|i| i.package = Some("core".into()), &["--package", "core"]),
            (|i| i.package = Some(String::new()), &[]),
            (|i| i.release = true, &["--release"]),
            (|i| i.features = vec!["a".into(), " b ".into(), String::new()], &["--features", "a,b"]),
            (|i| i.features = vec![String::new()], &[]),
            (|i| i.all_features = true, &["--all-features"]),
            (|i| i.no_default_features = true, &["--no-default-features"]),
            (|i| i.extra_args = vec!["--offline".into(), "-j".into(), "2".into()], &["--offline", "-j", "2"]),
            (|i| i.env = vec![("RUST_LOG".into(), "debug".into())], &[]),
        ];
        for (set, expected) in cases {
            let mut inv = plain(CargoCommand::Test);
            set(&mut inv);
            let mut want = vec!["test"];
            want.extend_from_slice(expected);
            assert_eq!(inv.args(), want);
        }
    }

    #[test]
    fn after_double_dash() {
        let mut inv = plain(CargoCommand::Test);
        inv.test_filter = Some("tests::adds".into());
        assert_eq!(inv.args(), ["test", "--", "tests::adds"]);
        inv.test_exact = true;
        assert_eq!(inv.args(), ["test", "--", "tests::adds", "--exact"]);
        inv.program_args = vec!["--nocapture".into()];
        assert_eq!(inv.args(), ["test", "--", "tests::adds", "--exact", "--nocapture"]);

        let mut exact_only = plain(CargoCommand::Test);
        exact_only.test_exact = true;
        assert_eq!(exact_only.args(), ["test", "--", "--exact"]);

        let mut run = plain(CargoCommand::Run);
        run.program_args = vec!["serve".into(), "--port".into(), "80".into()];
        assert_eq!(run.args(), ["run", "--", "serve", "--port", "80"]);

        let mut empty_filter = plain(CargoCommand::Test);
        empty_filter.test_filter = Some(String::new());
        assert_eq!(empty_filter.args(), ["test"]);
    }

    #[test]
    fn full_order() {
        let mut inv = CargoInvocation::new(CargoCommand::Test, "/p");
        inv.package = Some("core".into());
        inv.target = Some(TargetSelector::Test("smoke".into()));
        inv.release = true;
        inv.features = vec!["serde".into(), "derive".into()];
        inv.all_features = true;
        inv.no_default_features = true;
        inv.extra_args = vec!["--offline".into()];
        inv.test_filter = Some("it_works".into());
        inv.test_exact = true;
        inv.program_args = vec!["--nocapture".into(), "--test-threads=1".into()];
        assert_eq!(
            inv.args(),
            [
                "test",
                "--package",
                "core",
                "--test",
                "smoke",
                "--release",
                "--features",
                "serde,derive",
                "--all-features",
                "--no-default-features",
                JSON,
                "--offline",
                "--",
                "it_works",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ]
        );
        assert_eq!(
            inv.command_line(),
            format!(
                "cargo test --package core --test smoke --release --features serde,derive --all-features \
                 --no-default-features {JSON} --offline -- it_works --exact --nocapture --test-threads=1"
            )
        );
    }

    #[test]
    fn command_line_quoting() {
        let mut inv = plain(CargoCommand::Run);
        inv.program_args = vec!["hello world".into(), "it's".into(), String::new(), "$HOME".into(), "a;b".into()];
        assert_eq!(inv.command_line(), r"cargo run -- 'hello world' 'it'\''s' '' '$HOME' 'a;b'");
    }

    #[test]
    fn shell_quoting() {
        for safe in ["test", "--package", "--features=a,b", "src/lib.rs", "tests::adds", "x@1.0", "50%", "+nightly"] {
            assert_eq!(shell_quote(safe), safe);
        }
        for (raw, quoted) in [
            ("", "''"),
            ("a b", "'a b'"),
            ("tab\there", "'tab\there'"),
            ("line\nbreak", "'line\nbreak'"),
            ("it's", r"'it'\''s'"),
            ("\"q\"", "'\"q\"'"),
            ("a|b", "'a|b'"),
            ("glob*", "'glob*'"),
            ("~/x", "'~/x'"),
            ("#c", "'#c'"),
            ("(x)", "'(x)'"),
            ("`cmd`", "'`cmd`'"),
            ("back\\slash", "'back\\slash'"),
        ] {
            assert_eq!(shell_quote(raw), quoted, "{raw:?}");
        }
    }

    #[test]
    fn invocation_round_trips_through_serde() {
        let mut inv = CargoInvocation::new(CargoCommand::Clippy, "/p");
        inv.target = Some(TargetSelector::AllTargets);
        inv.env = vec![("A".into(), "1".into())];
        let json = serde_json::to_string(&inv).unwrap();
        assert_eq!(serde_json::from_str::<CargoInvocation>(&json).unwrap(), inv);
    }

    #[test]
    fn target_kinds() {
        let target = |kind: &str| Target { kind: vec![kind.into()], ..Target::default() };
        for lib in ["lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"] {
            assert!(target(lib).is_lib(), "{lib}");
            assert!(!target(lib).is_bin());
        }
        assert!(target("bin").is_bin());
        assert!(target("test").is_test());
        assert!(target("example").is_example());
        assert!(target("bench").is_bench());
        assert!(target("custom-build").is_build_script());
        assert!(!target("custom-build").is_lib());
    }

    #[test]
    fn diagnostic_primary_span_falls_back_to_first() {
        let span = |primary: bool, line: usize| DiagnosticSpan {
            file_name: "a.rs".into(),
            line_start: line,
            line_end: line,
            column_start: 1,
            column_end: 2,
            is_primary: primary,
            label: None,
            suggested_replacement: None,
        };
        let mut diagnostic = Diagnostic {
            level: Level::Error,
            message: "m".into(),
            code: None,
            spans: vec![span(false, 1), span(true, 2)],
            children: vec![],
            rendered: None,
        };
        assert_eq!(diagnostic.primary_span().map(|s| s.line_start), Some(2));
        diagnostic.spans = vec![span(false, 3), span(false, 4)];
        assert_eq!(diagnostic.primary_span().map(|s| s.line_start), Some(3));
        diagnostic.spans.clear();
        assert_eq!(diagnostic.primary_span(), None);
    }
}
