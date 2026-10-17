//! What happens between "Execute" and the worker: parameter prompts (`?`, `$1`, `:name`,
//! `${name}`), the read-only and production guards, manual-commit mode, and the query history.
//!
//! Parameter values are bound ([`codecraft_db::bind`]): the statement keeps placeholders in the
//! connection's native style and the values travel next to it, so a value can never change
//! what the statement does. The prompt's "SQL expression" kind is the one way to insert SQL
//! text on purpose.

use serde::Serialize;
use serde_json::{Value, json};

use codecraft_db::bind::{self, Bound, ParamKind, ParamValue, ParamValues};
use codecraft_db::{Cell, DbKind, Dialect, is_modifying};

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
    /// The statements as typed (recorded in the history), or already in the connection's
    /// native placeholder style when `bound` is set.
    pub statements: Vec<String>,
    /// Bind parameters per statement, for statements CodeCraft built (a Compare Data sync):
    /// they skip the parameter prompt.
    #[serde(skip)]
    pub bound: Vec<Vec<Cell>>,
}

impl PendingRun {
    pub fn new(action: ExecAction, source: String, worker: String, title: String, statements: Vec<String>) -> Self {
        Self { action, source, worker, title, statements, bound: Vec::new() }
    }
}

/// One parameter of the prompt (the UI edits `value` and `kind` in place).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParamField {
    pub name: String,
    pub value: String,
    /// How `value` is read: a value (`NULL`, numbers, `'quoted'` or plain text), exactly the
    /// text typed, or an SQL expression inserted as typed.
    pub kind: ParamKind,
}

impl ParamField {
    pub fn to_value(&self) -> ParamValue {
        bind::parse_param(&self.value, self.kind)
    }
}

/// The parameter dialog: values to bind before running.
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
    /// The statements as they will run (bound values listed after each).
    pub statements: Vec<String>,
    /// The ones that modify data.
    pub writes: Vec<String>,
    pub source_name: String,
    /// The parameter values used (confirming runs the same statements).
    #[serde(skip)]
    pub(crate) values: ParamValues,
}

fn kind_of(s: &Session, source: &str) -> DbKind {
    s.db.source(source).map(|d| d.config.kind).unwrap_or_default()
}

fn dialect(s: &Session, source: &str) -> Dialect {
    kind_of(s, source).dialect()
}

/// The parameters of a run's statements, in order of first appearance.
pub(crate) fn run_params(s: &Session, run: &PendingRun) -> Vec<String> {
    if !run.bound.is_empty() {
        return Vec::new();
    }
    let kind = kind_of(s, &run.source);
    let mut names: Vec<String> = Vec::new();
    for st in &run.statements {
        for n in bind::user_param_names(kind, st) {
            if !names.contains(&n) {
                names.push(n);
            }
        }
    }
    names
}

/// `params` of a command for `run`: an object `{name: value}` or an array of positional values
/// (see [`bind::values_from_json`]). `allow_sql`: `{"sql": "…"}` expressions are accepted.
pub(crate) fn json_values(
    s: &Session,
    run: &PendingRun,
    params: Option<&Value>,
    allow_sql: bool,
) -> Result<Option<ParamValues>, String> {
    match params {
        None | Some(Value::Null) => Ok(None),
        Some(v) => bind::values_from_json(&run_params(s, run), v, allow_sql),
    }
}

/// The text the prompt shows for a remembered value.
fn field_text(v: &ParamValue) -> (String, ParamKind) {
    match v {
        ParamValue::Sql(t) => (t.clone(), ParamKind::Sql),
        ParamValue::Value(Cell::Null) => ("NULL".into(), ParamKind::Auto),
        ParamValue::Value(Cell::Text(t)) => {
            // Text that Auto would read as something else keeps the Text kind.
            let kind = if bind::parse_param(t, ParamKind::Auto) == ParamValue::Value(Cell::Text(t.clone())) {
                ParamKind::Auto
            } else {
                ParamKind::Text
            };
            (t.clone(), kind)
        }
        ParamValue::Value(c) => (c.to_plain(), ParamKind::Auto),
    }
}

/// Start `run`: ask for parameters unless `values` has them, guard writes unless `confirmed`,
/// then send it to its worker. Returns what the command reports.
pub(crate) fn submit(s: &mut Session, run: PendingRun, values: Option<&ParamValues>, confirmed: bool) -> Result<Value, String> {
    let names = run_params(s, &run);
    let kind = kind_of(s, &run.source);
    let d = dialect(s, &run.source);
    let bound: Vec<Bound> = if !run.bound.is_empty() {
        if run.bound.len() != run.statements.len() {
            return Err("every statement needs its parameter list".into());
        }
        run.statements.iter().zip(&run.bound).map(|(sql, p)| Bound { sql: sql.clone(), params: p.clone() }).collect()
    } else if names.is_empty() {
        run.statements.iter().map(Bound::plain).collect()
    } else {
        let Some(values) = values else {
            let remembered = s.db.param_values.get(&run.worker).cloned().unwrap_or_default();
            let params = names
                .iter()
                .map(|n| {
                    let (value, kind) = remembered.get(n).map(field_text).unwrap_or_default();
                    ParamField { name: n.clone(), value, kind }
                })
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
        run.statements.iter().map(|st| bind::prepare(kind, st, values)).collect::<Result<Vec<_>, _>>()?
    };
    let Some(cfg) = s.db.source(&run.source).map(|d| d.config.clone()) else {
        return Err(format!("no data source `{}`", run.source));
    };
    // Bound values cannot change what a statement does: its text (placeholders, and any SQL
    // expression typed on purpose) decides whether it writes.
    let shown: Vec<String> = bound.iter().map(|b| b.preview(kind)).collect();
    let writes: Vec<String> = match run.action {
        ExecAction::Explain { analyze: false } => Vec::new(),
        _ => bound.iter().zip(&shown).filter(|(b, _)| is_modifying(&b.sql, d)).map(|(_, p)| p.clone()).collect(),
    };
    if !writes.is_empty() && cfg.read_only {
        let first = writes.first().map(|w| w.lines().next().unwrap_or("").to_string()).unwrap_or_default();
        return Err(format!("{} is read-only; this statement modifies data: {first}", cfg.name));
    }
    if !writes.is_empty() && cfg.confirm_writes && !confirmed {
        s.db.confirm = Some(ConfirmPrompt {
            run,
            statements: shown,
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
    let n = bound.len();
    let any_params = bound.iter().any(Bound::has_params);
    let (statements, params): (Vec<String>, Vec<Vec<Cell>>) = bound.into_iter().map(|b| (b.sql, b.params)).unzip();
    let id = s.db.start_run(RunSpec {
        worker: run.worker.clone(),
        source: run.source.clone(),
        owner,
        title: run.title.clone(),
        statements,
        params: if any_params { params } else { Vec::new() },
        // Manual commit applies to consoles (agents' `sql` runs on the query connection commit).
        manual: run.worker.starts_with("console:") && s.db.manual_commit(&run.worker, &run.source),
        explain,
        ..RunSpec::default()
    })?;
    if run.bound.is_empty() {
        for st in &run.statements {
            s.db.history.record(&run.source, st);
        }
    }
    Ok(json!({ "run": id, "statements": n, "dataSource": run.source }))
}
