//! C and C++ values under LLDB (M15 track DM, QA #23): a reference (`const Item& it`, `int& n`)
//! reads as the value it refers to, like CLion shows it, instead of its address. lldb-dap gives
//! a reference the referenced object's members as children (or one `&n` child for a scalar), so
//! those children are fetched once the scope's variables arrive and the value becomes `3` or
//! `{id=7, name="apple"}`; expanding it still shows the members.

use serde_json::Value;

use codecraft_dap as dap;

use super::{Pending, VariableInfo};
use crate::Session;

/// References summarised per scope (the rest keep LLDB's text).
const MAX_PER_SCOPE: usize = 64;
const MAX_ITEMS: usize = 10;
const MAX_SUMMARY_CHARS: usize = 120;

/// Where a summary goes: the variable `name` (with `reference`) among the children of `parent`.
#[derive(Clone, Debug)]
pub(crate) struct Slot {
    pub parent: i64,
    pub name: String,
    pub reference: i64,
}

/// A C++ reference type (`const Item &`, `int &&`).
pub fn is_reference_type(ty: &str) -> bool {
    let t = ty.trim();
    t.ends_with('&') && !t.ends_with("&&&")
}

fn is_bare_pointer(value: &str) -> bool {
    value.strip_prefix("0x").is_some_and(|hex| !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Ask for the children of the references among `vars` that read as an address.
pub(crate) fn summarize(s: &mut Session, parent: i64, vars: &[VariableInfo]) {
    let wanted: Vec<Slot> = vars
        .iter()
        .filter(|v| v.reference > 0 && is_bare_pointer(v.value.trim()))
        .filter(|v| v.type_name.as_deref().is_some_and(is_reference_type))
        .take(MAX_PER_SCOPE)
        .map(|v| Slot { parent, name: v.name.clone(), reference: v.reference })
        .collect();
    for slot in wanted {
        let Some(rt) = s.debug.rt.as_mut() else { return };
        let epoch = rt.epoch;
        let reference = slot.reference;
        let request = super::actions::ActionRequest::CppValue { slot };
        rt.request(Pending::Action { epoch, request }, |c| c.variables(reference));
    }
}

/// The referenced value from the reference's children: a scalar's one `&n` (or `*n`) child, or
/// the members of an object.
pub fn referenced_value(kids: &[dap::Variable]) -> Option<String> {
    match kids {
        [] => None,
        [only] if only.name.starts_with('&') || only.name.starts_with('*') => {
            let v = only.value.trim();
            (!v.is_empty()).then(|| v.to_owned())
        }
        _ => {
            let mut items: Vec<String> = kids
                .iter()
                .take(MAX_ITEMS)
                .map(|k| {
                    let v = k.value.trim();
                    let v = if v.is_empty() || v == k.type_.as_deref().unwrap_or_default() { "{…}" } else { v };
                    format!("{}={v}", k.name)
                })
                .collect();
            if kids.len() > MAX_ITEMS {
                items.push("…".into());
            }
            let text = format!("{{{}}}", items.join(", "));
            let mut out: String = text.chars().take(MAX_SUMMARY_CHARS).collect();
            if text.chars().count() > MAX_SUMMARY_CHARS {
                out.push('…');
            }
            Some(out)
        }
    }
}

/// The answer to one of the requests above.
pub(crate) fn on_response(s: &mut Session, current: bool, slot: Slot, result: Result<Value, String>) {
    if !current {
        return;
    }
    let kids = result
        .ok()
        .map(|body| dap::types::parse::<dap::VariablesBody>(&body).unwrap_or_default().variables)
        .unwrap_or_default();
    let Some(value) = referenced_value(&kids) else { return };
    let Some(session) = s.debug.session.as_mut() else { return };
    let Some(v) = session
        .variables
        .get_mut(&slot.parent)
        .and_then(|vars| vars.iter_mut().find(|v| v.name == slot.name && v.reference == slot.reference))
    else {
        return;
    };
    v.value = value;
    let is_locals = session.scopes.first().is_some_and(|x| x.reference == slot.parent);
    if is_locals && let Some(vars) = session.variables.get(&slot.parent).cloned() {
        super::compute_inline_values(s, &vars);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, ty: &str, value: &str) -> dap::Variable {
        dap::Variable { name: name.into(), value: value.into(), type_: Some(ty.into()), ..Default::default() }
    }

    #[test]
    fn references_read_as_what_they_refer_to() {
        assert!(is_reference_type("const Item &"));
        assert!(is_reference_type("int &&"));
        assert!(!is_reference_type("int *"));
        assert!(!is_reference_type("std::string"));
        assert_eq!(referenced_value(&[var("&n", "int", "3")]).as_deref(), Some("3"));
        assert_eq!(referenced_value(&[var("*p", "int", "4")]).as_deref(), Some("4"));
        let item = [var("id", "int", "7"), var("name", "std::string", "\"apple\""), var("pos", "Point", "")];
        assert_eq!(referenced_value(&item).as_deref(), Some("{id=7, name=\"apple\", pos={…}}"));
        assert!(referenced_value(&[]).is_none());
        assert!(referenced_value(&[var("&n", "int", "  ")]).is_none());
        let many: Vec<dap::Variable> = (0..30).map(|i| var(&format!("f{i}"), "int", &"9".repeat(20))).collect();
        let long = referenced_value(&many).unwrap();
        assert!(long.chars().count() <= MAX_SUMMARY_CHARS + 1 && long.ends_with('…'));
        // A single member that is not the `&n` child is an object with one field.
        assert_eq!(referenced_value(&[var("id", "int", "1")]).as_deref(), Some("{id=1}"));
    }
}
