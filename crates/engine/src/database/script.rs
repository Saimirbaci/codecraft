//! Run SQL file (track DC): every statement of an attached `.sql` file on the file's
//! connection, with an error mode (stop on error, continue, or ask), one result tab per
//! statement that returns rows, and an execution log (per statement: line, time, rows, error).
//!
//! A run is sent to the worker in segments: the whole rest of the script at once; in "ask" mode
//! a failure stops the segment and the user decides whether the next segment starts after the
//! failed statement. Statement outcomes arrive through [`on_statement`]; [`on_run_done`] closes
//! a segment.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use codecraft_db::{Dialect, ExecSummary, split_statements};

use super::{DbState, OutputLevel, RunSpec};
use crate::Session;

/// Script runs kept (older logs are dropped).
const MAX_SCRIPTS: usize = 20;
/// Largest script run from a file (chars).
pub const MAX_SCRIPT_CHARS: usize = 20_000_000;
/// Statements of one script at most.
pub const MAX_STATEMENTS: usize = 100_000;

/// What happens when a statement of a script fails.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScriptMode {
    /// Stop at the first error (the rest is skipped).
    #[default]
    Stop,
    /// Run the remaining statements anyway.
    Continue,
    /// Stop and ask whether to continue.
    Ask,
}

impl ScriptMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stop" | "stoponerror" | "stop-on-error" => Some(Self::Stop),
            "continue" | "ignore" | "continueonerror" => Some(Self::Continue),
            "ask" | "prompt" => Some(Self::Ask),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stop => "Stop on error",
            Self::Continue => "Continue on error",
            Self::Ask => "Ask on error",
        }
    }

    pub const ALL: [ScriptMode; 3] = [Self::Stop, Self::Continue, Self::Ask];
}

/// The outcome of one statement of a script.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum StmtStatus {
    Pending,
    Done {
        ms: u64,
        /// Rows fetched (statements that return rows).
        #[serde(skip_serializing_if = "Option::is_none")]
        rows: Option<u64>,
        /// Rows inserted, updated or deleted.
        #[serde(skip_serializing_if = "Option::is_none")]
        affected: Option<u64>,
    },
    Failed {
        error: String,
    },
    Skipped,
}

/// One statement of a script.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScriptStmt {
    pub text: String,
    /// Zero-based line where it starts in the file.
    pub line: usize,
    pub status: StmtStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ScriptPhase {
    Running,
    /// Stopped at the failed statement, waiting for Continue or Stop.
    Asking,
    Done,
    Stopped,
}

/// A Run 'file.sql' and its execution log.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptRun {
    pub id: u64,
    pub path: PathBuf,
    /// The file name.
    pub title: String,
    pub source: String,
    pub worker: String,
    pub mode: ScriptMode,
    pub statements: Vec<ScriptStmt>,
    pub state: ScriptPhase,
    /// Wall time of the whole run once it ended.
    pub elapsed_ms: Option<u64>,
    #[serde(skip)]
    started: Option<Instant>,
    /// The worker run of the segment in flight and its first statement.
    #[serde(skip)]
    segment: Option<(u64, usize)>,
    #[serde(skip)]
    segments: usize,
    #[serde(skip)]
    manual: bool,
}

impl ScriptRun {
    pub fn failed(&self) -> usize {
        self.statements.iter().filter(|s| matches!(s.status, StmtStatus::Failed { .. })).count()
    }

    pub fn done(&self) -> usize {
        self.statements.iter().filter(|s| matches!(s.status, StmtStatus::Done { .. })).count()
    }

    /// The failed statement a run in "ask" mode stopped at.
    pub fn asking_at(&self) -> Option<usize> {
        (self.state == ScriptPhase::Asking)
            .then(|| self.statements.iter().rposition(|s| matches!(s.status, StmtStatus::Failed { .. })))
            .flatten()
    }

    /// `12 statements: 11 succeeded, 1 failed in 40 ms`.
    pub fn summary(&self) -> String {
        let n = self.statements.len();
        let mut s = format!("{n} statement{}: {} succeeded", if n == 1 { "" } else { "s" }, self.done());
        let failed = self.failed();
        if failed > 0 {
            s.push_str(&format!(", {failed} failed"));
        }
        let skipped = self.statements.iter().filter(|s| s.status == StmtStatus::Skipped).count();
        if skipped > 0 {
            s.push_str(&format!(", {skipped} skipped"));
        }
        if let Some(ms) = self.elapsed_ms {
            s.push_str(&format!(" in {ms} ms"));
        }
        s
    }
}

/// The production guard's question for a script.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptConfirm {
    pub path: PathBuf,
    pub source: String,
    pub source_name: String,
    pub mode: ScriptMode,
    pub writes: Vec<String>,
}

/// Script runs of the session.
#[derive(Debug, Default)]
pub struct ScriptState {
    pub runs: Vec<ScriptRun>,
    /// The execution log shown in Services (instead of the Output log).
    pub shown: Option<u64>,
    /// A script waiting for the production guard's confirmation.
    pub confirm: Option<ScriptConfirm>,
    next: u64,
}

impl ScriptState {
    pub fn get(&self, id: u64) -> Option<&ScriptRun> {
        self.runs.iter().find(|r| r.id == id)
    }

    /// The latest run of a file.
    pub fn latest_for(&self, path: &Path) -> Option<&ScriptRun> {
        self.runs.iter().rev().find(|r| r.path == path)
    }

    /// Whether a script is running or waiting for an answer.
    pub fn busy(&self) -> bool {
        self.runs.iter().any(|r| r.state == ScriptPhase::Running)
    }
}

/// Split a script into statements with their (zero-based) start lines.
pub fn split(sql: &str, dialect: Dialect) -> Vec<(String, usize)> {
    let stmts = split_statements(sql, dialect);
    let mut out = Vec::with_capacity(stmts.len().min(MAX_STATEMENTS));
    let mut line = 0usize;
    let mut at = 0usize;
    let mut chars = sql.chars();
    for st in stmts.into_iter().take(MAX_STATEMENTS) {
        // Count newlines up to the statement's start (offsets only grow).
        let skip = st.start.saturating_sub(at);
        line += chars.by_ref().take(skip).filter(|c| *c == '\n').count();
        at = at.max(st.start);
        // Comments before the code belong to the statement; its line is the code's.
        let lead = st
            .text
            .lines()
            .take_while(|l| {
                let t = l.trim();
                t.is_empty() || t.starts_with("--")
            })
            .count();
        out.push((st.text, line + lead));
    }
    out
}

/// What [`start`] needs.
pub(crate) struct ScriptStart {
    pub path: PathBuf,
    pub text: String,
    pub source: String,
    pub worker: String,
    pub mode: ScriptMode,
    pub confirmed: bool,
    pub values: Option<BTreeMap<String, String>>,
}

/// Start running a script; returns its id (or `Ok(None)` when it waits for confirmation).
pub(crate) fn start(s: &mut Session, req: ScriptStart) -> Result<Option<u64>, String> {
    let ScriptStart { path, text, source, worker, mode, confirmed, values } = req;
    if text.chars().count() > MAX_SCRIPT_CHARS {
        return Err(format!("the script is larger than {MAX_SCRIPT_CHARS} characters"));
    }
    let cfg = s.db.source(&source).map(|d| d.config.clone()).ok_or_else(|| format!("no data source `{source}`"))?;
    let dialect = cfg.kind.dialect();
    let mut stmts = split(&text, dialect);
    if stmts.is_empty() {
        return Err("nothing to execute".into());
    }
    // Parameters (`?`, `:name`, …) need values.
    let mut names: Vec<String> = Vec::new();
    for (st, _) in &stmts {
        for n in codecraft_db::params::param_names(st, dialect) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    if !names.is_empty() {
        let Some(values) = &values else {
            return Err(format!("the script has parameters ({}): pass their values in `params`", names.join(", ")));
        };
        for (st, _) in &mut stmts {
            *st = codecraft_db::params::substitute(st, dialect, values)
                .map_err(|n| format!("no value for parameter `{n}`"))?;
        }
    }
    let writes: Vec<String> =
        stmts.iter().filter(|(st, _)| codecraft_db::is_modifying(st, dialect)).map(|(st, _)| st.clone()).collect();
    if !writes.is_empty() && cfg.read_only {
        let first = writes.first().map(|w| w.lines().next().unwrap_or("").to_string()).unwrap_or_default();
        return Err(format!("{} is read-only; this statement modifies data: {first}", cfg.name));
    }
    if !writes.is_empty() && cfg.confirm_writes && !confirmed {
        s.db.scripts.confirm = Some(ScriptConfirm { path, source, source_name: cfg.name.clone(), mode, writes });
        s.status = format!("Confirm data modification on {}", cfg.name);
        return Ok(None);
    }
    s.db.scripts.confirm = None;
    // A new run of the file replaces its earlier (unpinned) result tabs.
    let prefix = format!("script:{}#", path.display());
    let stale: Vec<u64> =
        s.db.results
            .iter()
            .filter(|r| r.owner.starts_with(&prefix) && !r.is_running() && !s.db.nav.pinned.contains(&r.id))
            .map(|r| r.id)
            .collect();
    s.db.results.retain(|r| !stale.contains(&r.id));
    if s.db.active_result.is_some_and(|a| stale.contains(&a)) {
        s.db.active_result = None;
    }
    s.db.scripts.next += 1;
    let id = s.db.scripts.next;
    let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "script".into());
    s.db.scripts.runs.push(ScriptRun {
        id,
        path,
        title: title.clone(),
        source: source.clone(),
        worker,
        mode,
        statements: stmts
            .into_iter()
            .map(|(text, line)| ScriptStmt { text, line, status: StmtStatus::Pending })
            .collect(),
        state: ScriptPhase::Running,
        elapsed_ms: None,
        started: Some(Instant::now()),
        segment: None,
        segments: 0,
        manual: cfg.manual_commit,
    });
    if s.db.scripts.runs.len() > MAX_SCRIPTS {
        let extra = s.db.scripts.runs.len() - MAX_SCRIPTS;
        s.db.scripts.runs.drain(..extra);
    }
    s.db.scripts.shown = Some(id);
    s.db.active_result = None;
    s.db.log(OutputLevel::Info, &source, format!("Run '{title}' ({})", mode.label()));
    start_segment(s, id, 0)?;
    Ok(Some(id))
}

/// Send statements `from..` of script `id` to its worker.
fn start_segment(s: &mut Session, id: u64, from: usize) -> Result<(), String> {
    let Some(sc) = s.db.scripts.runs.iter_mut().find(|r| r.id == id) else { return Err(format!("no script {id}")) };
    let statements: Vec<String> = sc.statements.iter().skip(from).map(|st| st.text.clone()).collect();
    if statements.is_empty() {
        finish(&mut s.db, id, ScriptPhase::Done);
        return Ok(());
    }
    sc.segments += 1;
    sc.state = ScriptPhase::Running;
    let spec = RunSpec {
        worker: sc.worker.clone(),
        source: sc.source.clone(),
        owner: format!("script:{}#{}", sc.path.display(), sc.segments),
        title: sc.title.clone(),
        statements,
        manual: sc.manual,
        keep_going: sc.mode == ScriptMode::Continue,
        ..RunSpec::default()
    };
    match s.db.start_run(spec) {
        Ok(run) => {
            if let Some(sc) = s.db.scripts.runs.iter_mut().find(|r| r.id == id) {
                sc.segment = Some((run, from));
            }
            s.status =
                format!("Running {} statement(s)…", s.db.scripts.get(id).map_or(0, |r| r.statements.len() - from));
            Ok(())
        }
        Err(e) => {
            finish(&mut s.db, id, ScriptPhase::Stopped);
            Err(e)
        }
    }
}

/// Mark what never ran as skipped and close the run.
fn finish(db: &mut DbState, id: u64, state: ScriptPhase) {
    let Some(sc) = db.scripts.runs.iter_mut().find(|r| r.id == id) else { return };
    for st in &mut sc.statements {
        if st.status == StmtStatus::Pending {
            st.status = StmtStatus::Skipped;
        }
    }
    sc.state = state;
    sc.segment = None;
    sc.elapsed_ms = sc.started.map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
    let (source, line) = (sc.source.clone(), format!("Run '{}': {}", sc.title, sc.summary()));
    let level = if sc.failed() > 0 { OutputLevel::Error } else { OutputLevel::Info };
    db.log(level, &source, line);
}

/// A statement of a worker run finished.
pub(crate) fn on_statement(db: &mut DbState, run: u64, stmt: usize, outcome: Result<&ExecSummary, &str>) {
    let Some(sc) = db.scripts.runs.iter_mut().find(|r| r.segment.is_some_and(|(r, _)| r == run)) else { return };
    let Some((_, from)) = sc.segment else { return };
    let Some(st) = sc.statements.get_mut(from + stmt) else { return };
    st.status = match outcome {
        Ok(sum) => StmtStatus::Done {
            ms: sum.elapsed_ms,
            rows: (!sum.columns.is_empty()).then(|| u64::try_from(sum.rows_fetched).unwrap_or(u64::MAX)),
            affected: sum.rows_affected,
        },
        Err(e) => StmtStatus::Failed { error: e.to_string() },
    };
}

/// A worker run ended: close the segment (stop, finish, or ask).
pub(crate) fn on_run_done(s: &mut Session, run: u64) {
    let Some(sc) = s.db.scripts.runs.iter().find(|r| r.segment.is_some_and(|(r, _)| r == run)) else { return };
    let id = sc.id;
    let from = sc.segment.map_or(0, |(_, f)| f);
    let failed_at =
        sc.statements.iter().enumerate().skip(from).find(|(_, st)| matches!(st.status, StmtStatus::Failed { .. }));
    let cancelled = failed_at
        .is_some_and(|(_, st)| matches!(&st.status, StmtStatus::Failed { error } if error == "Query cancelled"));
    let rest =
        failed_at.is_some_and(|(k, _)| sc.statements.iter().skip(k + 1).any(|st| st.status == StmtStatus::Pending));
    let failed = failed_at.map(|(k, _)| k);
    let mode = sc.mode;
    if let Some(sc) = s.db.scripts.runs.iter_mut().find(|r| r.id == id) {
        sc.segment = None;
    }
    match (failed, mode) {
        (Some(k), ScriptMode::Ask) if rest && !cancelled => {
            if let Some(sc) = s.db.scripts.runs.iter_mut().find(|r| r.id == id) {
                sc.state = ScriptPhase::Asking;
            }
            s.db.scripts.shown = Some(id);
            s.status = format!("Statement {} failed: continue the script?", k + 1);
        }
        (Some(_), ScriptMode::Stop | ScriptMode::Ask) => finish(&mut s.db, id, ScriptPhase::Stopped),
        _ => finish(&mut s.db, id, ScriptPhase::Done),
    }
    if let Some(sc) = s.db.scripts.get(id).filter(|r| r.state != ScriptPhase::Running && r.state != ScriptPhase::Asking)
    {
        s.status = format!("Run '{}': {}", sc.title, sc.summary());
    }
}

/// Answer an "ask" script: continue after the failed statement (`all`: and after later
/// failures), or stop.
pub(crate) fn decide(s: &mut Session, id: u64, go_on: bool, all: bool) -> Result<(), String> {
    let sc = s.db.scripts.get(id).ok_or_else(|| format!("no script {id}"))?;
    let at = sc.asking_at().ok_or("the script is not waiting for an answer")?;
    if !go_on {
        finish(&mut s.db, id, ScriptPhase::Stopped);
        return Ok(());
    }
    if all && let Some(sc) = s.db.scripts.runs.iter_mut().find(|r| r.id == id) {
        sc.mode = ScriptMode::Continue;
    }
    start_segment(s, id, at + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keeps_lines() {
        let sql = "select 1;\n\n-- c\nselect 2; select 3;\ncreate function f() returns int as $$ begin; return 1; end $$ language plpgsql;\n";
        let v = split(sql, Dialect::Postgres);
        let lines: Vec<usize> = v.iter().map(|(_, l)| *l).collect();
        assert_eq!(lines, [0, 3, 3, 4]);
        assert!(v[3].0.contains("return 1;"));
        assert!(split("", Dialect::Generic).is_empty());
        assert!(split("-- only a comment", Dialect::Generic).is_empty());
        for junk in ["'unterminated; x", "$$ open; ", "/* open; ", "\"q; ", ";;;;"] {
            let _ = split(junk, Dialect::Postgres);
        }
        let big = "insert into t values (1);\n".repeat(20_000);
        let v = split(&big, Dialect::Sqlite);
        assert_eq!(v.len(), 20_000);
        assert_eq!(v.last().map(|x| x.1), Some(19_999));
    }

    #[test]
    fn modes_parse() {
        assert_eq!(ScriptMode::parse("continue"), Some(ScriptMode::Continue));
        assert_eq!(ScriptMode::parse("ASK"), Some(ScriptMode::Ask));
        assert_eq!(ScriptMode::parse("stop"), Some(ScriptMode::Stop));
        assert_eq!(ScriptMode::parse("x"), None);
        assert_eq!(serde_json::to_value(ScriptMode::Continue).unwrap_or_default(), "continue");
    }
}
