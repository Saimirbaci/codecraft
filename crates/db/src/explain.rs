//! Explain Plan: the statement each database uses to describe a query plan, and parsers that
//! turn its output into one tree shape ([`PlanNode`]).
//!
//! - PostgreSQL: `EXPLAIN (FORMAT JSON)`, with `ANALYZE` for actual times and rows.
//! - SQLite: `EXPLAIN QUERY PLAN` rows `(id, parent, notused, detail)`; there is no analyze.
//! - MySQL: `EXPLAIN FORMAT=JSON`; `EXPLAIN ANALYZE` prints a text tree. MariaDB analyzes with
//!   `ANALYZE FORMAT=JSON` (`r_rows`, `r_total_time_ms`).
//! - SQL Server: the estimated plan from `SET SHOWPLAN_XML ON` (the `RelOp` tree).
//! - ClickHouse: `EXPLAIN indexes = 1`, an indented text tree.
//! - DuckDB: `EXPLAIN` / `EXPLAIN ANALYZE` through the CLI print a box-drawing tree
//!   ([`parse_duckdb_boxes`]).
//! - Redis: no plans.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Cell, Connection, DbError, DbKind, DbResult, query_all};

/// Nesting deeper than this is cut off (hostile or broken plans cannot recurse forever).
const MAX_DEPTH: usize = 200;

/// One step of a query plan.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanNode {
    /// What the step does: `Seq Scan`, `Hash Join`, `SCAN books`, `Table scan`.
    pub operation: String,
    /// The table (and alias) it reads, when it reads one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    /// Index, join type, conditions and filters, as `key: value; …`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_cost: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost: Option<f64>,
    /// Estimated rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<f64>,
    /// Actual time in milliseconds (per loop, as the database reports it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_time_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_rows: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loops: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    fn new(operation: impl Into<String>) -> Self {
        Self { operation: operation.into(), ..Self::default() }
    }

    fn add_detail(&mut self, key: &str, value: &str) {
        if value.is_empty() {
            return;
        }
        if !self.detail.is_empty() {
            self.detail.push_str("; ");
        }
        if key.is_empty() {
            self.detail.push_str(value);
        } else {
            self.detail.push_str(&format!("{key}: {value}"));
        }
    }

    /// Nodes in this subtree (itself included).
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(PlanNode::count).sum::<usize>()
    }
}

/// A parsed plan.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub roots: Vec<PlanNode>,
    /// Actual times and rows are included (the statement ran).
    pub analyzed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_ms: Option<f64>,
    /// The database's own output (JSON or text), for copying.
    pub raw: String,
    /// Something the user should know (e.g. SQLite cannot analyze).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The statement that explains `sql`, and whether it analyzes (runs the statement).
pub fn explain_statement(kind: DbKind, sql: &str, analyze: bool) -> (String, bool) {
    let sql = sql.trim().trim_end_matches(';').trim_end();
    match (kind, analyze) {
        (DbKind::Postgres, false) => (format!("EXPLAIN (FORMAT JSON) {sql}"), false),
        (DbKind::Postgres, true) => (format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}"), true),
        (DbKind::Sqlite, _) => (format!("EXPLAIN QUERY PLAN {sql}"), false),
        (DbKind::Mysql, false) | (DbKind::Mariadb, false) => (format!("EXPLAIN FORMAT=JSON {sql}"), false),
        (DbKind::Mysql, true) => (format!("EXPLAIN ANALYZE {sql}"), true),
        (DbKind::Mariadb, true) => (format!("ANALYZE FORMAT=JSON {sql}"), true),
        // Runs between SET SHOWPLAN_XML ON and OFF (see [`explain`]).
        (DbKind::SqlServer, _) => (sql.to_string(), false),
        (DbKind::ClickHouse, _) => (format!("EXPLAIN indexes = 1 {sql}"), false),
        (DbKind::DuckDb, false) | (DbKind::Redis, _) => (format!("EXPLAIN {sql}"), false),
        (DbKind::DuckDb, true) => (format!("EXPLAIN ANALYZE {sql}"), true),
    }
}

/// SQL Server: the statement under `SET SHOWPLAN_XML ON` returns its plan instead of running.
fn explain_mssql(conn: &mut dyn Connection, sql: &str) -> DbResult<Plan> {
    crate::tx::run(conn, "SET SHOWPLAN_XML ON")?;
    let result = query_all(conn, sql.trim().trim_end_matches(';').trim_end());
    let off = crate::tx::run(conn, "SET SHOWPLAN_XML OFF");
    let (_, rows, _) = result?;
    off?;
    parse_showplan_xml(&first_text(&rows)?)
}

/// Explain `sql` on `conn` (with `analyze`, where supported, the statement runs).
pub fn explain(conn: &mut dyn Connection, sql: &str, analyze: bool) -> DbResult<Plan> {
    let kind = conn.kind();
    if kind == DbKind::Redis {
        return Err(DbError::Unsupported(format!(
            "Explain Plan is not available for {}; run EXPLAIN in the console",
            kind.label()
        )));
    }
    let (statement, analyzed) = explain_statement(kind, sql, analyze);
    let mut plan = if kind == DbKind::SqlServer {
        explain_mssql(conn, &statement)?
    } else {
        let (_, rows, _) = query_all(conn, &statement)?;
        explain_rows(kind, &rows, analyzed)?
    };
    plan.analyzed = analyzed;
    if analyze && !analyzed {
        plan.note = Some(format!("{} cannot analyze a query; showing the estimated plan", kind.label()));
    }
    Ok(plan)
}

fn explain_rows(kind: DbKind, rows: &[Vec<Cell>], analyzed: bool) -> DbResult<Plan> {
    Ok(match kind {
        DbKind::Sqlite => {
            let eqp: Vec<(i64, i64, String)> = rows
                .iter()
                .map(|r| (int(r.first()), int(r.get(1)), r.get(3).map(Cell::to_plain).unwrap_or_default()))
                .collect();
            let raw = eqp.iter().map(|(id, p, d)| format!("{id}|{p}|{d}")).collect::<Vec<_>>().join("\n");
            Plan { roots: parse_sqlite(&eqp), raw, ..Plan::default() }
        }
        DbKind::Postgres => parse_postgres(&first_text(rows)?)?,
        DbKind::Mysql if analyzed => {
            let text = rows.iter().filter_map(|r| r.first().map(Cell::to_plain)).collect::<Vec<_>>().join("\n");
            parse_mysql_tree(&text)
        }
        DbKind::Mysql | DbKind::Mariadb => parse_mysql_json(&first_text(rows)?)?,
        DbKind::ClickHouse => {
            let text = rows.iter().filter_map(|r| r.first().map(Cell::to_plain)).collect::<Vec<_>>().join("\n");
            parse_indented(&text)
        }
        DbKind::SqlServer => parse_showplan_xml(&first_text(rows)?)?,
        DbKind::DuckDb => {
            // Rows `(explain_key, explain_value)`: the plan is the value's box drawing.
            let text = rows.iter().filter_map(|r| r.last().map(Cell::to_plain)).collect::<Vec<_>>().join("\n");
            if text.trim().is_empty() {
                return Err(DbError::Query("the database returned no plan".into()));
            }
            parse_duckdb_boxes(&text, analyzed)
        }
        DbKind::Redis => {
            return Err(DbError::Unsupported("Explain Plan is not available for this data source".into()));
        }
    })
}

/// One box of a DuckDB plan drawing: its position and text lines.
struct DuckBox {
    /// Line of the top border.
    top: usize,
    /// Column (in chars) of the left border.
    left: usize,
    lines: Vec<String>,
}

/// DuckDB's `EXPLAIN` output: operators drawn as boxes (`┌─┐ │ │ └─┘`), a child below its parent
/// and a second child to the right of the first. Boxes of one level start on the same line; a
/// box's children are the boxes of the next level from its column up to the next box of its own
/// level. The first text line of a box is the operator; the others become its detail (`~N Rows`
/// and, analyzed, `N Rows` / `(0.01s)` fill the row and time fields).
pub fn parse_duckdb_boxes(text: &str, analyzed: bool) -> Plan {
    let grid: Vec<Vec<char>> = text.lines().map(|l| l.chars().collect()).collect();
    let at = |r: usize, c: usize| grid.get(r).and_then(|l| l.get(c)).copied();
    let mut boxes: Vec<DuckBox> = Vec::new();
    for (r, line) in grid.iter().enumerate() {
        for (c, ch) in line.iter().enumerate() {
            if *ch != '┌' {
                continue;
            }
            // The right border: the first `┐` after the corner on this line.
            let Some(right) = line.iter().skip(c + 1).position(|x| *x == '┐').map(|k| c + 1 + k) else { continue };
            let mut lines = Vec::new();
            let mut row = r + 1;
            while row < grid.len() && lines.len() < 200 {
                match at(row, c) {
                    Some('└') => break,
                    Some('│' | '├' | '┤') => {
                        let inner: String = grid
                            .get(row)
                            .and_then(|l| l.get(c + 1..right.min(l.len())))
                            .unwrap_or(&[])
                            .iter()
                            .collect();
                        let t = inner.trim();
                        // Separator rows are dashes only.
                        if !t.is_empty() && !t.chars().all(|x| matches!(x, '─' | '-' | ' ')) {
                            lines.push(t.to_string());
                        }
                    }
                    _ => break,
                }
                row += 1;
            }
            if !lines.is_empty() {
                boxes.push(DuckBox { top: r, left: c, lines });
            }
        }
    }
    // Levels: boxes sharing a top line, left to right.
    let mut tops: Vec<usize> = boxes.iter().map(|b| b.top).collect();
    tops.sort_unstable();
    tops.dedup();
    let level_of = |b: &DuckBox| tops.iter().position(|t| *t == b.top).unwrap_or(0);
    fn node_of(b: &DuckBox, analyzed: bool) -> PlanNode {
        let mut it = b.lines.iter();
        let mut n = PlanNode::new(it.next().map(String::as_str).unwrap_or("?"));
        for l in it {
            let t = l.trim();
            let num = |s: &str| s.trim().replace(',', "").parse::<f64>().ok();
            if let Some(rows) = t.strip_prefix('~').and_then(|x| x.strip_suffix(" Rows").or(x.strip_suffix(" rows"))) {
                n.rows = num(rows);
            } else if let Some(rows) = t.strip_suffix(" Rows").or(t.strip_suffix(" rows")).filter(|_| analyzed) {
                n.actual_rows = num(rows);
            } else if let Some(secs) = t.strip_prefix('(').and_then(|x| x.strip_suffix("s)")).filter(|_| analyzed) {
                n.actual_time_ms = num(secs).map(|s| s * 1000.0);
            } else if let Some(table) = t.strip_prefix("Table: ") {
                n.relation = Some(table.trim().to_string());
                n.add_detail("", t);
            } else {
                n.add_detail("", t);
            }
        }
        n
    }
    fn build(i: usize, boxes: &[DuckBox], levels: &[usize], depth: usize, analyzed: bool) -> PlanNode {
        let Some(b) = boxes.get(i) else { return PlanNode::new("?") };
        let mut node = node_of(b, analyzed);
        let Some(level) = levels.get(i).copied() else { return node };
        if depth >= MAX_DEPTH {
            return node;
        }
        // The next box of the same level bounds the children on the right.
        let right = boxes
            .iter()
            .zip(levels)
            .filter(|(o, l)| **l == level && o.left > b.left)
            .map(|(o, _)| o.left)
            .min()
            .unwrap_or(usize::MAX);
        let mut kids: Vec<usize> = (0..boxes.len())
            .filter(|&k| levels.get(k) == Some(&(level + 1)))
            .filter(|&k| boxes.get(k).is_some_and(|o| o.left >= b.left && o.left < right))
            .collect();
        kids.sort_by_key(|&k| boxes.get(k).map(|o| o.left));
        for k in kids {
            node.children.push(build(k, boxes, levels, depth + 1, analyzed));
        }
        node
    }
    let levels: Vec<usize> = boxes.iter().map(level_of).collect();
    let mut roots: Vec<usize> = (0..boxes.len()).filter(|&k| levels.get(k) == Some(&0)).collect();
    roots.sort_by_key(|&k| boxes.get(k).map(|o| o.left));
    let roots: Vec<PlanNode> = roots.into_iter().map(|k| build(k, &boxes, &levels, 0, analyzed)).collect();
    let note = roots.is_empty().then(|| "the plan could not be read; see the raw output".to_string());
    Plan { roots, raw: text.to_string(), note, ..Plan::default() }
}

/// An indented text plan (ClickHouse `EXPLAIN`): each level two more spaces.
pub fn parse_indented(text: &str) -> Plan {
    let mut roots: Vec<PlanNode> = Vec::new();
    // (indent, node) of the open ancestors.
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    fn attach(stack: &mut [(usize, PlanNode)], roots: &mut Vec<PlanNode>, node: PlanNode) {
        match stack.last_mut() {
            Some((_, parent)) => parent.children.push(node),
            None => roots.push(node),
        }
    }
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - trimmed.len();
        // A detail line of the node above (`Condition: …`, `Keys: …`) is indented under it.
        let is_detail = !trimmed.contains('(') && trimmed.contains(':') && !stack.is_empty();
        if is_detail && stack.last().is_some_and(|(i, _)| indent > *i) {
            if let Some((_, n)) = stack.last_mut() {
                let (k, v) = trimmed.split_once(':').unwrap_or((trimmed, ""));
                n.add_detail(k.trim(), v.trim());
            }
            continue;
        }
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            if let Some((_, done)) = stack.pop() {
                attach(&mut stack, &mut roots, done);
            }
        }
        if stack.len() >= MAX_DEPTH {
            continue;
        }
        let (op, rest) = match trimmed.find(" (") {
            Some(i) => (trimmed.get(..i).unwrap_or(trimmed), trimmed.get(i + 1..).unwrap_or("")),
            None => (trimmed, ""),
        };
        let mut node = PlanNode::new(op.trim());
        node.add_detail("", rest.trim().trim_start_matches('(').trim_end_matches(')'));
        stack.push((indent, node));
    }
    while let Some((_, done)) = stack.pop() {
        attach(&mut stack, &mut roots, done);
    }
    Plan { roots, raw: text.to_string(), ..Plan::default() }
}

/// XML attribute values with the five predefined entities decoded.
fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// `name="value"` pairs of a tag's text.
fn xml_attrs(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let chars: Vec<char> = tag.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        let start = i;
        while chars.get(i).is_some_and(|c| !c.is_whitespace() && *c != '=') {
            i += 1;
        }
        let name: String = chars.get(start..i).unwrap_or(&[]).iter().collect();
        if chars.get(i) != Some(&'=') {
            i += 1;
            continue;
        }
        i += 1;
        let Some(&quote) = chars.get(i).filter(|c| **c == '"' || **c == '\'') else { continue };
        i += 1;
        let vstart = i;
        while chars.get(i).is_some_and(|c| *c != quote) {
            i += 1;
        }
        let value: String = chars.get(vstart..i).unwrap_or(&[]).iter().collect();
        i += 1;
        out.push((name, xml_unescape(&value)));
    }
    out
}

/// SQL Server's showplan XML: the nested `RelOp` elements as a tree (estimated rows and cost,
/// the object each reads).
pub fn parse_showplan_xml(xml: &str) -> DbResult<Plan> {
    let mut roots: Vec<PlanNode> = Vec::new();
    let mut open: Vec<PlanNode> = Vec::new();
    let mut statement: Option<String> = None;
    let mut rest = xml;
    let mut seen_relop = false;
    while let Some(lt) = rest.find('<') {
        let after = rest.get(lt + 1..).unwrap_or("");
        // The tag runs to the next `>` outside quotes.
        let mut end = None;
        let mut quote: Option<char> = None;
        for (i, c) in after.char_indices() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '"' | '\'') => quote = Some(c),
                (None, '>') => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        let tag = after.get(..end).unwrap_or("");
        rest = after.get(end + 1..).unwrap_or("");
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            if local_name(name.trim()) == "RelOp"
                && let Some(done) = open.pop()
            {
                match open.last_mut() {
                    Some(parent) => parent.children.push(done),
                    None => roots.push(done),
                }
            }
            continue;
        }
        let self_closing = tag.ends_with('/');
        let body = tag.trim_end_matches('/');
        let name = local_name(body.split_whitespace().next().unwrap_or(""));
        let attrs = || xml_attrs(body.split_once(char::is_whitespace).map(|(_, a)| a).unwrap_or(""));
        let get = |a: &[(String, String)], k: &str| a.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        match name {
            "StmtSimple" if statement.is_none() => statement = get(&attrs(), "StatementText"),
            "RelOp" => {
                seen_relop = true;
                let a = attrs();
                let physical = get(&a, "PhysicalOp").unwrap_or_else(|| "?".into());
                let mut node = PlanNode::new(physical.clone());
                if let Some(logical) = get(&a, "LogicalOp").filter(|l| *l != physical) {
                    node.add_detail("Logical", &logical);
                }
                node.rows = get(&a, "EstimateRows").and_then(|v| v.parse().ok());
                node.total_cost = get(&a, "EstimatedTotalSubtreeCost").and_then(|v| v.parse().ok());
                if self_closing {
                    match open.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => roots.push(node),
                    }
                } else if open.len() < MAX_DEPTH {
                    open.push(node);
                } else {
                    return Err(DbError::Query("the plan is nested too deeply".into()));
                }
            }
            "Object" => {
                if let Some(node) = open.last_mut()
                    && node.relation.is_none()
                {
                    let a = attrs();
                    let unq = |v: Option<String>| v.map(|s| s.trim_matches(['[', ']']).to_string());
                    let table = unq(get(&a, "Table"));
                    if let Some(t) = table {
                        let schema = unq(get(&a, "Schema"));
                        node.relation = Some(match schema {
                            Some(s) if !s.is_empty() => format!("{s}.{t}"),
                            _ => t,
                        });
                        if let Some(i) = unq(get(&a, "Index")) {
                            node.add_detail("Index", &i);
                        }
                    }
                }
            }
            "Predicate" | "SeekPredicates" => {
                if let Some(node) = open.last_mut() {
                    node.add_detail(name, "yes");
                }
            }
            _ => {}
        }
    }
    while let Some(done) = open.pop() {
        match open.last_mut() {
            Some(parent) => parent.children.push(done),
            None => roots.push(done),
        }
    }
    if !seen_relop {
        return Err(DbError::Query("SQL Server returned no plan for this statement".into()));
    }
    let mut plan = Plan { roots, raw: xml.to_string(), ..Plan::default() };
    if let Some(s) = statement {
        plan.note = Some(format!("Estimated plan of: {}", s.chars().take(200).collect::<String>()));
    }
    Ok(plan)
}

/// `p:RelOp` → `RelOp`.
fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn int(c: Option<&Cell>) -> i64 {
    match c {
        Some(Cell::Int(i)) => *i,
        Some(Cell::UInt(u)) => i64::try_from(*u).unwrap_or(i64::MAX),
        Some(other) => other.to_plain().trim().parse().unwrap_or(0),
        None => 0,
    }
}

fn first_text(rows: &[Vec<Cell>]) -> DbResult<String> {
    rows.first()
        .and_then(|r| r.first())
        .map(Cell::to_plain)
        .ok_or_else(|| DbError::Query("the database returned no plan".into()))
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(text_of).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------- SQLite

/// `EXPLAIN QUERY PLAN` rows `(id, parent, detail)` as a tree (roots have parent 0).
pub fn parse_sqlite(rows: &[(i64, i64, String)]) -> Vec<PlanNode> {
    fn build(rows: &[(i64, i64, String)], parent: i64, depth: usize) -> Vec<PlanNode> {
        if depth > MAX_DEPTH {
            return Vec::new();
        }
        rows.iter()
            .filter(|(id, p, _)| *p == parent && *id != parent)
            .map(|(id, _, detail)| {
                let mut node = PlanNode::new(detail.clone());
                let words: Vec<&str> = detail.split_whitespace().collect();
                if matches!(words.first(), Some(&("SCAN" | "SEARCH"))) {
                    let name = match words.get(1) {
                        Some(&"TABLE") => words.get(2),
                        other => other,
                    };
                    node.relation = name.map(|n| n.to_string());
                }
                node.children = build(rows, *id, depth + 1);
                node
            })
            .collect()
    }
    build(rows, 0, 0)
}

// ---------------------------------------------------------------- PostgreSQL

/// `EXPLAIN (FORMAT JSON)` output: `[{"Plan": {…}, "Planning Time": …, "Execution Time": …}]`.
pub fn parse_postgres(json: &str) -> DbResult<Plan> {
    let v: Value = serde_json::from_str(json).map_err(|e| DbError::Query(format!("cannot read the plan: {e}")))?;
    let top = match &v {
        Value::Array(a) => a.first().cloned().unwrap_or(Value::Null),
        other => other.clone(),
    };
    let root = top.get("Plan").ok_or_else(|| DbError::Query("the plan has no `Plan` node".into()))?;
    Ok(Plan {
        roots: vec![pg_node(root, 0)],
        analyzed: root.get("Actual Total Time").is_some(),
        planning_ms: num(top.get("Planning Time")),
        execution_ms: num(top.get("Execution Time")),
        raw: serde_json::to_string_pretty(&v).unwrap_or_else(|_| json.to_string()),
        note: None,
    })
}

fn pg_node(v: &Value, depth: usize) -> PlanNode {
    let s = |k: &str| v.get(k).map(text_of).unwrap_or_default();
    let mut op = s("Node Type");
    let strategy = s("Strategy");
    if !strategy.is_empty() && strategy != "Plain" {
        op = format!("{op} ({strategy})");
    }
    let mut node = PlanNode::new(if op.is_empty() { "?".to_string() } else { op });
    let relation = s("Relation Name");
    if !relation.is_empty() {
        let schema = s("Schema");
        let alias = s("Alias");
        let mut r = if schema.is_empty() { relation.clone() } else { format!("{schema}.{relation}") };
        if !alias.is_empty() && alias != relation {
            r.push_str(&format!(" {alias}"));
        }
        node.relation = Some(r);
    } else if let Some(name) =
        ["CTE Name", "Function Name", "Subplan Name"].iter().map(|k| s(k)).find(|n| !n.is_empty())
    {
        node.relation = Some(name);
    }
    for (key, label) in [
        ("Index Name", "index"),
        ("Join Type", "join"),
        ("Parent Relationship", "as"),
        ("Index Cond", "index cond"),
        ("Recheck Cond", "recheck"),
        ("Hash Cond", "hash cond"),
        ("Merge Cond", "merge cond"),
        ("Join Filter", "join filter"),
        ("Filter", "filter"),
        ("Sort Key", "sort"),
        ("Group Key", "group"),
    ] {
        let value = s(key);
        if key == "Parent Relationship" && matches!(value.as_str(), "Outer" | "Inner" | "Member") {
            continue;
        }
        node.add_detail(label, &value);
    }
    node.startup_cost = num(v.get("Startup Cost"));
    node.total_cost = num(v.get("Total Cost"));
    node.rows = num(v.get("Plan Rows"));
    node.actual_time_ms = num(v.get("Actual Total Time"));
    node.actual_rows = num(v.get("Actual Rows"));
    node.loops = num(v.get("Actual Loops"));
    if depth < MAX_DEPTH
        && let Some(Value::Array(children)) = v.get("Plans")
    {
        node.children = children.iter().map(|c| pg_node(c, depth + 1)).collect();
    }
    node
}

// ---------------------------------------------------------------- MySQL / MariaDB

/// `EXPLAIN FORMAT=JSON` (MySQL and MariaDB) and MariaDB's `ANALYZE FORMAT=JSON`.
pub fn parse_mysql_json(json: &str) -> DbResult<Plan> {
    let v: Value = serde_json::from_str(json).map_err(|e| DbError::Query(format!("cannot read the plan: {e}")))?;
    let Value::Object(top) = &v else { return Err(DbError::Query("the plan is not a JSON object".into())) };
    let roots: Vec<PlanNode> = top.iter().flat_map(|(k, val)| my_nodes(k, val, 0)).collect();
    let analyzed = json.contains("\"r_rows\"") || json.contains("\"r_total_time_ms\"");
    let execution_ms = v.get("query_block").and_then(|q| num(q.get("r_total_time_ms")));
    Ok(Plan {
        roots,
        analyzed,
        planning_ms: None,
        execution_ms,
        raw: serde_json::to_string_pretty(&v).unwrap_or_else(|_| json.to_string()),
        note: None,
    })
}

/// Keys that describe a step rather than nest one.
const MY_ATTRIBUTES: &[&str] = &[
    "cost_info",
    "used_columns",
    "possible_keys",
    "used_key_parts",
    "ref",
    "attached_condition",
    "key",
    "key_length",
    "message",
    "filtered",
    "r_filtered",
    "rows_examined_per_scan",
    "rows_produced_per_join",
    "rows",
    "r_rows",
    "r_loops",
    "r_total_time_ms",
    "r_table_time_ms",
    "r_other_time_ms",
    "access_type",
    "table_name",
    "select_id",
    "using_filesort",
    "using_temporary_table",
    "using_index",
    "index_condition",
    "partitions",
    "loops",
    "r_buffer_size",
    "r_engine_stats",
    "filesort_key",
];

fn my_nodes(key: &str, v: &Value, depth: usize) -> Vec<PlanNode> {
    if depth > MAX_DEPTH {
        return Vec::new();
    }
    match v {
        // Arrays (nested_loop, query_specifications, attached_subqueries) group their elements.
        Value::Array(items) => {
            let children: Vec<PlanNode> = items
                .iter()
                .flat_map(|item| match item {
                    Value::Object(m) => m.iter().flat_map(|(k, val)| my_nodes(k, val, depth + 1)).collect(),
                    _ => Vec::new(),
                })
                .collect();
            if children.is_empty() {
                return Vec::new();
            }
            let mut node = PlanNode::new(humanize(key));
            node.children = children;
            vec![node]
        }
        Value::Object(m) => {
            let mut node = if key == "table" {
                let access = m.get("access_type").map(text_of).unwrap_or_default();
                let op = match access.as_str() {
                    "ALL" => "Table scan".to_string(),
                    "index" => "Full index scan".into(),
                    "range" => "Index range scan".into(),
                    "ref" | "eq_ref" | "ref_or_null" => format!("Index lookup ({access})"),
                    "const" | "system" => "Single row".into(),
                    "" => "Table".into(),
                    other => other.to_string(),
                };
                let mut n = PlanNode::new(op);
                n.relation = m.get("table_name").map(text_of);
                n
            } else {
                let mut op = humanize(key);
                if key == "query_block"
                    && let Some(id) = m.get("select_id")
                {
                    op = format!("{op} #{}", text_of(id));
                }
                PlanNode::new(op)
            };
            for (k, label) in [("key", "index"), ("ref", "ref"), ("attached_condition", "condition"), ("message", "")] {
                if let Some(val) = m.get(k) {
                    node.add_detail(label, &text_of(val));
                }
            }
            if m.get("using_filesort").and_then(Value::as_bool) == Some(true) {
                node.add_detail("", "using filesort");
            }
            if m.get("using_temporary_table").and_then(Value::as_bool) == Some(true) {
                node.add_detail("", "using temporary table");
            }
            let cost = m.get("cost_info");
            node.total_cost =
                cost.and_then(|c| num(c.get("query_cost")).or(num(c.get("prefix_cost"))).or(num(c.get("sort_cost"))));
            node.rows = num(m.get("rows_examined_per_scan")).or(num(m.get("rows")));
            node.actual_rows = num(m.get("r_rows"));
            node.actual_time_ms = num(m.get("r_total_time_ms")).or(num(m.get("r_table_time_ms")));
            node.loops = num(m.get("r_loops"));
            for (k, val) in m {
                if MY_ATTRIBUTES.contains(&k.as_str()) || !matches!(val, Value::Object(_) | Value::Array(_)) {
                    continue;
                }
                node.children.extend(my_nodes(k, val, depth + 1));
            }
            vec![node]
        }
        _ => Vec::new(),
    }
}

fn humanize(key: &str) -> String {
    match key {
        "query_block" => "Query block".into(),
        "nested_loop" => "Nested loop".into(),
        "ordering_operation" => "Sort".into(),
        "grouping_operation" => "Group".into(),
        "duplicates_removal" => "Distinct".into(),
        "union_result" => "Union".into(),
        "query_specifications" => "Union members".into(),
        "materialized_from_subquery" => "Materialized subquery".into(),
        "attached_subqueries" => "Subqueries".into(),
        "windowing" => "Window".into(),
        other => {
            let mut s = other.replace('_', " ");
            if let Some(first) = s.get(..1) {
                s = format!("{}{}", first.to_uppercase(), s.get(1..).unwrap_or(""));
            }
            s
        }
    }
}

/// MySQL `EXPLAIN ANALYZE` / `FORMAT=TREE` text: `-> step  (cost=… rows=…) (actual time=a..b
/// rows=… loops=…)`, nested by four spaces of indentation.
pub fn parse_mysql_tree(text: &str) -> Plan {
    // (indent, node) for the open path from the root.
    let mut stack: Vec<(usize, PlanNode)> = Vec::new();
    let mut roots: Vec<PlanNode> = Vec::new();
    let close_to = |stack: &mut Vec<(usize, PlanNode)>, roots: &mut Vec<PlanNode>, indent: usize| {
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            let Some((_, node)) = stack.pop() else { break };
            match stack.last_mut() {
                Some((_, parent)) => parent.children.push(node),
                None => roots.push(node),
            }
        }
    };
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let body = line.trim();
        let Some(step) = body.strip_prefix("->") else {
            if let Some((_, last)) = stack.last_mut()
                && !body.is_empty()
            {
                last.add_detail("", body);
            }
            continue;
        };
        if stack.len() >= MAX_DEPTH {
            continue;
        }
        close_to(&mut stack, &mut roots, indent);
        stack.push((indent, mysql_step(step.trim())));
    }
    close_to(&mut stack, &mut roots, 0);
    let analyzed = text.contains("(actual time=");
    Plan { roots, analyzed, raw: text.to_string(), ..Plan::default() }
}

fn mysql_step(step: &str) -> PlanNode {
    let (op, rest) = match step.find("  (") {
        Some(i) => (step.get(..i).unwrap_or(step), step.get(i..).unwrap_or("")),
        None => match step.find(" (cost=").or_else(|| step.find(" (actual")) {
            Some(i) => (step.get(..i).unwrap_or(step), step.get(i..).unwrap_or("")),
            None => (step, ""),
        },
    };
    let mut node = PlanNode::new(op.trim());
    if let Some(i) = op.find(" on ") {
        let after = op.get(i + 4..).unwrap_or("");
        node.relation = after.split_whitespace().next().map(str::to_string);
    }
    for group in rest.split('(').skip(1) {
        let group = group.split(')').next().unwrap_or("");
        let mut fields = group.split_whitespace().peekable();
        let actual = fields.peek() == Some(&"actual");
        for f in fields {
            let Some((k, v)) = f.split_once('=') else { continue };
            match (k, actual) {
                ("cost", false) => node.total_cost = v.parse().ok(),
                ("rows", false) => node.rows = v.parse().ok(),
                ("time", true) => node.actual_time_ms = v.rsplit("..").next().and_then(|t| t.parse().ok()),
                ("rows", true) => node.actual_rows = v.parse().ok(),
                ("loops", true) => node.loops = v.parse().ok(),
                _ => {}
            }
        }
    }
    node
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_server_showplan_xml() {
        let xml = r#"<?xml version="1.0" encoding="utf-16"?><ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements><StmtSimple StatementText="SELECT * FROM t WHERE a &gt; 1"><QueryPlan><RelOp NodeId="0" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="10" EstimatedTotalSubtreeCost="0.5"><NestedLoops><RelOp NodeId="1" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="100" EstimatedTotalSubtreeCost="0.2"><IndexScan><Object Database="[app]" Schema="[dbo]" Table="[t]" Index="[PK_t]" /><Predicate /></IndexScan></RelOp><RelOp NodeId="2" PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="1" EstimatedTotalSubtreeCost="0.1"><IndexScan><Object Schema="[dbo]" Table="[u]" /></IndexScan></RelOp></NestedLoops></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#;
        let plan = parse_showplan_xml(xml).unwrap();
        assert_eq!(plan.roots.len(), 1);
        let root = &plan.roots[0];
        assert_eq!(root.operation, "Nested Loops");
        assert_eq!(root.detail, "Logical: Inner Join");
        assert_eq!(root.rows, Some(10.0));
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].relation.as_deref(), Some("dbo.t"));
        assert!(root.children[0].detail.contains("Index: PK_t"));
        assert_eq!(root.children[1].relation.as_deref(), Some("dbo.u"));
        assert!(plan.note.unwrap().contains("a > 1"));
        assert!(parse_showplan_xml("<ShowPlanXML/>").is_err());
        for bad in ["", "<", "<RelOp", "<RelOp PhysicalOp=\"x>", "</RelOp></RelOp>", "<RelOp><RelOp>", "<a b='>'>"] {
            let _ = parse_showplan_xml(bad);
        }
        let deep = "<RelOp PhysicalOp=\"x\">".repeat(MAX_DEPTH + 5);
        assert!(parse_showplan_xml(&deep).is_err());
    }

    #[test]
    fn clickhouse_indented_plans() {
        let text = "Expression ((Projection + Before ORDER BY))\n  Limit (preliminary LIMIT)\n    ReadFromMergeTree (default.hits)\n    Indexes:\n      PrimaryKey\n        Keys:\n          id\n  Filter";
        let plan = parse_indented(text);
        assert_eq!(plan.roots.len(), 1);
        let root = &plan.roots[0];
        assert_eq!(root.operation, "Expression");
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].operation, "Limit");
        assert_eq!(root.children[0].children[0].operation, "ReadFromMergeTree");
        assert_eq!(root.children[1].operation, "Filter");
        let _ = parse_indented("");
        let _ = parse_indented(&" x\n".repeat(5));
        let _ = parse_indented(&(0..500).map(|i| format!("{}n", " ".repeat(i))).collect::<Vec<_>>().join("\n"));
    }

    #[test]
    fn duckdb_box_plans() {
        let text = "\
┌───────────────────────────┐
│         PROJECTION        │
│    ────────────────────   │
│             a             │
│                           │
│          ~3 Rows          │
└─────────────┬─────────────┘
┌─────────────┴─────────────┐
│         HASH_JOIN         │
│    ────────────────────   │
│      Join Type: INNER     ├──────────────┐
│        Conditions:        │              │
│           a = b           │              │
└─────────────┬─────────────┘              │
┌─────────────┴─────────────┐┌─────────────┴─────────────┐
│         SEQ_SCAN          ││         SEQ_SCAN          │
│    ────────────────────   ││    ────────────────────   │
│          Table: t         ││          Table: u         │
│                           ││                           │
│          ~10 Rows         ││          ~20 Rows         │
└───────────────────────────┘└───────────────────────────┘
";
        let plan = parse_duckdb_boxes(text, false);
        assert_eq!(plan.roots.len(), 1, "{plan:?}");
        let root = &plan.roots[0];
        assert_eq!(root.operation, "PROJECTION");
        assert_eq!(root.rows, Some(3.0));
        assert_eq!(root.children.len(), 1);
        let join = &root.children[0];
        assert_eq!(join.operation, "HASH_JOIN");
        assert!(join.detail.contains("Join Type: INNER"), "{}", join.detail);
        assert_eq!(join.children.len(), 2);
        assert_eq!(join.children[0].relation.as_deref(), Some("t"));
        assert_eq!(join.children[1].relation.as_deref(), Some("u"));
        assert_eq!(join.children[1].rows, Some(20.0));
        let analyzed = "┌─────────────┐\n│ SEQ_SCAN    │\n│ 7 Rows      │\n│ (0.25s)     │\n└─────────────┘\n";
        let a = parse_duckdb_boxes(analyzed, true);
        assert_eq!(a.roots[0].actual_rows, Some(7.0));
        assert_eq!(a.roots[0].actual_time_ms, Some(250.0));
        assert!(parse_duckdb_boxes("not a plan", false).note.is_some());
        for junk in ["┌", "┌┐", "┌──┐\n│", "┌─┐\n│x│", "└┘┌", "┌─┐\n├─┤\n"]
        {
            let _ = parse_duckdb_boxes(junk, true);
        }
        assert_eq!(explain_statement(DbKind::DuckDb, "select 1", true), ("EXPLAIN ANALYZE select 1".into(), true));
        let rows = vec![vec![Cell::Text("physical_plan".into()), Cell::Text(text.into())]];
        assert_eq!(explain_rows(DbKind::DuckDb, &rows, false).unwrap().roots.len(), 1);
    }

    #[test]
    fn statements_per_database() {
        assert_eq!(explain_statement(DbKind::Postgres, "select 1;", false).0, "EXPLAIN (FORMAT JSON) select 1");
        assert_eq!(
            explain_statement(DbKind::Postgres, "select 1", true),
            ("EXPLAIN (ANALYZE, FORMAT JSON) select 1".into(), true)
        );
        assert_eq!(explain_statement(DbKind::Sqlite, "select 1", true), ("EXPLAIN QUERY PLAN select 1".into(), false));
        assert_eq!(explain_statement(DbKind::Mysql, "select 1", false).0, "EXPLAIN FORMAT=JSON select 1");
        assert_eq!(explain_statement(DbKind::Mariadb, "select 1", true).0, "ANALYZE FORMAT=JSON select 1");
    }

    #[test]
    fn sqlite_rows_become_a_tree() {
        let rows = vec![
            (2, 0, "SCAN b".to_string()),
            (5, 0, "SEARCH a USING INTEGER PRIMARY KEY (rowid=?)".to_string()),
            (9, 0, "USE TEMP B-TREE FOR ORDER BY".to_string()),
            (11, 9, "SCAN TABLE x".to_string()),
            (12, 12, "self loop".to_string()),
        ];
        let t = parse_sqlite(&rows);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].relation.as_deref(), Some("b"));
        assert_eq!(t[1].relation.as_deref(), Some("a"));
        assert_eq!(t[2].children[0].relation.as_deref(), Some("x"));
    }

    #[test]
    fn postgres_json() {
        let json = r#"[{"Plan": {"Node Type": "Hash Join", "Join Type": "Inner", "Startup Cost": 1.5, "Total Cost": 30.25,
            "Plan Rows": 12, "Actual Total Time": 0.42, "Actual Rows": 10, "Actual Loops": 1, "Hash Cond": "(b.a = a.id)",
            "Plans": [{"Node Type": "Seq Scan", "Relation Name": "books", "Schema": "public", "Alias": "b",
                       "Parent Relationship": "Outer", "Filter": "(id > 3)", "Total Cost": 20, "Plan Rows": 100},
                      {"Node Type": "Hash", "Plans": [{"Node Type": "Index Scan", "Relation Name": "authors",
                       "Alias": "authors", "Index Name": "authors_pkey"}]}]},
            "Planning Time": 0.1, "Execution Time": 0.5}]"#;
        let p = parse_postgres(json).unwrap();
        assert!(p.analyzed);
        assert_eq!((p.planning_ms, p.execution_ms), (Some(0.1), Some(0.5)));
        let root = &p.roots[0];
        assert_eq!(root.operation, "Hash Join");
        assert_eq!(root.detail, "join: Inner; hash cond: (b.a = a.id)");
        assert_eq!((root.total_cost, root.rows, root.actual_rows), (Some(30.25), Some(12.0), Some(10.0)));
        assert_eq!(root.children[0].relation.as_deref(), Some("public.books b"));
        assert_eq!(root.children[0].detail, "filter: (id > 3)");
        assert_eq!(root.children[1].children[0].detail, "index: authors_pkey");
        assert_eq!(root.count(), 4);
        assert!(parse_postgres("not json").is_err());
        assert!(parse_postgres("[{}]").is_err());
    }

    #[test]
    fn mysql_json_and_tree() {
        let json = r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "1.60"},
            "ordering_operation": {"using_filesort": true, "nested_loop": [
                {"table": {"table_name": "b", "access_type": "ALL", "rows_examined_per_scan": 4,
                           "cost_info": {"prefix_cost": "0.65"}, "attached_condition": "(b.id > 1)"}},
                {"table": {"table_name": "a", "access_type": "eq_ref", "key": "PRIMARY", "rows_examined_per_scan": 1}}]}}}"#;
        let p = parse_mysql_json(json).unwrap();
        let qb = &p.roots[0];
        assert_eq!((qb.operation.as_str(), qb.total_cost), ("Query block #1", Some(1.6)));
        let sort = &qb.children[0];
        assert_eq!(sort.operation, "Sort");
        assert!(sort.detail.contains("using filesort"));
        let nl = &sort.children[0];
        assert_eq!(nl.operation, "Nested loop");
        assert_eq!(nl.children[0].relation.as_deref(), Some("b"));
        assert_eq!(nl.children[0].operation, "Table scan");
        assert_eq!(nl.children[0].rows, Some(4.0));
        assert_eq!(nl.children[1].operation, "Index lookup (eq_ref)");
        assert!(!p.analyzed);
        assert!(parse_mysql_json("[1]").is_err());

        let tree = "-> Nested loop inner join  (cost=1.1 rows=2) (actual time=0.03..0.0418 rows=2 loops=1)\n    \
                    -> Filter: (b.author_id is not null)  (cost=0.45 rows=2) (actual time=0.02..0.025 rows=2 loops=1)\n        \
                    -> Table scan on b  (cost=0.45 rows=2) (actual time=0.019..0.023 rows=2 loops=1)\n    \
                    -> Single-row index lookup on a using PRIMARY (id=b.author_id)  (cost=0.3 rows=1) (actual time=0.004..0.004 rows=1 loops=2)\n";
        let p = parse_mysql_tree(tree);
        assert!(p.analyzed);
        let root = &p.roots[0];
        assert_eq!(root.operation, "Nested loop inner join");
        assert_eq!((root.total_cost, root.rows, root.actual_time_ms), (Some(1.1), Some(2.0), Some(0.0418)));
        assert_eq!(root.children.len(), 2);
        assert_eq!(root.children[0].children[0].relation.as_deref(), Some("b"));
        assert_eq!(root.children[1].relation.as_deref(), Some("a"));
        assert_eq!(root.children[1].loops, Some(2.0));
        assert!(parse_mysql_tree("").roots.is_empty());
    }
}
