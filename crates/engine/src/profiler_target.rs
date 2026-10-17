//! Profiling on a run target (M16 track RT): a configuration (or a command's `target`) on an
//! SSH host or in a Docker image/container/Compose service is profiled there and the snapshot
//! comes back to be opened here with its paths mapped to the project.
//!
//! - Python: cProfile (CPU, default) and tracemalloc (memory) through CodeCraft's helper script,
//!   which is copied to the target next to the output (in the Docker mount, or uploaded over
//!   SSH) and run by the target's interpreter (pstats files depend on the Python version, so
//!   they are converted there); py-spy when asked for and installed there.
//! - Native: samply, else perf (`perf script` runs there) on the target; Cargo targets are built
//!   there first (`cargo build --message-format=json-render-diagnostics`, the executable read
//!   from its artifacts by the script).
//!
//! The recording is one Run tab process (`ssh … sh -c '…'` then a download, or `docker …`),
//! so Stop and the Run window work as for local recordings. The files land in the project's
//! `.codecraft/remote-run/profile-<time>/`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use codecraft_remote::quote::{quote_path, sh_quote};

use super::{HELPER, JobState, ProfileKind, ProfilerTool, RecordRequest, Recording};
use crate::profiler_convert::Convert;
use crate::profiler_plan::{BuildStep, Target};
use crate::run_targets::RunTarget;
use crate::target_exec::{Ctx, Extras, RUN_DIR, script};
use crate::{CommandError, Session};

/// What a recording on a target may hand back, and how to map it.
#[derive(Clone, Debug)]
pub struct RemoteSnapshot {
    /// Files the recording may have written (the first one that exists is opened).
    candidates: Vec<(PathBuf, Convert)>,
    ctx: Ctx,
}

impl PartialEq for RemoteSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.candidates == other.candidates && self.ctx.target == other.ctx.target
    }
}

impl Eq for RemoteSnapshot {}

/// What runs on the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    CProfile,
    Tracemalloc,
    PySpy,
    /// samply, else perf (`Some`: only that one).
    Native(Option<ProfilerTool>),
}

fn choose(python: bool, kind: ProfileKind, tool: Option<ProfilerTool>, label: &str) -> Result<Tool, CommandError> {
    let unsupported = |what: &str| {
        CommandError::BadParams(format!("{what} can't profile on {label}: CodeCraft runs it only on this machine"))
    };
    match (python, kind, tool) {
        (true, _, Some(ProfilerTool::CProfile)) | (true, ProfileKind::Cpu, None) => Ok(Tool::CProfile),
        (true, _, Some(ProfilerTool::Tracemalloc)) | (true, ProfileKind::Memory, None) => Ok(Tool::Tracemalloc),
        (true, _, Some(ProfilerTool::PySpy)) => Ok(Tool::PySpy),
        (_, _, Some(t @ (ProfilerTool::Samply | ProfilerTool::Perf))) => Ok(Tool::Native(Some(t))),
        (false, ProfileKind::Cpu, None) => Ok(Tool::Native(None)),
        (false, ProfileKind::Memory, None) => Err(CommandError::BadParams(format!(
            "memory profiling of native programs on {label} isn't supported yet: profile CPU there, or memory here"
        ))),
        (false, _, Some(t)) if t.python_only() => {
            Err(CommandError::BadParams(format!("{} profiles Python programs only", t.program())))
        }
        (_, _, Some(t)) => Err(unsupported(t.program())),
    }
}

/// A word of the script: a project path as `"$cc_root"/rel`, anything else quoted.
fn word(local_root: &Path, w: &str) -> String {
    let (prefix, value) = match w.split_once('=') {
        Some((k, v)) if k.starts_with('-') => (format!("{k}="), v),
        _ => (String::new(), w),
    };
    let p = Path::new(value);
    let prefix = if prefix.is_empty() { String::new() } else { sh_quote(&prefix) };
    match codecraft_remote::deploy::rel_path(local_root, p).filter(|_| p.is_absolute()) {
        Some(rel) if rel.is_empty() => format!("{prefix}\"$cc_root\""),
        Some(rel) => format!("{prefix}\"$cc_root\"/{}", sh_quote(&rel)),
        None => sh_quote(w),
    }
}

/// Profile `req.plan` on `target`: one Run tab process that records there and brings the
/// snapshot back. Returns the job id.
pub(super) fn record(s: &mut Session, req: RecordRequest, target: RunTarget) -> Result<u64, CommandError> {
    let RecordRequest { mut plan, kind, tool, seconds, interval } = req;
    if interval.is_some() {
        plan.sample_interval_ms = interval.map(crate::profiler_plan::seconds_to_ms);
    }
    let ctx = Ctx::resolve(s, &target)?;
    let kind = tool.map_or(kind, ProfilerTool::kind);
    let chosen = choose(plan.is_python(), kind, tool, &ctx.label)?;
    let root = ctx.local_root.clone();
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let dir_rel = format!("{RUN_DIR}/profile-{stamp}");
    let local_dir = root.join(&dir_rel);
    std::fs::create_dir_all(&local_dir)
        .map_err(|e| CommandError::Failed(format!("cannot create {}: {e}", local_dir.display())))?;
    let w = |x: &str| word(&root, x);
    let mut text = format!("cd {} || exit 1; cc_root=$(pwd); ", quote_path(&ctx.remote_root));
    text.push_str(&format!("printf '%s%s\\n' '{}' \"$cc_root\"; ", script::ROOT_MARK));
    text.push_str("if [ -f \"$HOME/.cargo/env\" ]; then . \"$HOME/.cargo/env\"; fi; ");
    for (k, v) in &plan.env {
        if !codecraft_remote::quote::is_env_name(k) {
            return Err(CommandError::BadParams(format!("invalid environment variable name `{k}`")));
        }
        text.push_str(&format!("export {k}={}; ", sh_quote(v)));
    }
    text.push_str(&format!("cc_out=\"$cc_root\"/{}; mkdir -p \"$cc_out\"; ", sh_quote(&dir_rel)));
    text.push_str(&format!("cd {} || exit 1; ", w(&plan.cwd.display().to_string())));
    // A time limit runs on the target (Stop from here would end the transfer back too).
    if let Some(secs) = seconds {
        text.push_str(&format!(
            "cc_t=''; command -v timeout >/dev/null 2>&1 && cc_t='timeout -s INT {}'; ",
            secs.max(1)
        ));
    } else {
        text.push_str("cc_t=''; ");
    }
    let mut extras = Extras { sync: true, ..Default::default() };
    let helper_rel = format!("{dir_rel}/codecraft_profile_helper.py");
    let mut candidates: Vec<(PathBuf, Convert)> = Vec::new();
    let tool_label: ProfilerTool;
    match (&plan.target, chosen) {
        (Target::Python { python, interp, module, script: file, args }, tool) => {
            let py = ctx.python_for(Some(Path::new(python)));
            text.push_str(&script::require_program(&py, &script::python_missing_message(&py, &ctx.label)));
            let mut argv: Vec<String> = interp.iter().map(|a| sh_quote(a)).collect();
            let mut target_words: Vec<String> = Vec::new();
            match (module, file) {
                (Some(m), _) => {
                    target_words.push("-m".into());
                    target_words.push(sh_quote(m));
                }
                (None, Some(f)) => target_words.push(w(f)),
                (None, None) => return Err(CommandError::BadParams("the Python command runs no script or module".into())),
            }
            target_words.extend(args.iter().map(|a| w(a)));
            let needs_helper = matches!(tool, Tool::CProfile | Tool::Tracemalloc);
            if needs_helper {
                let helper = local_dir.join("codecraft_profile_helper.py");
                std::fs::write(&helper, HELPER)
                    .map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", helper.display())))?;
                if ctx.is_ssh() {
                    extras.uploads.push((helper, helper_rel.clone()));
                }
            }
            let qpy = sh_quote(&py);
            match tool {
                Tool::CProfile => {
                    argv.extend(["-m".into(), "cProfile".into(), "-o".into(), "\"$cc_out\"/profile.pstats".into()]);
                    argv.extend(target_words);
                    text.push_str(&format!("$cc_t {qpy} {}; cc_st=$?; ", argv.join(" ")));
                    text.push_str(&format!(
                        "if [ -f \"$cc_out\"/profile.pstats ]; then {qpy} \"$cc_out\"/codecraft_profile_helper.py pstats \"$cc_out\"/profile.pstats \"$cc_out\"/profile.pstats.json || cc_st=$?; fi; "
                    ));
                    candidates.push((local_dir.join("profile.pstats.json"), Convert::None));
                    tool_label = ProfilerTool::CProfile;
                }
                Tool::Tracemalloc => {
                    argv.push("\"$cc_out\"/codecraft_profile_helper.py".into());
                    argv.extend(["tracemalloc".into(), "\"$cc_out\"/profile.tracemalloc.json".into()]);
                    let interval = crate::profiler_plan::tracemalloc_interval(&plan);
                    if interval > 0.0 {
                        argv.extend(["--interval".into(), format!("{interval}")]);
                    }
                    argv.push("--".into());
                    argv.extend(target_words);
                    text.push_str(&format!("$cc_t {qpy} {}; cc_st=$?; ", argv.join(" ")));
                    candidates.push((local_dir.join("profile.tracemalloc.json"), Convert::None));
                    tool_label = ProfilerTool::Tracemalloc;
                }
                Tool::PySpy => {
                    text.push_str(&script::require_program("py-spy", &script::py_spy_missing_message(&py, &ctx.label)));
                    let mut all = vec![qpy.clone()];
                    all.extend(argv);
                    all.extend(target_words);
                    text.push_str(&format!(
                        "$cc_t py-spy record --format speedscope -o \"$cc_out\"/profile.speedscope.json -- {}; cc_st=$?; ",
                        all.join(" ")
                    ));
                    candidates.push((local_dir.join("profile.speedscope.json"), Convert::None));
                    tool_label = ProfilerTool::PySpy;
                }
                Tool::Native(_) => return Err(CommandError::BadParams("native profilers can't record Python here".into())),
            }
        }
        (Target::Native { program, args }, Tool::Native(only)) => {
            // Build there first.
            match &plan.build {
                Some(BuildStep::Cargo { invocation, pick }) => {
                    text.push_str(&script::require_program(
                        "cargo",
                        &format!("cargo was not found on {}: install Rust there (https://rustup.rs)", ctx.label),
                    ));
                    let mut inv = invocation.clone();
                    inv.json_messages = false;
                    let mut words: Vec<String> = inv.args().iter().map(|a| w(a)).collect();
                    words.push("--message-format=json-render-diagnostics".into());
                    let filter = match pick {
                        Some(name) => format!("grep -F {} | ", sh_quote(&format!("\"name\":\"{name}\""))),
                        None => String::new(),
                    };
                    if matches!(ctx.target, RunTarget::Docker(_)) {
                        text.push_str("export CARGO_TARGET_DIR=\"$cc_root\"/target/docker; ");
                    }
                    text.push_str(&format!(
                        "cc_exe=$(cargo {} | grep -F '\"reason\":\"compiler-artifact\"' | {filter}sed -n 's/.*\"executable\":\"\\([^\"]*\\)\".*/\\1/p' | tail -n 1); \
                         [ -n \"$cc_exe\" ] || {{ echo 'the build produced no executable to profile' >&2; exit 1; }}; ",
                        words.join(" ")
                    ));
                }
                Some(BuildStep::Shell(cmd)) => {
                    let script_words: Vec<String> = cmd.args.iter().map(|a| w(a)).collect();
                    text.push_str(&format!("( cd {} && {} {} ) || exit $?; ", w(&cmd.cwd.display().to_string()), sh_quote(&cmd.program), script_words.join(" ")));
                    let p = program.as_ref().ok_or_else(|| CommandError::Failed("the program to profile is unknown".into()))?;
                    text.push_str(&format!("cc_exe={}; ", w(&p.display().to_string())));
                }
                None => {
                    let p = program.as_ref().ok_or_else(|| CommandError::Failed("the program to profile is unknown".into()))?;
                    text.push_str(&format!("cc_exe={}; ", w(&p.display().to_string())));
                }
            }
            let rest: Vec<String> = args.iter().map(|a| w(a)).collect();
            let rest = rest.join(" ");
            let samply = format!("$cc_t samply record --save-only -o \"$cc_out\"/profile.json.gz -- \"$cc_exe\" {rest}; cc_st=$?; ");
            let perf = format!(
                "$cc_t perf record -g -o \"$cc_out\"/perf.data -- \"$cc_exe\" {rest}; cc_st=$?; perf script -i \"$cc_out\"/perf.data > \"$cc_out\"/profile.perf.txt 2>/dev/null; "
            );
            let missing = script::missing(&script::native_profiler_missing_message(&ctx.label));
            text.push_str(&match only {
                Some(ProfilerTool::Samply) => format!(
                    "if command -v samply >/dev/null 2>&1; then {samply}else {}; fi; ",
                    script::missing(&format!("samply is not installed on {}: `cargo install --locked samply` there", ctx.label))
                ),
                Some(_) => format!(
                    "if command -v perf >/dev/null 2>&1; then {perf}else {}; fi; ",
                    script::missing(&format!("perf is not installed on {} (Linux only: `apt-get install -y linux-perf`)", ctx.label))
                ),
                None => format!(
                    "if command -v samply >/dev/null 2>&1; then {samply}elif command -v perf >/dev/null 2>&1; then {perf}else {missing}; fi; "
                ),
            });
            let (atos, addr2line) = s.profiler.symbolizers();
            candidates.push((local_dir.join("profile.json.gz"), Convert::Samply { atos, addr2line }));
            candidates.push((local_dir.join("profile.perf.txt"), Convert::None));
            tool_label = only.unwrap_or(ProfilerTool::Samply);
        }
        (Target::Native { .. }, _) => {
            return Err(CommandError::BadParams("Python profilers can't record a native program".into()));
        }
    }
    text.push_str("exit $cc_st");
    if ctx.is_ssh() {
        extras.after.push((dir_rel.clone(), local_dir.clone(), true));
    }
    let wrapped = ctx.process(&text, &extras).map_err(CommandError::Failed)?;
    let title = match kind {
        ProfileKind::Cpu => format!("Profile {} on {}", plan.name, ctx.label),
        ProfileKind::Memory => format!("Profile Memory {} on {}", plan.name, ctx.label),
    };
    let job = s.profiler.new_job(title.clone(), kind, Some(tool_label), JobState::Recording);
    if let Some(j) = s.profiler.job_mut(job) {
        j.debug_build = plan.debug_build;
    }
    s.profiler.missing = None;
    let tab = match crate::runs::start_process_here(s, &wrapped, &title) {
        Ok(t) => t,
        Err(e) => {
            s.profiler.fail_job(Some(job), &e.to_string());
            return Err(e);
        }
    };
    if let Some(t) = s.ws.run_mut(tab) {
        t.push(codecraft_workspace::ConsoleKind::System, format!("Profiling on {} ({})…", ctx.label, tool_label.program()));
    }
    let output = candidates.first().map(|(p, _)| p.clone()).unwrap_or_else(|| local_dir.clone());
    s.profiler.recordings.push(Recording {
        tab,
        tool: tool_label,
        title,
        output,
        attached: None,
        started: Instant::now(),
        stop_requested: None,
        job,
        deadline: None,
        convert: Convert::None,
        remote: Some(RemoteSnapshot { candidates, ctx }),
    });
    if let Some(j) = s.profiler.job_mut(job) {
        j.tab = Some(tab);
    }
    Ok(job)
}

/// The recording on a target ended: the file it handed back (with its paths mapped), or why
/// there is none (`None`: the job failed).
pub(super) fn finish(s: &mut Session, mut rec: Recording, remote: RemoteSnapshot) -> Option<Recording> {
    let lines: Vec<String> = s.ws.run(rec.tab).map(|t| t.lines.iter().map(|l| l.text.clone()).collect()).unwrap_or_default();
    let written = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.len() > 0);
    if let Some((path, convert)) = remote.candidates.iter().find(|(p, _)| written(p)) {
        rec.output = path.clone();
        rec.convert = match remote.ctx.root_abs(lines.iter().map(String::as_str)) {
            Some(from) => Convert::Remap {
                inner: Box::new(convert.clone()),
                from,
                to: remote.ctx.local_root.display().to_string(),
            },
            None => convert.clone(),
        };
        return Some(rec);
    }
    let missing = lines.iter().rev().find_map(|l| script::marker(l, script::MISSING_MARK)).map(str::to_string);
    let msg = missing.unwrap_or_else(|| {
        format!("{}: the profiler on {} wrote no snapshot (see the Run tab for its output)", rec.title, remote.ctx.label)
    });
    if let Some(t) = s.ws.run_mut(rec.tab) {
        t.push(codecraft_workspace::ConsoleKind::System, msg.clone());
    }
    s.profiler.fail_job(Some(rec.job), &msg);
    s.profiler.error = Some(msg.clone());
    s.status = msg;
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_map_project_paths_to_the_target_root() {
        let root = Path::new("/p/app");
        assert_eq!(word(root, "/p/app/main.py"), "\"$cc_root\"/main.py");
        assert_eq!(word(root, "--data=/p/app/d x.csv"), "--data=\"$cc_root\"/'d x.csv'");
        assert_eq!(word(root, "/etc/hosts"), "/etc/hosts");
        assert_eq!(word(root, "a b"), "'a b'");
        assert_eq!(word(root, "/p/app"), "\"$cc_root\"");
    }

    #[test]
    fn tool_choice() {
        assert_eq!(choose(true, ProfileKind::Cpu, None, "x").unwrap(), Tool::CProfile);
        assert_eq!(choose(true, ProfileKind::Memory, None, "x").unwrap(), Tool::Tracemalloc);
        assert_eq!(choose(true, ProfileKind::Cpu, Some(ProfilerTool::PySpy), "x").unwrap(), Tool::PySpy);
        assert_eq!(choose(false, ProfileKind::Cpu, None, "x").unwrap(), Tool::Native(None));
        assert_eq!(choose(false, ProfileKind::Cpu, Some(ProfilerTool::Perf), "x").unwrap(), Tool::Native(Some(ProfilerTool::Perf)));
        assert!(choose(false, ProfileKind::Memory, None, "x").is_err());
        assert!(choose(true, ProfileKind::Cpu, Some(ProfilerTool::Xctrace), "x").is_err());
        assert!(choose(false, ProfileKind::Cpu, Some(ProfilerTool::CProfile), "x").is_err());
        assert!(choose(true, ProfileKind::Memory, Some(ProfilerTool::Memray), "x").is_err());
    }
}
