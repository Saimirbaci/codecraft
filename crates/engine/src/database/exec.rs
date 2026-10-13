//! What happens between "Execute" and the worker: parameter prompts (`?`, `$1`, `:name`,
//! `${name}`), the read-only and production guards, manual-commit mode, and the query history.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use codecraft_db::params::{param_names, substitute};
use codecraft_db::{Dialect, is_modifying};

use super::RunSpec;
use crate::Session;

/// What a console asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ExecAction {
    Execute,
    /// Explain Plan; `analyze` runs the statement for actual times.
    Explain {
        analyze: bool,
    },
}

/// Statements on their way to a worker.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRun {
    pub action: ExecAction,
    pub source: String,
    /// The connection (`console:<path>` or `query:<source>`).
    pub worker: String,
    pub title: String,
    /// The statements as typed (recorded in the history).
    pub statements: Vec<String>,
}

/// One parameter of the prompt (the UI edits `value` in place).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParamField {
    pub name: String,
    pub value: String,
}

/// The parameter dialog: values to substitute before running.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamPrompt {
    pub run: PendingRun,
    pub params: Vec<ParamField>,
}

/// The production guard's question: run these data-modifying statements?
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmPrompt {
    pub run: PendingRun,
    /// The statements after parameter substitution.
    pub statements: Vec<String>,
    /// The ones that modify data.
    pub writes: Vec<String>,
    pub source_name: String,
    /// The parameter values used (confirming runs the same statements).
    #[serde(skip)]
    pub(crate) values: BTreeMap<String, String>,
}

fn dialect(s: &Session, source: &str) -> Dialect {
    s.db.source(source).map(|d| d.config.kind.dialect()).unwrap_or_default()
}

/// The parameters of a run's statements, in order of first appearance.
pub(crate) fn run_params(s: &Session, run: &PendingRun) -> Vec<String> {
    let d = dialect(s, &run.source);
    let mut names: Vec<String> = Vec::new();
    for st in &run.statements {
        for n in param_names(st, d) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    names
}

/// Start `run`: ask for parameters unless `values` has them, guard writes unless `confirmed`,
/// then send it to its worker. Returns what the command reports.
pub(crate) fn submit(
    s: &mut Session,
    run: PendingRun,
    values: Option<&BTreeMap<String, String>>,
    confirmed: bool,
) -> Result<Value, String> {
    let names = run_params(s, &run);
    let d = dialect(s, &run.source);
    let statements = if names.is_empty() {
        run.statements.clone()
    } else {
        let Some(values) = values else {
            let remembered = s.db.param_values.get(&run.worker).cloned().unwrap_or_default();
            let params = names
                .iter()
                .map(|n| ParamField { name: n.clone(), value: remembered.get(n).cloned().unwrap_or_default() })
                .collect();
            s.db.param_prompt = Some(ParamPrompt { run, params });
            s.status = "Enter parameter values".into();
            return Ok(json!({ "parameters": names, "prompt": true }));
        };
        let remembered = s.db.param_values.entry(run.worker.clone()).or_default();
        for n in &names {
            if let Some(v) = values.get(n) {
                remembered.insert(n.clone(), v.clone());
            }
        }
        run.statements
            .iter()
            .map(|st| substitute(st, d, values).map_err(|name| format!("no value for parameter `{name}`")))
            .collect::<Result<Vec<_>, _>>()?
    };
    let Some(cfg) = s.db.source(&run.source).map(|d| d.config.clone()) else {
        return Err(format!("no data source `{}`", run.source));
    };
    let writes: Vec<String> = match run.action {
        ExecAction::Explain { analyze: false } => Vec::new(),
        _ => statements.iter().filter(|st| is_modifying(st, d)).cloned().collect(),
    };
    if !writes.is_empty() && cfg.read_only {
        let first = writes.first().map(|w| w.lines().next().unwrap_or("").to_string()).unwrap_or_default();
        return Err(format!("{} is read-only; this statement modifies data: {first}", cfg.name));
    }
    if !writes.is_empty() && cfg.confirm_writes && !confirmed {
        s.db.confirm = Some(ConfirmPrompt {
            run,
            statements,
            writes: writes.clone(),
            source_name: cfg.name.clone(),
            values: values.cloned().unwrap_or_default(),
        });
        s.status = format!("Confirm data modification on {}", cfg.name);
        return Ok(json!({ "confirm": true, "writes": writes }));
    }
    let (owner, explain) = match run.action {
        ExecAction::Execute => (run.worker.clone(), None),
        ExecAction::Explain { analyze } => (format!("plan:{}", run.worker), Some(analyze)),
    };
    let n = statements.len();
    let id = s.db.start_run(RunSpec {
        worker: run.worker.clone(),
        source: run.source.clone(),
        owner,
        title: run.title.clone(),
        statements,
        // Manual commit applies to consoles (agents' `sql` runs on the query connection commit).
        manual: cfg.manual_commit && run.worker.starts_with("console:"),
        explain,
        ..RunSpec::default()
    })?;
    for st in &run.statements {
        s.db.history.record(&run.source, st);
    }
    Ok(json!({ "run": id, "statements": n, "dataSource": run.source }))
}
