//! Analysis jobs by id, so an agent can start a run and ask for its results later: the running
//! job, the current results (`Session::inspections.results`) and the last few finished runs.

use std::collections::VecDeque;

use serde::Serialize;

use crate::Session;
use crate::inspections::InspectionResults;

/// Finished runs kept besides the current results.
const KEEP: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Done,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct JobRecord {
    pub id: u64,
    pub status: JobStatus,
    /// The format `analysis.results` answers in by default (`json`, `sarif`, `html`).
    pub format: String,
    /// A copy of the results once done (the live ones are `Session::inspections.results`).
    pub results: Option<Box<InspectionResults>>,
}

#[derive(Debug, Default)]
pub struct Jobs {
    pub list: VecDeque<JobRecord>,
}

impl Jobs {
    pub fn get(&self, id: u64) -> Option<&JobRecord> {
        self.list.iter().find(|j| j.id == id)
    }

    pub fn last_id(&self) -> Option<u64> {
        self.list.back().map(|j| j.id)
    }
}

/// A run started (called by `inspections::start_opts`).
pub(crate) fn started(s: &mut Session, job: u64) {
    let jobs = &mut s.analysis.jobs.list;
    jobs.push_back(JobRecord { id: job, status: JobStatus::Running, format: "json".into(), results: None });
    while jobs.len() > KEEP {
        jobs.pop_front();
    }
    // The open documents' texts the findings will refer to (M15 EA).
    crate::analysis::editor::job_started(s, job);
}

/// A run ended: finished (its results are the session's current ones) or cancelled.
pub(crate) fn ended(s: &mut Session, job: u64, cancelled: bool) {
    let results = if cancelled {
        None
    } else {
        s.inspections.results.as_ref().filter(|r| r.job == job).map(|r| Box::new(r.clone()))
    };
    if let Some(rec) = s.analysis.jobs.list.iter_mut().find(|j| j.id == job) {
        rec.status = if cancelled { JobStatus::Cancelled } else { JobStatus::Done };
        rec.results = results;
    }
    crate::analysis::commit_check::job_ended(s, job, cancelled);
    // Show the findings in the editor (M15 EA).
    crate::analysis::editor::job_ended(s, job, cancelled);
}

/// Remember the format a job's results are wanted in.
pub(crate) fn set_format(s: &mut Session, job: u64, format: &str) {
    if let Some(rec) = s.analysis.jobs.list.iter_mut().find(|j| j.id == job) {
        rec.format = format.to_string();
    }
}
