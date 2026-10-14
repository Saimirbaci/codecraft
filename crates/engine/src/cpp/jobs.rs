//! Background processes with a log (CMake reload, CTest listing, Conan/vcpkg installs, test
//! listings): output is collected on worker threads (codecraft-cargo's runner) and drained in
//! [`super::pump`]. Logs are capped; a job that runs too long can be given a deadline.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};

use codecraft_cargo::{RunEvent, RunHandle};

/// Log lines kept per job.
pub const MAX_LOG_LINES: usize = 20_000;
/// Captured stdout/stderr kept per job for parsing (bytes).
pub const MAX_CAPTURE: usize = 16 * 1024 * 1024;
/// Finished jobs kept.
const MAX_JOBS: usize = 40;

/// What a job is for (decides what happens when it ends).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum JobKind {
    /// `cmake` configure for a profile.
    CmakeReload { profile: String },
    /// `ctest --show-only=json-v1`.
    CtestList,
    /// `conan install` / `vcpkg install`.
    Dependencies { manager: String },
    /// A test executable listing its tests.
    ListTests { executable: PathBuf, framework: crate::testing::Framework },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    Running,
    Succeeded,
    Failed(Option<i32>),
    Stopped,
}

#[derive(Serialize)]
pub struct Job {
    pub id: u64,
    pub kind: JobKind,
    pub title: String,
    pub command_line: String,
    pub status: JobStatus,
    pub lines: Vec<String>,
    /// Lines dropped from the front of the log.
    pub dropped: usize,
    #[serde(skip)]
    pub stdout: String,
    #[serde(skip)]
    pub stderr: String,
    pub duration_ms: Option<u64>,
    #[serde(skip)]
    handle: Option<RunHandle>,
    #[serde(skip)]
    events: Option<Receiver<RunEvent>>,
    #[serde(skip)]
    started: Option<Instant>,
    #[serde(skip)]
    deadline: Option<Duration>,
}

impl Job {
    pub fn is_running(&self) -> bool {
        self.status == JobStatus::Running
    }

    fn log(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > MAX_LOG_LINES {
            let extra = self.lines.len() - MAX_LOG_LINES;
            self.lines.drain(..extra);
            self.dropped += extra;
        }
    }
}

fn capture(buf: &mut String, line: &str) {
    if buf.len() + line.len() < MAX_CAPTURE {
        buf.push_str(line);
        buf.push('\n');
    }
}

#[derive(Default)]
pub struct Jobs {
    pub list: Vec<Job>,
    next: u64,
}

impl Jobs {
    pub fn busy(&self) -> bool {
        self.list.iter().any(Job::is_running)
    }

    pub fn get(&self, id: u64) -> Option<&Job> {
        self.list.iter().find(|j| j.id == id)
    }

    /// A running job of this kind (one reload at a time, …).
    pub fn running(&self, pred: impl Fn(&JobKind) -> bool) -> Option<&Job> {
        self.list.iter().find(|j| j.is_running() && pred(&j.kind))
    }

    /// Start `program args` in `cwd`.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        kind: JobKind,
        title: &str,
        program: &Path,
        args: &[String],
        cwd: &Path,
        env: &[(String, String)],
        deadline: Option<Duration>,
    ) -> Result<u64, String> {
        let command_line = std::iter::once(program.display().to_string())
            .chain(args.iter().cloned())
            .map(|a| codecraft_cargo::shell_quote(&a))
            .collect::<Vec<_>>()
            .join(" ");
        let (handle, events) =
            codecraft_cargo::spawn_process(program, args, cwd, env, false).map_err(|e| format!("{title}: {e}"))?;
        self.next += 1;
        let id = self.next;
        let mut job = Job {
            id,
            kind,
            title: title.to_string(),
            command_line: command_line.clone(),
            status: JobStatus::Running,
            lines: Vec::new(),
            dropped: 0,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: None,
            handle: Some(handle),
            events: Some(events),
            started: Some(Instant::now()),
            deadline,
        };
        job.log(format!("$ {command_line}"));
        self.list.push(job);
        while self.list.len() > MAX_JOBS {
            match self.list.iter().position(|j| !j.is_running()) {
                Some(i) => {
                    self.list.remove(i);
                }
                None => break,
            }
        }
        Ok(id)
    }

    /// Stop a running job.
    pub fn stop(&mut self, id: u64) -> bool {
        let Some(job) = self.list.iter_mut().find(|j| j.id == id && j.is_running()) else { return false };
        if let Some(h) = &job.handle {
            let _ = h.kill();
        }
        job.status = JobStatus::Stopped;
        true
    }

    pub fn stop_all(&mut self) {
        let ids: Vec<u64> = self.list.iter().filter(|j| j.is_running()).map(|j| j.id).collect();
        for id in ids {
            self.stop(id);
        }
    }

    /// Drain events; returns (changed, ids of jobs that finished now).
    pub fn pump(&mut self) -> (bool, Vec<u64>) {
        let mut changed = false;
        let mut finished = Vec::new();
        for job in self.list.iter_mut() {
            let Some(rx) = job.events.clone() else { continue };
            for _ in 0..5000 {
                let Ok(ev) = rx.try_recv() else { break };
                changed = true;
                match ev {
                    RunEvent::Stdout(l) => {
                        let l = crate::runs::strip_ansi(&l);
                        capture(&mut job.stdout, &l);
                        job.log(l);
                    }
                    RunEvent::Stderr(l) => {
                        let l = crate::runs::strip_ansi(&l);
                        capture(&mut job.stderr, &l);
                        job.log(l);
                    }
                    RunEvent::Error(e) => job.log(e),
                    RunEvent::Exited { code, success, duration } => {
                        if job.status == JobStatus::Running {
                            job.status = if success { JobStatus::Succeeded } else { JobStatus::Failed(code) };
                        }
                        job.duration_ms = Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX));
                        let code = code.map_or("signal".to_string(), |c| c.to_string());
                        job.log(format!("Process finished with exit code {code}"));
                        job.events = None;
                        job.handle = None;
                        finished.push(job.id);
                        break;
                    }
                    RunEvent::Started { .. } | RunEvent::Message(_) | RunEvent::Test(_) => {}
                }
            }
            if let (Some(started), Some(deadline), Some(h)) = (job.started, job.deadline, &job.handle)
                && job.status == JobStatus::Running
                && started.elapsed() > deadline
            {
                let _ = h.kill();
                job.log(format!("Stopped after {} s", deadline.as_secs()));
                job.status = JobStatus::Failed(None);
                changed = true;
            }
        }
        (changed, finished)
    }
}

/// A `PathBuf` from a job kind's executable, for display.
pub fn exe_label(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}
