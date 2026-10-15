//! Python values made readable in the Variables view and for agents, like PyCharm's: a pandas
//! `DataFrame` reads `DataFrame 3 rows × 2 columns: a, b` (not its multi-line table), a `Series`
//! `Series 3 values, int64, name a`, a numpy array `ndarray shape (2, 3), float64`, and a plain
//! object `<__main__.Point object at 0x…>` reads `Point(x=1, y=2)` from its attributes.
//!
//! The shapes come from evaluating a small expression on the variable (`evaluateName`) in the
//! selected frame; the attributes from its children. Values that already read well are left.

use serde_json::Value;

use codecraft_dap as dap;

use super::{Pending, VariableInfo};
use crate::Session;

const MAX_PER_SCOPE: usize = 64;
const MAX_ATTRIBUTES: usize = 6;
const MAX_SUMMARY_CHARS: usize = 120;

#[derive(Clone, Debug)]
pub(crate) struct Slot {
    pub parent: i64,
    pub name: String,
    pub reference: i64,
}

#[derive(Clone, Debug)]
pub(crate) enum Stage {
    /// The answer of a shape expression (DataFrame, Series, ndarray).
    Shape,
    /// The object's attributes (its children).
    Attributes { class: String },
}

/// The expression that describes a pandas or numpy value, by its type.
pub fn shape_expression(type_name: &str, expr: &str) -> Option<String> {
    let e = format!("({expr})");
    match type_name {
        "DataFrame" => Some(format!(
            "f\"DataFrame {{{e}.shape[0]}} rows × {{{e}.shape[1]}} columns: {{', '.join(map(str, list({e}.columns)[:8]))}}{{'…' if {e}.shape[1] > 8 else ''}}\""
        )),
        "Series" => Some(format!("f\"Series {{len({e})}} values, {{{e}.dtype}}, name {{{e}.name}}\"")),
        "ndarray" => Some(format!("f\"ndarray shape {{{e}.shape}}, {{{e}.dtype}}\"")),
        _ => None,
    }
}

/// `<__main__.Point object at 0x10c3>` → `Point`.
pub fn default_repr_class(value: &str) -> Option<String> {
    let inner = value.strip_prefix('<')?.strip_suffix('>')?;
    let (path, _) = inner.split_once(" object at 0x")?;
    let class = path.rsplit('.').next().unwrap_or(path);
    (!class.is_empty() && class.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| class.to_owned())
}

/// Ask for summaries of the variables of `parent` that need one.
pub(crate) fn summarize(s: &mut Session, parent: i64, vars: &[VariableInfo]) {
    let frame = s.debug.session.as_ref().and_then(|x| x.selected_frame()).map(|f| f.id);
    let mut sent = 0;
    for v in vars {
        if sent >= MAX_PER_SCOPE {
            break;
        }
        let slot = Slot { parent, name: v.name.clone(), reference: v.reference };
        let shape = match (v.type_name.as_deref(), v.evaluate_name.as_deref()) {
            (Some(t), Some(e)) if !e.is_empty() => shape_expression(t, e),
            _ => None,
        };
        let Some(rt) = s.debug.rt.as_mut() else { return };
        let epoch = rt.epoch;
        if let Some(expression) = shape {
            let request = super::actions::ActionRequest::PythonValue { slot, stage: Stage::Shape };
            rt.request(Pending::Action { epoch, request }, |c| c.evaluate(&expression, frame, "watch"));
            sent += 1;
        } else if v.reference > 0
            && let Some(class) = default_repr_class(&v.value)
        {
            let request = super::actions::ActionRequest::PythonValue { slot, stage: Stage::Attributes { class } };
            let reference = v.reference;
            rt.request(Pending::Action { epoch, request }, |c| c.variables(reference));
            sent += 1;
        }
    }
}

/// A Python string literal's text (`'a b'` → `a b`).
fn unquote(text: &str) -> String {
    let t = text.trim();
    for q in ['\'', '"'] {
        if let Some(inner) = t.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner.replace("\\'", "'").replace("\\\"", "\"");
        }
    }
    t.to_owned()
}

/// debugpy's grouping nodes (`special variables`, `function variables`, `class variables`,
/// `protected variables`): structure for the Variables view, noise in an agent's `locals` (QA
/// M15: every Python stop summary started with `{"name": "special variables", "value": ""}`).
pub(crate) fn is_grouping_node(name: &str, value: &str) -> bool {
    value.is_empty()
        && matches!(name, "special variables" | "function variables" | "class variables" | "protected variables")
}

/// `Point(x=1, y=2)` from an object's children (without debugpy's grouping nodes and dunders).
pub fn attributes_summary(class: &str, children: &[(String, String)]) -> String {
    let attributes: Vec<String> = children
        .iter()
        .filter(|(n, _)| !n.contains(' ') && !n.starts_with("__") && !n.is_empty())
        .take(MAX_ATTRIBUTES + 1)
        .enumerate()
        .map(|(i, (n, v))| if i == MAX_ATTRIBUTES { "…".to_owned() } else { format!("{n}={v}") })
        .collect();
    shorten(&format!("{class}({})", attributes.join(", ")))
}

fn shorten(text: &str) -> String {
    let flat = text.replace('\n', " ");
    let mut out: String = flat.chars().take(MAX_SUMMARY_CHARS).collect();
    if flat.chars().count() > MAX_SUMMARY_CHARS {
        out.push('…');
    }
    out
}

pub(crate) fn on_response(s: &mut Session, current: bool, slot: Slot, stage: Stage, result: Result<Value, String>) {
    if !current {
        return;
    }
    let Ok(body) = result else { return };
    let summary = match stage {
        Stage::Shape => {
            let body: dap::EvaluateBody = dap::types::parse(&body).unwrap_or_default();
            shorten(&unquote(&body.result))
        }
        Stage::Attributes { class } => {
            let kids: Vec<(String, String)> = dap::types::parse::<dap::VariablesBody>(&body)
                .unwrap_or_default()
                .variables
                .into_iter()
                .map(|v| (v.name, v.value))
                .collect();
            attributes_summary(&class, &kids)
        }
    };
    if summary.is_empty() {
        return;
    }
    let Some(session) = s.debug.session.as_mut() else { return };
    let Some(v) = session
        .variables
        .get_mut(&slot.parent)
        .and_then(|vars| vars.iter_mut().find(|v| v.name == slot.name && v.reference == slot.reference))
    else {
        return;
    };
    v.value = summary;
    let is_locals = session.scopes.first().is_some_and(|x| x.reference == slot.parent);
    if is_locals && let Some(vars) = session.variables.get(&slot.parent).cloned() {
        super::compute_inline_values(s, &vars);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_and_attributes() {
        assert!(shape_expression("DataFrame", "df").unwrap().contains("(df).shape[0]"));
        assert!(shape_expression("Series", "s").is_some());
        assert!(shape_expression("ndarray", "a").is_some());
        assert!(shape_expression("dict", "d").is_none());
        assert_eq!(default_repr_class("<__main__.Point object at 0x11cf5fac0>").as_deref(), Some("Point"));
        assert_eq!(default_repr_class("<pkg.mod.Thing object at 0x1>").as_deref(), Some("Thing"));
        assert!(default_repr_class("Point(x=1)").is_none());
        assert!(default_repr_class("<module 'os'>").is_none());
        let kids = vec![
            ("special variables".to_owned(), String::new()),
            ("function variables".to_owned(), String::new()),
            ("x".to_owned(), "1".to_owned()),
            ("y".to_owned(), "'a'".to_owned()),
        ];
        assert_eq!(attributes_summary("Point", &kids), "Point(x=1, y='a')");
        let many: Vec<(String, String)> = (0..10).map(|i| (format!("a{i}"), i.to_string())).collect();
        assert!(attributes_summary("Big", &many).ends_with("…)"));
        assert_eq!(unquote("'3 rows × 2'"), "3 rows × 2");
        assert_eq!(shorten("a\nb"), "a b");
    }

    #[test]
    fn grouping_nodes_are_recognised() {
        assert!(is_grouping_node("special variables", ""));
        assert!(is_grouping_node("function variables", ""));
        assert!(!is_grouping_node("special variables", "1"), "a real variable with that name");
        assert!(!is_grouping_node("x", ""));
    }
}
